//! Serialize SQLite connection lifetimes inside this process. Native and task
//! transactions nest on one thread; other threads must not enter the VFS while
//! a connection is being opened, used or closed.
use rusqlite::Connection as SqliteConnection;
use std::{
    cell::RefCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    path::Path,
    rc::Rc,
    sync::{Mutex, MutexGuard},
};

static DATABASE: Mutex<()> = Mutex::new(());
#[derive(Default)]
struct ThreadState {
    depth: usize,
    lock: Option<MutexGuard<'static, ()>>,
}
thread_local! {
    static STATE: RefCell<ThreadState> = RefCell::new(ThreadState::default());
}
struct Guard(PhantomData<Rc<()>>);
impl Guard {
    fn enter() -> Self {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            if state.depth == 0 {
                state.lock = Some(DATABASE.lock().unwrap_or_else(|e| e.into_inner()));
            }
            state.depth += 1;
        });
        Self(PhantomData)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.depth -= 1;
            if state.depth == 0 {
                state.lock.take();
            }
        });
    }
}

pub(crate) struct Connection {
    // Fields drop in declaration order: SQLite closes before the guard unlocks.
    inner: SqliteConnection,
    _guard: Guard,
}
impl Connection {
    pub(crate) fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let guard = Guard::enter();
        Ok(Self {
            inner: SqliteConnection::open(path)?,
            _guard: guard,
        })
    }
}
impl Deref for Connection {
    type Target = SqliteConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DerefMut for Connection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_database_work_does_not_block_the_http_reactor() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        std::fs::write(home.join("provider.key"), "fixture-key").unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let state = home.clone();
        let server = tokio::spawn(async move { crate::provider::serve(&state, address).await });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(700))
            .build()
            .unwrap();
        let url = format!("http://{address}");
        let mut started = false;
        for _ in 0..100 {
            if client.get(format!("{url}/version")).send().await.is_ok() {
                started = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(started, "provider failed to start");
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let database_thread = std::thread::spawn(move || {
            let connection = Connection::open(home.join("registry.sqlite")).unwrap();
            held_tx.send(()).unwrap();
            // A failing assertion must never leave the test process deadlocked.
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
            drop(connection);
        });
        held_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let native_client = client.clone();
        let native_url = url.clone();
        let native = tokio::spawn(async move {
            native_client
                .post(format!("{native_url}/responses"))
                .header(crate::provider::KEY_HEADER, "fixture-key")
                .body(r#"{"model":"agentrelay-claude"}"#)
                .send()
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let health = client.get(format!("{url}/version")).send().await;
        let rejection = client
            .post(format!("{url}/responses"))
            .body("{}")
            .send()
            .await;
        release_tx.send(()).unwrap();
        database_thread.join().unwrap();
        let _ = native.await;
        server.abort();
        assert_eq!(health.unwrap().status(), reqwest::StatusCode::OK);
        assert_eq!(
            rejection.unwrap().status(),
            reqwest::StatusCode::BAD_REQUEST
        );
    }
    #[test]
    fn nested_connections_release_only_after_the_last_connection_closes() {
        let outer = Connection::open(":memory:").unwrap();
        let inner = Connection::open(":memory:").unwrap();
        drop(outer);
        STATE.with(|s| assert_eq!(s.borrow().depth, 1));
        inner.execute_batch("CREATE TABLE kept (id)").unwrap();
        drop(inner);
        STATE.with(|s| {
            assert_eq!(s.borrow().depth, 0);
            assert!(s.borrow().lock.is_none());
        });
        assert!(Connection::open("/nonexistent-agentrelay-parent/database.sqlite").is_err());
        STATE.with(|s| assert_eq!(s.borrow().depth, 0));
    }
    #[test]
    fn concurrent_connection_open_use_and_close_are_serialized_across_both_registries() {
        let dir = tempfile::tempdir().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let mut threads = vec![];
        for _ in 0..8 {
            let home = dir.path().to_path_buf();
            let active = active.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    let mut native = Connection::open(home.join("native.sqlite")).unwrap();
                    assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
                    native
                        .execute_batch("CREATE TABLE IF NOT EXISTS native (id)")
                        .unwrap();
                    let tx = native.transaction().unwrap();
                    let tasks = Connection::open(home.join("registry.sqlite")).unwrap();
                    tasks
                        .execute_batch(
                            "CREATE TABLE IF NOT EXISTS tasks (id); INSERT INTO tasks VALUES (1)",
                        )
                        .unwrap();
                    drop(tasks);
                    tx.commit().unwrap();
                    assert_eq!(active.fetch_sub(1, Ordering::SeqCst), 1);
                    drop(native);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let conn = Connection::open(dir.path().join("registry.sqlite")).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            400
        );
    }
}

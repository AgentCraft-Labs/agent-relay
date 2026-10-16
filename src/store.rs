use crate::database::Connection;
use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Store {
    pub home: PathBuf,
}

impl Store {
    pub fn open(home: &Path) -> Result<Self> {
        std::fs::create_dir_all(home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
        }
        let store = Self {
            home: home.to_owned(),
        };
        let conn = store.conn()?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version <= 1,
            "Registry schema {version} is newer than this executable"
        );
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, body TEXT NOT NULL, token TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS operations (id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, task_id TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events (sequence INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, generation INTEGER NOT NULL, timestamp INTEGER NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS events_task ON events(task_id, sequence);
             CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, body TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
             PRAGMA user_version=1;",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                store.home.join("registry.sqlite"),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }
        Ok(store)
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(self.home.join("registry.sqlite"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    pub fn get(&self, id: &str) -> Result<Task> {
        let conn = self.conn()?;
        read_task(&conn, id)
    }

    pub fn list(&self, root: Option<&str>) -> Result<Vec<Task>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT body FROM tasks ORDER BY rowid DESC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut tasks = Vec::new();
        for row in rows {
            let task: Task = serde_json::from_str(&row?)?;
            if root.is_none() || task.request.root_id.as_deref() == root {
                tasks.push(task);
            }
        }
        Ok(tasks)
    }

    pub fn begin(&self, key: &str, request: RunRequest) -> Result<Claim> {
        validate_key(key)?;
        validate_request(&request)?;
        let fingerprint = fingerprint(&request)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = format!("run:{key}");
        if let Some(task) = existing_operation(&tx, &operation, &fingerprint)? {
            return Ok(Claim {
                task,
                token: String::new(),
                execute: false,
            });
        }
        let active = self.list_active_in(&tx)?;
        ensure!(active < 8, "At most 8 runners may execute concurrently");
        let now = now_ms();
        let task = Task {
            id: Uuid::new_v4().to_string(),
            generation: 1,
            status: Status::Running,
            request,
            session_id: None,
            actual_model: None,
            result: None,
            error: None,
            cancel_requested: false,
            created_at: now,
            updated_at: now,
            heartbeat_at: now,
            runner_pid: Some(std::process::id()),
            worker: None,
        };
        let token = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO tasks(id,body,token) VALUES(?1,?2,?3)",
            params![task.id, serde_json::to_string(&task)?, token],
        )?;
        save_operation(&tx, &operation, &fingerprint, &task.id)?;
        append_event(
            &tx,
            &task,
            "accepted",
            json!({"runtime":task.request.runtime, "agent_path":task.request.agent_path}),
        )?;
        tx.commit()?;
        Ok(Claim {
            task,
            token,
            execute: true,
        })
    }

    pub fn resume(&self, id: &str, key: &str, prompt: &str) -> Result<Claim> {
        validate_key(key)?;
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= MAX_FRAME,
            "Prompt must contain 1..={MAX_FRAME} bytes"
        );
        let fingerprint = fingerprint(&json!({"task":id,"prompt":prompt}))?;
        let operation = format!("resume:{id}:{key}");
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(task) = existing_operation(&tx, &operation, &fingerprint)? {
            return Ok(Claim {
                task,
                token: String::new(),
                execute: false,
            });
        }
        let mut task = read_task(&tx, id)?;
        ensure!(
            task.status.terminal(),
            "Task is running; use message to queue input or interrupt it first"
        );
        ensure!(
            task.session_id.is_some(),
            "Runtime has not provided a resumable session ID"
        );
        if let Some(pid) = task.runner_pid {
            ensure!(
                !crate::runner::pid_exists(pid),
                "Previous runner is still alive; wait for cleanup before resuming"
            );
        }
        ensure!(
            self.list_active_in(&tx)? < 8,
            "At most 8 runners may execute concurrently"
        );
        let mut messages = Vec::new();
        {
            let mut stmt = tx
                .prepare("SELECT body FROM messages WHERE task_id=?1 AND consumed=0 ORDER BY id")?;
            for message in stmt.query_map([id], |row| row.get::<_, String>(0))? {
                messages.push(message?);
            }
        }
        let combined = if messages.is_empty() {
            prompt.to_owned()
        } else {
            format!(
                "Queued messages from the parent:\n{}\n\nNew task:\n{prompt}",
                messages.join("\n\n")
            )
        };
        ensure!(
            combined.len() <= MAX_FRAME,
            "Combined queued messages exceed the prompt limit"
        );
        tx.execute(
            "UPDATE messages SET consumed=1 WHERE task_id=?1 AND consumed=0",
            [id],
        )?;
        task.request.prompt = combined;
        task.generation = task
            .generation
            .checked_add(1)
            .context("Generation exhausted")?;
        task.status = Status::Running;
        task.result = None;
        task.error = None;
        task.cancel_requested = false;
        task.worker = None;
        task.runner_pid = Some(std::process::id());
        task.heartbeat_at = now_ms();
        let token = Uuid::new_v4().to_string();
        write_task(&tx, &mut task)?;
        tx.execute("UPDATE tasks SET token=?2 WHERE id=?1", params![id, token])?;
        save_operation(&tx, &operation, &fingerprint, id)?;
        append_event(
            &tx,
            &task,
            "resumed",
            json!({"session_id":task.session_id,"queued_messages":messages.len()}),
        )?;
        tx.commit()?;
        Ok(Claim {
            task,
            token,
            execute: true,
        })
    }

    fn list_active_in(&self, conn: &rusqlite::Connection) -> Result<usize> {
        let mut stmt = conn.prepare("SELECT body FROM tasks")?;
        let mut count = 0;
        for body in stmt.query_map([], |row| row.get::<_, String>(0))? {
            let task: Task = serde_json::from_str(&body?)?;
            if task.status == Status::Running {
                count += 1;
            }
        }
        Ok(count)
    }

    pub fn message(&self, id: &str, key: &str, message: &str) -> Result<Value> {
        validate_key(key)?;
        ensure!(
            !message.trim().is_empty() && message.len() <= MAX_FRAME,
            "Message must contain 1..={MAX_FRAME} bytes"
        );
        let fingerprint = fingerprint(&json!({"task":id,"message":message}))?;
        let operation = format!("message:{id}:{key}");
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate = existing_operation(&tx, &operation, &fingerprint)?.is_some();
        let task = read_task(&tx, id)?;
        if !duplicate {
            tx.execute(
                "INSERT INTO messages(task_id,body) VALUES(?1,?2)",
                params![id, message],
            )?;
            save_operation(&tx, &operation, &fingerprint, id)?;
            append_event(
                &tx,
                &task,
                "message_queued",
                json!({"delivery":"next_turn"}),
            )?;
        }
        tx.commit()?;
        Ok(
            json!({"task_id":id,"accepted":true,"duplicate":duplicate,"delivery":"next_turn","starts_idle_task":false}),
        )
    }

    pub fn interrupt(&self, id: &str, reason: &str) -> Result<Task> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = read_task(&tx, id)?;
        if task.status == Status::Running && !task.cancel_requested {
            task.cancel_requested = true;
            task.error = Some(reason.to_owned());
            write_task(&tx, &mut task)?;
            append_event(&tx, &task, "interrupt_requested", json!({"reason":reason}))?;
        }
        tx.commit()?;
        Ok(task)
    }

    pub fn heartbeat(&self, claim: &Claim, worker: Option<ProcessIdentity>) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = checked_task(&tx, claim)?;
        if task.status != Status::Running {
            return Ok(false);
        }
        task.heartbeat_at = now_ms();
        if worker.is_some() {
            task.worker = worker;
        }
        write_task(&tx, &mut task)?;
        tx.commit()?;
        Ok(!task.cancel_requested)
    }

    pub fn observe(&self, claim: &Claim, event: RuntimeEvent) -> Result<Event> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = checked_task(&tx, claim)?;
        ensure!(
            task.status == Status::Running && !task.cancel_requested,
            "Task no longer accepts runtime output"
        );
        let (kind, data) = match event {
            RuntimeEvent::Model { id } => {
                task.actual_model = Some(id.clone());
                ("model", json!({"model":id}))
            }
            RuntimeEvent::Session { id, model } => {
                if let Some(previous) = &task.session_id {
                    ensure!(previous == &id, "Runtime changed its session identity");
                }
                task.session_id = Some(id.clone());
                if model.is_some() {
                    task.actual_model = model.clone();
                }
                ("session", json!({"session_id":id,"model":model}))
            }
            RuntimeEvent::Text(text) => ("text", json!({"text":text})),
            RuntimeEvent::Tool(data) => ("tool", data),
            RuntimeEvent::Usage(data) => ("usage", data),
            RuntimeEvent::Diagnostic(data) => ("diagnostic", data),
            RuntimeEvent::Result { text, session_id } => {
                if let Some(id) = session_id {
                    if let Some(previous) = &task.session_id {
                        ensure!(previous == &id, "Runtime changed its session identity");
                    }
                    task.session_id = Some(id);
                }
                ("runtime_result", json!({"text":text}))
            }
            RuntimeEvent::Error(error) => ("runtime_error", json!({"error":error})),
        };
        write_task(&tx, &mut task)?;
        let event = append_event(&tx, &task, kind, data)?;
        tx.commit()?;
        Ok(event)
    }

    pub fn finish(
        &self,
        claim: &Claim,
        status: Status,
        result: Option<String>,
        error: Option<String>,
    ) -> Result<Task> {
        ensure!(status.terminal(), "Finish requires a terminal state");
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = checked_task(&tx, claim)?;
        if task.status.terminal() {
            task.runner_pid = None;
            task.worker = None;
            write_task(&tx, &mut task)?;
            tx.commit()?;
            return Ok(task);
        }
        task.status = if task.cancel_requested {
            Status::Interrupted
        } else {
            status
        };
        task.result = if task.status == Status::Completed {
            result
        } else {
            None
        };
        if error.is_some() {
            task.error = error;
        }
        task.runner_pid = None;
        task.worker = None;
        write_task(&tx, &mut task)?;
        append_event(
            &tx,
            &task,
            "finished",
            json!({"status":task.status,"result":task.result,"error":task.error}),
        )?;
        tx.commit()?;
        Ok(task)
    }

    pub fn events(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        self.get(id)?;
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT sequence,generation,timestamp,kind,data FROM events WHERE task_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 500")?;
        let rows = stmt.query_map(params![id, after], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (sequence, generation, timestamp, kind, data) = row?;
            events.push(Event {
                sequence,
                task_id: id.to_owned(),
                generation,
                timestamp,
                kind,
                data: serde_json::from_str(&data)?,
            });
        }
        Ok(events)
    }

    pub fn reconcile(&self, at: i64) -> Result<usize> {
        let mut recovered = 0;
        for task in self.list(None)? {
            if task.status != Status::Running || at - task.heartbeat_at <= LEASE_MS {
                continue;
            }
            let mut conn = self.conn()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = read_task(&tx, &task.id)?;
            if current.generation != task.generation
                || current.status != Status::Running
                || at - current.heartbeat_at <= LEASE_MS
            {
                continue;
            }
            if let Some(worker) = &current.worker {
                crate::runner::kill_verified_worker(worker);
            }
            current.status = Status::Interrupted;
            current.cancel_requested = true;
            current.error =
                Some("Runner lease expired; session may be resumed after cleanup".into());
            current.worker = None;
            // Preserve runner PID so a new generation cannot race a live stale runner.
            write_task(&tx, &mut current)?;
            append_event(
                &tx,
                &current,
                "lease_expired",
                json!({"status":current.status}),
            )?;
            tx.commit()?;
            recovered += 1;
        }
        Ok(recovered)
    }
}

fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.len() <= 256,
        "Idempotency key must contain 1..=256 bytes"
    );
    Ok(())
}

fn validate_request(request: &RunRequest) -> Result<()> {
    ensure!(
        !request.runtime.trim().is_empty(),
        "Runtime name is required"
    );
    ensure!(
        !request.prompt.trim().is_empty() && request.prompt.len() <= MAX_FRAME,
        "Prompt must contain 1..={MAX_FRAME} bytes"
    );
    ensure!(
        (1..=86_400).contains(&request.timeout_secs),
        "Timeout must be between 1 and 86400 seconds"
    );
    Ok(())
}

fn fingerprint(value: &impl serde::Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

fn read_task(conn: &rusqlite::Connection, id: &str) -> Result<Task> {
    let body: Option<String> = conn
        .query_row("SELECT body FROM tasks WHERE id=?1", [id], |row| row.get(0))
        .optional()?;
    serde_json::from_str(&body.with_context(|| format!("Unknown task {id}"))?)
        .context("Invalid registry task")
}

fn checked_task(conn: &rusqlite::Connection, claim: &Claim) -> Result<Task> {
    let task = read_task(conn, &claim.task.id)?;
    let token: String =
        conn.query_row("SELECT token FROM tasks WHERE id=?1", [&task.id], |row| {
            row.get(0)
        })?;
    ensure!(
        task.generation == claim.task.generation && token == claim.token,
        "Stale runner generation or invalid ownership token"
    );
    Ok(task)
}

fn write_task(conn: &rusqlite::Connection, task: &mut Task) -> Result<()> {
    task.updated_at = now_ms();
    conn.execute(
        "UPDATE tasks SET body=?2 WHERE id=?1",
        params![task.id, serde_json::to_string(task)?],
    )?;
    Ok(())
}

fn existing_operation(
    conn: &rusqlite::Connection,
    id: &str,
    fingerprint: &str,
) -> Result<Option<Task>> {
    let existing = conn
        .query_row(
            "SELECT fingerprint,task_id FROM operations WHERE id=?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((previous, task_id)) = existing {
        if previous != fingerprint {
            bail!("Idempotency key already belongs to a different request");
        }
        return Ok(Some(read_task(conn, &task_id)?));
    }
    Ok(None)
}

fn save_operation(
    conn: &rusqlite::Connection,
    id: &str,
    fingerprint: &str,
    task_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO operations(id,fingerprint,task_id) VALUES(?1,?2,?3)",
        params![id, fingerprint, task_id],
    )?;
    Ok(())
}

fn append_event(
    conn: &rusqlite::Connection,
    task: &Task,
    kind: &str,
    data: Value,
) -> Result<Event> {
    let encoded = serde_json::to_string(&data)?;
    ensure!(
        encoded.len() <= MAX_FRAME,
        "Event exceeds {MAX_FRAME} bytes"
    );
    let timestamp = now_ms();
    conn.execute(
        "INSERT INTO events(task_id,generation,timestamp,kind,data) VALUES(?1,?2,?3,?4,?5)",
        params![task.id, task.generation, timestamp, kind, encoded],
    )?;
    Ok(Event {
        sequence: conn.last_insert_rowid(),
        task_id: task.id.clone(),
        generation: task.generation,
        timestamp,
        kind: kind.to_owned(),
        data,
    })
}

#![cfg(target_os = "macos")]
use anyhow::{Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[test]
fn executable_resolution_rejects_scripts_data_and_non_executable_files() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    let script = home.path().join("node-launcher");
    std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('codex');\n")?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    assert!(agent_relay::openai_bridge::executable(Some(&script)).is_err());
    let data = home.path().join("data");
    std::fs::write(&data, b"not a native executable")?;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755))?;
    assert!(agent_relay::openai_bridge::executable(Some(&data)).is_err());
    let native = home.path().join("native");
    std::fs::copy(std::env::current_exe()?, &native)?;
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o644))?;
    assert!(agent_relay::openai_bridge::executable(Some(&native)).is_err());
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o755))?;
    assert_eq!(
        agent_relay::openai_bridge::executable(Some(&native))?,
        native.canonicalize()?
    );
    Ok(())
}

#[test]
fn kernel_peer_requires_exact_executable_and_tcp_tuple() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let client = TcpStream::connect(listener.local_addr()?)?;
    let (server, peer) = listener.accept()?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let local = server.local_addr()?;
    assert_eq!(
        agent_relay::openai_bridge::verify_peer(peer, local, &executable)?,
        std::process::id()
    );
    assert!(
        agent_relay::openai_bridge::verify_peer(peer, local, std::path::Path::new("/usr/bin/curl"))
            .is_err()
    );
    assert!(agent_relay::openai_bridge::verify_peer(local, peer, &executable).is_ok());
    let mut wrong = peer;
    wrong.set_port(peer.port().wrapping_add(1));
    assert!(agent_relay::openai_bridge::verify_peer(wrong, local, &executable).is_err());
    drop(client);
    Ok(())
}

#[tokio::test]
async fn bridge_refuses_legacy_backends_and_nonlocal_endpoints() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("provider.key"), "fixture-key")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let backend = listener.local_addr()?;
    let router = Router::new().route(
        "/version",
        get(|| async { axum::Json(json!({"service":"AgentRelay","version":"0.3.4"})) }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    let exe = std::env::current_exe()?;
    let error =
        agent_relay::openai_bridge::serve(home.path(), "127.0.0.1:0".parse()?, backend, &exe)
            .await
            .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("lacks native checkpoint support")
    );
    assert!(
        agent_relay::openai_bridge::serve(home.path(), "0.0.0.0:0".parse()?, backend, &exe)
            .await
            .is_err()
    );
    assert!(
        agent_relay::openai_bridge::serve(
            home.path(),
            "127.0.0.1:0".parse()?,
            "192.0.2.1:1234".parse()?,
            &exe
        )
        .await
        .is_err()
    );
    task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_admits_kernel_owner_rejects_spoofing_and_preserves_transport() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("provider.key"), "private-fixture-key")?;
    let records = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = records.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_backend = cancelled.clone();
    let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let backend = backend_listener.local_addr()?;
    let router = Router::new()
        .route(
            "/version",
            get(|| async {
                axum::Json(json!({"service":"AgentRelay","native_checkpoints_v1":true}))
            }),
        )
        .route(
            "/responses",
            post(move |headers: HeaderMap, body: Bytes| {
                let observed = observed.clone();
                let cancelled = cancelled_backend.clone();
                async move {
                    let decoded = serde_json::from_slice::<Value>(&body).unwrap();
                    observed.lock().unwrap().push(json!({
                "key": headers.get(agent_relay::provider::KEY_HEADER).unwrap().to_str().unwrap(),
                "authorization": headers.get("authorization").unwrap().to_str().unwrap(),
                "encoding": headers.get("content-encoding").map(|v|v.to_str().unwrap()),
                    "body": decoded,
            }));
                    if decoded["hold"] == true {
                        let (tx, rx) =
                            tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
                        tokio::spawn(async move {
                            let _ = tx
                                .send(Ok(Bytes::from_static(b"event: started\ndata: {}\n\n")))
                                .await;
                            tx.closed().await;
                            cancelled.store(true, Ordering::SeqCst);
                        });
                        return axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(Body::from_stream(
                                tokio_stream::wrappers::ReceiverStream::new(rx),
                            ))
                            .unwrap();
                    }
                    (
                        [
                            ("content-type", "text/event-stream"),
                            ("x-test-upstream", "preserved"),
                        ],
                        "event: response.completed\ndata: {}\n\n",
                    )
                        .into_response()
                }
            }),
        );
    let backend_task = tokio::spawn(async move { axum::serve(backend_listener, router).await });
    let socket = TcpListener::bind("127.0.0.1:0")?;
    let address = socket.local_addr()?;
    drop(socket);
    let state_home = home.path().to_owned();
    let executable = std::env::current_exe()?;
    let bridge_task = tokio::spawn(async move {
        agent_relay::openai_bridge::serve(&state_home, address, backend, &executable).await
    });
    let client = reqwest::Client::builder().no_proxy().build()?;
    let url = format!("http://{address}");
    for _ in 0..100 {
        if client.get(format!("{url}/health")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let body = json!({"model":"agentrelay-claude","client_metadata":{"session_id":"fixture-root"}});
    let response = client
        .post(format!("{url}/responses"))
        .header("authorization", "Bearer fixture-auth")
        .header(agent_relay::provider::KEY_HEADER, "forged-key")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await?;
    ensure!(response.status() == 200 && response.headers()["x-test-upstream"] == "preserved");
    ensure!(response.text().await?.contains("response.completed"));
    let compressed = zstd::encode_all(serde_json::to_vec(&body)?.as_slice(), 1)?;
    let response = client
        .post(format!("{url}/responses"))
        .header("authorization", "Bearer fixture-auth")
        .header("content-encoding", "zstd")
        .body(compressed)
        .send()
        .await?;
    ensure!(response.status() == 200);
    assert_eq!(
        records.lock().unwrap().as_slice(),
        &[
            json!({"key":"private-fixture-key","authorization":"Bearer fixture-auth","encoding":null,"body":body}),
            json!({"key":"private-fixture-key","authorization":"Bearer fixture-auth","encoding":null,"body":body}),
        ]
    );
    let denied = client
        .post(format!("{url}/responses"))
        .header("origin", "http://localhost")
        .body("{}")
        .send()
        .await?;
    assert_eq!(denied.status(), 403);
    let fallback = client
        .get(format!("{url}/responses"))
        .header("upgrade", "websocket")
        .send()
        .await?;
    assert_eq!(fallback.status(), 426);
    // Actual different executable, despite knowing the key and forging identity headers.
    let denied = tokio::process::Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--max-time",
            "5",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--header",
            "x-agentrelay-key: private-fixture-key",
            "--header",
            "x-codex-pid: 1",
            "--header",
            "originator: codex",
            "--data",
            "{}",
            &format!("{url}/responses"),
        ])
        .output()
        .await?;
    assert_eq!(String::from_utf8_lossy(&denied.stdout), "403");
    assert_eq!(records.lock().unwrap().len(), 2);
    let mut live = client
        .post(format!("{url}/responses"))
        .header("authorization", "Bearer fixture-auth")
        .body(r#"{"hold":true}"#)
        .send()
        .await?;
    let chunk = tokio::time::timeout(Duration::from_secs(2), live.chunk()).await??;
    assert!(chunk.is_some_and(|b| b.starts_with(b"event: started")));
    assert!(!cancelled.load(Ordering::SeqCst));
    drop(live);
    for _ in 0..100 {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        cancelled.load(Ordering::SeqCst),
        "Downstream cancellation did not close the backend stream"
    );
    let oversized = vec![b' '; 16 * agent_relay::model::MAX_FRAME + 1];
    let compressed = zstd::encode_all(oversized.as_slice(), 1)?;
    let rejected = client
        .post(format!("{url}/responses"))
        .header("content-encoding", "zstd")
        .body(compressed)
        .send()
        .await?;
    assert_eq!(rejected.status(), 502);
    assert_eq!(records.lock().unwrap().len(), 3);
    bridge_task.abort();
    backend_task.abort();
    Ok(())
}

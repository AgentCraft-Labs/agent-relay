#![cfg(target_os = "macos")]
use agent_relay::transport::{self, Openai};
use anyhow::{Result, ensure};
use std::{
    net::{SocketAddr, TcpListener, TcpStream},
    process::Stdio,
    time::Duration,
};

fn address() -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap()
}
fn settings(home: &std::path::Path) -> Result<Openai> {
    std::fs::write(home.join("provider.key"), "transport-test-key")?;
    let settings = Openai {
        schema_version: 1,
        listen: address(),
        backend: address(),
        codex_bin: std::env::current_exe()?,
    };
    agent_relay::codex::atomic_write(
        &home.join(transport::FILE),
        &serde_json::to_vec(&settings)?,
        false,
    )?;
    Ok(settings)
}
async fn start(home: &std::path::Path, settings: &Openai) -> Result<tokio::process::Child> {
    Ok(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_agentrelay"))
            .arg("--home")
            .arg(home)
            .args(["serve", "--listen", &settings.backend.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?,
    )
}
async fn ready(home: &std::path::Path, settings: &Openai) -> Result<()> {
    for _ in 0..100 {
        if transport::healthy(home, settings.backend).unwrap_or(false) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("Integrated fixture did not become ready")
}

#[tokio::test]
async fn integrated_service_owns_both_listeners_and_restarts_together() -> Result<()> {
    let home = tempfile::tempdir()?;
    let settings = settings(home.path())?;
    let mut process = start(home.path(), &settings).await?;
    ready(home.path(), &settings).await?;
    let client = reqwest::Client::new();
    let backend: serde_json::Value = serde_json::from_slice(
        &client
            .get(format!("http://{}/version", settings.backend))
            .send()
            .await?
            .bytes()
            .await?,
    )?;
    let bridge: serde_json::Value = serde_json::from_slice(
        &client
            .get(format!("http://{}/version", settings.listen))
            .send()
            .await?
            .bytes()
            .await?,
    )?;
    ensure!(backend["pid"] == process.id().unwrap() && bridge["pid"] == backend["pid"]);
    let response = client
        .get(format!("http://{}/responses", settings.listen))
        .send()
        .await?;
    ensure!(
        response.status() == 426,
        "Kernel-admitted official protocol fallback failed"
    );
    let original = process.id().unwrap();
    process.kill().await?;
    process.wait().await?;
    ensure!(!transport::healthy(home.path(), settings.backend)?);
    let mut recovered = start(home.path(), &settings).await?;
    ready(home.path(), &settings).await?;
    ensure!(recovered.id() != Some(original));
    recovered.kill().await?;
    recovered.wait().await?;
    Ok(())
}

#[tokio::test]
async fn bridge_bind_failure_terminates_backend_without_touching_foreign_listener() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut settings = settings(home.path())?;
    let foreign = TcpListener::bind("127.0.0.1:0")?;
    settings.listen = foreign.local_addr()?;
    agent_relay::codex::atomic_write(
        &home.path().join(transport::FILE),
        &serde_json::to_vec(&settings)?,
        false,
    )?;
    let mut process = start(home.path(), &settings).await?;
    let status = tokio::time::timeout(Duration::from_secs(10), process.wait()).await??;
    ensure!(!status.success());
    ensure!(
        TcpStream::connect(settings.backend).is_err(),
        "Partial backend survived bridge failure"
    );
    ensure!(
        TcpStream::connect(settings.listen).is_ok(),
        "Foreign listener was affected"
    );
    Ok(())
}

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
        additional_codex_bins: Vec::new(),
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

#[test]
fn previous_transport_manifest_defaults_to_no_additional_executables() -> Result<()> {
    let settings: Openai = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "listen": "127.0.0.1:39092", "backend": "127.0.0.1:39091",
        "codex_bin": "/absolute/codex"
    }))?;
    settings.validate()?;
    assert!(settings.additional_codex_bins.is_empty());
    let mut invalid = settings;
    invalid.additional_codex_bins.push("relative/codex".into());
    assert!(invalid.validate().is_err());
    Ok(())
}

#[test]
fn unresolved_path_cli_stops_migration_before_any_configuration_change() -> Result<()> {
    let home = tempfile::tempdir()?;
    let cli = home.path().join("codex");
    std::fs::write(&cli, "#!/usr/bin/env node\n// fixture launcher\n")?;
    let codex_home = home.path().join("config");
    std::fs::create_dir(&codex_home)?;
    let config = codex_home.join("config.toml");
    let original = "model_provider = \"agentrelay\"\n";
    std::fs::write(&config, original)?;
    let state = home.path().join("state");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agentrelay"))
        .arg("--home")
        .arg(&state)
        .args(["codex", "migrate", "--codex-home"])
        .arg(&codex_home)
        .arg("--codex-bin")
        .arg(std::env::current_exe()?)
        .env("PATH", home.path())
        .output()?;
    ensure!(!output.status.success());
    ensure!(String::from_utf8_lossy(&output.stderr).contains("--also-codex-bin"));
    assert_eq!(std::fs::read_to_string(config)?, original);
    assert!(!state.exists());
    assert!(!codex_home.join("config.before-openai-bridge.toml").exists());
    Ok(())
}

#[tokio::test]
async fn additional_executables_are_admitted_by_exact_path_without_breaking_primary() -> Result<()>
{
    let home = tempfile::tempdir()?;
    let mut settings = settings(home.path())?;
    // Deliberately authorize curl only in this isolated fixture. A copy of
    // the primary test executable at another path must still be refused.
    settings.additional_codex_bins =
        vec!["/usr/bin/curl".into(), home.path().join("removed-codex")];
    agent_relay::codex::atomic_write(
        &home.path().join(transport::FILE),
        &serde_json::to_vec(&settings)?,
        false,
    )?;
    let mut process = start(home.path(), &settings).await?;
    ready(home.path(), &settings).await?;
    let url = format!("http://{}/responses", settings.listen);
    let response = reqwest::Client::new().get(&url).send().await?;
    assert_eq!(response.status(), 426);
    let output = tokio::process::Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--max-time",
            "5",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--header",
            "x-codex-pid: 1",
            &url,
        ])
        .output()
        .await?;
    ensure!(output.status.success(), "Fixture curl failed");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "426");
    let unlisted = home.path().join("unlisted-native-client");
    std::fs::copy(std::env::current_exe()?, &unlisted)?;
    let output = tokio::process::Command::new(&unlisted)
        .args(["--exact", "transport_http_client_fixture", "--nocapture"])
        .env("AGENTRELAY_ADMISSION_URL", &url)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "Copied native client failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(String::from_utf8_lossy(&output.stdout).contains("fixture_status=403"));
    process.kill().await?;
    process.wait().await?;
    Ok(())
}

#[test]
fn transport_http_client_fixture() -> Result<()> {
    let Ok(url) = std::env::var("AGENTRELAY_ADMISSION_URL") else {
        return Ok(());
    };
    let response = reqwest::blocking::Client::new()
        .get(url)
        .header("x-codex-pid", std::process::id())
        .header("originator", "codex")
        .send()?;
    println!("fixture_status={}", response.status().as_u16());
    Ok(())
}

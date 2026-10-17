//! Persisted transport configuration and the shared provider/bridge lifecycle.
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

pub const FILE: &str = "transport.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Openai {
    pub schema_version: u32,
    pub listen: SocketAddr,
    pub backend: SocketAddr,
    pub codex_bin: PathBuf,
    #[serde(default)]
    pub additional_codex_bins: Vec<PathBuf>,
}

impl Openai {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "Unsupported transport configuration version"
        );
        ensure!(
            self.listen.is_ipv4()
                && self.listen.ip().is_loopback()
                && self.backend.is_ipv4()
                && self.backend.ip().is_loopback()
                && self.listen.port() != 0
                && self.backend.port() != 0
                && self.listen != self.backend,
            "Distinct nonzero IPv4 loopback endpoints required"
        );
        ensure!(
            self.codex_bin.is_absolute()
                && self
                    .additional_codex_bins
                    .iter()
                    .all(|path| path.is_absolute()),
            "Codex executable must be an absolute path"
        );
        Ok(())
    }
}

pub fn load(home: &Path) -> Result<Option<Openai>> {
    let path = home.join(FILE);
    if !path.exists() {
        return Ok(None);
    }
    let value: Openai = serde_json::from_slice(&std::fs::read(path)?)?;
    value.validate()?;
    Ok(Some(value))
}

pub async fn serve(home: &Path, address: SocketAddr) -> Result<()> {
    let Some(settings) = load(home)? else {
        return crate::provider::serve(home, address).await;
    };
    ensure!(
        settings.backend == address,
        "Service address differs from its saved transport"
    );
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("Built-in OpenAI transport currently requires macOS");
    #[cfg(target_os = "macos")]
    {
        let root = home.to_owned();
        let mut backend = tokio::spawn(async move { crate::provider::serve(&root, address).await });
        // Abort a partially started backend on every error/early return.
        struct Abort(tokio::task::AbortHandle);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _backend_guard = Abort(backend.abort_handle());
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(300))
            .build()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if backend.is_finished() {
                return backend.await.context("Provider task failed")?;
            }
            if let Ok(response) = client.get(format!("http://{address}/version")).send().await
                && response.status().is_success()
            {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "Backend startup timed out"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let bridge =
            crate::openai_bridge::serve(home, settings.listen, address, &settings.codex_bin);
        tokio::pin!(bridge);
        tokio::select! {
            result=&mut backend => result.context("Provider task failed")?,
            result=&mut bridge => result,
        }
    }
}

/// Readiness of every listener owned by this service, including PID agreement.
pub fn healthy(home: &Path, address: SocketAddr) -> Result<bool> {
    let Some(settings) = load(home)? else {
        return crate::service::healthy(address);
    };
    ensure!(
        settings.backend == address,
        "Health address differs from saved transport"
    );
    let Some(backend) = crate::service::probe(address)? else {
        return Ok(false);
    };
    let Some(bridge) = crate::service::probe(settings.listen)? else {
        return Ok(false);
    };
    ensure!(
        bridge["transport"] == "openai_bridge",
        "Bridge port belongs to another service"
    );
    ensure!(
        backend["pid"].as_u64().is_some() && backend["pid"] == bridge["pid"],
        "Transport listeners belong to different processes"
    );
    Ok(true)
}

pub fn wait_ready(home: &Path, address: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if healthy(home, address)? {
            return Ok(());
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "Service listeners are not ready; inspect provider.err"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

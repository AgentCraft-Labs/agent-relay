//! Stage listeners first, switch Codex only after readiness, restore on failure.
use crate::{
    codex::atomic_write,
    transport::{self, Openai},
};
use anyhow::{Context, Result, ensure};
use std::{net::SocketAddr, path::Path};

fn restore(path: &Path, bytes: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = bytes {
        atomic_write(path, bytes, false)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn change(
    home: &Path,
    codex_home: &Path,
    next: Option<&Openai>,
    mut service: impl FnMut() -> Result<()>,
    configure: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let manifest = home.join(transport::FILE);
    let config = codex_home.join("config.toml");
    let previous = if manifest.exists() {
        Some(std::fs::read(&manifest)?)
    } else {
        None
    };
    let original = if config.exists() {
        Some(std::fs::read(&config)?)
    } else {
        None
    };
    let staged = next.map(serde_json::to_vec_pretty).transpose()?;
    restore(&manifest, staged.as_deref())?;
    if let Err(error) = service().and_then(|_| configure()) {
        restore(&manifest, previous.as_deref())?;
        restore(&config, original.as_deref())?;
        if let Err(recovery) = service() {
            anyhow::bail!(
                "Transport change failed ({error:#}); settings restored but service recovery failed ({recovery:#})"
            );
        }
        anyhow::bail!("Transport change failed; previous settings and service restored: {error:#}");
    }
    Ok(())
}

fn idle(home: &Path) -> Result<()> {
    ensure!(
        crate::store::Store::open(home)?
            .list(None)?
            .iter()
            .all(|task| task.status.terminal()),
        "Wait for active workers before changing transport"
    );
    Ok(())
}

fn activate_with(
    home: &Path,
    codex_home: &Path,
    catalog: &Path,
    settings: &Openai,
    service: impl FnMut() -> Result<()>,
) -> Result<()> {
    settings.validate()?;
    idle(home)?;
    let path = codex_home.join("config.toml");
    let original =
        std::fs::read_to_string(&path).context("Configure AgentRelay before migration")?;
    let config: toml::Value = toml::from_str(&original)?;
    ensure!(
        matches!(
            config.get("model_provider").and_then(toml::Value::as_str),
            Some("agentrelay" | "openai")
        ),
        "Only an existing AgentRelay/OpenAI configuration can be migrated"
    );
    if let Some(url) = config.get("openai_base_url").and_then(toml::Value::as_str) {
        let previous = transport::load(home)?;
        ensure!(
            previous
                .as_ref()
                .is_some_and(|p| url == format!("http://{}", p.listen)),
            "Existing OpenAI endpoint is not owned by AgentRelay; refusing to replace it"
        );
    }
    let backup = codex_home.join("config.before-openai-bridge.toml");
    if !backup.exists() {
        atomic_write(&backup, original.as_bytes(), false)?;
    }
    change(home, codex_home, Some(settings), service, || {
        crate::codex::configure(home, codex_home, catalog, settings.backend)
    })?;
    crate::runner::emit(
        &serde_json::json!({"transport_activated":"openai","legacy_provider_preserved":true,
        "chat_history_rewritten":false,"app_reload_required":true,"listen":settings.listen}),
    )
}

pub fn activate(home: &Path, codex_home: &Path, catalog: &Path, settings: &Openai) -> Result<()> {
    let _lock = lock(home)?;
    activate_with(home, codex_home, catalog, settings, || {
        crate::codex::install_service(home, settings.backend)
    })
}

fn rollback_with(
    home: &Path,
    codex_home: &Path,
    service: impl FnMut() -> Result<()>,
) -> Result<()> {
    idle(home)?;
    let previous = transport::load(home)?.context("OpenAI bridge is not configured")?;
    let backup = std::fs::read_to_string(codex_home.join("config.before-openai-bridge.toml"))?;
    let backup: toml_edit::DocumentMut = backup.parse()?;
    let current = std::fs::read_to_string(codex_home.join("config.toml"))?;
    let mut config: toml_edit::DocumentMut = current.parse()?;
    ensure!(
        config.get("model_provider").and_then(|v| v.as_str()) == Some("openai")
            && config.get("openai_base_url").and_then(|v| v.as_str())
                == Some(format!("http://{}", previous.listen).as_str()),
        "Current OpenAI routing was edited; refusing to overwrite it"
    );
    for field in ["model_provider", "openai_base_url"] {
        if let Some(value) = backup.get(field) {
            config[field] = value.clone();
        } else {
            config.remove(field);
        }
    }
    change(home, codex_home, None, service, || {
        atomic_write(
            &codex_home.join("config.toml"),
            config.to_string().as_bytes(),
            false,
        )
    })?;
    crate::runner::emit(
        &serde_json::json!({"transport_rollback":true,"chat_history_rewritten":false}),
    )
}

pub fn rollback(home: &Path, codex_home: &Path) -> Result<()> {
    let _lock = lock(home)?;
    let settings = transport::load(home)?.context("OpenAI bridge is not configured")?;
    rollback_with(home, codex_home, || {
        crate::codex::install_service(home, settings.backend)
    })
}

/// Default backend used by the persisted service; not a chat database migration.
pub fn backend(home: &Path, fallback: SocketAddr) -> Result<SocketAddr> {
    Ok(transport::load(home)?.map_or(fallback, |settings| settings.backend))
}

#[cfg(unix)]
fn lock(home: &Path) -> Result<std::fs::File> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    std::fs::create_dir_all(home)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(home.join("transport-migration.lock"))?;
    // SAFETY: file owns the valid descriptor and releases its lock on drop.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another transport migration is running"
    );
    Ok(file)
}
#[cfg(not(unix))]
fn lock(_home: &Path) -> Result<std::fs::File> {
    anyhow::bail!("Transport migration requires Unix")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_lock_excludes_concurrent_changes() {
        let dir = tempfile::tempdir().unwrap();
        let guard = lock(dir.path()).unwrap();
        assert!(lock(dir.path()).is_err());
        drop(guard);
        assert!(lock(dir.path()).is_ok());
    }
    fn fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        Openai,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("state");
        let codex = dir.path().join("codex");
        let catalog = dir.path().join("catalog.json");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(&catalog, r#"{"models":[{"slug":"test"}]}"#).unwrap();
        std::fs::write(codex.join("config.toml"), "# preferences\nmodel=\"test\"\n").unwrap();
        crate::codex::configure(&home, &codex, &catalog, "127.0.0.1:39101".parse().unwrap())
            .unwrap();
        let config = Openai {
            schema_version: 1,
            listen: "127.0.0.1:39102".parse().unwrap(),
            backend: "127.0.0.1:39101".parse().unwrap(),
            codex_bin: std::env::current_exe().unwrap(),
        };
        (dir, home, codex, catalog, config)
    }
    #[test]
    fn migration_preserves_legacy_provider_and_rollback_keeps_later_preferences() {
        let (_dir, home, codex, catalog, config) = fixture();
        activate_with(&home, &codex, &catalog, &config, || Ok(())).unwrap();
        let text = std::fs::read_to_string(codex.join("config.toml")).unwrap();
        let value: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(value["model_provider"].as_str(), Some("openai"));
        assert_eq!(
            value["model_providers"]["agentrelay"]["base_url"].as_str(),
            Some("http://127.0.0.1:39101")
        );
        std::fs::write(
            codex.join("config.toml"),
            format!("new_preference = true\n{text}"),
        )
        .unwrap();
        rollback_with(&home, &codex, || Ok(())).unwrap();
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(codex.join("config.toml")).unwrap()).unwrap();
        assert_eq!(value["model_provider"].as_str(), Some("agentrelay"));
        assert!(value["new_preference"].as_bool().unwrap());
        assert!(value.get("openai_base_url").is_none());
        assert!(transport::load(&home).unwrap().is_none());
    }
    #[test]
    fn failed_activation_restores_exact_config_and_previous_service() {
        let (_dir, home, codex, catalog, config) = fixture();
        let original = std::fs::read(codex.join("config.toml")).unwrap();
        let mut attempts = 0;
        let error = activate_with(&home, &codex, &catalog, &config, || {
            attempts += 1;
            if attempts == 1 {
                anyhow::bail!("occupied bridge port")
            };
            Ok(())
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("previous settings and service restored")
        );
        assert_eq!(attempts, 2);
        assert_eq!(std::fs::read(codex.join("config.toml")).unwrap(), original);
        assert!(transport::load(&home).unwrap().is_none());
    }
    #[test]
    fn migration_refuses_unowned_endpoint() {
        let (_dir, home, codex, catalog, config) = fixture();
        let text = std::fs::read_to_string(codex.join("config.toml")).unwrap();
        std::fs::write(
            codex.join("config.toml"),
            format!("openai_base_url=\"https://example.invalid/v1\"\n{text}"),
        )
        .unwrap();
        assert!(
            activate_with(&home, &codex, &catalog, &config, || panic!(
                "must not restart"
            ))
            .is_err()
        );
    }
}

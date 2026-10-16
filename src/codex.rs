//! Installation uses supported user skills, native roles and provider config.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

const SKILL: &str = include_str!("../integrations/codex/skills/agentrelay/SKILL.md");
const METADATA: &str = include_str!("../integrations/codex/skills/agentrelay/agents/openai.yaml");
pub const ROLE: &str = include_str!("../integrations/codex/agentrelay_claude.toml");

#[derive(Default, Serialize, Deserialize)]
struct Receipt {
    version: String,
    files: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hook_command: Option<String>, // Only for removing previously installed handlers.
}

#[derive(Default, Serialize, Deserialize)]
struct ProfileReceipt {
    files: BTreeMap<String, String>,
    profiles: BTreeMap<String, crate::profiles::Profile>,
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Remove only our recorded legacy handler; preserve all unrelated hooks.
pub fn remove_owned_hooks(mut value: Value, command: &str) -> Result<Value> {
    if let Some(groups) = value.get_mut("hooks").and_then(Value::as_object_mut) {
        for groups in groups.values_mut() {
            for group in groups
                .as_array_mut()
                .context("Invalid hook groups")?
                .iter_mut()
            {
                if let Some(handlers) = group["hooks"].as_array_mut() {
                    handlers.retain(|handler| {
                        !(handler["type"] == "command" && handler["command"] == command)
                    });
                }
            }
            groups.as_array_mut().unwrap().retain(|group| {
                group["hooks"]
                    .as_array()
                    .is_none_or(|handlers| !handlers.is_empty())
            });
        }
    }
    Ok(value)
}

pub fn install(root: &Path, user: bool) -> Result<()> {
    let root = std::fs::canonicalize(root)?;
    let receipt_path = root.join(".codex/agentrelay-install.json");
    let mut previous: Receipt = if receipt_path.exists() {
        serde_json::from_slice(&std::fs::read(&receipt_path)?)?
    } else {
        Receipt::default()
    };
    // Migrate the original self-contained user skill only if it still matches
    // its recorded bundle. This is data migration, never an execution fallback.
    if user && previous.files.is_empty() {
        let old = root.join(".local/share/agentrelay/installation.json");
        if old.is_file() {
            let old: Value = serde_json::from_slice(&std::fs::read(old)?)?;
            if let Some(bundle) = old["bundle_path"].as_str() {
                for relative in ["SKILL.md", "agents/openai.yaml", "scripts/agentrelay"] {
                    let original = Path::new(bundle).join("skills/agentrelay").join(relative);
                    if original.is_file() {
                        previous.files.insert(
                            format!(".agents/skills/agentrelay/{relative}"),
                            digest(&std::fs::read(original)?),
                        );
                    }
                }
            }
        }
    }
    let files = BTreeMap::from([
        (
            ".agents/skills/agentrelay/SKILL.md".to_owned(),
            SKILL.as_bytes().to_vec(),
        ),
        (
            ".agents/skills/agentrelay/agents/openai.yaml".to_owned(),
            METADATA.as_bytes().to_vec(),
        ),
        (
            ".agents/skills/agentrelay/scripts/agentrelay".to_owned(),
            std::fs::read(std::env::current_exe()?)?,
        ),
        (
            ".codex/agents/agentrelay_claude.toml".to_owned(),
            ROLE.as_bytes().to_vec(),
        ),
    ]);
    for (relative, bytes) in &files {
        let path = root.join(relative);
        if path.exists() {
            let current = digest(&std::fs::read(&path)?);
            ensure!(
                current == digest(bytes) || previous.files.get(relative) == Some(&current),
                "Refusing to overwrite modified or unrelated file {}",
                path.display()
            );
        }
    }
    let legacy = ".codex/agents/external_worker.toml";
    if let Some(hash) = previous.files.get(legacy) {
        let path = root.join(legacy);
        if path.is_file() {
            ensure!(
                &digest(&std::fs::read(&path)?) == hash,
                "Legacy role was edited; inspect it before removal"
            );
        }
    }
    let hooks = root.join(".codex/hooks.json");
    let cleaned_hooks = if hooks.exists() {
        previous
            .hook_command
            .as_ref()
            .map(|command| -> Result<Vec<u8>> {
                Ok(serde_json::to_vec_pretty(&remove_owned_hooks(
                    serde_json::from_slice(&std::fs::read(&hooks)?)?,
                    command,
                )?)?)
            })
            .transpose()?
    } else {
        None
    };
    for (relative, bytes) in &files {
        atomic_write(
            &root.join(relative),
            bytes,
            relative.ends_with("scripts/agentrelay"),
        )?;
    }
    if previous.files.contains_key(legacy) && root.join(legacy).exists() {
        std::fs::remove_file(root.join(legacy))?;
    }
    if let Some(bytes) = cleaned_hooks {
        atomic_write(&hooks, &bytes, false)?;
    }
    let receipt = Receipt {
        version: env!("CARGO_PKG_VERSION").into(),
        files: files
            .into_iter()
            .map(|(path, bytes)| (path, digest(&bytes)))
            .collect(),
        hook_command: None,
    };
    atomic_write(&receipt_path, &serde_json::to_vec_pretty(&receipt)?, false)?;
    if user {
        atomic_write(
            &root.join(".local/share/agentrelay/installation.json"),
            &serde_json::to_vec_pretty(
                &json!({"version":env!("CARGO_PKG_VERSION"),"scope":"user","skill_path":root.join(".agents/skills/agentrelay"),"binary_sha256":receipt.files[".agents/skills/agentrelay/scripts/agentrelay"],"native_role":"agentrelay_claude","execution_mode":"responses_provider","ownership_receipt":receipt_path}),
            )?,
            false,
        )?;
    }
    crate::runner::emit(
        &json!({"installed":true,"scope":if user{"user"}else{"project"},"native_role":"agentrelay_claude","provider_configuration_required":true,"codex_binary_modified":false}),
    )
}

pub fn export(out: &Path) -> Result<()> {
    ensure!(!out.exists(), "Export directory already exists");
    atomic_write(
        &out.join("skills/agentrelay/SKILL.md"),
        SKILL.as_bytes(),
        false,
    )?;
    atomic_write(
        &out.join("skills/agentrelay/agents/openai.yaml"),
        METADATA.as_bytes(),
        false,
    )?;
    atomic_write(
        &out.join("skills/agentrelay/scripts/agentrelay"),
        &std::fs::read(std::env::current_exe()?)?,
        true,
    )?;
    atomic_write(
        &out.join("assets/agentrelay_claude.toml"),
        ROLE.as_bytes(),
        false,
    )?;
    let manifest = json!({"name":"agentrelay","version":env!("CARGO_PKG_VERSION"),"description":"External CLI runtimes for native Codex agents","skills":"./skills/"});
    atomic_write(
        &out.join("plugin.json"),
        &serde_json::to_vec_pretty(&manifest)?,
        false,
    )?;
    atomic_write(
        &out.join(".codex-plugin/plugin.json"),
        &serde_json::to_vec_pretty(&manifest)?,
        false,
    )?;
    crate::runner::emit(&json!({"exported":out,"provider_configuration_required":true}))
}

pub fn configure(
    home: &Path,
    codex_home: &Path,
    catalog: &Path,
    address: SocketAddr,
) -> Result<()> {
    ensure!(
        address.ip().is_loopback(),
        "Only a loopback provider is supported"
    );
    crate::provider::Registry::open(home)?;
    let key_path = home.join("provider.key");
    let key = if key_path.exists() {
        std::fs::read_to_string(&key_path)?
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    let mut models: Value = serde_json::from_slice(
        &std::fs::read(catalog).context("A current Codex models_cache.json is required")?,
    )?;
    let list = models["models"]
        .as_array_mut()
        .context("Invalid model catalog")?;
    let template = list.first().context("Empty model catalog")?.clone();
    list.retain(|model| {
        !model["slug"]
            .as_str()
            .is_some_and(|slug| slug.starts_with("agentrelay-"))
    });
    let mut runtimes = std::collections::BTreeSet::from(["claude".to_owned(), "codex".to_owned()]);
    let runtime_config = home.join("runtimes.toml");
    if runtime_config.is_file() {
        let settings: toml::Value = toml::from_str(&std::fs::read_to_string(runtime_config)?)?;
        if let Some(configured) = settings["runtimes"].as_table() {
            runtimes.extend(configured.keys().cloned());
        }
    }
    for runtime in runtimes {
        ensure!(
            !runtime.is_empty()
                && runtime
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Invalid configured runtime name"
        );
        list.push(crate::catalog::runtime_entry(&template, &runtime));
    }
    let profiles = crate::profiles::load(home)?;
    let profile_receipt_path = codex_home.join("agentrelay-models-install.json");
    let previous_profiles: ProfileReceipt = if profile_receipt_path.is_file() {
        serde_json::from_slice(&std::fs::read(&profile_receipt_path)?)?
    } else {
        ProfileReceipt::default()
    };
    let mut profile_files = BTreeMap::new();
    for (name, profile) in &profiles {
        if let Some(previous) = previous_profiles.profiles.get(name) {
            ensure!(
                profile == previous,
                "Native model profile {name} is immutable; add a new profile rather than relabel existing threads"
            );
        }
        list.push(crate::profiles::catalog_entry(&template, name, profile));
        let relative = format!("agents/{}.toml", crate::profiles::role_name(name));
        let bytes = crate::profiles::role_document(name, profile)?.into_bytes();
        let destination = codex_home.join(&relative);
        if destination.exists() {
            let current = digest(&std::fs::read(&destination)?);
            ensure!(
                current == digest(&bytes)
                    || previous_profiles.files.get(&relative) == Some(&current),
                "Refusing to overwrite modified or unrelated native role {}",
                destination.display()
            );
        }
        profile_files.insert(relative, bytes);
    }
    std::fs::create_dir_all(codex_home)?;
    let generated = home.join("models.json");
    let config_path = codex_home.join("config.toml");
    let original = if config_path.exists() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };
    let mut config: toml_edit::DocumentMut = original.parse()?;
    if let Some(provider) = config.get("model_provider").and_then(|v| v.as_str()) {
        ensure!(
            matches!(provider, "openai" | "agentrelay"),
            "Existing custom provider requires manual configuration; refusing to replace it"
        );
    }
    config["model_provider"] = toml_edit::value("agentrelay");
    config["model_catalog_json"] = toml_edit::value(generated.to_string_lossy().as_ref());
    config["model_providers"]["agentrelay"]["name"] = toml_edit::value("AgentRelay");
    config["model_providers"]["agentrelay"]["base_url"] =
        toml_edit::value(format!("http://{address}"));
    config["model_providers"]["agentrelay"]["wire_api"] = toml_edit::value("responses");
    config["model_providers"]["agentrelay"]["requires_openai_auth"] = toml_edit::value(true);
    config["model_providers"]["agentrelay"]["supports_websockets"] = toml_edit::value(false);
    config["model_providers"]["agentrelay"]["http_headers"][crate::provider::KEY_HEADER] =
        toml_edit::value(key.trim());
    if config
        .get("features")
        .is_none_or(|v| v.get("multi_agent_v2").is_none())
    {
        config["features"]["multi_agent_v2"] = toml_edit::value(true);
    }
    let backup = codex_home.join("config.before-agentrelay.toml");
    if !backup.exists() {
        atomic_write(&backup, original.as_bytes(), false)?;
    }
    atomic_write(&key_path, key.trim().as_bytes(), false)?;
    atomic_write(&generated, &serde_json::to_vec_pretty(&models)?, false)?;
    for (relative, bytes) in &profile_files {
        atomic_write(&codex_home.join(relative), bytes, false)?;
    }
    let profile_receipt = ProfileReceipt {
        files: profile_files
            .into_iter()
            .map(|(path, bytes)| (path, digest(&bytes)))
            .collect(),
        profiles,
    };
    atomic_write(
        &profile_receipt_path,
        &serde_json::to_vec_pretty(&profile_receipt)?,
        false,
    )?;
    atomic_write(&config_path, config.to_string().as_bytes(), false)?;
    crate::runner::emit(
        &json!({"configured":true,"provider":"agentrelay","native_model_profiles":profile_receipt.profiles.len(),"new_conversation_required":true,"service_start_required":true,"catalog_refresh":"rerun configure after upstream models change"}),
    )
}

pub fn user_root() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("HOME is not set")?,
    ))
}

/// Apply the user integration only between external turns. Existing workers
/// keep their frozen native binding; an active CLI is never restarted here.
#[cfg(target_os = "macos")]
pub async fn upgrade(home: &Path, wait_idle: bool, address: SocketAddr) -> Result<()> {
    let store = crate::store::Store::open(home)?;
    let mut announced = false;
    loop {
        let active = store
            .list(None)?
            .iter()
            .filter(|t| !t.status.terminal())
            .count();
        if active == 0 {
            break;
        }
        ensure!(
            wait_idle,
            "Active workers are running; use codex upgrade --wait-idle or retry when they finish"
        );
        if !announced {
            crate::runner::emit(
                &json!({"upgrade_waiting_for_idle":true,"active_workers":active,"provider_restarted":false}),
            )?;
            announced = true;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
            _ = crate::runner::shutdown_signal() => anyhow::bail!("Upgrade cancelled; provider left running"),
        }
    }
    let root = user_root()?;
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".codex"));
    // Migrate old bindings while their old model profiles are still intact.
    crate::provider::Registry::open(home)?;
    install(&root, true)?;
    crate::profiles::install_defaults(home)?;
    configure(
        home,
        &codex_home,
        &codex_home.join("models_cache.json"),
        address,
    )?;
    install_service(home, address)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()?;
    for _ in 0..40 {
        if let Ok(response) = client.get(format!("http://{address}/version")).send().await
            && let Ok(body) = response.bytes().await
            && let Ok(version) = serde_json::from_slice::<Value>(&body)
            && version["version"] == env!("CARGO_PKG_VERSION")
        {
            return crate::runner::emit(
                &json!({"upgrade_applied":true,"version":env!("CARGO_PKG_VERSION"),"new_conversation_required":true}),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("Provider did not report the updated version; inspect the local service log")
}

#[cfg(not(target_os = "macos"))]
pub async fn upgrade(_home: &Path, _wait_idle: bool, _address: SocketAddr) -> Result<()> {
    anyhow::bail!(
        "Automatic upgrades are supported on macOS; update the configured service manager on this platform"
    )
}

#[cfg(target_os = "macos")]
pub fn install_service(home: &Path, address: SocketAddr) -> Result<()> {
    ensure!(address.ip().is_loopback(), "Only loopback is allowed");
    let store = crate::store::Store::open(home)?;
    ensure!(
        store.list(None)?.iter().all(|task| task.status.terminal()),
        "Wait for active workers before restarting the provider"
    );
    ensure!(
        home.join("provider.key").is_file(),
        "Configure the provider before installing its service"
    );
    let root = user_root()?;
    let binary = root.join(".agents/skills/agentrelay/scripts/agentrelay");
    ensure!(binary.is_file(), "Install the user skill first");
    use crate::service::xml;
    let args = [
        binary.to_string_lossy().into_owned(),
        "--home".into(),
        home.to_string_lossy().into_owned(),
        "serve".into(),
        "--listen".into(),
        address.to_string(),
    ];
    let arguments = args
        .iter()
        .map(|arg| format!("<string>{}</string>", xml(arg)))
        .collect::<String>();
    let log = home.join("provider.log");
    let error = home.join("provider.err");
    let guardian_log = home.join("guardian.log");
    let guardian_error = home.join("guardian.err");
    for path in [&log, &error, &guardian_log, &guardian_error] {
        if !path.exists() {
            atomic_write(path, b"", false)?;
        }
    }
    let executable_path =
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>com.agentrelay.provider</string><key>ProgramArguments</key><array>{arguments}</array><key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>",
        xml(&executable_path),
        xml(&log.to_string_lossy()),
        xml(&error.to_string_lossy())
    );
    let destination = root.join("Library/LaunchAgents/com.agentrelay.provider.plist");
    let guardian = root.join("Library/LaunchAgents/com.agentrelay.guardian.plist");
    let guardian_plist = crate::service::guardian_document(&binary, home, address)?;
    // All fallible file writes precede stopping either service. In particular a
    // full disk or permission failure must leave the old provider running.
    atomic_write(&destination, plist.as_bytes(), false)?;
    atomic_write(&guardian, guardian_plist.as_bytes(), false)?;
    // SAFETY: getuid has no pointer arguments or side effects.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let guardian_target = format!("{domain}/{}", crate::service::GUARDIAN_LABEL);
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &guardian_target])
        .output()?;
    // Recovery is registered before touching the provider. If this installer
    // exits between bootout and bootstrap, the next guardian check repairs it.
    bootstrap_service(&domain, &guardian, &guardian_target)?;
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &format!("{domain}/com.agentrelay.provider")])
        .output()?;
    bootstrap_service(
        &domain,
        &destination,
        &format!("{domain}/com.agentrelay.provider"),
    )?;
    crate::service::ensure_service(&root, address)?;
    crate::runner::emit(
        &json!({"service_installed":true,"label":"com.agentrelay.provider","listen":address,"guardian_installed":true,"recovery_interval_seconds":crate::service::CHECK_INTERVAL_SECONDS}),
    )
}

#[cfg(target_os = "macos")]
fn bootstrap_service(domain: &str, destination: &Path, target: &str) -> Result<()> {
    // bootout can return before launchd has released the old registration.
    let mut output = std::process::Command::new("/bin/launchctl")
        .args(["bootstrap", domain])
        .arg(destination)
        .output()?;
    for attempt in 0..5 {
        if output.status.success()
            || std::process::Command::new("/bin/launchctl")
                .args(["print", target])
                .output()?
                .status
                .success()
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100 << attempt));
        output = std::process::Command::new("/bin/launchctl")
            .args(["bootstrap", domain])
            .arg(destination)
            .output()?;
    }
    ensure!(
        output.status.success(),
        "launchctl bootstrap failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install_service(_home: &Path, _address: SocketAddr) -> Result<()> {
    anyhow::bail!(
        "Automatic service installation is supported on macOS; run agentrelay serve under your service manager"
    )
}

pub fn atomic_write(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let parent = path.parent().context("Missing parent directory")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".agentrelay-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o755 } else { 0o600 });
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

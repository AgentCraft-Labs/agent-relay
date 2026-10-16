//! Independent launchd recovery for the provider used by ordinary Codex chats.
use anyhow::{Result, ensure};
#[cfg(target_os = "macos")]
use serde_json::json;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::Duration,
};

pub const PROVIDER_LABEL: &str = "com.agentrelay.provider";
pub const GUARDIAN_LABEL: &str = "com.agentrelay.guardian";
pub const CHECK_INTERVAL_SECONDS: u64 = 10;

pub(crate) fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn guardian_document(binary: &Path, home: &Path, address: SocketAddr) -> Result<String> {
    ensure!(address.ip().is_loopback(), "Only loopback is allowed");
    let args = [
        binary.to_string_lossy().into_owned(),
        "--home".into(),
        home.to_string_lossy().into_owned(),
        "codex".into(),
        "service-ensure".into(),
        "--listen".into(),
        address.to_string(),
    ];
    let arguments = args
        .iter()
        .map(|s| format!("<string>{}</string>", xml(s)))
        .collect::<String>();
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\"><plist version=\"1.0\"><dict><key>Label</key><string>{GUARDIAN_LABEL}</string><key>ProgramArguments</key><array>{arguments}</array><key>RunAtLoad</key><true/><key>StartInterval</key><integer>{CHECK_INTERVAL_SECONDS}</integer><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>",
        xml(&home.join("guardian.log").to_string_lossy()),
        xml(&home.join("guardian.err").to_string_lossy())
    ))
}

/// Probe only the loopback service identity; never send credentials or prompts.
pub fn healthy(address: SocketAddr) -> Result<bool> {
    ensure!(address.ip().is_loopback(), "Only loopback is allowed");
    let mut stream = match TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
        Ok(stream) => stream,
        Err(_) => return Ok(false),
    };
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = format!("GET /version HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    if stream.take(16384).read_to_end(&mut bytes).is_err() {
        return Ok(false);
    }
    let response = String::from_utf8_lossy(&bytes);
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return Ok(false);
    };
    if !headers.starts_with("HTTP/1.1 200 ") && !headers.starts_with("HTTP/1.0 200 ") {
        return Ok(false);
    }
    let version: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    // A different listener must never be killed or replaced by this guardian.
    ensure!(
        version["service"] == "AgentRelay",
        "Configured port is occupied by another service"
    );
    Ok(true)
}

#[cfg(any(target_os = "macos", test))]
trait Controller {
    fn loaded(&mut self) -> Result<bool>;
    fn bootstrap(&mut self) -> Result<()>;
    fn start(&mut self) -> Result<()>;
}

/// Never unload a service or kill a healthy provider. A second guardian may
/// win the bootstrap race; its successful registration is sufficient.
#[cfg(any(target_os = "macos", test))]
fn recover(controller: &mut impl Controller, ready: bool) -> Result<bool> {
    if ready {
        return Ok(false);
    }
    if controller.loaded()? {
        controller.start()?;
    } else if let Err(error) = controller.bootstrap()
        && !controller.loaded()?
    {
        return Err(error);
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
pub fn ensure_service(root: &Path, address: SocketAddr) -> Result<()> {
    use std::process::Command;
    struct Launchd {
        domain: String,
        path: std::path::PathBuf,
    }
    impl Controller for Launchd {
        fn loaded(&mut self) -> Result<bool> {
            Ok(Command::new("/bin/launchctl")
                .args(["print", &format!("{}/{PROVIDER_LABEL}", self.domain)])
                .output()?
                .status
                .success())
        }
        fn bootstrap(&mut self) -> Result<()> {
            let output = Command::new("/bin/launchctl")
                .args(["bootstrap", &self.domain])
                .arg(&self.path)
                .output()?;
            ensure!(
                output.status.success(),
                "Cannot restore provider registration: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        }
        fn start(&mut self) -> Result<()> {
            // No -k: leave an already running process and its workers intact.
            let output = Command::new("/bin/launchctl")
                .args(["kickstart", &format!("{}/{PROVIDER_LABEL}", self.domain)])
                .output()?;
            ensure!(
                output.status.success(),
                "Cannot start provider: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        }
    }
    ensure!(address.ip().is_loopback(), "Only loopback is allowed");
    let ready = healthy(address)?;
    let path = root.join("Library/LaunchAgents/com.agentrelay.provider.plist");
    ensure!(
        ready || path.is_file(),
        "Provider service is not installed; run codex service-install"
    );
    // SAFETY: getuid has no pointer arguments or side effects.
    let mut controller = Launchd {
        domain: format!("gui/{}", unsafe { libc::getuid() }),
        path,
    };
    let recovered = recover(&mut controller, ready)?;
    if recovered {
        for _ in 0..30 {
            if healthy(address)? {
                return crate::runner::emit(
                    &json!({"service_ready":true,"registration_recovered":true}),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        anyhow::bail!(
            "Provider is registered but not responding; inspect provider.err; running workers were not killed"
        );
    }
    // Keep periodic checks silent while healthy, without growing log files.
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_service(_root: &Path, _address: SocketAddr) -> Result<()> {
    anyhow::bail!(
        "Automatic service recovery is supported on macOS; use your platform service manager"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        loaded: bool,
        fail_load: bool,
        race: bool,
        actions: Vec<&'static str>,
    }
    impl Controller for Fake {
        fn loaded(&mut self) -> Result<bool> {
            self.actions.push("loaded");
            Ok(self.loaded)
        }
        fn bootstrap(&mut self) -> Result<()> {
            self.actions.push("bootstrap");
            if self.race {
                self.loaded = true;
            }
            ensure!(!self.fail_load, "fixture load failure");
            self.loaded = true;
            Ok(())
        }
        fn start(&mut self) -> Result<()> {
            self.actions.push("start_without_kill");
            Ok(())
        }
    }
    #[test]
    fn healthy_provider_is_never_restarted() {
        let mut fake = Fake::default();
        assert!(!recover(&mut fake, true).unwrap());
        assert!(fake.actions.is_empty());
    }
    #[test]
    fn unloaded_provider_is_registered_again() {
        let mut fake = Fake::default();
        assert!(recover(&mut fake, false).unwrap());
        assert!(fake.loaded);
        assert_eq!(fake.actions, vec!["loaded", "bootstrap"]);
        assert!(!recover(&mut fake, true).unwrap());
    }
    #[test]
    fn loaded_provider_is_started_without_killing_workers() {
        let mut fake = Fake {
            loaded: true,
            ..Fake::default()
        };
        recover(&mut fake, false).unwrap();
        assert_eq!(fake.actions, vec!["loaded", "start_without_kill"]);
    }
    #[test]
    fn bootstrap_failure_is_reported_and_concurrent_recovery_is_accepted() {
        let mut fake = Fake {
            fail_load: true,
            ..Fake::default()
        };
        assert!(recover(&mut fake, false).is_err());
        let mut race = Fake {
            fail_load: true,
            race: true,
            ..Fake::default()
        };
        assert!(recover(&mut race, false).unwrap());
    }
    #[test]
    fn guardian_is_independent_periodic_and_does_not_shell_expand_paths() {
        let address = "127.0.0.1:39091".parse().unwrap();
        let doc = guardian_document(
            Path::new("/test/a & b/agentrelay"),
            Path::new("/test/state"),
            address,
        )
        .unwrap();
        assert!(doc.contains("<string>/test/a &amp; b/agentrelay</string>"));
        assert!(doc.contains("<string>service-ensure</string>"));
        assert!(doc.contains("<key>StartInterval</key><integer>10</integer>"));
        assert!(!doc.contains("/bin/sh"));
        assert!(!doc.contains("<key>KeepAlive</key>"));
        assert!(
            guardian_document(
                Path::new("/binary"),
                Path::new("/state"),
                "0.0.0.0:39091".parse().unwrap()
            )
            .is_err()
        );
    }
}

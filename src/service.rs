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
pub const HEALTH_FAILURE_LIMIT: u32 = 3;

#[cfg(any(target_os = "macos", test))]
#[derive(serde::Serialize, serde::Deserialize)]
struct Failures {
    pid: u32,
    start_time: u64,
    count: u32,
    checked_at: i64,
}
#[cfg(any(target_os = "macos", test))]
fn note_failure(previous: Option<Failures>, pid: u32, start_time: u64, now: i64) -> Failures {
    if let Some(mut previous) = previous
        && previous.pid == pid
        && previous.start_time == start_time
        && (0..=90_000).contains(&(now - previous.checked_at))
    {
        if now - previous.checked_at >= CHECK_INTERVAL_SECONDS as i64 * 1000 {
            previous.count = previous.count.saturating_add(1);
            previous.checked_at = now;
        }
        return previous;
    }
    Failures {
        pid,
        start_time,
        count: 1,
        checked_at: now,
    }
}

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
pub(crate) fn wait_ready(address: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if healthy(address)? {
            return Ok(());
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "Provider is not ready; inspect provider.err and the next guardian health check"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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
pub fn ensure_service(root: &Path, home: &Path, address: SocketAddr) -> Result<()> {
    ensure_service_named(root, home, address, PROVIDER_LABEL)
}

#[cfg(target_os = "macos")]
fn ensure_service_named(root: &Path, home: &Path, address: SocketAddr, label: &str) -> Result<()> {
    use std::process::Command;
    struct Launchd<'a> {
        domain: String,
        path: std::path::PathBuf,
        label: &'a str,
    }
    impl Controller for Launchd<'_> {
        fn loaded(&mut self) -> Result<bool> {
            Ok(Command::new("/bin/launchctl")
                .args(["print", &format!("{}/{}", self.domain, self.label)])
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
                .args(["kickstart", &format!("{}/{}", self.domain, self.label)])
                .output()?;
            ensure!(
                output.status.success(),
                "Cannot start provider: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        }
    }
    impl Launchd<'_> {
        fn owned_process(&self, root: &Path) -> Result<Option<crate::model::ProcessIdentity>> {
            use std::{
                ffi::{CStr, OsStr},
                os::unix::ffi::OsStrExt,
            };
            let output = Command::new("/bin/launchctl")
                .args(["print", &format!("{}/{}", self.domain, self.label)])
                .output()?;
            let pid = String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix("pid = ")
                        .and_then(|p| p.parse::<u32>().ok())
                });
            let Some(pid) = pid.filter(|p| *p > 1) else {
                return Ok(None);
            };
            let mut path = [0u8; 4096];
            // SAFETY: proc_pidpath writes at most the supplied buffer size.
            let length = unsafe {
                libc::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32)
            };
            ensure!(
                length > 0,
                "Cannot verify provider executable; refusing to restart it"
            );
            let path = CStr::from_bytes_until_nul(&path)?.to_bytes();
            let actual = std::fs::canonicalize(Path::new(OsStr::from_bytes(path)))?;
            let expected =
                std::fs::canonicalize(root.join(".agents/skills/agentrelay/scripts/agentrelay"))?;
            ensure!(
                actual == expected,
                "Registered process is not the installed AgentRelay executable; refusing to restart it"
            );
            let identity = crate::runner::process_identity(pid);
            ensure!(
                identity.as_ref().is_some_and(|p| p.start_time > 0),
                "Cannot verify provider process identity"
            );
            Ok(identity)
        }
        fn restart_owned(
            &self,
            root: &Path,
            identity: &crate::model::ProcessIdentity,
        ) -> Result<()> {
            let current = self.owned_process(root)?;
            ensure!(
                current
                    .as_ref()
                    .is_some_and(|p| p.pid == identity.pid && p.start_time == identity.start_time),
                "Provider process changed during health checks; restart deferred"
            );
            // Kill only this verified provider PID, never its process group or
            // an arbitrary listener. launchd KeepAlive starts its replacement.
            // SAFETY: identity has been checked against launchd and proc_pidpath.
            ensure!(
                unsafe { libc::kill(identity.pid as i32, libc::SIGKILL) } == 0,
                "Cannot restart unresponsive provider: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
    }
    ensure!(address.ip().is_loopback(), "Only loopback is allowed");
    std::fs::create_dir_all(home)?;
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(home.join("guardian.lock"))?;
    // SAFETY: the descriptor belongs to lock and remains open until return.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(error.into());
    }
    let failures_path = home.join("service-health.json");
    let ready = healthy(address)?;
    if ready {
        if failures_path.exists() {
            std::fs::remove_file(&failures_path)?;
        }
        return Ok(());
    }
    let path = root.join("Library/LaunchAgents/com.agentrelay.provider.plist");
    ensure!(
        ready || path.is_file(),
        "Provider service is not installed; run codex service-install"
    );
    // SAFETY: getuid has no pointer arguments or side effects.
    let mut controller = Launchd {
        domain: format!("gui/{}", unsafe { libc::getuid() }),
        path,
        label,
    };
    if controller.loaded()?
        && let Some(identity) = controller.owned_process(root)?
    {
        let previous = std::fs::read(&failures_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let failures = note_failure(
            previous,
            identity.pid,
            identity.start_time,
            crate::model::now_ms(),
        );
        crate::codex::atomic_write(&failures_path, &serde_json::to_vec(&failures)?, false)?;
        ensure!(
            failures.count >= HEALTH_FAILURE_LIMIT,
            "Provider health failed {}/{} consecutive checks; restart deferred",
            failures.count,
            HEALTH_FAILURE_LIMIT
        );
        // Recheck readiness before terminating a process that may have recovered.
        if healthy(address)? {
            std::fs::remove_file(&failures_path)?;
            return Ok(());
        }
        controller.restart_owned(root, &identity)?;
        std::fs::remove_file(&failures_path)?;
        crate::runner::emit(
            &json!({"unresponsive_provider_restarted":true,"failed_health_checks":failures.count}),
        )?;
    }
    let recovered = recover(&mut controller, ready)?;
    if recovered {
        wait_ready(address, Duration::from_secs(5))?;
        return crate::runner::emit(&json!({"service_ready":true,"registration_recovered":true}));
    }
    // Keep periodic checks silent while healthy, without growing log files.
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_service(_root: &Path, _home: &Path, _address: SocketAddr) -> Result<()> {
    anyhow::bail!(
        "Automatic service recovery is supported on macOS; use your platform service manager"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cold_start_readiness_waits_for_a_delayed_listener() {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            let listener = std::net::TcpListener::bind(address).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"service":"AgentRelay"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        wait_ready(address, Duration::from_secs(3)).unwrap();
        server.join().unwrap();
    }
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "Starts an isolated macOS LaunchAgent and freezes only its fixture process"]
    fn registered_hang_is_restarted_without_touching_other_services() {
        use std::{
            process::Command,
            time::{Duration, Instant},
        };
        let binary =
            std::env::var_os("AGENTRELAY_SERVICE_TEST_BIN").expect("Set the fixture binary path");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("state");
        let executable = root.join(".agents/skills/agentrelay/scripts/agentrelay");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::copy(binary, &executable).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("provider.key"), "fixture").unwrap();
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let label = format!("com.agentrelay.regression.{}", uuid::Uuid::new_v4());
        // SAFETY: getuid has no pointer arguments or side effects.
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let target = format!("{domain}/{label}");
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("launchctl")
                    .args(["bootout", &self.0])
                    .output();
            }
        }
        let _cleanup = Cleanup(target.clone());
        let plist = root.join("Library/LaunchAgents/com.agentrelay.provider.plist");
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        let document = format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>--home</string><string>{}</string><string>serve</string><string>--listen</string><string>{address}</string></array><key>KeepAlive</key><true/><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>",
            xml(&executable.to_string_lossy()),
            xml(&home.to_string_lossy()),
            xml(&home.join("out.log").to_string_lossy()),
            xml(&home.join("err.log").to_string_lossy())
        );
        std::fs::write(&plist, document).unwrap();
        assert!(
            Command::new("launchctl")
                .args(["bootstrap", &domain])
                .arg(&plist)
                .output()
                .unwrap()
                .status
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !healthy(address).unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(100));
        }
        let pid = || {
            let output = Command::new("launchctl")
                .args(["print", &target])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|l| {
                    l.trim()
                        .strip_prefix("pid = ")
                        .and_then(|p| p.parse::<u32>().ok())
                })
                .unwrap()
        };
        let original = pid();
        assert!(ensure_service_named(root, &home, address, &label).is_ok());
        assert_eq!(pid(), original);
        // SAFETY: this is the isolated fixture PID read from its unique label.
        assert_eq!(unsafe { libc::kill(original as i32, libc::SIGSTOP) }, 0);
        let start = Instant::now();
        assert!(
            ensure_service_named(root, &home, address, &label)
                .unwrap_err()
                .to_string()
                .contains("1/3")
        );
        assert_eq!(pid(), original);
        std::thread::sleep(Duration::from_secs(10));
        assert!(
            ensure_service_named(root, &home, address, &label)
                .unwrap_err()
                .to_string()
                .contains("2/3")
        );
        assert_eq!(pid(), original);
        std::thread::sleep(Duration::from_secs(10));
        ensure_service_named(root, &home, address, &label).unwrap();
        assert!(healthy(address).unwrap());
        assert_ne!(pid(), original);
        assert!(!home.join("service-health.json").exists());
        println!(
            "isolated registered-hang recovery passed in {:.2}s",
            start.elapsed().as_secs_f64()
        );
    }
    #[test]
    fn hangs_require_three_separated_failures_of_the_same_process() {
        let one = note_failure(None, 10, 20, 100_000);
        let duplicate = note_failure(Some(one), 10, 20, 101_000);
        assert_eq!(duplicate.count, 1);
        let two = note_failure(Some(duplicate), 10, 20, 110_000);
        assert_eq!(two.count, 2);
        let three = note_failure(Some(two), 10, 20, 120_000);
        assert_eq!(three.count, HEALTH_FAILURE_LIMIT);
        let replaced = note_failure(Some(three), 10, 21, 130_000);
        assert_eq!(replaced.count, 1);
        let stale = note_failure(Some(replaced), 10, 21, 300_000);
        assert_eq!(stale.count, 1);
    }
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

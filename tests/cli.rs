#![cfg(unix)]
use agent_relay::{model::*, store::Store};
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agentrelay"))
}
fn fixture(script: &str) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    let store = Store::open(&home).unwrap();
    let executable = dir.path().join("fixture.sh");
    std::fs::write(
        &executable,
        format!("#!/bin/sh\ncat >/dev/null\n{script}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("runtimes.toml"),
        format!(
            "[runtimes.fixture]\nkind = \"jsonl\"\nbinary = {:?}\nsession_resume = true\n",
            executable.to_str().unwrap()
        ),
    )
    .unwrap();
    (dir, store)
}
fn run(store: &Store, workspace: &std::path::Path, key: &str) -> std::process::Output {
    let mut child = binary()
        .arg("--home")
        .arg(&store.home)
        .args(["run", "--runtime", "fixture", "--workspace"])
        .arg(workspace)
        .args([
            "--key",
            key,
            "--root-id",
            "native-root",
            "--agent-id",
            "native-worker",
            "--agent-path",
            "/root/fixture",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Read fixture")
        .unwrap();
    child.wait_with_output().unwrap()
}
fn task(store: &Store) -> Task {
    store.list(None).unwrap().into_iter().next().unwrap()
}
fn wait_for_worker(store: &Store) -> Task {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(task) = store.list(None).unwrap().into_iter().next()
            && task.worker.is_some()
        {
            return task;
        }
        assert!(Instant::now() < until, "runner did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn runtime_terminal_contract_failure_modes() {
    for (script, success) in [
        (
            "echo '{\"type\":\"session\",\"session_id\":\"external\"}'\necho '{\"type\":\"result\",\"status\":\"completed\",\"text\":\"ok\"}'",
            true,
        ),
        (
            "echo '{\"type\":\"error\",\"message\":\"semantic failure\"}'",
            false,
        ),
        (
            "echo '{\"type\":\"text\",\"text\":\"not a result\"}'",
            false,
        ),
        ("echo 'invalid json'", false),
        (
            "echo '{\"type\":\"result\",\"status\":\"completed\",\"text\":\"ok\"}'\nexit 7",
            false,
        ),
        (
            "echo '{\"type\":\"result\",\"status\":\"completed\",\"text\":\"one\"}'\necho '{\"type\":\"result\",\"status\":\"completed\",\"text\":\"two\"}'",
            false,
        ),
    ] {
        let (dir, store) = fixture(script);
        let output = run(&store, dir.path(), "case");
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let task = task(&store);
        assert_eq!(
            task.status,
            if success {
                Status::Completed
            } else {
                Status::Failed
            }
        );
        assert_eq!(task.result.is_some(), success);
        assert!(task.worker.is_none());
        for line in String::from_utf8(output.stdout).unwrap().lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }
}

#[test]
fn real_process_cancellation_and_timeout_cleanup() {
    for timeout in [false, true] {
        let (dir, store) =
            fixture("echo '{\"type\":\"session\",\"session_id\":\"external\"}'\nsleep 30 &\nwait");
        let prompt = dir.path().join("prompt.txt");
        std::fs::write(&prompt, "Wait").unwrap();
        let mut child = binary()
            .arg("--home")
            .arg(&store.home)
            .args(["run", "--runtime", "fixture", "--workspace"])
            .arg(dir.path())
            .args(["--key", "cancel", "--prompt-file"])
            .arg(prompt)
            .args(["--timeout-seconds", if timeout { "1" } else { "30" }])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let running = wait_for_worker(&store);
        let identity = running.worker.unwrap();
        if !timeout {
            store.interrupt(&running.id, "test interrupt").unwrap();
        }
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            if Instant::now() >= until {
                child.kill().unwrap();
                panic!("runner failed to stop");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let done = task(&store);
        assert_eq!(
            done.status,
            if timeout {
                Status::Failed
            } else {
                Status::Interrupted
            }
        );
        assert!(done.result.is_none());
        assert!(!agent_relay::runner::pid_exists(identity.pid));
        assert_eq!(
            unsafe { libc::kill(-(identity.pid as i32), 0) },
            -1,
            "worker descendants survived"
        );
    }
}

#[test]
fn killed_runner_can_be_reconciled_without_accepting_success() {
    let (dir, store) =
        fixture("echo '{\"type\":\"session\",\"session_id\":\"external\"}'\nsleep 30");
    let prompt = dir.path().join("prompt");
    std::fs::write(&prompt, "wait").unwrap();
    let mut child = binary()
        .arg("--home")
        .arg(&store.home)
        .args(["run", "--runtime", "fixture", "--workspace"])
        .arg(dir.path())
        .args(["--key", "crash", "--prompt-file"])
        .arg(prompt)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = wait_for_worker(&store);
    let worker = running.worker.unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(store.reconcile(now_ms() + LEASE_MS + 1000).unwrap(), 1);
    let recovered = task(&store);
    assert_eq!(recovered.status, Status::Interrupted);
    assert!(recovered.result.is_none());
    let until = Instant::now() + Duration::from_secs(3);
    while agent_relay::runner::pid_exists(worker.pid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!agent_relay::runner::pid_exists(worker.pid));
}

#[test]
fn project_install_export_and_user_file_protection() {
    let project = tempfile::tempdir().unwrap();
    let home = project.path().join("state");
    std::fs::create_dir(project.path().join(".codex")).unwrap();
    std::fs::write(
        project.path().join(".codex/hooks.json"),
        json!({"hooks":{"Interrupt":[{"hooks":[{"type":"command","command":"custom-handler"}]}]}})
            .to_string(),
    )
    .unwrap();
    for _ in 0..2 {
        assert!(
            binary()
                .arg("--home")
                .arg(&home)
                .args(["codex", "install", "--project"])
                .arg(project.path())
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let hooks = std::fs::read_to_string(project.path().join(".codex/hooks.json")).unwrap();
    assert!(hooks.contains("custom-handler"));
    assert_eq!(hooks.matches("custom-handler").count(), 1);
    let skill = project.path().join(".agents/skills/agentrelay/SKILL.md");
    std::fs::write(&skill, "user edits").unwrap();
    assert!(
        !binary()
            .arg("--home")
            .arg(&home)
            .args(["codex", "install", "--project"])
            .arg(project.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(std::fs::read_to_string(skill).unwrap(), "user edits");
    let out = project.path().join("plugin");
    assert!(
        binary()
            .args(["codex", "export", "--out"])
            .arg(&out)
            .output()
            .unwrap()
            .status
            .success()
    );
    let exported = std::fs::read_to_string(out.join("skills/agentrelay/SKILL.md")).unwrap();
    assert!(!exported.contains(out.to_str().unwrap()));
    assert!(exported.contains("scripts/agentrelay"));
    assert!(out.join("plugin.json").exists());
}

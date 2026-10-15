use agent_relay::{
    model::*,
    runtime::{CliAdapter, RuntimeAdapter, RuntimeKind},
    selection,
    store::Store,
};
use serde_json::json;

fn request(workspace: &std::path::Path, model: Option<&str>, effort: Option<&str>) -> RunRequest {
    RunRequest {
        runtime: "claude".into(),
        workspace: workspace.into(),
        prompt: "Test task".into(),
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
        access: Access::ReadOnly,
        root_id: None,
        agent_id: None,
        agent_path: None,
        timeout_secs: 10,
    }
}

#[test]
fn model_and_effort_are_normalized_without_guessing_or_cross_runtime_rewriting() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "Haiku 5.5",
        "haiku5.5",
        "Claude Haiku 5.5",
        "claude-haiku-5-5",
    ] {
        let mut request = request(dir.path(), Some(name), Some(" HIGH "));
        selection::normalize(RuntimeKind::Claude, &mut request).unwrap();
        assert_eq!(request.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(request.effort.as_deref(), Some("high"));
    }
    for name in [
        "claude-future-model-99",
        "arn:aws:bedrock:region:account:inference-profile/ModelID",
    ] {
        let mut request = request(dir.path(), Some(name), None);
        selection::normalize(RuntimeKind::Claude, &mut request).unwrap();
        assert_eq!(request.model.as_deref(), Some(name));
    }
    let mut ambiguous = request(dir.path(), Some("Haiku 5.5 high"), None);
    assert!(selection::normalize(RuntimeKind::Claude, &mut ambiguous).is_err());
    let mut codex = request(dir.path(), Some("gpt-6.1-sol"), Some("xhigh"));
    selection::normalize(RuntimeKind::Codex, &mut codex).unwrap();
    assert_eq!(codex.model.as_deref(), Some("gpt-6.1-sol"));
}

#[test]
fn unsupported_effort_and_outdated_cli_are_rejected_instead_of_downgraded() {
    let dir = tempfile::tempdir().unwrap();
    for (kind, model, effort) in [
        (RuntimeKind::Claude, "claude-haiku-4-5", "high"),
        (RuntimeKind::Claude, "claude-opus-4-6", "xhigh"),
        (RuntimeKind::Claude, "claude-sonnet-4-6", "xhigh"),
        (RuntimeKind::Claude, "claude-haiku-5-5", "minimal"),
        (RuntimeKind::Codex, "gpt-6.1-sol", "ultracode"),
    ] {
        assert!(
            selection::normalize(kind, &mut request(dir.path(), Some(model), Some(effort)))
                .is_err()
        );
    }
    assert!(selection::check_claude_version("claude-haiku-5-5", "2.1.283 (Claude Code)").is_err());
    assert!(selection::check_claude_version("claude-haiku-5-5", "2.1.293 (Claude Code)").is_ok());
    assert!(selection::check_claude_version("claude-haiku-5-5", "2.1.300 (Claude Code)").is_ok());
    assert!(selection::check_claude_version("claude-haiku-5-5", "unknown").is_err());
}

#[test]
fn selection_is_delivered_on_initial_turn_and_resume_and_aliases_are_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let claude = CliAdapter::resolve(dir.path(), "claude").unwrap();
    let mut selected = request(dir.path(), Some("haiku"), Some("high"));
    claude.normalize_request(&mut selected).unwrap();
    let claim = store.begin("first", selected).unwrap();
    let launch = claude.launch(&claim.task).unwrap();
    assert!(launch.args.windows(2).any(|p| p == ["--model", "haiku"]));
    assert!(launch.args.windows(2).any(|p| p == ["--effort", "high"]));
    assert!(
        launch
            .env_remove
            .contains(&"CLAUDE_CODE_EFFORT_LEVEL".into())
    );
    store
        .observe(
            &claim,
            RuntimeEvent::Session {
                id: "external".into(),
                model: Some("claude-haiku-5-5".into()),
            },
        )
        .unwrap();
    store
        .finish(&claim, Status::Completed, Some("ok".into()), None)
        .unwrap();
    let resumed = store.resume(&claim.task.id, "next", "Continue").unwrap();
    assert_eq!(resumed.task.request.effort.as_deref(), Some("high"));
    let launch = claude.launch(&resumed.task).unwrap();
    assert!(
        launch
            .args
            .windows(2)
            .any(|p| p == ["--model", "claude-haiku-5-5"])
    );
    assert!(launch.args.windows(2).any(|p| p == ["--effort", "high"]));
    assert!(
        launch
            .args
            .windows(2)
            .any(|p| p == ["--resume", "external"])
    );
    let codex = CliAdapter::resolve(dir.path(), "codex")
        .unwrap()
        .launch(&resumed.task)
        .unwrap();
    assert!(
        codex
            .args
            .windows(2)
            .any(|p| p == ["--config", "model_reasoning_effort=\"high\""])
    );
}

#[test]
fn jsonl_effort_requires_capability_and_is_optional_for_legacy_drivers() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("runtimes.toml");
    std::fs::write(
        &config,
        "[runtimes.custom]\nkind='jsonl'\nbinary='unused-driver'\n",
    )
    .unwrap();
    let mut selected = request(dir.path(), Some("vendor-model"), Some("deep"));
    let legacy = CliAdapter::resolve(dir.path(), "custom").unwrap();
    assert!(legacy.normalize_request(&mut selected).is_err());
    std::fs::write(
        &config,
        "[runtimes.custom]\nkind='jsonl'\nbinary='unused-driver'\nsupports_effort=true\n",
    )
    .unwrap();
    let adapter = CliAdapter::resolve(dir.path(), "custom").unwrap();
    adapter.normalize_request(&mut selected).unwrap();
    let claim = Store::open(dir.path())
        .unwrap()
        .begin("custom", selected)
        .unwrap();
    let input: serde_json::Value =
        serde_json::from_slice(&adapter.launch(&claim.task).unwrap().stdin).unwrap();
    assert_eq!(input["effort"], "deep");
    let mut task = claim.task;
    task.request.effort = None;
    let input: serde_json::Value =
        serde_json::from_slice(&adapter.launch(&task).unwrap().stdin).unwrap();
    assert!(input.get("effort").is_none());
}

#[test]
fn observed_model_mismatch_is_detected_including_mid_turn_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = CliAdapter::resolve(dir.path(), "claude").unwrap();
    let selected = request(dir.path(), Some("claude-haiku-5-5"), Some("high"));
    assert!(
        adapter
            .validate_observed_model(&selected, "claude-haiku-5-5")
            .is_ok()
    );
    assert!(
        adapter
            .validate_observed_model(&selected, "claude-haiku-5-5-20261001")
            .is_ok()
    );
    assert!(
        adapter
            .validate_observed_model(&selected, "claude-sonnet-5-5")
            .is_err()
    );
    assert!(
        adapter
            .validate_observed_model(&selected, "claude-haiku-5-50")
            .is_err()
    );
    let mut decode = CliAdapter::resolve(dir.path(), "claude").unwrap();
    let events = decode
        .decode(json!({"type":"assistant","message":{"model":"claude-sonnet-5-5","content":[]}}))
        .unwrap();
    assert!(matches!(&events[0],RuntimeEvent::Model{id} if id=="claude-sonnet-5-5"));
}

#[test]
fn old_requests_keep_their_serialized_shape_and_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let original = request(dir.path(), None, None);
    let json = serde_json::to_value(&original).unwrap();
    assert!(json.get("effort").is_none());
    let restored: RunRequest = serde_json::from_value(json.clone()).unwrap();
    assert!(restored.effort.is_none());
    assert_eq!(serde_json::to_value(restored).unwrap(), json);
}

#[cfg(unix)]
#[test]
fn runner_records_real_model_and_fails_when_cli_substitutes_it() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    std::fs::create_dir(&home).unwrap();
    let driver = dir.path().join("claude-fixture");
    // Report the requested init model, then a different actual inference model.
    std::fs::write(&driver,"#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.300 (Claude Code)'; exit 0; fi\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fixture\",\"model\":\"claude-haiku-5-5\"}' '{\"type\":\"assistant\",\"message\":{\"model\":\"claude-sonnet-5-5\",\"content\":[]}}' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"fixture\",\"result\":\"must not succeed\"}'\n").unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("runtimes.toml"),
        format!(
            "[runtimes.claude]\nkind='claude'\nbinary={:?}\nsession_resume=true\n",
            driver.to_str().unwrap()
        ),
    )
    .unwrap();
    let prompt = dir.path().join("prompt");
    std::fs::write(&prompt, "fixture task").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agentrelay"))
        .arg("--home")
        .arg(&home)
        .args([
            "run",
            "--runtime",
            "claude",
            "--model",
            "Haiku 5.5",
            "--effort",
            "high",
            "--workspace",
        ])
        .arg(dir.path())
        .args(["--key", "fixture", "--prompt-file"])
        .arg(prompt)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let task = Store::open(&home).unwrap().list(None).unwrap().remove(0);
    assert_eq!(task.status, Status::Failed);
    assert_eq!(task.actual_model.as_deref(), Some("claude-sonnet-5-5"));
    assert!(task.error.as_deref().unwrap().contains("substituted model"));
    assert!(task.result.is_none());
}

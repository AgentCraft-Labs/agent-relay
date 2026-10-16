use agent_relay::{
    model::*,
    provider::{Assignment, Delivery, Registry},
    store::Store,
};
use serde_json::json;

fn assignment(workspace: &std::path::Path) -> Assignment {
    Assignment {
        root_id: "root-a".into(),
        agent_path: "/root/worker".into(),
        request: RunRequest {
            runtime: "claude".into(),
            workspace: workspace.into(),
            prompt: "Read the fixture".into(),
            model: None,
            effort: None,
            access: Access::ReadOnly,
            root_id: None,
            agent_id: None,
            agent_path: None,
            timeout_secs: 30,
        },
    }
}
fn request(root: &str, child: &str, turn: &str) -> serde_json::Value {
    json!({"model":"agentrelay-claude","client_metadata":{"session_id":root,"thread_id":child,"turn_id":turn,"x-codex-turn-metadata":json!({"agent_name":"/root/worker","sandbox_mode":"read-only"}).to_string()}})
}

#[test]
fn native_binding_rejects_wrong_root_path_runtime_and_duplicate_execution() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.path().join("state")).unwrap();
    registry.prepare(&assignment(dir.path())).unwrap();
    assert!(registry.claim(&request("root-b", "child", "turn")).is_err());
    let mut wrong = request("root-a", "child", "turn");
    wrong["model"] = json!("agentrelay-codex");
    assert!(registry.claim(&wrong).is_err());
    let first = registry.claim(&request("root-a", "child", "turn")).unwrap();
    let Delivery::Execute { claim, .. } = first else {
        panic!("Expected new claim")
    };
    assert!(registry.claim(&request("root-a", "child", "turn")).is_err());
    assert!(
        registry
            .claim(&request("root-a", "other-child", "other-turn"))
            .is_err()
    );
    assert!(registry.prepare(&assignment(dir.path())).is_err());
    let store = Store::open(&dir.path().join("state")).unwrap();
    store
        .finish(&claim, Status::Failed, None, Some("fixture".into()))
        .unwrap();
    // Recovery snapshots a terminal result without another CLI execution.
    let Delivery::Replay(result) = registry.claim(&request("root-a", "child", "turn")).unwrap()
    else {
        panic!("Expected saved result")
    };
    assert_eq!(result["task"]["status"], "failed");
    assert!(matches!(
        registry.claim(&request("root-a", "child", "turn")).unwrap(),
        Delivery::Replay(_)
    ));
    assert_eq!(store.list(None).unwrap().len(), 1);
}

#[test]
fn prepared_prompt_is_snapshotted_and_write_access_cannot_be_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.path().join("state")).unwrap();
    let original = assignment(dir.path());
    registry.prepare(&original).unwrap();
    registry.prepare(&original).unwrap();
    let mut changed = original.clone();
    changed.request.prompt = "Different task".into();
    assert!(registry.prepare(&changed).is_err());
    let Delivery::Execute { claim, .. } =
        registry.claim(&request("root-a", "child", "turn")).unwrap()
    else {
        panic!()
    };
    assert_eq!(claim.task.request.prompt, original.request.prompt);
    Store::open(&dir.path().join("state"))
        .unwrap()
        .finish(&claim, Status::Completed, Some("result".into()), None)
        .unwrap();
    changed.request.access = Access::WorkspaceWrite;
    assert!(registry.prepare(&changed).is_err());
    let mut effort_change = original.clone();
    effort_change.request.prompt = "Continue".into();
    effort_change.request.effort = Some("high".into());
    assert!(registry.prepare(&effort_change).is_err());
    let writable = Registry::open(&dir.path().join("other-state")).unwrap();
    writable.prepare(&changed).unwrap();
    assert!(writable.claim(&request("root-a", "child", "turn")).is_err());
}

#[test]
fn provider_configuration_keeps_models_and_existing_settings_and_backs_up_once() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    let codex = dir.path().join("codex");
    std::fs::create_dir(&codex).unwrap();
    let original = "# User settings\nmodel = \"original\"\n";
    std::fs::write(codex.join("config.toml"), original).unwrap();
    let catalog = dir.path().join("catalog.json");
    let original_model = json!({"slug":"original","custom":"kept","visibility":"list",
        "priority":1,"availability_nux":{"message":"Try this OpenAI model"},
        "upgrade":{"model":"next-openai-model"},"model_messages":{"upgrade":"OpenAI copy"}});
    std::fs::write(&catalog, json!({"models":[original_model]}).to_string()).unwrap();
    for _ in 0..2 {
        agent_relay::codex::configure(&home, &codex, &catalog, "127.0.0.1:39091".parse().unwrap())
            .unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(codex.join("config.before-agentrelay.toml")).unwrap(),
        original
    );
    let config = std::fs::read_to_string(codex.join("config.toml")).unwrap();
    assert!(config.contains("# User settings"));
    assert!(config.contains("model = \"original\""));
    let models: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("models.json")).unwrap()).unwrap();
    assert_eq!(models["models"][0], original_model);
    assert_eq!(models["models"].as_array().unwrap().len(), 3);
    for model in models["models"].as_array().unwrap().iter().skip(1) {
        assert_eq!(model["visibility"], "hide");
        assert_eq!(model["priority"], i32::MAX);
        for field in ["availability_nux", "upgrade", "model_messages"] {
            assert!(model[field].is_null(), "Inherited {field}: {model}");
        }
    }
    assert!(
        agent_relay::codex::configure(&home, &codex, &catalog, "0.0.0.0:39091".parse().unwrap())
            .is_err()
    );
}

#[test]
fn native_profiles_install_real_catalog_roles_and_reject_mismatched_picker_settings() {
    use agent_relay::profiles::{self, Profile};
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    let codex = dir.path().join("codex");
    let registry = Registry::open(&home).unwrap();
    let profile = Profile {
        runtime: "claude".into(),
        model: "Haiku 5.5".into(),
        effort: Some("high".into()),
        display_name: "Claude Haiku 5.5".into(),
    };
    let canonical = profiles::add(&home, "haiku55_high", profile.clone()).unwrap();
    assert_eq!(canonical.model, "claude-haiku-5-5");
    profiles::add(&home, "haiku55_high", profile.clone()).unwrap();
    let mut changed = profile;
    changed.effort = Some("low".into());
    assert!(profiles::add(&home, "haiku55_high", changed).is_err());
    let catalog = dir.path().join("catalog.json");
    std::fs::write(
        &catalog,
        json!({"models":[{"slug":"openai","retained":true,"visibility":"list",
            "priority":1,"availability_nux":{"message":"OpenAI promotion"},
            "upgrade":{"model":"next-openai-model"}}]})
        .to_string(),
    )
    .unwrap();
    agent_relay::codex::configure(&home, &codex, &catalog, "127.0.0.1:39091".parse().unwrap())
        .unwrap();
    let generated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("models.json")).unwrap()).unwrap();
    let model = generated["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == profiles::model_alias("haiku55_high"))
        .unwrap();
    assert_eq!(model["display_name"], "Claude Haiku 5.5");
    assert_eq!(model["default_reasoning_level"], "high");
    assert_eq!(model["visibility"], "hide");
    assert_eq!(model["priority"], i32::MAX);
    assert!(model["availability_nux"].is_null());
    assert!(model["upgrade"].is_null());
    let role = codex.join("agents/agentrelay_profile_haiku55_high.toml");
    let role_data = std::fs::read_to_string(&role).unwrap();
    assert!(role_data.contains("model_reasoning_effort = \"high\""));
    let mut task = assignment(dir.path());
    task.request.model = Some("Haiku 5.5".into());
    task.request.effort = Some("high".into());
    registry.prepare(&task).unwrap();
    assert!(registry.claim(&request("root-a", "child", "turn")).is_err());
    let mut input = request("root-a", "child", "turn");
    input["model"] = json!(profiles::model_alias("haiku55_high"));
    input["reasoning"] = json!({"effort":"low"});
    assert!(registry.claim(&input).is_err());
    input["reasoning"]["effort"] = json!("high");
    assert!(matches!(
        registry.claim(&input).unwrap(),
        Delivery::Execute { .. }
    ));
    std::fs::write(&role, "user edited role").unwrap();
    assert!(
        agent_relay::codex::configure(&home, &codex, &catalog, "127.0.0.1:39091".parse().unwrap())
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(role).unwrap(), "user edited role");
}

#[cfg(unix)]
#[tokio::test]
async fn http_authentication_replay_and_disconnect_cleanup() {
    use std::{os::unix::fs::PermissionsExt, process::Stdio, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    let registry = Registry::open(&home).unwrap();
    let driver = dir.path().join("driver.sh");
    std::fs::write(&driver,"#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"session\",\"session_id\":\"fixture-session\"}'\nif [ -f slow ]; then sleep 20; fi\nprintf x >> executions\nprintf '%s\\n' '{\"type\":\"result\",\"status\":\"completed\",\"text\":\"actual fixture result\",\"session_id\":\"fixture-session\"}'\n").unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("runtimes.toml"),
        format!(
            "[runtimes.fixture]\nkind=\"jsonl\"\nbinary={:?}\nsession_resume=true\n",
            driver.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::write(home.join("provider.key"), "test-secret").unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_agentrelay"))
        .arg("--home")
        .arg(&home)
        .args(["serve", "--listen", &address.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = reqwest::Client::new();
    let url = format!("http://{address}");
    for _ in 0..100 {
        if client.get(format!("{url}/health")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut assignment = assignment(dir.path());
    assignment.request.runtime = "fixture".into();
    registry.prepare(&assignment).unwrap();
    let mut input = request("root-a", "child", "turn-one");
    input["model"] = json!("agentrelay-fixture");
    let request = |input: &serde_json::Value| {
        client
            .post(format!("{url}/responses"))
            .header("content-type", "application/json")
            .body(input.to_string())
    };
    assert_eq!(request(&input).send().await.unwrap().status(), 400);
    let first = request(&input)
        .header(agent_relay::provider::KEY_HEADER, "test-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(first.contains("actual fixture result"));
    assert!(first.contains("Read the fixture"));
    assert!(first.contains("response.completed"));
    let replay = request(&input)
        .header(agent_relay::provider::KEY_HEADER, "test-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(replay.contains("actual fixture result"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("executions")).unwrap(),
        "x"
    );
    // Resume a slow CLI turn, then close the actual HTTP response body.
    std::fs::write(dir.path().join("slow"), "").unwrap();
    assignment.request.prompt = "Wait for cancellation".into();
    registry.prepare(&assignment).unwrap();
    input["client_metadata"]["turn_id"] = json!("turn-two");
    let response = request(&input)
        .header(agent_relay::provider::KEY_HEADER, "test-secret")
        .send()
        .await
        .unwrap();
    let store = Store::open(&home).unwrap();
    let mut worker = None;
    for _ in 0..100 {
        let task = store.list(None).unwrap().remove(0);
        if task.generation == 2 && task.worker.is_some() {
            worker = task.worker;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let worker = worker.expect("CLI did not start");
    drop(response);
    let mut stopped = false;
    for _ in 0..150 {
        let task = store.list(None).unwrap().remove(0);
        if task.status == Status::Interrupted && !agent_relay::runner::pid_exists(worker.pid) {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(stopped, "Disconnect did not stop/reap external process");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("executions")).unwrap(),
        "x"
    );
    process.kill().await.unwrap();
    process.wait().await.unwrap();
}

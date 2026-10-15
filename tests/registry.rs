use agent_relay::{codex::remove_owned_hooks, model::*, store::Store};
use serde_json::json;
use std::sync::{Arc, Barrier};

fn request() -> RunRequest {
    RunRequest {
        runtime: "claude".into(),
        workspace: std::env::current_dir().unwrap(),
        prompt: "Read the fixture".into(),
        model: None,
        access: Access::ReadOnly,
        root_id: Some("root-a".into()),
        agent_id: Some("worker-a".into()),
        agent_path: Some("/root/worker_a".into()),
        timeout_secs: 30,
    }
}

#[test]
fn concurrent_duplicate_submission_executes_once_and_conflicts_fail() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let barrier = Arc::new(Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.begin("same-key", request()).unwrap()
            })
        })
        .collect();
    let claims: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(claims.iter().filter(|c| c.execute).count(), 1);
    assert!(claims.iter().all(|c| c.task.id == claims[0].task.id));
    let mut changed = request();
    changed.prompt = "Something different".into();
    assert!(store.begin("same-key", changed).is_err());
    assert_eq!(store.events(&claims[0].task.id, 0).unwrap().len(), 1);
}

#[test]
fn resume_consumes_fifo_once_and_rejects_stale_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let first = store.begin("first", request()).unwrap();
    let id = &first.task.id;
    store
        .observe(
            &first,
            RuntimeEvent::Session {
                id: "external-session".into(),
                model: Some("actual-model".into()),
            },
        )
        .unwrap();
    store.message(id, "one", "FIRST").unwrap();
    store.message(id, "two", "SECOND").unwrap();
    assert_eq!(
        store.message(id, "one", "FIRST").unwrap()["duplicate"],
        true
    );
    assert!(store.message(id, "one", "CHANGED").is_err());
    assert!(store.resume(id, "next", "Do more").is_err());
    store
        .finish(&first, Status::Completed, Some("done".into()), None)
        .unwrap();
    let next = store.resume(id, "next", "Do more").unwrap();
    assert_eq!(next.task.generation, 2);
    assert_eq!(next.task.session_id.as_deref(), Some("external-session"));
    assert!(
        next.task.request.prompt.find("FIRST").unwrap()
            < next.task.request.prompt.find("SECOND").unwrap()
    );
    assert!(!store.resume(id, "next", "Do more").unwrap().execute);
    assert!(
        store
            .observe(&first, RuntimeEvent::Text("stale".into()))
            .is_err()
    );
    assert!(
        store
            .finish(&first, Status::Completed, Some("stale".into()), None)
            .is_err()
    );
    store
        .finish(&next, Status::Completed, Some("next done".into()), None)
        .unwrap();
    let third = store.resume(id, "third", "Fresh prompt").unwrap();
    assert_eq!(third.task.request.prompt, "Fresh prompt");
    let events = store.events(id, 0).unwrap();
    assert!(events.windows(2).all(|p| p[0].sequence < p[1].sequence));
    assert!(
        store
            .events(id, events.last().unwrap().sequence)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn interruption_wins_over_late_success_and_expiry_cleans_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let claim = store.begin("cancel", request()).unwrap();
    store.interrupt(&claim.task.id, "cancel").unwrap();
    assert!(!store.heartbeat(&claim, None).unwrap());
    assert!(
        store
            .observe(&claim, RuntimeEvent::Text("late".into()))
            .is_err()
    );
    let task = store
        .finish(&claim, Status::Completed, Some("late success".into()), None)
        .unwrap();
    assert_eq!(task.status, Status::Interrupted);
    assert!(task.result.is_none());
    let claim = store.begin("expire", request()).unwrap();
    store
        .observe(
            &claim,
            RuntimeEvent::Session {
                id: "session".into(),
                model: None,
            },
        )
        .unwrap();
    assert_eq!(store.reconcile(now_ms() + LEASE_MS + 1000).unwrap(), 1);
    assert!(store.resume(&claim.task.id, "blocked", "next").is_err());
    let task = store
        .finish(&claim, Status::Completed, Some("late".into()), None)
        .unwrap();
    assert_eq!(task.status, Status::Interrupted);
    assert!(task.runner_pid.is_none());
    assert!(
        store
            .resume(&claim.task.id, "after-cleanup", "next")
            .unwrap()
            .execute
    );
}

#[test]
fn session_identity_cannot_change_and_capacity_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let claim = store.begin("first", request()).unwrap();
    store
        .observe(
            &claim,
            RuntimeEvent::Session {
                id: "one".into(),
                model: None,
            },
        )
        .unwrap();
    assert!(
        store
            .observe(
                &claim,
                RuntimeEvent::Result {
                    text: "result".into(),
                    session_id: Some("two".into())
                }
            )
            .is_err()
    );
    for n in 1..8 {
        store.begin(&format!("key-{n}"), request()).unwrap();
    }
    assert!(store.begin("ninth", request()).is_err());
    assert!(store.list(Some("other-root")).unwrap().is_empty());
    assert_eq!(store.list(Some("root-a")).unwrap().len(), 8);
}

#[test]
fn legacy_hook_removal_preserves_unrelated_handlers() {
    let value = json!({"hooks":{"Interrupt":[{"hooks":[{"type":"command","command":"old relay"},{"type":"command","command":"unrelated"}]}]}});
    let cleaned = remove_owned_hooks(value, "old relay").unwrap();
    assert!(!cleaned.to_string().contains("old relay"));
    assert!(cleaned.to_string().contains("unrelated"));
    assert_eq!(
        remove_owned_hooks(cleaned.clone(), "old relay").unwrap(),
        cleaned
    );
}

#[cfg(unix)]
#[test]
fn registry_permissions_and_wrong_process_identity() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path()).unwrap();
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("registry.sqlite"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut identity = agent_relay::runner::process_identity(std::process::id()).unwrap();
    identity.start_time += 1;
    assert!(!agent_relay::runner::kill_verified_worker(&identity));
}

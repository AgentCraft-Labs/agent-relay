use agent_relay::{
    model::*,
    runner::read_frame,
    runtime::{CliAdapter, RuntimeAdapter},
    store::Store,
};
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};

#[test]
fn claude_semantic_failure_and_codex_result_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let mut claude = CliAdapter::resolve(dir.path(), "claude").unwrap();
    let failure=claude.decode(json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":["denied"],"session_id":"session"})).unwrap();
    assert!(matches!(failure.last(),Some(RuntimeEvent::Error(text)) if text.contains("denied")));
    assert!(
        claude
            .decode(json!({"type":"result","subtype":"success","is_error":false}))
            .is_err()
    );
    let mut codex = CliAdapter::resolve(dir.path(), "codex").unwrap();
    assert!(codex.decode(json!({"type":"turn.completed"})).is_err());
    codex
        .decode(
            json!({"type":"item.completed","item":{"type":"agent_message","text":"actual result"}}),
        )
        .unwrap();
    let events = codex
        .decode(json!({"type":"turn.completed","usage":{"input_tokens":1}}))
        .unwrap();
    assert!(matches!(events.last(),Some(RuntimeEvent::Result{text,..}) if text=="actual result"));
    assert!(!claude.capabilities().live_input);
    assert!(!claude.capabilities().cooperative_interrupt);
}

#[test]
fn commands_have_explicit_access_and_resume_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let claim = store
        .begin(
            "key",
            RunRequest {
                runtime: "claude".into(),
                workspace: dir.path().into(),
                prompt: "literal $(do-not-execute)".into(),
                model: None,
                access: Access::ReadOnly,
                root_id: None,
                agent_id: None,
                agent_path: None,
                timeout_secs: 10,
            },
        )
        .unwrap();
    let claude = CliAdapter::resolve(dir.path(), "claude").unwrap();
    let launch = claude.launch(&claim.task).unwrap();
    assert!(launch.args.iter().any(|arg| arg == "Read,Glob,Grep"));
    assert!(!launch.args.iter().any(|arg| arg.contains("bypass")));
    assert_eq!(launch.stdin, b"literal $(do-not-execute)");
    store
        .observe(
            &claim,
            RuntimeEvent::Session {
                id: "same-session".into(),
                model: None,
            },
        )
        .unwrap();
    store
        .finish(&claim, Status::Completed, Some("ok".into()), None)
        .unwrap();
    let next = store.resume(&claim.task.id, "next", "again").unwrap();
    let launch = claude.launch(&next.task).unwrap();
    assert!(
        launch
            .args
            .windows(2)
            .any(|p| p == ["--resume", "same-session"])
    );
    let codex = CliAdapter::resolve(dir.path(), "codex")
        .unwrap()
        .launch(&next.task)
        .unwrap();
    assert!(!codex.args.iter().any(|a| a == "--color"));
    assert!(
        codex
            .args
            .windows(2)
            .any(|p| p == ["resume", "same-session"])
    );
}

#[tokio::test]
async fn partial_frame_survives_cancelled_read_and_size_limit() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let mut reader = BufReader::new(reader);
    let mut pending = Vec::new();
    writer.write_all(b"{\"type\":").await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            read_frame(&mut reader, &mut pending)
        )
        .await
        .is_err()
    );
    assert_eq!(pending, b"{\"type\":");
    writer.write_all(b"\"text\"}\n").await.unwrap();
    assert_eq!(
        read_frame(&mut reader, &mut pending)
            .await
            .unwrap()
            .unwrap(),
        "{\"type\":\"text\"}\n"
    );
    let oversized = vec![b'x'; MAX_FRAME + 1];
    let mut reader = BufReader::new(oversized.as_slice());
    assert!(read_frame(&mut reader, &mut pending).await.is_err());
}

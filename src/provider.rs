//! Local Responses provider. Native Codex threads remain owned by Codex;
//! only requests explicitly bound to an external runtime execute a CLI.
use crate::database::Connection;
use crate::{model::*, runner, runtime::CliAdapter, store::Store};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub const UPSTREAM: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const KEY_HEADER: &str = "x-agentrelay-key";

#[derive(Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub root_id: String,
    pub agent_path: String,
    pub request: RunRequest,
}

#[derive(Clone)]
pub struct Registry {
    home: PathBuf,
}

pub enum Delivery {
    Execute { claim: Box<Claim>, turn: String },
    Replay(Value),
}

impl Registry {
    pub fn open(home: &Path) -> Result<Self> {
        Store::open(home)?;
        let registry = Self {
            home: home.to_owned(),
        };
        registry.conn()?.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS assignments (
                 root TEXT NOT NULL, path TEXT NOT NULL, body TEXT NOT NULL,
                 child TEXT, task TEXT, pending INTEGER NOT NULL,
                 native TEXT,
                 PRIMARY KEY(root,path));
             CREATE TABLE IF NOT EXISTS native_turns (
                 id TEXT PRIMARY KEY, root TEXT NOT NULL, child TEXT NOT NULL,
                 task TEXT NOT NULL, generation INTEGER NOT NULL, result TEXT);",
        )?;
        // Snapshot legacy bindings before profiles can be added or changed.
        // New selections must never relabel or strand an existing worker.
        let mut conn = registry.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_native = tx
            .prepare("PRAGMA table_info(assignments)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "native");
        if !has_native {
            tx.execute("ALTER TABLE assignments ADD COLUMN native TEXT", [])?;
        }
        let legacy = tx
            .prepare("SELECT root,path,body FROM assignments WHERE native IS NULL")?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (root, path, body) in legacy {
            let assignment: Assignment = serde_json::from_str(&body)?;
            let mut native = crate::profiles::native_selection(home, &assignment.request)?;
            if native["profile"].is_null()
                && (assignment.request.model.is_some() || assignment.request.effort.is_some())
            {
                native["legacy_native_label"] = json!(true);
            }
            tx.execute(
                "UPDATE assignments SET native=?1 WHERE root=?2 AND path=?3",
                params![native.to_string(), root, path],
            )?;
        }
        tx.commit()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                home.join("native.sqlite"),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }
        Ok(registry)
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(self.home.join("native.sqlite"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    pub fn prepare(&self, assignment: &Assignment) -> Result<Value> {
        let mut assignment = assignment.clone();
        CliAdapter::resolve(&self.home, &assignment.request.runtime)?
            .normalize_request(&mut assignment.request)?;
        let assignment = &assignment;
        ensure!(
            !assignment.root_id.is_empty(),
            "An actual root session ID is required"
        );
        ensure!(
            assignment.agent_path.starts_with("/root/") && !assignment.agent_path.contains(".."),
            "Use a canonical native agent path"
        );
        ensure!(
            !assignment.request.prompt.trim().is_empty()
                && assignment.request.prompt.len() <= MAX_FRAME,
            "Invalid task length"
        );
        ensure!(
            assignment.request.workspace.is_absolute() && assignment.request.workspace.is_dir(),
            "Workspace must be an existing absolute directory"
        );
        ensure!(
            (1..=86400).contains(&assignment.request.timeout_secs),
            "Invalid timeout"
        );
        ensure!(
            assignment
                .request
                .runtime
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Invalid runtime name"
        );
        CliAdapter::resolve(&self.home, &assignment.request.runtime)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, Option<String>, bool, String)> = tx
            .query_row(
                "SELECT body,task,pending,native FROM assignments WHERE root=?1 AND path=?2",
                params![assignment.root_id, assignment.agent_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let native = if let Some((body, task, pending, native)) = previous {
            let native: Value = serde_json::from_str(&native)?;
            let old: Assignment = serde_json::from_str(&body)?;
            let mut settings = old.request.clone();
            settings.prompt = assignment.request.prompt.clone();
            let original = serde_json::to_value(settings)?;
            let updated = serde_json::to_value(&assignment.request)?;
            let original_fields = original.as_object().context("Invalid request settings")?;
            let updated_fields = updated.as_object().context("Invalid updated settings")?;
            let keys = original_fields
                .keys()
                .chain(updated_fields.keys())
                .collect::<std::collections::BTreeSet<_>>();
            let changed = keys
                .into_iter()
                .filter(|key| original.get(*key) != updated.get(*key))
                .map(|key| key.as_str())
                .collect::<Vec<_>>();
            ensure!(
                changed.is_empty(),
                "A follow-up must preserve execution settings (changed: {}); use a new agent path to change model, effort, workspace or access",
                changed.join(", ")
            );
            if pending {
                ensure!(
                    body == serde_json::to_string(assignment)?,
                    "An unconsumed task is already prepared"
                );
                return Ok(native);
            }
            if let Some(task) = task {
                ensure!(
                    Store::open(&self.home)?.get(&task)?.status.terminal(),
                    "Wait for the active external turn before preparing a follow-up"
                );
            }
            native
        } else {
            let native = crate::profiles::native_selection(&self.home, &assignment.request)?;
            ensure!(
                (assignment.request.model.is_none() && assignment.request.effort.is_none())
                    || native["profile"].is_string(),
                "No native profile matches the requested model/effort. Register it with profile add, run codex configure, and start a new Codex conversation before preparing this task; refusing a misleading generic model/effort label"
            );
            native
        };
        tx.execute(
            "INSERT INTO assignments(root,path,body,pending,native) VALUES(?1,?2,?3,1,?4)
            ON CONFLICT(root,path) DO UPDATE SET body=excluded.body,pending=1",
            params![
                assignment.root_id,
                assignment.agent_path,
                serde_json::to_string(assignment)?,
                native.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(native)
    }

    pub fn claim(&self, request: &Value) -> Result<Delivery> {
        let meta = &request["client_metadata"];
        let root = meta["session_id"]
            .as_str()
            .context("Missing native root identity")?;
        let child = meta["thread_id"]
            .as_str()
            .context("Missing native thread identity")?;
        let turn = meta["turn_id"]
            .as_str()
            .context("Missing native turn identity")?;
        ensure!(child != root, "CLI models are reserved for native children");
        let extra: Value = serde_json::from_str(
            meta["x-codex-turn-metadata"]
                .as_str()
                .context("Missing native agent metadata")?,
        )?;
        let path = extra["agent_name"]
            .as_str()
            .context("Missing canonical native agent path")?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String, i64, Option<String>)> = tx
            .query_row(
                "SELECT root,child,task,generation,result FROM native_turns WHERE id=?1",
                [turn],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((owner, thread, task, generation, result)) = existing {
            ensure!(
                owner == root && thread == child,
                "Native turn belongs to another thread"
            );
            if let Some(result) = result {
                return Ok(Delivery::Replay(serde_json::from_str(&result)?));
            }
            let task = Store::open(&self.home)?.get(&task)?;
            ensure!(
                task.generation == generation && task.status.terminal(),
                "Native turn is active or recovery is required; refusing duplicate execution"
            );
            let result = json!({"prompt":task.request.prompt,"task":runner::summary(&task)});
            tx.execute(
                "UPDATE native_turns SET result=?1 WHERE id=?2",
                params![result.to_string(), turn],
            )?;
            tx.commit()?;
            return Ok(Delivery::Replay(result));
        }
        let (body, bound_child, task, pending, native): (
            String,
            Option<String>,
            Option<String>,
            bool,
            String,
        ) = tx
            .query_row(
                "SELECT body,child,task,pending,native FROM assignments WHERE root=?1 AND path=?2",
                params![root, path],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .context("No explicit task prepared for this root and native agent path")?;
        ensure!(
            pending,
            "No new task prepared; refusing to reuse an old prompt"
        );
        ensure!(
            bound_child.as_deref().is_none_or(|id| id == child),
            "This task path is already bound to another native thread"
        );
        let assignment: Assignment = serde_json::from_str(&body)?;
        let native: Value = serde_json::from_str(&native)?;
        ensure!(
            request["model"] == native["model"],
            "Native model does not match the prepared selection; spawn role {} with model {}",
            native["role"],
            native["model"]
        );
        if native["profile"].is_string()
            && let Some(effort) = &assignment.request.effort
        {
            ensure!(
                request["reasoning"]["effort"].as_str() == Some(effort.as_str()),
                "Native effort does not match the pinned external profile; choose its matching role"
            );
        }
        if assignment.request.access == Access::WorkspaceWrite {
            ensure!(
                matches!(
                    extra["sandbox_mode"].as_str(),
                    Some("workspace-write" | "danger-full-access")
                ),
                "Native execution policy does not allow workspace writes"
            );
        }
        // These are transport assertions, not an OS security boundary. Explicit
        // workspace/access settings were granted when the task was prepared.
        let store = Store::open(&self.home)?;
        let key = format!("native-{turn}");
        let claim = if let Some(task) = task {
            store.resume(&task, &key, &assignment.request.prompt)?
        } else {
            let mut run = assignment.request;
            run.root_id = Some(root.to_owned());
            run.agent_id = Some(child.to_owned());
            run.agent_path = Some(path.to_owned());
            store.begin(&key, run)?
        };
        ensure!(
            claim.execute,
            "A previous attempt already owns this turn; inspect registry and reconcile before retrying"
        );
        tx.execute(
            "INSERT INTO native_turns(id,root,child,task,generation) VALUES(?1,?2,?3,?4,?5)",
            params![turn, root, child, claim.task.id, claim.task.generation],
        )?;
        tx.execute(
            "UPDATE assignments SET pending=0,child=?1,task=?2 WHERE root=?3 AND path=?4",
            params![child, claim.task.id, root, path],
        )?;
        tx.commit()?;
        Ok(Delivery::Execute {
            claim: Box::new(claim),
            turn: turn.to_owned(),
        })
    }

    pub fn complete(&self, turn: &str, result: &Value) -> Result<()> {
        ensure!(
            self.conn()?.execute(
                "UPDATE native_turns SET result=?1 WHERE id=?2 AND result IS NULL",
                params![serde_json::to_string(result)?, turn]
            )? == 1,
            "Native turn completion is already recorded or missing"
        );
        Ok(())
    }
}

#[derive(Clone)]
struct Service {
    home: PathBuf,
    key: String,
    client: reqwest::Client,
}

pub fn task_receipt(prompt: &str) -> String {
    format!("Assigned task (recorded by AgentRelay):\n\n{prompt}")
}

/// CLI flags prove delivery of effort, not the provider's applied reasoning.
pub fn execution_details(summary: &Value) -> String {
    if summary["requested_model"].is_null() && summary["requested_effort"].is_null() {
        return String::new();
    }
    format!(
        "\n\nExecution settings (recorded by AgentRelay):\nRuntime: {}\nRequested model: {}\nObserved model: {}\nRequested effort: {} (passed to CLI; applied effort is not independently reported)",
        summary["runtime"].as_str().unwrap_or("unknown"),
        summary["requested_model"]
            .as_str()
            .unwrap_or("runtime default"),
        summary["model"]
            .as_str()
            .unwrap_or("not reported when this receipt was recorded; see the final result"),
        summary["requested_effort"]
            .as_str()
            .unwrap_or("runtime default")
    )
}

pub fn message(text: &str, phase: &str) -> Value {
    json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","id":uuid::Uuid::new_v4().to_string(),"phase":phase,"content":[{"type":"output_text","text":text}]}})
}

fn frame(event: Value) -> Result<Bytes> {
    Ok(Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        event["type"].as_str().context("Event type")?,
        event
    )))
}

async fn send(tx: &mpsc::Sender<Result<Bytes, Infallible>>, event: Value) -> bool {
    match frame(event) {
        Ok(bytes) => tx.send(Ok(bytes)).await.is_ok(),
        Err(_) => false,
    }
}

pub async fn serve(home: &Path, address: SocketAddr) -> Result<()> {
    ensure!(
        address.ip().is_loopback(),
        "Provider must listen on loopback"
    );
    Registry::open(home)?;
    let key =
        std::fs::read_to_string(home.join("provider.key")).context("Run codex configure first")?;
    let state = Arc::new(Service {
        home: home.to_owned(),
        key: key.trim().to_owned(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .build()?,
    });
    let router = Router::new()
        .route("/health", get(|| async { "AgentRelay" }))
        .route("/version", get(|| async { axum::Json(json!({"service":"AgentRelay","version":env!("CARGO_PKG_VERSION"),"sse_heartbeat_seconds":15})) }))
        .route("/responses", post(responses))
        .route("/v1/responses", post(responses))
        .layer(DefaultBodyLimit::max(16 * MAX_FRAME))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address).await?;
    let recovery = Store::open(home)?;
    let watchdog = tokio::spawn(async move {
        loop {
            let store = recovery.clone();
            let _ = tokio::task::spawn_blocking(move || store.reconcile(now_ms())).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    runner::emit(&json!({"provider_listening":listener.local_addr()?,"upstream":UPSTREAM}))?;
    let outcome = axum::serve(listener, router)
        .with_graceful_shutdown(runner::shutdown_signal())
        .await;
    watchdog.abort();
    outcome?;
    Ok(())
}

async fn responses(State(state): State<Arc<Service>>, headers: HeaderMap, body: Bytes) -> Response {
    match respond(state, headers, body).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":{"message":format!("{error:#}"),"type":"agentrelay_error"}})),
        )
            .into_response(),
    }
}

async fn respond(state: Arc<Service>, headers: HeaderMap, body: Bytes) -> Result<Response> {
    ensure!(
        headers.get(KEY_HEADER).and_then(|v| v.to_str().ok()) == Some(state.key.as_str()),
        "Invalid local provider key"
    );
    let request: Value = serde_json::from_slice(&body)?;
    let model = request["model"].as_str().context("Missing model")?;
    if !model.starts_with("agentrelay-") {
        let mut outgoing = state.client.post(UPSTREAM);
        for (name, value) in &headers {
            if !matches!(
                name.as_str(),
                "host"
                    | "content-length"
                    | "connection"
                    | "accept-encoding"
                    | "content-encoding"
                    | KEY_HEADER
            ) {
                outgoing = outgoing.header(name, value);
            }
        }
        // Preserve streaming, status and upstream headers; never log credentials.
        let upstream = outgoing.body(body).send().await?;
        let mut response = Response::builder().status(upstream.status());
        for (name, value) in upstream.headers() {
            if !matches!(
                name.as_str(),
                "connection" | "transfer-encoding" | "content-length"
            ) {
                response = response.header(name, value);
            }
        }
        return Ok(response.body(Body::from_stream(upstream.bytes_stream()))?);
    }
    let home = state.home.clone();
    let (registry, delivery) = tokio::task::spawn_blocking(move || -> Result<_> {
        let registry = Registry::open(&home)?;
        let delivery = registry.claim(&request)?;
        Ok((registry, delivery))
    })
    .await??;
    let (tx, rx) = mpsc::channel(8);
    // Isolate synchronous database/VFS work and process inspection from the
    // HTTP reactor. A slow worker must not stall normal OpenAI chats or health.
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Native worker runtime");
        runtime.block_on(deliver_native(state, registry, delivery, tx));
    });
    Ok(Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(ReceiverStream::new(rx)))?)
}

async fn deliver_native(
    state: Arc<Service>,
    registry: Registry,
    delivery: Delivery,
    tx: mpsc::Sender<Result<Bytes, Infallible>>,
) {
    let id = uuid::Uuid::new_v4().to_string();
    if !send(&tx, json!({"type":"response.created","response":{"id":id}})).await {
        if let Delivery::Execute { claim, .. } = &delivery {
            let store = Store::open(&state.home).expect("Existing registry");
            let _ = store.finish(
                claim,
                Status::Interrupted,
                None,
                Some("Client disconnected before startup".into()),
            );
        }
        return;
    }
    let outcome: Result<Value> = async {
            match delivery {
                Delivery::Replay(result) => Ok(result),
                Delivery::Execute { claim, turn } => {
                    let prompt = claim.task.request.prompt.clone();
                    let receipt = format!(
                        "{}{}",
                        task_receipt(&prompt),
                        execution_details(&runner::summary(&claim.task))
                    );
                    send(&tx, message(&receipt, "commentary")).await;
                    let store = Store::open(&state.home)?;
                    let adapter = CliAdapter::resolve(&state.home, &claim.task.request.runtime)?;
                    let task_id = claim.task.id.clone();
                    let run = runner::execute(&store, *claim, adapter);
                    tokio::pin!(run);
                    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
                    heartbeat.tick().await;
                    let task = loop {
                        tokio::select! {
                            task = &mut run => break task?,
                            _ = tx.closed() => {
                                store.interrupt(&task_id,"Native provider stream disconnected")?;
                                break run.await?;
                            }
                            _ = heartbeat.tick() => {
                                // A real SSE data event resets Codex's idle timer.
                                // SSE comments alone are swallowed by its parser.
                                let progress = frame(json!({"type":"response.in_progress","response":{"id":id,"status":"in_progress"}}))?;
                                match tx.try_send(Ok(progress)) {
                                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {},
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        store.interrupt(&task_id,"Native provider stream disconnected")?;
                                        break run.await?;
                                    }
                                }
                            }
                        }
                    };
                    let result = json!({"prompt":prompt,"task":runner::summary(&task)});
                    registry.complete(&turn, &result)?;
                    Ok(result)
                }
            }
        }
        .await;
    match outcome {
        Ok(result) if result["task"]["status"] == "completed" => {
            let answer = result["task"]["result"].as_str().unwrap_or_default();
            // Repeat the receipt in the final message so Summary views,
            // which retain only the last assistant item, keep the task too.
            let final_text = format!(
                "{}{}\n\n---\n\nResult:\n\n{}",
                task_receipt(result["prompt"].as_str().unwrap_or_default()),
                execution_details(&result["task"]),
                answer
            );
            send(&tx, message(&final_text, "final_answer")).await;
            // CLI billing remains in the registry. No fabricated OpenAI usage.
            send(
                &tx,
                json!({"type":"response.completed","response":{"id":id}}),
            )
            .await;
        }
        outcome => {
            let error = match outcome {
                Ok(result) => result["task"].to_string(),
                Err(error) => format!("{error:#}"),
            };
            send(&tx,json!({"type":"response.failed","response":{"id":id,"error":{"code":"agentrelay_runtime_error","message":error}}})).await;
        }
    }
}

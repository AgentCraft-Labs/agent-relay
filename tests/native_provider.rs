//! Opt-in black-box probes against an unmodified official Codex app-server.
//! Seven probes use a scripted parent. Selected positive probes launch real
//! Claude turns. The separately ignored live-parent probe also calls OpenAI.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader as SyncReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
};

const PARENT: &str = "agentrelay-parent-fixture";
const CHILD: &str = "agentrelay-claude";
const TASK: &str = "Read numbers.txt from this workspace using Read. Compute the sum. Return exactly NATIVE_CLAUDE_SUM=73 if the sum is 73. Do not modify files or call other agents.";
const FOLLOWUP: &str = "Without rereading the file, recall the sum from your previous turn and double it. Return exactly NATIVE_CLAUDE_DOUBLED=146. Do not modify files or call other agents.";

struct State {
    requests: Mutex<Vec<Value>>,
    root_calls: AtomicUsize,
    claude_calls: AtomicUsize,
    route_claude: bool,
    plaintext: bool,
    role: String,
    workspace: PathBuf,
    relay: PathBuf,
    runtime_results: Mutex<Vec<Value>>,
    handoff: Mutex<Option<Value>>,
    relay_task: Mutex<Option<String>>,
    followup: bool,
    live_parent: bool,
    forward_statuses: Mutex<Vec<u16>>,
    gateway: Option<String>,
}

struct Server {
    address: String,
    stop: Arc<AtomicBool>,
    state: Arc<State>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(state: State) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = format!("http://{}", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(state);
        let stopped = stop.clone();
        let shared = state.clone();
        let handle = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = shared.clone();
                        std::thread::spawn(move || {
                            if let Err(error) = serve(stream, &state) {
                                eprintln!("fixture connection: {error:#}");
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            stop,
            state,
            handle: Some(handle),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(mut stream: TcpStream, state: &State) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = SyncReader::new(stream.try_clone()?);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(());
        }
        result => {
            result?;
        }
    }
    let request_line = line.clone();
    let mut length = 0;
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>()?;
        }
        if let Some((name, value)) = line.split_once(':')
            && !matches!(
                name.to_ascii_lowercase().as_str(),
                "host" | "content-length" | "connection" | "accept-encoding" | "content-encoding"
            )
        {
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
    }
    ensure!(length < 4_000_000, "Fixture body too large");
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    if request_line.starts_with("GET /version ") {
        let gateway = state
            .gateway
            .as_ref()
            .context("No production provider for capability probe")?;
        let response = reqwest::blocking::get(format!("{gateway}/version"))?
            .error_for_status()?
            .text()?;
        return http(&mut stream, "application/json", &response);
    }
    if !request_line.starts_with("POST ") {
        return http(&mut stream, "application/json", "{\"models\":[]}");
    }
    let request: Value =
        serde_json::from_slice(&body).context("Expected uncompressed JSON request")?;
    state.requests.lock().unwrap().push(request.clone());
    if state.live_parent && request["model"] != CHILD {
        ensure!(
            state.root_calls.fetch_add(1, Ordering::SeqCst) < 6,
            "Live parent exceeded six inference requests"
        );
        ensure!(
            headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
            "Official client did not supply its normal authorization header"
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(35))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut outgoing = client.post("https://chatgpt.com/backend-api/codex/responses");
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case(agent_relay::provider::KEY_HEADER) {
                outgoing = outgoing.header(name, value);
            }
        }
        let response = outgoing.body(body).send()?;
        let status = response.status();
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_owned();
        state.forward_statuses.lock().unwrap().push(status.as_u16());
        let bytes = response.bytes()?;
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Upstream"),
            kind,
            bytes.len()
        )?;
        stream.write_all(&bytes)?;
        stream.flush()?;
        return Ok(());
    }
    if (request["model"] == CHILD
        || request["model"]
            .as_str()
            .is_some_and(|model| model.starts_with("agentrelay-profile-")))
        && let Some(gateway) = &state.gateway
    {
        let response = reqwest::blocking::Client::new()
            .post(format!("{gateway}/responses"))
            .header(agent_relay::provider::KEY_HEADER, "native-test-key")
            .header("content-type", "application/json")
            .body(body)
            .send()?;
        ensure!(
            response.status().is_success(),
            "Production provider rejected request: {}",
            response.text()?
        );
        let output = response.text()?;
        let extra: Value = serde_json::from_str(
            request["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap_or("{}"),
        )?;
        if extra["request_kind"] == "compaction" {
            return http(&mut stream, "text/event-stream", &output);
        }
        let store = agent_relay::store::Store::open(&state.workspace.join("relay-state"))?;
        let task = store
            .list(None)?
            .into_iter()
            .next()
            .context("Missing production CLI task")?;
        let mut terminal = agent_relay::runner::summary(&task);
        terminal["observed_tool_names"] = json!(
            store
                .events(&task.id, 0)?
                .into_iter()
                .filter(|event| event.generation == task.generation
                    && event.kind == "tool"
                    && event.data["phase"] == "started")
                .filter_map(|event| event.data["name"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        );
        state.runtime_results.lock().unwrap().push(terminal);
        state.claude_calls.fetch_add(1, Ordering::SeqCst);
        return http(&mut stream, "text/event-stream", &output);
    }
    let events = if request["model"] == PARENT {
        match state.root_calls.fetch_add(1, Ordering::SeqCst) {
            0 => {
                let task_message = if state.handoff.lock().unwrap().is_some() {
                    "OPAQUE_NATIVE_MESSAGE_NOT_USED_AS_EXTERNAL_TASK"
                } else {
                    TASK
                };
                let mut call = json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"native-spawn","namespace":"collaboration","name":"spawn_agent","arguments":json!({"task_name":"claude_worker","message":task_message,"fork_turns":"none","agent_type":state.role}).to_string()}});
                if state.plaintext {
                    call["item"]["encrypted_function_args"] = json!([]);
                }
                vec![call]
            }
            2 if state.followup => {
                let handoff = state
                    .handoff
                    .lock()
                    .unwrap()
                    .clone()
                    .context("Missing follow-up handoff")?;
                std::fs::write(handoff["prompt_file"].as_str().unwrap(), FOLLOWUP)?;
                if state.gateway.is_some() {
                    prepare_production(
                        &state.workspace,
                        handoff["root_id"].as_str().unwrap(),
                        FOLLOWUP,
                    )?;
                }
                vec![
                    json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"native-followup","namespace":"collaboration","name":"followup_task","arguments":"{\"target\":\"claude_worker\",\"message\":\"OPAQUE_FOLLOWUP_NOT_USED_AS_EXTERNAL_TASK\"}"}}),
                ]
            }
            1 | 3 => vec![
                json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"native-wait","namespace":"collaboration","name":"wait_agent","arguments":"{\"timeout_ms\":60000}"}}),
            ],
            _ => vec![message("PARENT_DONE")],
        }
    } else if state.route_claude {
        let handoff = state.handoff.lock().unwrap().clone();
        let metadata = request["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .and_then(|value| serde_json::from_str::<Value>(value).ok());
        let prepared = handoff
            .as_ref()
            .filter(|handoff| {
                request["client_metadata"]["session_id"] == handoff["root_id"]
                    && metadata
                        .as_ref()
                        .is_some_and(|meta| meta["agent_name"] == handoff["agent_path"])
            })
            .map(|handoff| std::fs::read_to_string(handoff["prompt_file"].as_str().unwrap()))
            .transpose()?;
        let direct = request["input"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .filter(|item| item["type"] == "agent_message")
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter(|part| part["type"] == "input_text")
            .filter_map(|part| part["text"].as_str())
            .find(|text| text.contains("Read numbers.txt"));
        if let Some(prompt) = prepared.as_deref().or(direct) {
            let prompt = prompt
                .split_once("Payload:\n")
                .map(|(_, payload)| payload)
                .unwrap_or(prompt);
            state.claude_calls.fetch_add(1, Ordering::SeqCst);
            let mut command = std::process::Command::new(&state.relay);
            command
                .arg("--home")
                .arg(state.workspace.join("relay-state"));
            let existing = state.relay_task.lock().unwrap().clone();
            if let Some(task) = existing {
                command.args(["resume", &task]);
            } else {
                command
                    .args([
                        "run",
                        "--runtime",
                        "claude",
                        "--timeout-seconds",
                        "45",
                        "--workspace",
                    ])
                    .arg(&state.workspace)
                    .args([
                        "--root-id",
                        request["client_metadata"]["session_id"]
                            .as_str()
                            .context("Root identity")?,
                        "--agent-id",
                        request["client_metadata"]["thread_id"]
                            .as_str()
                            .context("Child identity")?,
                    ]);
                if let Some(meta) = &metadata {
                    command.args([
                        "--agent-path",
                        meta["agent_name"].as_str().context("Native path")?,
                    ]);
                }
            }
            command.args([
                "--key",
                &format!(
                    "provider-{}",
                    request["client_metadata"]["turn_id"]
                        .as_str()
                        .context("Turn identity")?
                ),
            ]);
            let output = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    child.stdin.take().unwrap().write_all(prompt.as_bytes())?;
                    child.wait_with_output()
                })?;
            let decoded = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>();
            let mut terminal = decoded
                .iter()
                .find(|frame| frame["type"] == "task_result")
                .cloned()
                .context("Missing real Claude terminal result")?;
            terminal["observed_tool_names"] = json!(
                decoded
                    .iter()
                    .filter(|frame| frame["kind"] == "tool" && frame["data"]["phase"] == "started")
                    .filter_map(|frame| frame["data"]["name"].as_str())
                    .collect::<Vec<_>>()
            );
            *state.relay_task.lock().unwrap() = terminal["task_id"].as_str().map(str::to_owned);
            state.runtime_results.lock().unwrap().push(terminal.clone());
            ensure!(
                output.status.success() && terminal["status"] == "completed",
                "Real Claude failed: {terminal}"
            );
            vec![message(terminal["result"].as_str().context("Result text")?)]
        } else {
            state.runtime_results.lock().unwrap().push(json!({"rejected":"No explicit plaintext task; encrypted content was not decoded or reinterpreted"}));
            vec![message("REJECTED_OPAQUE_TASK")]
        }
    } else {
        vec![message("FIXTURE_CHILD_EXECUTED_BY_PARENT_PROVIDER")]
    };
    let mut frames =
        vec![json!({"type":"response.created","response":{"id":uuid::Uuid::new_v4().to_string()}})];
    frames.extend(events);
    frames.push(json!({"type":"response.completed","response":{"id":uuid::Uuid::new_v4().to_string(),"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}));
    let output = frames
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            )
        })
        .collect::<String>();
    http(&mut stream, "text/event-stream", &output)
}

fn message(text: &str) -> Value {
    json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","id":uuid::Uuid::new_v4().to_string(),"content":[{"type":"output_text","text":text}]}})
}

fn http(stream: &mut TcpStream, kind: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    notifications: Vec<Value>,
    next_id: u64,
}

impl Client {
    async fn start(binary: &str, home: &Path, stderr: &Path) -> Result<Self> {
        let mut command = tokio::process::Command::new(binary);
        command
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("CODEX_API_KEY")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("CODEX_SESSION_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(stderr)?)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut client = Self {
            child,
            stdin,
            lines,
            notifications: Vec::new(),
            next_id: 1,
        };
        client.rpc("initialize",json!({"clientInfo":{"name":"agentrelay-native-provider-tests","version":"0.1.0"},"capabilities":{"experimentalApi":true}})).await?;
        client
            .send(json!({"method":"initialized","params":{}}))
            .await?;
        Ok(client)
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        self.stdin
            .write_all(format!("{value}\n").as_bytes())
            .await?;
        self.stdin.flush().await?;
        Ok(())
    }
    async fn next(&mut self) -> Result<Value> {
        let line = tokio::time::timeout(Duration::from_secs(70), self.lines.next_line())
            .await??
            .context("App-server stdout closed")?;
        Ok(serde_json::from_str(&line)?)
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let value = self.next().await?;
            if value["id"] == id {
                if !value["error"].is_null() {
                    bail!("{method}: {}", value["error"]);
                }
                return Ok(value["result"].clone());
            }
            self.notifications.push(value);
        }
    }
}

fn models(root_model: &str) -> Result<Value> {
    let path = std::env::var_os("AGENTRELAY_MODEL_METADATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap()).join(".codex/models_cache.json")
        });
    let data: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let mut model = data["models"]
        .as_array()
        .and_then(|models| models.first())
        .context("Need installed model metadata")?
        .clone();
    model["model_messages"] = Value::Null;
    model["base_instructions"] = json!("Execute only the explicit acceptance-test task.");
    model["tool_mode"] = json!("direct");
    model["multi_agent_version"] = json!("v2");
    model["default_reasoning_level"] = json!("low");
    let child = agent_relay::catalog::runtime_entry(&model, "claude");
    model["slug"] = json!(root_model);
    model["display_name"] = json!("Scripted parent fixture");
    Ok(json!({"models":[model,child]}))
}

fn prepare_production(workspace: &Path, root: &str, prompt: &str) -> Result<()> {
    use agent_relay::{
        model::{Access, RunRequest},
        provider::{Assignment, Registry},
    };
    let previous = agent_relay::store::Store::open(&workspace.join("relay-state"))?
        .list(Some(root))?
        .into_iter()
        .next();
    let request = if let Some(previous) = previous {
        let mut request = previous.request;
        request.prompt = prompt.into();
        request.root_id = None;
        request.agent_id = None;
        request.agent_path = None;
        request
    } else {
        RunRequest {
            runtime: "claude".into(),
            workspace: workspace.into(),
            prompt: prompt.into(),
            model: None,
            effort: None,
            access: Access::ReadOnly,
            root_id: None,
            agent_id: None,
            agent_path: None,
            timeout_secs: 45,
        }
    };
    Registry::open(&workspace.join("relay-state"))?.prepare(&Assignment {
        root_id: root.into(),
        agent_path: "/root/claude_worker".into(),
        request,
    })?;
    Ok(())
}

async fn probe(
    name: &str,
    route_claude: bool,
    plaintext: bool,
    override_provider: bool,
) -> Result<Value> {
    let evidence_name = name;
    let builtin_openai = name.starts_with("builtin-");
    let name = name.strip_prefix("builtin-").unwrap_or(name);
    let force_compaction = name.starts_with("compact-");
    let name = name.strip_prefix("compact-").unwrap_or(name);
    let restart_bridge = name.starts_with("restart-");
    let name = name.strip_prefix("restart-").unwrap_or(name);
    let integrated = name.starts_with("integrated-");
    let name = name.strip_prefix("integrated-").unwrap_or(name);
    let direct_backend = name.starts_with("direct-");
    let name = name.strip_prefix("direct-").unwrap_or(name);
    let parent_compact = name.starts_with("parentcompact-");
    let name = name.strip_prefix("parentcompact-").unwrap_or(name);
    let legacy_migration = name.starts_with("migration-");
    let name = name.strip_prefix("migration-").unwrap_or(name);
    let binary = std::env::var("AGENTRELAY_CODEX_TEST_BIN")
        .context("Set the path to an official Codex executable")?;
    let temporary = tempfile::tempdir()?;
    let home = temporary.path().join("codex-home");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(home.join("agents"))?;
    std::fs::create_dir(&workspace)?;
    std::fs::write(workspace.join("numbers.txt"), "19, 23, 31\n")?;
    let relay = PathBuf::from(env!("CARGO_BIN_EXE_agentrelay"));
    let live_parent = name == "real-openai-parent";
    let production = live_parent || name.starts_with("product-");
    let mut gateway_process = None;
    let gateway = if production {
        let socket = TcpListener::bind("127.0.0.1:0")?;
        let address = socket.local_addr()?;
        drop(socket);
        let registry = workspace.join("relay-state");
        agent_relay::provider::Registry::open(&registry)?;
        std::fs::write(registry.join("provider.key"), "native-test-key")?;
        let child = tokio::process::Command::new(&relay)
            .arg("--home")
            .arg(&registry)
            .args(["serve", "--listen", &address.to_string()])
            .stdout(std::fs::File::create(
                temporary.path().join("provider.log"),
            )?)
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        gateway_process = Some(child);
        let url = format!("http://{address}");
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client.get(format!("{url}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Some(url)
    } else {
        None
    };
    let root_model = if live_parent {
        std::env::var("AGENTRELAY_REAL_PARENT_MODEL").unwrap_or_else(|_| "gpt-6.1-sol".into())
    } else {
        PARENT.into()
    };
    let create_state = || State {
        requests: Mutex::new(Vec::new()),
        root_calls: AtomicUsize::new(0),
        claude_calls: AtomicUsize::new(0),
        route_claude,
        plaintext,
        role: if let Some(family) = profile_family(name) {
            format!("agentrelay_profile_{family}55_high")
        } else {
            "claude_direct".into()
        },
        workspace: workspace.clone(),
        relay: relay.clone(),
        runtime_results: Mutex::new(Vec::new()),
        handoff: Mutex::new(None),
        relay_task: Mutex::new(None),
        followup: matches!(
            name,
            "prepared-followup" | "product-followup" | "product-haiku-high" | "product-opus-high"
        ),
        live_parent,
        forward_statuses: Mutex::new(Vec::new()),
        gateway: gateway.clone(),
    };
    let server = Server::start(create_state())?;
    let alternate = Server::start(create_state())?;
    let mut bridge_process = None;
    let bridge = if builtin_openai || legacy_migration {
        let socket = TcpListener::bind("127.0.0.1:0")?;
        let address = socket.local_addr()?;
        drop(socket);
        let backend = if direct_backend {
            gateway.as_ref().context("Missing production provider")?
        } else {
            &server.address
        }
        .strip_prefix("http://")
        .unwrap();
        let mut process = tokio::process::Command::new(&relay)
            .arg("--home")
            .arg(workspace.join("relay-state"))
            .args([
                "serve-openai",
                "--listen",
                &address.to_string(),
                "--backend",
                backend,
                "--codex-bin",
                &binary,
            ])
            .stdout(std::fs::File::create(temporary.path().join("bridge.log"))?)
            .stderr(std::fs::File::create(
                temporary.path().join("bridge.stderr.log"),
            )?)
            .kill_on_drop(true)
            .spawn()?;
        if integrated {
            process.kill().await?;
            process.wait().await?;
            let old = gateway_process.as_mut().context("Missing backend")?;
            old.kill().await?;
            old.wait().await?;
            let state = workspace.join("relay-state");
            let settings = agent_relay::transport::Openai {
                schema_version: 1,
                listen: address,
                backend: backend.parse()?,
                codex_bin: PathBuf::from(&binary),
                additional_codex_bins: Vec::new(),
            };
            agent_relay::codex::atomic_write(
                &state.join(agent_relay::transport::FILE),
                &serde_json::to_vec(&settings)?,
                false,
            )?;
            *old = tokio::process::Command::new(&relay)
                .arg("--home")
                .arg(&state)
                .args(["serve", "--listen", backend])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
        } else {
            bridge_process = Some(process);
        }
        let url = format!("http://{address}");
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client.get(format!("{url}/health")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        ensure!(
            client
                .post(format!("{url}/responses"))
                .body("{}")
                .send()
                .await?
                .status()
                == 403,
            "Untrusted test executable bypassed peer admission"
        );
        Some(url)
    } else {
        None
    };
    let mut catalog = models(&root_model)?;
    if let Some(family) = profile_family(name) {
        let profile_name = format!("{family}55_high");
        let profile = agent_relay::profiles::add(
            &workspace.join("relay-state"),
            &profile_name,
            agent_relay::profiles::Profile {
                runtime: "claude".into(),
                model: format!("{family} 5.5"),
                effort: Some("high".into()),
                display_name: format!("Claude {}{} 5.5", family[..1].to_uppercase(), &family[1..]),
            },
        )?;
        let template = catalog["models"][0].clone();
        let mut entry = agent_relay::profiles::catalog_entry(&template, &profile_name, &profile);
        if force_compaction {
            entry["auto_compact_token_limit"] = json!(0);
        }
        catalog["models"].as_array_mut().unwrap().push(entry);
        std::fs::write(
            home.join(format!("agents/agentrelay_profile_{profile_name}.toml")),
            agent_relay::profiles::role_document(&profile_name, &profile)?,
        )?;
    }
    std::fs::write(
        home.join("models.json"),
        serde_json::to_vec_pretty(&catalog)?,
    )?;
    let marker = workspace.join("ROLE_COMMAND_WAS_EXECUTED");
    let role = format!(
        "name = \"claude_direct\"\ndescription = \"Direct CLI backend probe\"\nmodel = \"{CHILD}\"\nmodel_reasoning_effort = \"low\"\ndeveloper_instructions = \"Perform only the assigned task.\"\n{}",
        if override_provider {
            "model_provider = \"alternate\"\n".to_owned()
        } else {
            String::new()
        }
    );
    std::fs::write(home.join("agents/claude_direct.toml"), role)?;
    let config = format!(
        "model = \"{root_model}\"\nmodel_provider = \"fixture\"\nmodel_catalog_json = {:?}\nmodel_reasoning_effort = \"low\"\nweb_search = \"disabled\"\n[features]\nmulti_agent_v2 = true\napps = false\nplugins = false\n[skills]\ninclude_instructions = false\n[model_providers.fixture]\nname = \"Local fixture\"\nbase_url = {:?}\nwire_api = \"responses\"\nrequires_openai_auth = {live_parent}\n[model_providers.alternate]\nname = \"Requested child provider\"\nbase_url = {:?}\nwire_api = \"responses\"\nrequires_openai_auth = false\n",
        home.join("models.json").to_str().unwrap(),
        server.address,
        alternate.address
    );
    let config = if legacy_migration {
        config
            .replace(
                "model_provider = \"fixture\"",
                "model_provider = \"agentrelay\"",
            )
            .replace("[model_providers.fixture]", "[model_providers.agentrelay]")
    } else {
        config
    };
    let legacy_config = config.clone();
    let config = if let Some(bridge) = bridge.as_ref().filter(|_| !legacy_migration) {
        format!(
            "openai_base_url = {bridge:?}\n{}",
            config.replace(
                "model_provider = \"fixture\"",
                "model_provider = \"openai\""
            )
        )
    } else {
        config
    };
    std::fs::write(home.join("config.toml"), config)?;
    if live_parent {
        let normal_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".codex"));
        let auth = normal_home.join("auth.json");
        ensure!(
            auth.is_file(),
            "No existing file-backed Codex login available; no new login or credential extraction attempted"
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(auth, home.join("auth.json"))?;
    } else if builtin_openai || legacy_migration {
        std::fs::write(
            home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"fixture-not-a-real-key","auth_mode":"apikey"}"#,
        )?;
    }
    let stderr = temporary.path().join("app-server.log");
    let outcome=async {
        let mut client=Client::start(&binary,&home,&stderr).await?;
        let model_catalog=client.rpc("model/list",json!({"includeHidden":true,"limit":100})).await?;
        let picker_catalog=client.rpc("model/list",json!({"limit":100})).await?;
        for worker in model_catalog["data"].as_array().context("Missing model catalog")?.iter()
            .filter(|model| model["model"]==CHILD || model["model"].as_str().is_some_and(|slug| slug.starts_with("agentrelay-profile-"))) {
            ensure!(worker["hidden"]==true && worker["isDefault"]==false,"Worker advertised as visible/default: {worker}");
            ensure!(worker["availabilityNux"].is_null() && worker["upgrade"].is_null() && worker["upgradeInfo"].is_null(),"Worker inherited an OpenAI promotion: {worker}");
            ensure!(!picker_catalog["data"].as_array().context("Missing picker catalog")?.iter()
                .any(|model|model["model"]==worker["model"]),"Worker leaked into the default model list");
        }
        let developer=if live_parent {"This is a native delegation acceptance test. A bound plaintext handoff is already prepared for /root/claude_worker. You must call collaboration.spawn_agent exactly once with task_name claude_worker, agent_type claude_direct, fork_turns none, and a short message saying to use the prepared handoff. Do not create a Codex supervisor or solve the task yourself. Wait for the child result, then report it. Do not use shell or other tools."}else{"Do not use tools except the scripted collaboration tools."};
        let started=client.rpc("thread/start",json!({"model":root_model,"cwd":workspace,"approvalPolicy":"never","sandbox":"read-only","baseInstructions":"Follow the explicit test task.","developerInstructions":developer})).await?;
        let root=started["thread"]["id"].as_str().context("Root thread ID")?.to_owned();
        if builtin_openai { ensure!(started["thread"]["modelProvider"]=="openai", "Built-in provider identity was not retained"); }
        if name.starts_with("prepared-") || production {
            let prompt_file=workspace.join("explicit-handoff.txt");std::fs::write(&prompt_file,TASK)?;
            let root_binding=if name=="prepared-wrong-root" {"unrelated-root"}else{&root};
            *server.state.handoff.lock().unwrap()=Some(json!({"root_id":root_binding,"agent_path":"/root/claude_worker","prompt_file":prompt_file}));
            if production {
                if let Some(family)=profile_family(name) {
                    let prompt=workspace.join("explicit-handoff.txt");
                    let output=std::process::Command::new(&relay).arg("--home").arg(workspace.join("relay-state"))
                        .args(["prepare","--runtime","claude","--model",&format!("{family} 5.5"),"--effort","high","--timeout-seconds","45","--root-id",&root,"--agent-path","/root/claude_worker","--workspace"])
                        .arg(&workspace).arg("--prompt-file").arg(prompt).output()?;
                    ensure!(output.status.success(),"Model selection preflight failed: {}",String::from_utf8_lossy(&output.stderr));
                }else{prepare_production(&workspace,&root,TASK)?;}
            }
        }
        client.rpc("turn/start",json!({"threadId":root,"input":[{"type":"text","text":"Run the native child acceptance test."}]})).await?;
        loop {
            let event=client.next().await?;
            let completed=event["method"]=="turn/completed" && event["params"]["threadId"]==root;
            client.notifications.push(event);
            if completed {break;}
        }
        let mut migration_new_provider=Value::Null;
        if legacy_migration {
            ensure!(started["thread"]["modelProvider"]=="agentrelay");
            client.child.kill().await?;client.child.wait().await?;
            let updated=format!("openai_base_url = {:?}\n{}",bridge.as_ref().unwrap(),
                legacy_config.replace("model_provider = \"agentrelay\"","model_provider = \"openai\""));
            std::fs::write(home.join("config.toml"),updated)?;
            let prior=std::mem::take(&mut client.notifications);
            client=Client::start(&binary,&home,&temporary.path().join("resumed.stderr.log")).await?;
            client.notifications.extend(prior);
            let resumed=client.rpc("thread/resume",json!({"threadId":root,"cwd":workspace})).await?;
            ensure!(resumed["thread"]["id"]==root,"Migration replaced the existing root");
            let fresh=client.rpc("thread/start",json!({"model":PARENT,"cwd":workspace,"approvalPolicy":"never","sandbox":"read-only"})).await?;
            migration_new_provider=fresh["thread"]["modelProvider"].clone();
            server.state.root_calls.store(2,Ordering::SeqCst);
            client.rpc("turn/start",json!({"threadId":root,"input":[{"type":"text","text":"Continue the existing native agent after configuration migration."}]})).await?;
            loop {
                let event=client.next().await?;
                let done=event["method"]=="turn/completed" && event["params"]["threadId"]==root;
                client.notifications.push(event);
                if done {break;}
            }
        }
        if restart_bridge {
            let process=bridge_process.as_mut().context("Missing bridge process")?;
            process.kill().await?;
            process.wait().await?;
            let url=bridge.as_ref().unwrap();
            let address=url.strip_prefix("http://").unwrap();
            let backend=server.address.strip_prefix("http://").unwrap();
            *process=tokio::process::Command::new(&relay).arg("--home").arg(workspace.join("relay-state"))
                .args(["serve-openai","--listen",address,"--backend",backend,"--codex-bin",&binary])
                .stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
            let transport=reqwest::Client::new();
            for _ in 0..100 {
                if transport.get(format!("{url}/health")).send().await.is_ok() {break;}
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            server.state.root_calls.store(2,Ordering::SeqCst);
            client.rpc("turn/start",json!({"threadId":root,"input":[{"type":"text","text":"Resume the existing agent after the bridge restart."}]})).await?;
            loop {
                let event=client.next().await?;
                let done=event["method"]=="turn/completed" && event["params"]["threadId"]==root;
                client.notifications.push(event);
                if done {break;}
            }
        }
        let mut parent_compaction=Value::Null;
        if parent_compact {
            let account=client.rpc("account/read",json!({"refreshToken":false})).await?;
            ensure!(account["account"]["type"]=="chatgpt","Normal ChatGPT account was not available");
            let limits=client.rpc("account/rateLimits/read",json!({})).await?;
            ensure!(!limits.is_null(),"Normal account rate limit API failed");
            client.rpc("thread/compact/start",json!({"threadId":root})).await?;
            loop {
                let event=client.next().await?;
                let done=event["method"]=="turn/completed" && event["params"]["threadId"]==root;
                if done {parent_compaction=event.clone();}
                client.notifications.push(event);
                if done {break;}
            }
        }
        if direct_backend {
            let store=agent_relay::store::Store::open(&workspace.join("relay-state"))?;
            let tasks=store.list(None)?;
            server.state.claude_calls.store(tasks.len(),Ordering::SeqCst);
            for task in tasks {server.state.runtime_results.lock().unwrap().push(agent_relay::runner::summary(&task));}
        }
        let parent=client.rpc("thread/read",json!({"threadId":root,"includeTurns":true})).await?;
        let child_id=client.notifications.iter().filter_map(|event|event.pointer("/params/item"))
            .find(|item|item["type"]=="subAgentActivity" && item["kind"]=="started")
            .and_then(|item|item["agentThreadId"].as_str()).map(str::to_owned);
        let compaction = client.notifications.iter().filter(|event|
            event["method"]=="item/completed" && event["params"]["item"]["type"]=="contextCompaction")
            .cloned().collect::<Vec<_>>();
        let child=if let Some(id)=&child_id {Some(client.rpc("thread/read",json!({"threadId":id,"includeTurns":true})).await?)}else{None};
        let result=json!({"case":name,"parent_compaction":parent_compaction,"builtin_openai_bridge":builtin_openai,"integrated_service":integrated,"migration_new_provider":migration_new_provider,"direct_production_backend":direct_backend,"compaction":compaction,"official_binary":binary,"root_thread_id":root,"child_thread_id":child_id,"parent":parent,"child":child,"model_catalog":model_catalog,"picker_catalog":picker_catalog,"notifications":client.notifications,"provider_requests":server.state.requests.lock().unwrap().clone(),"alternate_provider_requests":alternate.state.requests.lock().unwrap().clone(),"claude_cli_invocations":server.state.claude_calls.load(Ordering::SeqCst),"runtime_results":server.state.runtime_results.lock().unwrap().clone(),"role_command_executed":marker.exists(),"root_inference":if live_parent {"Real authenticated OpenAI parent via official HTTPS upstream"}else{"scripted local fixture; no OpenAI inference"},"upstream_statuses":server.state.forward_statuses.lock().unwrap().clone(),"user_configuration_modified":false,"codex_binary_recompiled":false});
        client.child.kill().await?;
        Ok::<_,anyhow::Error>(result)
    }.await;
    if let Some(mut child) = bridge_process {
        child.kill().await?;
        child.wait().await?;
    }
    if let Some(mut child) = gateway_process {
        child.kill().await?;
        child.wait().await?;
    }
    let evidence = std::env::var_os("AGENTRELAY_TEST_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evidence/private/native-provider")
        });
    std::fs::create_dir_all(&evidence)?;
    std::fs::copy(
        &stderr,
        evidence.join(format!("{evidence_name}.stderr.log")),
    )?;
    if let Ok(result) = &outcome {
        std::fs::write(
            evidence.join(format!("{evidence_name}.json")),
            serde_json::to_vec_pretty(result)?,
        )?;
    }
    outcome
}

#[tokio::test]
#[ignore = "Opt-in official-binary probe; isolated CODEX_HOME, local scripted inference"]
async fn native_role_provider_override_is_ignored() -> Result<()> {
    let result = probe("role-command-provider", false, true, true).await?;
    ensure!(
        !result["role_command_executed"].as_bool().unwrap(),
        "Role command unexpectedly executed"
    );
    ensure!(
        result["alternate_provider_requests"]
            .as_array()
            .unwrap()
            .is_empty(),
        "Provider override took effect; revise architecture"
    );
    ensure!(
        result["provider_requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["model"] == CHILD),
        "Native child did not reach inherited provider; inspect evidence"
    );
    ensure!(
        result["child_thread_id"].is_string(),
        "No native child was created"
    );
    ensure!(
        result["child"]["thread"]["modelProvider"] == "fixture",
        "Child provider did not remain inherited"
    );
    Ok(())
}

fn profile_family(name: &str) -> Option<&'static str> {
    match name {
        "product-haiku-high" => Some("haiku"),
        "product-opus-high" => Some("opus"),
        _ => None,
    }
}

fn verify_native_answer(result: &Value, answer: &str) -> Result<()> {
    let thread = &result["child"]["thread"];
    ensure!(
        thread["parentThreadId"] == result["root_thread_id"],
        "Child is not registered under the actual parent"
    );
    let expected_model = if let Some(family) = profile_family(result["case"].as_str().unwrap()) {
        format!("agentrelay-profile-{family}55_high")
    } else {
        CHILD.to_owned()
    };
    ensure!(
        thread["model"] == expected_model,
        "Unexpected child model identity"
    );
    ensure!(
        thread["source"].get("subAgent").is_some(),
        "Not a native subagent source"
    );
    let turns = thread["turns"].as_array().context("Missing native turns")?;
    ensure!(
        turns.iter().any(|turn| turn["status"] == "completed"
            && turn["items"]
                .as_array()
                .is_some_and(
                    |items| items.iter().any(|item| item["type"] == "agentMessage"
                        && item["text"]
                            .as_str()
                            .is_some_and(|text| text.ends_with(answer)))
                )),
        "Actual native assistant answer does not match the external result"
    );
    ensure!(
        result["notifications"].as_array().unwrap().iter().any(
            |event| event["params"]["item"]["type"] == "subAgentActivity"
                && event["params"]["item"]["kind"] == "completed"
                && event["params"]["item"]["agentThreadId"] == result["child_thread_id"]
        ),
        "Missing native completion activity"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Official-binary hidden model catalog and native spawn; local scripted inference only"]
async fn hidden_worker_is_spawnable_without_becoming_a_picker_model() -> Result<()> {
    let result = probe("hidden-worker-catalog", false, false, false).await?;
    verify_native_answer(&result, "FIXTURE_CHILD_EXECUTED_BY_PARENT_PROVIDER")?;
    ensure!(
        result["claude_cli_invocations"] == 0,
        "Unexpected external inference"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Opt-in probe; launches one real Claude Code task through the Rust runner"]
async fn native_child_real_claude_via_local_provider() -> Result<()> {
    let result = probe("local-provider-real-claude", true, true, false).await?;
    ensure!(
        result["child_thread_id"].is_string(),
        "No native child was created"
    );
    ensure!(
        result["claude_cli_invocations"] == 1,
        "Expected exactly one real Claude CLI turn"
    );
    ensure!(
        result["runtime_results"][0]["result"]
            .as_str()
            .is_some_and(|text| text.contains("NATIVE_CLAUDE_SUM=73")),
        "Claude did not produce the real fixture result"
    );
    ensure!(
        result["runtime_results"][0]["observed_tool_names"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "Read"),
        "Real Claude did not read the fixture file"
    );
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    ensure!(
        result["provider_requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["model"] == CHILD)
            .count()
            == 1,
        "Extra child model requests were made"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Opt-in encrypted-message boundary probe; must not call Claude with opaque content"]
async fn native_opaque_task_is_not_reinterpreted_as_plaintext() -> Result<()> {
    let result = probe("opaque-task-boundary", true, false, false).await?;
    ensure!(
        result["claude_cli_invocations"] == 0,
        "Opaque input reached the external runtime"
    );
    ensure!(
        result["runtime_results"].to_string().contains("rejected"),
        "Opaque boundary was not exercised"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Opt-in probe; actual Claude uses an explicit handoff without decoding native encrypted input"]
async fn native_opaque_message_can_use_explicit_bound_handoff() -> Result<()> {
    let result = probe("prepared-explicit-handoff", true, false, false).await?;
    ensure!(
        result["claude_cli_invocations"] == 1,
        "Explicit handoff did not execute exactly once"
    );
    ensure!(
        result["runtime_results"][0]["result"]
            .as_str()
            .is_some_and(|text| text.ends_with("NATIVE_CLAUDE_SUM=73")),
        "Wrong external result"
    );
    ensure!(
        result["runtime_results"][0]["observed_tool_names"]
            .to_string()
            .contains("Read"),
        "Fixture not read"
    );
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    ensure!(
        result["provider_requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["model"] == CHILD)
            .count()
            == 1,
        "Extra child inference request"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Opt-in negative control; no Claude call for another parent's handoff"]
async fn native_handoff_cannot_be_claimed_by_wrong_root() -> Result<()> {
    let result = probe("prepared-wrong-root", true, false, false).await?;
    ensure!(
        result["claude_cli_invocations"] == 0,
        "Wrong root claimed a handoff"
    );
    ensure!(
        result["runtime_results"].to_string().contains("rejected"),
        "Root mismatch did not reject"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Opt-in official role-loader probe, no model inference"]
async fn native_role_executable_field_is_rejected() -> Result<()> {
    let binary = std::env::var("AGENTRELAY_CODEX_TEST_BIN")?;
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("codex-home");
    std::fs::create_dir_all(home.join("agents"))?;
    let marker = temp.path().join("command-executed");
    std::fs::write(
        home.join("config.toml"),
        "[features]\nplugins = false\napps = false\n",
    )?;
    std::fs::write(
        home.join("agents/direct_exec.toml"),
        format!(
            "name = \"direct_exec\"\ndescription = \"Executable role test\"\ndeveloper_instructions = \"Execute the task\"\ncommand = [\"/usr/bin/touch\", {:?}]\n",
            marker.to_str().unwrap()
        ),
    )?;
    let stderr = temp.path().join("stderr.log");
    let mut client = Client::start(&binary, &home, &stderr).await?;
    client
        .rpc(
            "skills/list",
            json!({"cwds":[temp.path()],"forceReload":true}),
        )
        .await?;
    client.child.kill().await?;
    let log = std::fs::read_to_string(&stderr)?;
    ensure!(
        log.contains("unknown field `command`"),
        "Executable field was not rejected; inspect role schema"
    );
    ensure!(!marker.exists(), "Executable role unexpectedly ran");
    let evidence =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evidence/private/native-provider");
    std::fs::create_dir_all(&evidence)?;
    std::fs::write(evidence.join("role-command-rejected.stderr.log"), log)?;
    Ok(())
}

#[tokio::test]
#[ignore = "Opt-in native follow-up probe; two real Claude turns, no Codex worker inference"]
async fn native_followup_preserves_cli_session() -> Result<()> {
    let result = probe("prepared-followup", true, false, false).await?;
    ensure!(
        result["claude_cli_invocations"] == 2,
        "Expected two Claude turns"
    );
    let turns = result["runtime_results"]
        .as_array()
        .context("Missing results")?;
    ensure!(turns.len() == 2, "Unexpected result count");
    ensure!(
        turns[0]["task_id"] == turns[1]["task_id"]
            && turns[0]["session_id"] == turns[1]["session_id"],
        "External session/task was not preserved"
    );
    ensure!(
        turns[0]["generation"] == 1 && turns[1]["generation"] == 2,
        "Generation did not advance"
    );
    ensure!(
        turns[1]["observed_tool_names"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "Follow-up reread files"
    );
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    verify_native_answer(&result, "NATIVE_CLAUDE_DOUBLED=146")?;
    ensure!(
        result["child"]["thread"]["turns"].as_array().unwrap().len() == 2,
        "Follow-up did not reuse the native thread"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit live acceptance: uses existing Codex login for real parent plus one Claude turn"]
async fn real_openai_parent_executes_native_cli_child() -> Result<()> {
    let result = probe("real-openai-parent", true, false, false).await?;
    ensure!(
        result["claude_cli_invocations"] == 1,
        "Live parent did not execute one native Claude child; inspect saved upstream/turn evidence"
    );
    ensure!(
        result["upstream_statuses"]
            .as_array()
            .is_some_and(|v| !v.is_empty() && v.iter().all(|status| status == 200)),
        "Official upstream did not accept the parent"
    );
    ensure!(
        result["provider_requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|request| request["model"] != CHILD)
            .all(|request| request["client_metadata"]["thread_id"] == result["root_thread_id"]),
        "An OpenAI request originated from a non-root thread"
    );
    let native_children = result["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| {
            let item = &event["params"]["item"];
            (item["type"] == "subAgentActivity" && item["kind"] == "started")
                .then(|| item["agentThreadId"].as_str())
                .flatten()
        })
        .collect::<std::collections::HashSet<_>>();
    ensure!(
        native_children.len() == 1,
        "Unexpected intermediate native agents were created"
    );
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    ensure!(
        result["parent"]["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|turn| turn["status"] == "completed"
                && turn["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "agentMessage"
                        && item["text"] == "NATIVE_CLAUDE_SUM=73")),
        "Real parent did not report the child's actual answer"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Official app-server plus production Rust provider and real Claude turn"]
async fn product_native_thread_contains_full_assignment_and_result() -> Result<()> {
    let result = probe("product-native-conversation", true, false, false).await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    let turns = result["child"]["thread"]["turns"]
        .as_array()
        .context("Missing turns")?;
    let items = turns[0]["items"].as_array().context("Missing items")?;
    let messages = items
        .iter()
        .filter(|item| item["type"] == "agentMessage")
        .collect::<Vec<_>>();
    ensure!(
        messages.len() == 2,
        "Expected assignment receipt and final result"
    );
    ensure!(
        messages[0]["text"] == agent_relay::provider::task_receipt(TASK),
        "Actual assigned task missing from native conversation"
    );
    ensure!(
        messages[0]["phase"] == "commentary",
        "Task receipt lacks commentary phase"
    );
    ensure!(
        messages[1]["text"]
            .as_str()
            .is_some_and(|text| text.contains(TASK) && text.ends_with("NATIVE_CLAUDE_SUM=73")),
        "Final/summary view would lose the task or result"
    );
    ensure!(
        result["claude_cli_invocations"] == 1,
        "Unexpected extra CLI invocation"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Production provider follow-up, official native child and two real Claude turns"]
async fn product_followup_preserves_native_and_external_session() -> Result<()> {
    let result = probe("product-followup", true, false, false).await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    verify_native_answer(&result, "NATIVE_CLAUDE_DOUBLED=146")?;
    let turns = result["runtime_results"]
        .as_array()
        .context("No runtime results")?;
    ensure!(
        turns.len() == 2
            && turns[0]["task_id"] == turns[1]["task_id"]
            && turns[0]["session_id"] == turns[1]["session_id"],
        "Follow-up changed external identity"
    );
    ensure!(turns[1]["generation"] == 2, "Did not resume");
    ensure!(
        result["child"]["thread"]["turns"].as_array().unwrap().len() == 2,
        "Changed native identity"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Explicit model-selection acceptance: real Haiku 5.5 with high effort through official native child"]
async fn product_haiku_55_high_is_selected_and_visible_in_native_thread() -> Result<()> {
    let result = probe("product-haiku-high", true, false, false).await?;
    verify_selected_profile(&result, "haiku", "Claude Haiku 5.5")
}

#[tokio::test]
#[ignore = "Real Opus 5.5 high, official native thread and same-session follow-up"]
async fn product_opus_55_high_is_selected_and_visible_in_native_thread() -> Result<()> {
    let result = probe("product-opus-high", true, false, false).await?;
    verify_selected_profile(&result, "opus", "Claude Opus 5.5")
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Opt-in built-in OpenAI bridge: real Haiku native agent and same-session follow-up"]
async fn builtin_openai_bridge_preserves_haiku_profile_and_session() -> Result<()> {
    let result = probe("builtin-product-haiku-high", true, false, false).await?;
    verify_selected_profile(&result, "haiku", "Claude Haiku 5.5")?;
    ensure!(result["child"]["thread"]["modelProvider"] == "openai");
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Opt-in built-in OpenAI bridge: real Opus native agent and same-session follow-up"]
async fn builtin_openai_bridge_preserves_opus_profile_and_session() -> Result<()> {
    let result = probe("builtin-product-opus-high", true, false, false).await?;
    verify_selected_profile(&result, "opus", "Claude Opus 5.5")?;
    ensure!(result["child"]["thread"]["modelProvider"] == "openai");
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Opt-in built-in OpenAI bridge: authenticated OpenAI parent and real native Claude child"]
async fn builtin_openai_bridge_real_parent_delegates_to_claude() -> Result<()> {
    let result = probe("builtin-real-openai-parent", true, false, false).await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    ensure!(result["parent"]["thread"]["modelProvider"] == "openai");
    ensure!(
        result["claude_cli_invocations"] == 1,
        "Unexpected extra external inference"
    );
    ensure!(
        result["provider_requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|request| request["model"] != CHILD)
            .all(|request| request["client_metadata"]["thread_id"] == result["root_thread_id"]),
        "An extra OpenAI inference originated in a child"
    );
    ensure!(
        result["upstream_statuses"]
            .as_array()
            .is_some_and(|s| !s.is_empty() && s.iter().all(|s| s == 200))
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Opt-in built-in OpenAI bridge: real parent and child directly through the production provider"]
async fn builtin_openai_bridge_direct_production_parent_and_child() -> Result<()> {
    let result = probe("builtin-direct-real-openai-parent", true, false, false).await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    ensure!(
        result["provider_requests"].as_array().unwrap().is_empty(),
        "Traffic unexpectedly used the scripted fixture"
    );
    ensure!(
        result["claude_cli_invocations"] == 1
            && result["runtime_results"][0]["status"] == "completed",
        "Expected one completed external task"
    );
    ensure!(
        result["parent"]["thread"]["modelProvider"] == "openai"
            && result["child"]["thread"]["modelProvider"] == "openai"
    );
    ensure!(
        result["parent"]["thread"]["turns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|turn| turn["status"] == "completed"),
        "Real parent turn failed"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Real authenticated parent and Claude native child through the shared service process"]
async fn integrated_service_real_parent_and_child() -> Result<()> {
    let result = probe(
        "builtin-integrated-direct-real-openai-parent",
        true,
        false,
        false,
    )
    .await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    ensure!(result["integrated_service"] == true && result["claude_cli_invocations"] == 1);
    ensure!(
        result["parent"]["thread"]["modelProvider"] == "openai"
            && result["child"]["thread"]["modelProvider"] == "openai"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Opt-in bridge restart: preserve native child and resume the same real Claude session"]
async fn builtin_openai_bridge_restart_preserves_existing_agent() -> Result<()> {
    let result = probe("builtin-restart-product-haiku-high", true, false, false).await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    verify_native_answer(&result, "NATIVE_CLAUDE_DOUBLED=146")?;
    let turns = result["runtime_results"]
        .as_array()
        .context("Missing external results")?;
    ensure!(
        turns.len() == 3,
        "Expected initial execution, follow-up and post-restart follow-up"
    );
    for (index, turn) in turns.iter().enumerate() {
        ensure!(
            turn["task_id"] == turns[0]["task_id"]
                && turn["session_id"] == turns[0]["session_id"]
                && turn["generation"] == index + 1
                && turn["model"] == "claude-haiku-5-5"
                && turn["requested_effort"] == "high",
            "Restart lost external identity or selection"
        );
    }
    ensure!(result["child"]["thread"]["modelProvider"] == "openai");
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Resume a persisted legacy chat/agent after switching global provider to OpenAI"]
async fn legacy_chat_and_child_resume_after_provider_migration() -> Result<()> {
    let result = probe("migration-product-haiku-high", true, false, false).await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    verify_native_answer(&result, "NATIVE_CLAUDE_DOUBLED=146")?;
    ensure!(result["migration_new_provider"] == "openai");
    let turns = result["runtime_results"]
        .as_array()
        .context("Missing external turns")?;
    ensure!(turns.len() == 3, "Persisted child did not resume");
    ensure!(
        turns
            .iter()
            .all(|turn| turn["session_id"] == turns[0]["session_id"]
                && turn["task_id"] == turns[0]["task_id"]),
        "Provider migration changed the external session"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Opt-in built-in OpenAI bridge: force native compaction, then resume the same real Claude session"]
async fn builtin_openai_bridge_compaction_preserves_external_session() -> Result<()> {
    let result = probe("builtin-compact-product-haiku-high", true, false, false).await?;
    ensure!(
        result["compaction"]
            .as_array()
            .is_some_and(|events| !events.is_empty()),
        "No completed native compaction; inspect private evidence"
    );
    verify_selected_profile(&result, "haiku", "Claude Haiku 5.5")?;
    let turns = result["runtime_results"]
        .as_array()
        .context("Missing external results")?;
    ensure!(
        turns.len() == 2,
        "Compaction launched an extra CLI turn or follow-up did not execute"
    );
    ensure!(
        turns
            .iter()
            .all(|turn| turn["session_id"] == turns[0]["session_id"]),
        "Compaction replaced the external session"
    );
    Ok(())
}

fn verify_selected_profile(result: &Value, family: &str, display_name: &str) -> Result<()> {
    let external_model = format!("claude-{family}-5-5");
    let native_id = format!("agentrelay-profile-{family}55_high");
    verify_native_answer(result, "NATIVE_CLAUDE_SUM=73")?;
    verify_native_answer(result, "NATIVE_CLAUDE_DOUBLED=146")?;
    let observed = &result["runtime_results"][0];
    ensure!(
        result["child"]["thread"]["reasoningEffort"] == "high",
        "Native effort selector does not reflect the profile"
    );
    let native_model = result["model_catalog"]["data"]
        .as_array()
        .context("Missing native model catalog")?
        .iter()
        .find(|model| model["model"] == native_id)
        .context("Profile missing from the official native model list")?;
    ensure!(
        native_model["displayName"] == display_name
            && native_model["defaultReasoningEffort"] == "high",
        "Native picker label or effort mismatch"
    );
    ensure!(
        observed["requested_model"] == external_model,
        "Human model name was not normalized"
    );
    ensure!(
        observed["model"] == external_model,
        "A different model actually answered"
    );
    ensure!(
        observed["requested_effort"] == "high",
        "Effort selection was lost"
    );
    ensure!(
        observed["effort_verification"] == "cli_argument_only",
        "Unproven applied effort was claimed"
    );
    let final_text = result["child"]["thread"]["turns"][0]["items"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["text"]
        .as_str()
        .unwrap();
    ensure!(
        final_text.contains(&format!("Requested model: {external_model}"))
            && final_text.contains(&format!("Observed model: {external_model}"))
            && final_text.contains("Requested effort: high"),
        "Selection details are missing from native history"
    );
    ensure!(
        result["claude_cli_invocations"] == 2,
        "Expected the initial turn and one follow-up"
    );
    let followup = &result["runtime_results"][1];
    ensure!(
        followup["requested_model"] == observed["requested_model"]
            && followup["model"] == observed["model"]
            && followup["requested_effort"] == "high",
        "Follow-up lost model or effort"
    );
    ensure!(
        followup["task_id"] == observed["task_id"]
            && followup["session_id"] == observed["session_id"],
        "Follow-up changed external identity"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Real parent compaction plus account and usage APIs through the integrated service"]
async fn integrated_service_parent_compaction_and_account_apis() -> Result<()> {
    let result = probe(
        "builtin-integrated-direct-parentcompact-real-openai-parent",
        true,
        false,
        false,
    )
    .await?;
    verify_native_answer(&result, "NATIVE_CLAUDE_SUM=73")?;
    ensure!(
        result["parent_compaction"]["params"]["turn"]["status"] == "completed",
        "Normal parent compaction failed"
    );
    ensure!(
        result["claude_cli_invocations"] == 1,
        "Parent compaction launched an extra CLI turn"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "Installed user integration acceptance: creates one real test chat and Haiku native child in the app's normal state"]
async fn installed_openai_service_native_chat_and_followup() -> Result<()> {
    let binary = std::env::var("AGENTRELAY_CODEX_TEST_BIN")?;
    let user = PathBuf::from(std::env::var_os("HOME").context("Missing home")?);
    let home = user.join(".codex");
    let registry = user.join(".local/state/agentrelay");
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(home.join("config.toml"))?)?;
    ensure!(
        config["model_provider"].as_str() == Some("openai"),
        "Activate the installed bridge first"
    );
    let settings =
        agent_relay::transport::load(&registry)?.context("Installed bridge is missing")?;
    ensure!(agent_relay::transport::healthy(
        &registry,
        settings.backend
    )?);
    let evidence = PathBuf::from(
        std::env::var_os("AGENTRELAY_TEST_EVIDENCE").context("Set a private evidence directory")?,
    );
    std::fs::create_dir_all(&evidence)?;
    let workspace = tempfile::tempdir()?;
    std::fs::write(workspace.path().join("numbers.txt"), "19, 23, 31\n")?;
    let mut client = Client::start(&binary, &home, &evidence.join("installed.stderr.log")).await?;
    let model = config["model"].as_str().unwrap_or("gpt-6.1-sol");
    let started=client.rpc("thread/start",json!({"model":model,"cwd":workspace.path(),"approvalPolicy":"never","sandbox":"read-only",
        "baseInstructions":"Follow the acceptance task exactly. Do not read unrelated files.",
        "developerInstructions":"This is an AgentRelay installed-service acceptance test. A plaintext assignment is already prepared for /root/claude_worker. On the first turn, call collaboration.spawn_agent exactly once with task_name claude_worker, agent_type agentrelay_profile_haiku55_high, fork_turns none, and message Use the prepared assignment. Wait for its actual result. On later turns use collaboration.followup_task for that same agent and wait. Do not run shell commands or solve the task yourself. Report the child's actual answer."})).await?;
    let root = started["thread"]["id"]
        .as_str()
        .context("Missing root")?
        .to_owned();
    ensure!(started["thread"]["modelProvider"] == "openai");
    let relay = user.join(".agents/skills/agentrelay/scripts/agentrelay");
    for (index, prompt) in [TASK, FOLLOWUP].into_iter().enumerate() {
        let file = workspace.path().join("assignment.txt");
        std::fs::write(&file, prompt)?;
        let output = std::process::Command::new(&relay)
            .arg("--home")
            .arg(&registry)
            .args([
                "prepare",
                "--runtime",
                "claude",
                "--model",
                "Haiku 5.5",
                "--effort",
                "high",
                "--root-id",
                &root,
                "--agent-path",
                "/root/claude_worker",
                "--timeout-seconds",
                "60",
                "--workspace",
            ])
            .arg(workspace.path())
            .arg("--prompt-file")
            .arg(file)
            .output()?;
        ensure!(
            output.status.success(),
            "Installed preflight failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        client.rpc("turn/start",json!({"threadId":root,"input":[{"type":"text","text":if index==0{"Execute the prepared task with the native Haiku agent."}else{"Follow up the same claude_worker with the new prepared assignment. Do not spawn another agent."}}]})).await?;
        loop {
            let event = client.next().await?;
            let done = event["method"] == "turn/completed" && event["params"]["threadId"] == root;
            let successful = !done || event["params"]["turn"]["status"] == "completed";
            client.notifications.push(event);
            ensure!(successful, "Installed parent turn failed");
            if done {
                break;
            }
        }
    }
    let child_id = client
        .notifications
        .iter()
        .filter_map(|n| n.pointer("/params/item"))
        .find(|i| i["type"] == "subAgentActivity" && i["kind"] == "started")
        .and_then(|i| i["agentThreadId"].as_str())
        .context("No installed native child")?
        .to_owned();
    let child = client
        .rpc(
            "thread/read",
            json!({"threadId":child_id,"includeTurns":true}),
        )
        .await?;
    ensure!(
        child["thread"]["modelProvider"] == "openai"
            && child["thread"]["model"] == "agentrelay-profile-haiku55_high"
            && child["thread"]["reasoningEffort"] == "high"
    );
    for answer in ["NATIVE_CLAUDE_SUM=73", "NATIVE_CLAUDE_DOUBLED=146"] {
        ensure!(
            child["thread"]["turns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|turn| turn["status"] == "completed"
                    && turn["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["type"] == "agentMessage"
                            && item["text"]
                                .as_str()
                                .is_some_and(|text| text.ends_with(answer)))),
            "Installed native result is missing"
        );
    }
    let tasks = agent_relay::store::Store::open(&registry)?.list(Some(&root))?;
    ensure!(
        tasks.len() == 1
            && tasks[0].generation == 2
            && tasks[0].actual_model.as_deref() == Some("claude-haiku-5-5")
    );
    let account = client
        .rpc("account/read", json!({"refreshToken":false}))
        .await?;
    ensure!(account["account"]["type"] == "chatgpt");
    client.rpc("account/rateLimits/read", json!({})).await?;
    std::fs::write(
        evidence.join("installed.json"),
        serde_json::to_vec_pretty(&json!({"root_thread_id":root,"child_thread_id":child_id,
        "child":child,"native_provider":"openai","observed_model":tasks[0].actual_model,"external_generations":2,"account_api":true,"rate_limits_api":true,"rendered_ui_verified":false}))?,
    )?;
    client.child.kill().await?;
    client.child.wait().await?;
    Ok(())
}

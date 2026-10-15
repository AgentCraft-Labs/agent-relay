use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Claude,
    Codex,
    Jsonl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSpec {
    pub kind: RuntimeKind,
    pub binary: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default = "version_args")]
    pub version_args: Vec<String>,
    #[serde(default)]
    pub session_resume: bool,
}

fn version_args() -> Vec<String> {
    vec!["--version".into()]
}

#[derive(Default, Deserialize)]
struct RuntimeConfig {
    #[serde(default)]
    runtimes: BTreeMap<String, RuntimeSpec>,
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub binary: String,
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
}

/// Driver implementations own command construction and event translation. The
/// coordinator never interprets a vendor-specific frame or calls a model API.
pub trait RuntimeAdapter: Send {
    fn capabilities(&self) -> Capabilities;
    fn launch(&self, task: &Task) -> Result<Launch>;
    fn decode(&mut self, frame: Value) -> Result<Vec<RuntimeEvent>>;
}

pub struct CliAdapter {
    pub spec: RuntimeSpec,
    last_codex_message: Option<String>,
}

impl CliAdapter {
    pub fn resolve(home: &Path, name: &str) -> Result<Self> {
        let file = home.join("runtimes.toml");
        let mut configured = if file.exists() {
            toml::from_str::<RuntimeConfig>(&std::fs::read_to_string(&file)?)?.runtimes
        } else {
            BTreeMap::new()
        };
        let spec = if let Some(spec) = configured.remove(name) {
            spec
        } else {
            match name {
                "claude" => RuntimeSpec {
                    kind: RuntimeKind::Claude,
                    binary: std::env::var("AGENTRELAY_CLAUDE_BIN")
                        .unwrap_or_else(|_| "claude".into()),
                    args: vec![],
                    resume_args: vec![],
                    version_args: version_args(),
                    session_resume: true,
                },
                "codex" => RuntimeSpec {
                    kind: RuntimeKind::Codex,
                    binary: std::env::var("AGENTRELAY_CODEX_BIN")
                        .unwrap_or_else(|_| "codex".into()),
                    args: vec![],
                    resume_args: vec![],
                    version_args: version_args(),
                    session_resume: true,
                },
                _ => bail!("Unknown runtime {name}; declare it in {}", file.display()),
            }
        };
        ensure!(!spec.binary.is_empty(), "Runtime binary cannot be empty");
        ensure!(
            matches!(spec.kind, RuntimeKind::Jsonl)
                || (spec.args.is_empty() && spec.resume_args.is_empty()),
            "Built-in runtime adapters accept a binary override, not custom protocol arguments"
        );
        Ok(Self {
            spec,
            last_codex_message: None,
        })
    }

    pub async fn probe(&self) -> Result<Value> {
        let mut command = tokio::process::Command::new(&self.spec.binary);
        command.args(&self.spec.version_args).kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output())
            .await
            .context("Runtime version probe timed out")??;
        ensure!(output.status.success(), "Runtime version probe failed");
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        ensure!(version.len() <= 4096, "Unexpectedly large version response");
        Ok(json!({"binary":self.spec.binary,"version":version,"capabilities":self.capabilities()}))
    }

    fn claude_decode(&self, frame: Value) -> Result<Vec<RuntimeEvent>> {
        let mut events = Vec::new();
        match frame["type"].as_str() {
            Some("system") if frame["subtype"] == "init" => {
                let id = frame["session_id"]
                    .as_str()
                    .context("Claude init has no session ID")?;
                events.push(RuntimeEvent::Session {
                    id: id.into(),
                    model: frame["model"].as_str().map(str::to_owned),
                });
            }
            Some("stream_event") => {
                let event = &frame["event"];
                if event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta"
                {
                    events.push(RuntimeEvent::Text(
                        event["delta"]["text"].as_str().unwrap_or_default().into(),
                    ));
                }
            }
            Some("assistant") => {
                if let Some(blocks) = frame["message"]["content"].as_array() {
                    for block in blocks {
                        if block["type"] == "tool_use" {
                            events.push(RuntimeEvent::Tool(json!({"phase":"started","id":block["id"],"name":block["name"],"input":block["input"]})));
                        }
                    }
                }
            }
            Some("user") => {
                if let Some(blocks) = frame["message"]["content"].as_array() {
                    for block in blocks {
                        if block["type"] == "tool_result" {
                            events.push(RuntimeEvent::Tool(json!({"phase":"completed","id":block["tool_use_id"],"is_error":block["is_error"],"content":block["content"]})));
                        }
                    }
                }
            }
            Some("result") => {
                if let Some(id) = frame["session_id"].as_str() {
                    events.push(RuntimeEvent::Session {
                        id: id.into(),
                        model: None,
                    });
                }
                if !frame["usage"].is_null() {
                    events.push(RuntimeEvent::Usage(frame["usage"].clone()));
                }
                if frame["is_error"] != false || frame["subtype"] != "success" {
                    events.push(RuntimeEvent::Error(format!(
                        "Claude terminal error: {}",
                        frame["errors"]
                            .as_array()
                            .map(|v| v
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("; "))
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| frame["subtype"]
                                .as_str()
                                .unwrap_or("unknown")
                                .into())
                    )));
                } else {
                    events.push(RuntimeEvent::Result {
                        text: frame["result"]
                            .as_str()
                            .context("Claude success has no text result")?
                            .into(),
                        session_id: frame["session_id"].as_str().map(str::to_owned),
                    });
                }
            }
            Some("rate_limit_event") => {} // Account metadata is not copied into task logs.
            Some(_) => {}
            None => bail!("Runtime frame has no type"),
        }
        Ok(events)
    }
}

impl RuntimeAdapter for CliAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            structured_events: true,
            session_resume: self.spec.session_resume,
            live_input: false,
            cooperative_interrupt: false,
            usage: !matches!(self.spec.kind, RuntimeKind::Jsonl),
        }
    }

    fn launch(&self, task: &Task) -> Result<Launch> {
        let request = &task.request;
        ensure!(
            task.generation == 1 || self.capabilities().session_resume,
            "This runtime does not support session resume"
        );
        let resume = if task.generation > 1 {
            Some(
                task.session_id
                    .as_deref()
                    .context("Missing external session ID")?,
            )
        } else {
            None
        };
        let mut args = Vec::<String>::new();
        let stdin;
        match self.spec.kind {
            RuntimeKind::Claude => {
                args.extend(
                    [
                        "-p",
                        "--output-format",
                        "stream-json",
                        "--verbose",
                        "--include-partial-messages",
                        "--strict-mcp-config",
                        "--mcp-config",
                        "{\"mcpServers\":{}}",
                        "--setting-sources",
                        "",
                        "--settings",
                        "{\"disableAllHooks\":true}",
                        "--disable-slash-commands",
                        "--no-chrome",
                        "--permission-prompts",
                        "none",
                    ]
                    .map(str::to_owned),
                );
                let tools = match request.access {
                    Access::ReadOnly => "Read,Glob,Grep",
                    Access::WorkspaceWrite => "Read,Glob,Grep,Edit,Write,Bash",
                };
                args.extend(
                    [
                        "--tools",
                        tools,
                        "--allowedTools",
                        tools,
                        "--permission-mode",
                        "dontAsk",
                    ]
                    .map(str::to_owned),
                );
                if let Some(id) = resume {
                    args.extend(["--resume".into(), id.into()]);
                } else {
                    args.extend(["--session-id".into(), uuid::Uuid::new_v4().to_string()]);
                }
                if let Some(model) = &request.model {
                    args.extend(["--model".into(), model.clone()]);
                }
                // CLI owns its authentication. Bare mode would disable existing OAuth.
                stdin = request.prompt.as_bytes().to_vec();
            }
            RuntimeKind::Codex => {
                args.extend([
                    "--cd".into(),
                    request.workspace.to_string_lossy().into_owned(),
                    "--ask-for-approval".into(),
                    "never".into(),
                    "--sandbox".into(),
                    match request.access {
                        Access::ReadOnly => "read-only",
                        Access::WorkspaceWrite => "workspace-write",
                    }
                    .into(),
                    "exec".into(),
                ]);
                if let Some(id) = resume {
                    args.extend(["resume".into(), id.into()]);
                }
                args.extend(
                    ["--json", "--skip-git-repo-check", "--color", "never"].map(str::to_owned),
                );
                // resume does not expose the exec-only color flag.
                if resume.is_some() {
                    args.truncate(args.len() - 2);
                }
                if let Some(model) = &request.model {
                    args.extend(["--model".into(), model.clone()]);
                }
                args.push("-".into());
                stdin = request.prompt.as_bytes().to_vec();
            }
            RuntimeKind::Jsonl => {
                args = self.spec.args.clone();
                if let Some(id) = resume {
                    args.extend(
                        self.spec
                            .resume_args
                            .iter()
                            .map(|arg| arg.replace("{session_id}", id)),
                    );
                }
                stdin = serde_json::to_vec(
                    &json!({"protocol_version":1,"task_id":task.id,"generation":task.generation,"prompt":request.prompt,"workspace":request.workspace,"model":request.model,"access":request.access,"resume_session_id":resume}),
                )?;
            }
        }
        Ok(Launch {
            binary: self.spec.binary.clone(),
            args,
            stdin,
        })
    }

    fn decode(&mut self, frame: Value) -> Result<Vec<RuntimeEvent>> {
        match self.spec.kind {
            RuntimeKind::Claude => self.claude_decode(frame),
            RuntimeKind::Codex => {
                let events = match frame["type"].as_str() {
                    Some("thread.started") => vec![RuntimeEvent::Session {
                        id: frame["thread_id"]
                            .as_str()
                            .context("Codex thread has no ID")?
                            .into(),
                        model: None,
                    }],
                    Some("item.completed") if frame["item"]["type"] == "agent_message" => {
                        let text = frame["item"]["text"]
                            .as_str()
                            .context("Codex message has no text")?
                            .to_owned();
                        self.last_codex_message = Some(text.clone());
                        vec![RuntimeEvent::Text(text)]
                    }
                    Some("item.started" | "item.updated" | "item.completed") => {
                        vec![RuntimeEvent::Tool(
                            json!({"phase":frame["type"],"item":frame["item"]}),
                        )]
                    }
                    Some("turn.completed") => vec![
                        RuntimeEvent::Usage(frame["usage"].clone()),
                        RuntimeEvent::Result {
                            text: self
                                .last_codex_message
                                .take()
                                .context("Codex completed without a final agent message")?,
                            session_id: None,
                        },
                    ],
                    Some("turn.failed") => vec![RuntimeEvent::Error(
                        frame["error"]["message"]
                            .as_str()
                            .unwrap_or("Codex turn failed")
                            .into(),
                    )],
                    Some("error") => vec![RuntimeEvent::Diagnostic(
                        json!({"message":frame["message"]}),
                    )],
                    Some(_) => vec![],
                    None => bail!("Codex frame has no type"),
                };
                Ok(events)
            }
            RuntimeKind::Jsonl => {
                let event = match frame["type"].as_str() {
                    Some("session") => RuntimeEvent::Session {
                        id: frame["session_id"]
                            .as_str()
                            .context("Session ID required")?
                            .into(),
                        model: frame["model"].as_str().map(str::to_owned),
                    },
                    Some("text") => {
                        RuntimeEvent::Text(frame["text"].as_str().context("Text required")?.into())
                    }
                    Some("tool") => RuntimeEvent::Tool(frame["data"].clone()),
                    Some("usage") => RuntimeEvent::Usage(frame["data"].clone()),
                    Some("diagnostic") => RuntimeEvent::Diagnostic(frame["data"].clone()),
                    Some("result") if frame["status"] == "completed" => RuntimeEvent::Result {
                        text: frame["text"]
                            .as_str()
                            .context("Result text required")?
                            .into(),
                        session_id: frame["session_id"].as_str().map(str::to_owned),
                    },
                    Some("error") => RuntimeEvent::Error(
                        frame["message"]
                            .as_str()
                            .unwrap_or("External runtime failed")
                            .into(),
                    ),
                    _ => bail!("Unsupported JSONL runtime frame"),
                };
                Ok(vec![event])
            }
        }
    }
}

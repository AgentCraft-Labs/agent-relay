use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const MAX_FRAME: usize = 1_048_576;
pub const LEASE_MS: i64 = 15_000;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    ReadOnly,
    WorkspaceWrite,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl Status {
    pub fn terminal(self) -> bool {
        self != Self::Running
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    pub runtime: String,
    pub workspace: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub access: Access,
    pub root_id: Option<String>,
    #[serde(alias = "supervisor_id")]
    pub agent_id: Option<String>,
    #[serde(alias = "supervisor_path")]
    pub agent_path: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub generation: i64,
    pub status: Status,
    pub request: RunRequest,
    pub session_id: Option<String>,
    pub actual_model: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub cancel_requested: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub heartbeat_at: i64,
    pub runner_pid: Option<u32>,
    pub worker: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: i64,
    pub task_id: String,
    pub generation: i64,
    pub timestamp: i64,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub structured_events: bool,
    pub session_resume: bool,
    pub live_input: bool,
    pub cooperative_interrupt: bool,
    pub usage: bool,
}

#[derive(Debug, Clone)]
pub struct Claim {
    pub task: Task,
    pub token: String,
    pub execute: bool,
}

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    Model {
        id: String,
    },
    Session {
        id: String,
        model: Option<String>,
    },
    Text(String),
    Tool(Value),
    Usage(Value),
    Diagnostic(Value),
    Result {
        text: String,
        session_id: Option<String>,
    },
    Error(String),
}

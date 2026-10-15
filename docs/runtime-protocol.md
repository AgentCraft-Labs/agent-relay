# Runtime adapters and lifecycle contract

AgentRelay owns task identity, persistence, cancellation and process lifetime. An adapter owns CLI arguments and translation of that CLI's structured output. The CLI runner does not call model APIs or manage provider credentials. The Responses router forwards parent model requests; each installed CLI uses its own existing login.

## Built-in adapters

`claude` uses Claude Code's print mode with stream JSON, partial messages and a stable session ID. The runner disables MCP, slash commands, browser integration and external Claude hooks/settings sources for this invocation. Read-only exposes Read, Glob and Grep; workspace-write also exposes Edit, Write and Bash. Approval mode is `dontAsk`; unavailable permissions produce an actual runtime failure. OAuth authentication remains owned by Claude Code.

`codex` uses the official `codex exec --json` interface, and `codex exec resume` for subsequent turns. It explicitly selects read-only or workspace-write sandboxing and never requests an approval bypass.

Neither adapter promises live message input or a cooperative model interruption. `message` only persists input for a later turn. `interrupt` requests termination of the runner-owned process group. A successful exit requires exactly one successful structured terminal result and a successful process exit; an error frame with exit code zero is still a failure.

These tool restrictions are not a new OS sandbox. The worker inherits the runner's actual execution boundary. Read access is not restricted to the workspace by AgentRelay. Custom adapters must enforce or honestly document the access request; their implementation is trusted local code.

## Configuration

The default state directory is `~/.local/state/agentrelay`, overridden by `--home` or `AGENTRELAY_HOME`. Put runtime configuration in `runtimes.toml` inside that directory. Built-in adapters allow executable overrides, including aliases with the same protocol:

```toml
[runtimes.claude]
kind = "claude"
binary = "/absolute/path/to/claude"
session_resume = true

[runtimes.myagent]
kind = "jsonl"
binary = "/absolute/path/to/myagent-driver"
args = ["serve-one-turn"]
resume_args = ["--resume", "{session_id}"]
version_args = ["--version"]
session_resume = true
```

Arguments are passed directly to a process, never evaluated as shell expressions. Only custom JSONL adapters accept argument templates. Environment overrides `AGENTRELAY_CLAUDE_BIN` and `AGENTRELAY_CODEX_BIN` apply to built-in defaults when no explicit configuration entry exists.

## Custom JSONL protocol, version 1

The executable receives one UTF-8 JSON object on stdin followed by EOF. The object contains:

```json
{
  "protocol_version": 1,
  "task_id": "stable-relay-task-id",
  "generation": 1,
  "prompt": "The authorized task",
  "workspace": "/absolute/workspace",
  "model": null,
  "access": "read_only",
  "resume_session_id": null
}
```

For a resumed turn, generation increases and resume_session_id contains the previous external ID. The driver must preserve that session identity. Access is `read_only` or `workspace_write`.

Stdout must contain JSON objects separated by newlines. Logging belongs on stderr. Supported events:

```jsonl
{"type":"session","session_id":"provider-session","model":"actual-model"}
{"type":"text","text":"Incremental output"}
{"type":"tool","data":{"name":"read","path":"fixture.txt"}}
{"type":"usage","data":{"input_tokens":100,"output_tokens":20}}
{"type":"diagnostic","data":{"message":"Optional diagnostic"}}
{"type":"result","status":"completed","text":"The final answer","session_id":"provider-session"}
```

Failure uses `{"type":"error","message":"Reason"}` instead of result. Emit exactly one terminal event. Unknown event types, changed session IDs, invalid UTF-8/JSON, oversized frames, duplicate terminal events, missing results and nonzero process exits fail the task. Each input prompt and output frame is limited to 1 MiB, including the newline for a frame. Stderr is drained while retaining only its last 8 KiB for an exit failure diagnostic. SQLite stores prompts and outputs locally with a private state directory; retention/deletion automation is not implemented.

## Concurrency, cancellation and recovery

Submission keys are required. Repeating the same operation and key with the same request returns the existing task without launching a second worker. Reusing a key with different content fails. Keys are scoped to the registry for new tasks and to the task for resume/message operations. Include the native root and agent in the logical submission key.

The registry serializes claims with SQLite immediate transactions, limits active runners to eight and verifies an ownership token plus generation on updates. Event cursors are monotonically increasing registry sequence numbers. `events` returns up to 500 events; continue from its returned cursor.

`message` queues input in FIFO order without waking an idle session. `resume` requires a terminal task, an external session ID and cleanup of any prior runner. It consumes queued messages once and starts a new generation. It does not change the runtime, model, workspace or access mode.

The runner heartbeats every 250 ms. A 15-second expired lease can be reconciled by `watchdog --once` or a running `watchdog`. The watchdog never starts workers. It checks worker PID, creation time and process-group ownership before signaling a recorded process, marks the task interrupted, and prevents a new generation while the old runner is still alive. The provider service runs this watchdog automatically; standalone runners can use explicit recovery after an unclean exit.

The runner sends SIGTERM to its process group, allows 750 ms for cleanup and then sends SIGKILL. Signal interruption, timeout, malformed output and process exit also clean up the group. A CLI that intentionally detaches into a new process group is outside this process-group guarantee. Windows is not supported in this initial release.

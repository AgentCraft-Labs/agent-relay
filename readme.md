# AgentRelay

Run Claude Code and other agent CLIs as **native agents in the official Codex app**, with no MCP, no Codex rebuild, and no intermediate Codex model executing the delegated task.

AgentRelay is a Rust companion with a local Responses provider, CLI adapters, a durable task registry, a user skill and native agent roles. Codex owns the parent/child threads, activity and messaging. The companion fulfills a child's model request using an actual CLI session.

## How it works

```mermaid
flowchart TD
    App[Official Codex app] --> Parent[Codex parent]
    Parent --> Native[Native child / agentrelay-claude]
    Parent --> Router[AgentRelay Rust provider]
    Native --> Router
    Router --> OpenAI[Official OpenAI Responses upstream]
    Router --> Runner[Rust CLI runner]
    Runner --> Claude[Claude Code]
    Runner --> Other[Codex CLI / custom JSONL driver]
    Router <--> Registry[Private SQLite registries]
```

Normal model requests pass through to the fixed official OpenAI HTTPS endpoint. Requests for `agentrelay-RUNTIME` execute an explicitly prepared CLI task bound to the actual native root, canonical agent path, child thread and turn. The child uses the same provider as its parent. A role selects the CLI model alias; a skill alone cannot replace a native agent backend.

There is **no extra Codex model inside the child**. The parent still consumes its normal Codex usage for planning, delegation, waiting and reporting. Claude Code consumes its own usage through its own login. AgentRelay itself performs no model inference. External usage events stay in the local registry; they are not fabricated as OpenAI token usage.

### One delegation, end to end

The parent prepares the full task before spawning the child. Codex manages the native thread; AgentRelay executes the external turn and records both the assignment and its answer.

```mermaid
sequenceDiagram
    participant Parent as Codex parent
    participant Prepare as AgentRelay prepare
    participant Store as Private registries
    participant Child as Native Codex child
    participant Provider as Rust provider
    participant CLI as Claude Code / CLI
    Parent->>Prepare: Full task, actual root ID, agent path, workspace
    Prepare->>Store: Snapshot task and authorized access
    Parent->>Child: spawn_agent with CLI model alias
    Child->>Provider: Responses request with native thread/turn IDs
    Provider->>Store: Validate binding and claim turn once
    Provider-->>Child: Assigned task commentary
    Provider->>CLI: Execute or resume the external session
    Note over Provider,CLI: No intermediate Codex model inference
    CLI-->>Provider: Actual structured result
    Provider->>Store: Persist result and external session
    Provider-->>Child: Full assignment and final result
    Child-->>Parent: Native completion and answer
```

## Build and install

Requirements: Rust/Cargo 1.95+, macOS or Linux, a recent Codex version supporting multi-agent v2/custom providers, and independently installed/authenticated external CLIs. Windows is not supported. Automatic background service installation currently targets macOS.

```bash
cargo build --release --locked
target/release/agentrelay doctor --runtime claude
target/release/agentrelay codex install --user
target/release/agentrelay codex configure
target/release/agentrelay codex service-install
```

Start a **new conversation in the official Codex app** after configuration. Existing conversations can retain their original provider. Confirm that the conversation uses `agentrelay`; do not select a CLI alias for the main conversation. Keep the ordinary OpenAI model for the parent.

`codex configure` preserves existing model entries/settings, adds the CLI aliases, selects the provider for new conversations, and saves the original configuration once as `~/.codex/config.before-agentrelay.toml`. It uses the current `~/.codex/models_cache.json`. Rerun it when Codex adds or changes upstream models. Conflicting custom providers are rejected for manual reconciliation.

On Linux, or for a foreground service, run:

```bash
target/release/agentrelay serve --listen 127.0.0.1:39091
```

Keep the service running while using the configured provider. On macOS `service-install` installs/restarts `com.agentrelay.provider` through LaunchAgent and preserves the installation PATH for finding the external CLIs. It refuses to restart while tasks are active. The service is local and uses no model.

### Installation scope and paths

| Path | Purpose |
| --- | --- |
| `~/.agents/skills/agentrelay/` | Self-contained user skill and runner, across local projects |
| `~/.codex/agents/agentrelay_claude.toml` | Native Claude Code role |
| `~/.codex/config.toml` | Local provider and model catalog configuration |
| `~/.local/state/agentrelay/` | Private tasks, assignments, sessions, events and provider key |
| `~/Library/LaunchAgents/com.agentrelay.provider.plist` | Optional macOS service |

User installation applies to this operating-system user on this computer. Remote hosts and other users need their own installation. Project installation is available with `codex install --project /absolute/project`; provider configuration/service are still required on that host. Skills discovered at multiple scopes may appear twice.

The installer protects unrelated or modified files. It migrates previously recorded integration files, removes the owned obsolete role and hooks, and preserves task history. It never changes the Codex executable. Standalone skill copies avoid plugin namespace changes caused by symlinking into an exported plugin bundle.

`codex export --out dist/agentrelay` creates a distributable skill package and role asset. Exporting or installing a skill/plugin alone does not configure the provider or register every role. There is no execution fallback that introduces an intermediate Codex model.

## Use it in Codex

For a new read-only task:

```text
Use $agentrelay to delegate [TASK] to Claude Code as a native agent named
claude_review. Work read-only. Use the prepared-task provider path and report
the actual result and checks.
```

Italian equivalent:

```text
Usa $agentrelay per delegare a Claude Code questo task: [TASK].
Crea l'agente nativo claude_review, in sola lettura, tramite il provider
AgentRelay. Riporta il risultato e le verifiche realmente eseguite.
```

For authorized edits, explicitly request `workspace-write`. The parent prepares the assignment, then creates one native child using role `agentrelay_claude` and fresh context. The full task is persisted before spawning; opaque native encrypted messages are never decoded or treated as a plaintext task.

### What appears in the agent detail

The child conversation records **the complete assigned task and the actual external result**. AgentRelay emits an “Assigned task (recorded by AgentRelay)” commentary message before execution and includes the assignment again in the final result. Codex's compact history projection retains only the final assistant message, so both remain available there too.

This is an honestly labeled assignment receipt, not a fabricated user-message bubble or a second model response. The provider adds it without inference. Claude's individual tool calls are stored in AgentRelay events; they are not rendered as native Codex tool cards.

The official app-server test confirms both messages in native child history. Rendered desktop layout has not been visually verified; see [verification](docs/verification.md).

### Follow-ups and cancellation

```text
Use AgentRelay to ask claude_review [FOLLOW-UP]. Reuse the same native agent
and Claude session; prepare the follow-up before sending followup_task.
```

Wait for the current turn to end. Prepare a new task with the same root/path/runtime/workspace/access/model, then use the native follow-up tool. The same relay task and CLI session continue with a new generation. Runtime, model and access changes require a new agent path/task.

```mermaid
stateDiagram-v2
    [*] --> Prepared: Explicit task snapshot
    Prepared --> Running: Native turn claims assignment
    Running --> Completed: Structured success and exit zero
    Running --> Failed: Runtime or protocol error
    Running --> Interrupted: Cancel, timeout or disconnected stream
    Completed --> Completed: Retry same turn returns saved result
    Completed --> Prepared: Prepare follow-up for same child and CLI session
    note right of Running
        Duplicate active turn is rejected.
        Ownership and generation checks
        prevent stale results.
    end note
```

For cancellation, ask to interrupt the associated AgentRelay task and confirm its terminal state. A disconnected provider stream also requests cancellation and reaps the CLI process group. Desktop Stop-button behavior remains a separate host-level acceptance check; do not assume every UI control closes that stream immediately.

## Direct commands and other CLIs

The parent prepares native work using:

```bash
agentrelay prepare --runtime claude --workspace /absolute/project \
  --root-id ACTUAL_ROOT_SESSION_ID --agent-path /root/claude_review \
  --prompt-file /private/task.txt
```

Then it spawns the native child with model `agentrelay-claude`. `prepare` snapshots the file content; editing the file afterward does not silently change the task. `CODEX_SESSION_ID` can supply the actual root ID when running inside Codex.

Built-in runtime names are `claude` and `codex`; model aliases are `agentrelay-claude` and `agentrelay-codex`. Other CLIs need a driver implementing the [JSONL protocol](docs/runtime-protocol.md). Configure it in `AGENTRELAY_HOME/runtimes.toml`, rerun `codex configure` to add its catalog alias, and create a native role using model `agentrelay-RUNTIME`. Arbitrary interactive CLIs are not automatically compatible.

Recovery and standalone CLI execution:

```bash
agentrelay list --root ACTUAL_ROOT_SESSION_ID
agentrelay show TASK_ID
agentrelay events TASK_ID --after 0 --wait-seconds 10
agentrelay interrupt TASK_ID
agentrelay watchdog --once
agentrelay run --runtime claude --workspace /absolute/project \
  --key unique-task-key --prompt-file /private/task.txt
agentrelay resume TASK_ID --key unique-followup-key --prompt-file /private/next.txt
```

Standalone `run` is a diagnostic/automation interface; it does not create a native Codex agent. `message` queues input for the next resumed turn and does not wake an idle agent or interrupt active inference. `events` returns up to 500 events; continue from the returned cursor. The default timeout is 1800 seconds, configurable from 1 to 86400. At most eight CLI turns run per registry.

State-directory precedence is `--home`, then `AGENTRELAY_HOME`, then `~/.local/state/agentrelay`. Always query the registry that created the task. `--agent-id` and `--agent-path` associate standalone runs with confirmed native identities. Old stored identity fields are read only for data migration.

## Trust and operational limits

- The provider binds only to loopback and requires a private local header key. OpenAI authentication is supplied normally by Codex, held in memory, and forwarded only to the fixed official HTTPS upstream. Redirects are disabled and credentials are not logged. The service is a trusted local component that can see model requests.
- The companion runs under the local user's permissions. Claude tool restrictions are **not an OS sandbox**, and read access is not limited to the workspace. Native write-policy metadata additionally gates workspace-write tasks. Custom drivers are trusted code. Use only with authorized local projects and tasks.
- Registries contain prompts, results and external session IDs. Keep state and service logs private. No retention/deletion automation or cross-machine synchronization is provided.
- Native turn IDs are idempotent. Retries replay saved terminal results; active/unresolved attempts do not launch duplicates. Leases, ownership tokens and generations reject stale output. The service reconciles expired workers; `watchdog --once` also works offline.
- Process groups are cleaned on cancellation, timeout, malformed output and exit. Deliberately detached processes in another group are outside that guarantee.
- This integration depends on current Codex provider, catalog and native-agent contracts. Official app updates remain installable, but compatibility must be retested; the model catalog needs refresh. The desktop UI itself is not open source.

To disable routing, first end active tasks, restore the relevant provider/catalog settings from `config.before-agentrelay.toml`, and stop the service. On macOS: `launchctl bootout gui/$(id -u)/com.agentrelay.provider`. Keep registries if you need resumable CLI sessions. Review any later edits before restoring the entire backup.

## Validation

```bash
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
# Explicit live tests; use existing CLI logins and consume model usage:
AGENTRELAY_CODEX_TEST_BIN=/absolute/path/to/official/codex \
  cargo test --test native_provider --locked -- --ignored --test-threads=1
```

See [architecture](docs/architecture.md), [implementation plan](docs/implementation-plan.md), [verification](docs/verification.md), and [sanitized acceptance results](evidence/native-provider-results.json). Private test transcripts, credentials, local installations and generated binaries are excluded from the repository.

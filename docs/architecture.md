# Architecture

AgentRelay attaches at Codex's Responses-provider boundary. The official executable and desktop app remain unchanged. Native spawning still creates a genuine child thread with a parent link, canonical path, activity and mailbox. Its configured CLI model alias is fulfilled by a Rust companion instead of an additional OpenAI inference.

## Components and responsibility

| Component | Responsibility |
| --- | --- |
| Native Codex parent | Plan, prepare an explicit task, spawn/wait/follow up |
| Native child harness | Native thread identity, lifecycle, messages and completion |
| Local Responses provider | Route normal models to OpenAI and CLI aliases to a prepared task |
| Assignment registry | Bind plaintext tasks to root/path, pin the actual child, snapshot native turns |
| Runner registry | External session, generation, ownership, results, messages and events |
| Runtime adapter | Construct argument arrays and translate CLI output |
| Rust runner | Own the process group, heartbeat, validate results and cancel/reap |
| Skill and native role | Guide the parent and select the external model alias |

There is no secondary Codex model interpreting the external response. Parent usage and external CLI usage remain separate. A custom JSONL adapter supports other CLIs without changing Codex or the provider protocol.

## Why this attachment point

The inspected upstream source exposes `AgentControl` as an embedding interface. Replacing it requires a custom binary and does not meet the requirement to retain official updates. The role loader rejects a `command` field. Native role overrides do not switch the child's model provider in the tested binary. These behaviors are covered by black-box tests against an unmodified official executable.

The chosen route uses one inherited provider for parent and child and dispatches by model alias. The parent retains its normal OpenAI model. Plugins/skills package instructions and assets; they cannot independently register an arbitrary CLI backend or change the app's internal agent manager. Neither private database writes nor archive patches are used.

Relevant source areas in the downloaded `openai/codex` revision `f0c5077c34ec7a76b4ad9134835925e6ae3d72ec` include `core/src/agent/role.rs`, `core/src/thread_spawn.rs`, `core/src/tools/handlers/multi_agents_v2/spawn.rs`, and `app-server-protocol/src/protocol/thread_history.rs`. Live tests use the installed official binary, not a rebuilt source checkout.

## Assignment and native turn protocol

1. The parent writes a private plaintext task and calls `prepare` with its actual root ID and planned canonical child path. The registry snapshots task text and immutable runtime settings.
2. The parent spawns one native child using an `agentrelay-RUNTIME` model alias.
3. The provider reads native root/thread/turn metadata, looks up the exact root/path binding, verifies the model alias and pins the first actual child ID. Another root or child cannot claim it.
4. An immediate SQLite transaction consumes the pending assignment and records the native turn and runner claim. The native turn ID supplies the idempotency key.
5. The provider emits a labeled task receipt and runs the adapter. Opaque native messages are never decrypted, decoded or treated as plaintext task content.
6. The runner requires one successful structured terminal event plus successful process exit. The provider persists a terminal snapshot and emits the actual result.
7. A repeated native turn replays that snapshot. An in-progress or unresolved claim is rejected; recovery never silently repeats work. If the runner finished before the native snapshot was saved, retry reconstructs it from the matching terminal generation.
8. A follow-up requires a fresh prepared task and the same native child. The runner resumes the same external session with a new generation.

The two registries intentionally use separate SQLite files. Claims use deterministic operation keys to prevent duplicate runner execution across a crash between registry commits. An orphaned runner claim must be inspected/reconciled; automatic task re-execution is deliberately not attempted.

## Conversation visibility

Native inter-agent task input is not projected as a normal user message in the tested child's history. AgentRelay therefore records the **actual explicit assignment** as assistant commentary labeled “Assigned task (recorded by AgentRelay)”. The final message repeats that assignment followed by the external result. This also survives Codex's Summary projection, which retains the final assistant item. It is not a synthetic human message.

The native history contract is verified with a real external CLI and `thread/read(includeTurns=true)`. External tool events remain in AgentRelay's registry; no native tool cards or alternate desktop model badges are fabricated.

## Transport and credentials

The HTTP server binds only to loopback, checks a private per-installation header and accepts bounded JSON bodies. Parent requests stream to the fixed official `https://chatgpt.com/backend-api/codex/responses` endpoint with normal Codex-provided authentication. Redirects are disabled. The local header is stripped before forwarding. Upstream status and headers are preserved. Credentials are neither extracted from the account file nor logged by the provider.

CLI sessions authenticate independently. Claude launches with MCP, external hooks, browser integration and slash commands disabled for that invocation. CLI tool access follows the prepared access mode; native read-only metadata additionally prevents a workspace-write assignment from running.

The companion is a trusted process with the user's permissions, not an extension of Codex's OS sandbox. Read-only tool lists are not filesystem isolation. Production environments requiring strict confinement need an execution boundary around the service/drivers; this release does not claim to supply one.

## Cancellation and recovery

A dropped provider response body interrupts the owned runner and waits for process cleanup. Explicit registry interruption also works. The runner heartbeats every 250 ms, owns a separate process group, sends TERM then KILL after 750 ms and reaps it. The provider watchdog reconciles leases older than 15 seconds. PID start times and process-group ownership prevent killing recycled PIDs. Generation/token checks reject stale results.

The transport cancellation test does not establish the timing of every desktop Stop control. Likewise, app-server history checks establish the data contract, not a rendered screenshot. These acceptance boundaries are recorded explicitly in verification.

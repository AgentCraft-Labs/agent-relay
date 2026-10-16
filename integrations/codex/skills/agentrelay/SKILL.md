---
name: agentrelay
description: Delegate explicitly requested work to Claude Code or another configured CLI using a native Codex agent and the AgentRelay provider, without MCP or an intermediate Codex model.
---

# AgentRelay

Keep the conversation in the official Codex app. The native child executes through the AgentRelay Responses provider. There is one native child per external worker, with no extra Codex model inference inside it.

Resolve `scripts/agentrelay` relative to this skill directory. Check the installed runner with `--version` and confirm that this conversation uses the `agentrelay` model provider. The native role `agentrelay_claude` must be available. If this conversation uses a different provider, explain that a new conversation using the configured provider is required. Do not fall back to a Codex worker that calls the CLI, and do not run the CLI directly from the parent as a substitute for native visibility.

## New delegation

1. Choose a canonical native path, for example `/root/claude_review`. Read the actual `CODEX_SESSION_ID` in the parent; never invent a root or thread ID.
2. Write the authorized task to a private UTF-8 file. Include the complete request, relevant context, expected output and workspace. Do not forward encrypted native history.
3. Prepare the assignment using separate, quoted shell arguments:

   ```text
   scripts/agentrelay prepare --runtime claude --workspace ABSOLUTE_WORKSPACE --root-id ACTUAL_ROOT_ID --agent-path /root/claude_review --prompt-file PRIVATE_TASK_FILE
   ```

   Read-only is the default. Add `--access workspace-write` only for authorized edits when native execution policy permits writes. The companion is a separate local process; CLI tool restrictions do not provide an OS sandbox. Do not claim it inherits the native sandbox.
   If the user selects a model or effort, pass them as execution settings, not merely task text. For “use Haiku 5.5 high”, add `--model "Haiku 5.5" --effort high`. Version/model incompatibility is an explicit error; never substitute another model without the user's instruction.
4. Read the `native` object returned by `prepare` and spawn exactly one child with that role/model/effort and fresh context (`fork_turns: none`). Default profiles include Haiku 5.5 high, Sonnet 5.5 high and Opus 5.5 high. For Opus, use `agentrelay_profile_opus55_high` / `agentrelay-profile-opus55_high`, with native effort `high`. This exposes the external selection through Codex's actual model catalog and native thread settings. A new task with an explicit model or effort requires a matching configured profile; preparation fails rather than showing a generic low/Light label for a high-effort CLI task. If no profile matches, register one with `profile add`, run `codex configure`, and reload the app/start a new conversation before delegating. If a returned profile role is unavailable, reload it instead of silently falling back. Generic Claude role/model `agentrelay_claude` / `agentrelay-claude` are for runtime defaults or preserved legacy workers. The native message can say “Execute the prepared AgentRelay task”; the full task is already bound to the root and canonical path.
5. Wait using the native agent tools. The provider records the full assigned task and the actual external result in the child conversation. Report failures honestly. Native tool cards for Claude's tools are not implemented; use AgentRelay events for the external transcript.

## Follow-up and cancellation

For a follow-up, wait until the current turn ends, then call `prepare` again with the same runtime, root, path, workspace, model and access, and a new task file. Use the native `followup_task` on the existing child. AgentRelay resumes the same external session. Do not spawn a replacement child with the same canonical path.
Repeat the same `--model` and `--effort` flags; recover them with `show TASK_ID`. These settings are immutable per worker, so a changed selection needs a new agent path. Generic native role effort does not control the external CLI's effort. Report requested model, observed model and requested effort separately; a submitted flag does not independently verify provider-side applied effort.

Native routing is frozen per worker. A legacy worker keeps its old generic native label even after a matching profile is added; follow the `native` returned for that existing path. A new worker receives the dedicated profile. Do not restart an active provider to update a label. On macOS `scripts/agentrelay codex upgrade --wait-idle` waits until external workers are idle, updates the integration/default profiles and restarts the service. The provider emits `response.in_progress` every 15 seconds during quiet CLI work to prevent Codex's idle-stream timeout. An initial receipt is a snapshot; the final result contains the observed model.

For explicit cancellation, find the external task with `list --root ACTUAL_ROOT_ID`, call `interrupt TASK_ID`, then confirm its terminal state. Closing the provider response stream also requests external cancellation; app Stop wiring must not be claimed without an actual test in that host. `message` only queues input for the next resumed turn.

Recovery: `show TASK_ID`, `events TASK_ID --after CURSOR --wait-seconds 10`, and `watchdog --once`. Repeated native turn IDs replay the saved result, without repeating CLI work. An active or unresolved turn is rejected instead of relaunched.

## Other CLIs

Built-in model aliases are `agentrelay-claude` and `agentrelay-codex`. Additional runtimes use the documented JSONL adapter in `AGENTRELAY_HOME/runtimes.toml`, a model alias `agentrelay-RUNTIME`, and a matching native role/catalog entry. Never present arbitrary interactive CLI output as a supported adapter. Each CLI uses its own existing authentication.

For a dedicated native model label, register an immutable selection with `profile add NAME --runtime RUNTIME --model MODEL --effort LEVEL --display-name LABEL`, then run `codex configure`. `profile list` lists registered selections. Profiles must be configured before starting the conversation so their catalog entries and roles are available to the parent. A new model/effort selection requires a new profile rather than relabeling existing threads.

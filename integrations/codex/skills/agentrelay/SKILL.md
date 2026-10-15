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
4. Spawn exactly one native child: task name `claude_review`, role `agentrelay_claude`, fresh context (`fork_turns: none`). Its configured model must be `agentrelay-claude`. If role selection is unavailable but model selection exists, use that alias explicitly. The native message can say “Execute the prepared AgentRelay task”; the full task is already bound to the root and canonical path.
5. Wait using the native agent tools. The provider records the full assigned task and the actual external result in the child conversation. Report failures honestly. Native tool cards for Claude's tools are not implemented; use AgentRelay events for the external transcript.

## Follow-up and cancellation

For a follow-up, wait until the current turn ends, then call `prepare` again with the same runtime, root, path, workspace, model and access, and a new task file. Use the native `followup_task` on the existing child. AgentRelay resumes the same external session. Do not spawn a replacement child with the same canonical path.

For explicit cancellation, find the external task with `list --root ACTUAL_ROOT_ID`, call `interrupt TASK_ID`, then confirm its terminal state. Closing the provider response stream also requests external cancellation; app Stop wiring must not be claimed without an actual test in that host. `message` only queues input for the next resumed turn.

Recovery: `show TASK_ID`, `events TASK_ID --after CURSOR --wait-seconds 10`, and `watchdog --once`. Repeated native turn IDs replay the saved result, without repeating CLI work. An active or unresolved turn is rejected instead of relaunched.

## Other CLIs

Built-in model aliases are `agentrelay-claude` and `agentrelay-codex`. Additional runtimes use the documented JSONL adapter in `AGENTRELAY_HOME/runtimes.toml`, a model alias `agentrelay-RUNTIME`, and a matching native role/catalog entry. Never present arbitrary interactive CLI output as a supported adapter. Each CLI uses its own existing authentication.

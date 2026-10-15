# Verification

Tests run against the official desktop-bundled `codex-cli 0.162.0-alpha.17.2` on macOS. The executable was not patched or rebuilt. Live probes use temporary workspaces and isolated Codex homes, except explicitly identified desktop-backend checks. Existing CLI authentication remains with each CLI.

## Automated coverage

Ordinary tests cover runtime argument construction, structured event translation, bounded and cancellation-safe framing, terminal errors, stale generations, duplicate claims, queued messages, capacity, permissions, process cancellation, timeouts, recovery, installation protection and owned legacy-hook removal.

Provider tests additionally cover root/child/runtime binding, prompt snapshots, immutable access settings, native turn replay, configuration preservation, authentication rejection and **actual HTTP-disconnect cleanup of an external process**.

## Native acceptance suite

| Test | Verified behavior |
| --- | --- |
| Executable role field | Official role loader rejects `command`; no executable is launched |
| Child provider override | Child retains the inherited parent provider |
| Direct child request | Actual Claude Code executes the fixture through the Rust runner |
| Opaque task boundary | Encrypted content is not reinterpreted as a plaintext task |
| Explicit handoff | A root/path-bound plaintext task executes despite opaque native input |
| Wrong root | Another root cannot claim the handoff |
| Native follow-up | Same native child, relay task and Claude session; generation advances |
| Production provider task visibility | Child history contains full assignment commentary and final assignment/result |
| Production provider follow-up | Same task/session through the shipped provider implementation |
| Real OpenAI parent | Authenticated parent creates one native CLI child and reports its actual result |
| Haiku 5.5 high native profile | Real Haiku selection, same-session follow-up, catalog display name, native model/effort and execution settings in history |

The fixture file contains 19, 23 and 31. Actual Claude reads it and returns `NATIVE_CLAUDE_SUM=73`; the follow-up recalls the sum and returns `NATIVE_CLAUDE_DOUBLED=146`. Tests check real Read activity, native thread source/parent links, native completion events and session identity. The live parent probe checks that all forwarded OpenAI requests originate from the root and that no extra native agent is inserted.

The real-parent probe uses a controlled routing fixture for observing parent requests and the production provider for its CLI child. This distinction matters: an isolated official app-server is not by itself a rendered desktop-app test.

A subsequent installed-provider acceptance also passed using the normal user configuration and globally installed service, with both parent and child routed through the production provider. The desktop app's own `read_thread` capability returned the completed native child containing the exact assignment and real result, and `navigate_to_codex_page` successfully opened it in the app. This confirms app-accessible persistence and navigation, while still leaving rendered layout unverified.

The current suite includes eleven native tests. The selection extension used Claude Code 2.1.300 and adds twenty-five ordinary checks in total, including native profile installation and mismatched picker-setting rejection. Exact aggregate checks and hashes belong in [sanitized results](../evidence/native-provider-results.json). Full transcripts and local identifiers remain excluded from version control.

## Desktop verification boundary

Native thread history is confirmed through the official app-server protocol. The current agent-detail implementation contains the task receipt and result in that history. Screenshots and rendered layout are not verified: the computer-use host denied access to the Codex application. No alternate capture method was used to circumvent that restriction.

HTTP response disconnect is tested to interrupt and reap the actual worker. The desktop Stop button's event timing remains unverified. External tool calls are not claimed as native Codex tool cards.

## Reproduce

```bash
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
AGENTRELAY_CODEX_TEST_BIN=/absolute/path/to/official/codex \
  cargo test --test native_provider --locked -- --ignored --test-threads=1
```

The ignored suite uses real external model usage. Set `AGENTRELAY_REAL_PARENT_MODEL` only to an available model for the existing Codex account. `AGENTRELAY_MODEL_METADATA` can select a current models cache; `AGENTRELAY_TEST_EVIDENCE` chooses a private output directory.

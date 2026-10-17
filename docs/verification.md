# Verification

Tests run against the official desktop-bundled Codex executable on macOS. Its exact version/hash are recorded in the sanitized results for each release. The executable was not patched or rebuilt. Live probes use temporary workspaces and isolated Codex homes, except explicitly identified desktop-backend checks. Existing CLI authentication remains with each CLI.

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
| Hidden worker models | Default model list excludes worker aliases/profiles; explicit hidden lookup retains labels without promotional/default flags; native spawn and follow-up still work |
| Opus 5.5 high native profile | Real Opus execution and same-session follow-up, native profile model/effort and observed model agree |

The fixture file contains 19, 23 and 31. Actual Claude reads it and returns `NATIVE_CLAUDE_SUM=73`; the follow-up recalls the sum and returns `NATIVE_CLAUDE_DOUBLED=146`. Tests check real Read activity, native thread source/parent links, native completion events and session identity. The live parent probe checks that all forwarded OpenAI requests originate from the root and that no extra native agent is inserted.

The real-parent probe uses a controlled routing fixture for observing parent requests and the production provider for its CLI child. This distinction matters: an isolated official app-server is not by itself a rendered desktop-app test.

A subsequent installed-provider acceptance also passed using the normal user configuration and globally installed service, with both parent and child routed through the production provider. The desktop app's own `read_thread` capability returned the completed native child containing the exact assignment and real result, and `navigate_to_codex_page` successfully opened it in the app. This confirms app-accessible persistence and navigation, while still leaving rendered layout unverified.

The 0.3.2 macOS suite includes thirteen native tests and twenty-eight ordinary checks. New checks reject explicit selections without a native profile, preserve legacy generic worker bindings through profile installation and session resume, and prevent upgrades from restarting an active worker by default. The HTTP test runs a quiet CLI for 17 seconds with a 16-second per-chunk idle deadline: a real `response.in_progress` arrives after 15 seconds and the task completes without fabricated text or token usage. Disconnect cleanup remains covered. Exact aggregate checks and hashes belong in [sanitized results](../evidence/native-provider-results.json). Full transcripts and local identifiers remain excluded from version control.

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

## macOS provider registration recovery (0.3.3)

Regression checks cover the difference between an exited process and an entirely unloaded launchd registration. Unit fixtures verify missing-registration bootstrap, concurrent recovery, bootstrap failure reporting and leaving healthy services untouched. HTTP checks validate the loopback service identity without authentication, refuse nonlocal addresses, and report an unrelated listener.

The installed recovery path is tested by unloading the real provider twice while leaving the independent guardian registered. Acceptance requires both automatic restorations without manual bootstrap, an unchanged healthy provider PID across a guardian check, a responding 0.3.3 `/version`, and a successful authenticated ordinary OpenAI inference through the restored proxy. This operational test does not submit work to or rewrite existing user chats. Evidence is published separately from the native-model acceptance captured on 0.3.2.

Local 0.3.3 acceptance passed 35 ordinary tests, formatting, clippy with warnings denied, the release build and the packaged Apple Silicon binary/checksum. The two real automatic recoveries took 2.03 and 10.10 seconds. Authenticated OpenAI inference through the restored provider returned the exact expected response. See [service recovery results](../evidence/service-recovery-results.json).

## Registered provider hangs (0.3.4)

An installed provider was sampled while registered and listening but unable to answer `/version`. Four asynchronous worker threads were waiting in SQLite connection open/close mutex paths, including `findReusableFd` and `unixLock`. A symbol-bearing reconstruction had identical executable text/address bytes to the installed binary, allowing the stack addresses to be resolved without guessing from a different build. Raw stacks and task/session identifiers remain private.

The regression suite verifies process-local serialization across 400 nested transactions on eight threads, same-thread nested connection lifetime cleanup, and HTTP health/auth rejection while database work is deliberately blocked. A separately opted-in macOS test registers a uniquely named fixture LaunchAgent, freezes only that process with `SIGSTOP`, verifies the first two checks leave its PID intact, and verifies the third check produces a new healthy process. Recovery took 28.19 seconds. The fixture registration is removed on exit, including assertion failure; the user's running provider is not used for this fault test.

All 40 ordinary macOS tests passed. Startup readiness is tested with a deliberately delayed listener; a cold start is not treated as a hang. The real Opus 5.5 high native-profile test also passed initial execution and follow-up with the same native and external sessions. The full historical native matrix was not rerun for this patch. See [provider availability results](../evidence/provider-availability-results.json).

## Managed built-in OpenAI bridge (0.4.1)

The [alternative transport](builtin-openai-bridge.md) keeps native provider ID `openai`, authenticates the actual macOS TCP peer and adds the private backend header. All 55 ordinary checks in the release profile and all 22 isolated native cases passed, including the 13 historical cases. A separately opted-in installed-service case also passed: a real OpenAI parent created a Haiku 5.5 high child and resumed its same external session. The app's own `read_thread` returned both completed native turns with their assignment/result, and navigation to the acceptance chat succeeded.

The isolated launchd fixture recovered both listeners after three spaced failures of its frozen process (28.20 seconds), then recovered from a fully unloaded registration. The installed 0.4.1 service was also unloaded while no external workers were active; the periodic guardian restored both listeners in 8.80 seconds, sharing a new PID. A healthy guardian check preserved the original PID. Migration persisted a legacy chat, restarted the official app-server with the new global default, and resumed the original child/task/session; new threads selected `openai`. Parent compaction and normal account/rate-limit APIs passed through the integrated service.

The user installation and configuration were activated locally. Formatting, clippy with warnings denied and the release build passed. Exact aggregate scope is recorded with the [sanitized results](../evidence/openai-bridge-results.json). Computer Use explicitly denies `com.openai.codex` for safety reasons, so no rendered screenshot is claimed or captured through another path. Forced in-flight restart may interrupt work; recovery never automatically repeats a task.

Version 0.4.1 also verifies multiple exact-path native executable admission, rejection of an identical unlisted copy, legacy manifests, optional CLI removal and mutation-free rejection of unresolved PATH launchers. Live Homebrew Codex 0.157.1 ran GPT-5.6 Sol through the installed bridge; the bundled CLI ran GPT-6.1 Sol. The AgentRelay `codex` adapter completed an initial turn and a same-session recall turn. The older CLI rejected GPT-6.1 Sol both with and without the bridge; that account/model compatibility error was not a local admission failure.

A local debug rerun failed to observe a model from its substitution fixture. Subsequent debug executable launches stalled before `main` in macOS `dyld`, with valid on-disk signatures; private process samples are retained. The complete release-profile suite and the fresh GitHub runner checks are separate verification paths. This loader observation does not establish the cause of the earlier fixture failure.

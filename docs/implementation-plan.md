# Implementation and verification plan

## Delivered

1. Inspect native spawn, role loading, provider inheritance and history projection in the upstream source.
2. Verify extension boundaries using the unmodified official desktop-bundled executable. Test executable-role rejection, ignored provider overrides and encrypted-input handling.
3. Implement Rust CLI adapters, private SQLite registries, process ownership, timeouts, cancellation, session resume and idempotent submissions.
4. Implement the loopback Responses provider with fixed HTTPS parent forwarding and CLI model aliases.
5. Add explicit task preparation, root/path/child/turn bindings, immutable access settings, duplicate rejection and terminal replay.
6. Record complete assignments and results in native child history, including the final message retained by compact history projections.
7. Replace the former integration instructions and role with the provider path. Remove the execution path that needed an intermediate Codex model. Retain only compatibility readers and owned-file migration needed to preserve user data.
8. Provide protected project/user installation, augmented model catalog configuration and macOS LaunchAgent setup.
9. Exercise real Claude work, native follow-up, a real authenticated OpenAI parent, HTTP cancellation and failure boundaries.
10. Publish only source, templates, documentation and sanitized aggregate evidence after checking the exact Git payload for sensitive data.
11. Support explicit external model/effort selection with normalization, version/capability checks, resume preservation and observed-model validation.
12. Register immutable profiles through the native model catalog and agent roles; verify display names, native reasoning settings and execution alignment through the official app-server.
13. Persist native selections per worker, migrate legacy bindings before adding profiles, and reject explicit new selections without a matching native profile.
14. Install common Claude high-effort profiles through an upgrade command that waits for idle workers by default; verify active-session recovery during an explicitly authorized local restart.
15. Emit periodic SSE progress events during quiet CLI execution to prevent Codex's idle-stream timeout while preserving disconnect cancellation.

16. Recover missing macOS provider registrations with an independent periodic guardian; write service files before unload and verify a real unload/recovery plus authenticated parent inference.

## Release checks

Run formatting, all ordinary tests and clippy. Explicitly run the ignored native acceptance suite only when authorized to consume external model usage. Keep raw transcripts, account configuration and session registries private. Validate the installed skill with the system skill validator and discovery from unrelated projects.

Start a new official-app conversation using the configured provider and confirm that its parent uses a normal OpenAI model and its child uses a CLI alias. Inspect root/child relationship, native activity, complete assigned task and actual external answer. Navigation can expose the native thread in the app; do not substitute API history for visual acceptance.

## Remaining acceptance and operational work

- Rendered desktop verification of the agent panel and compact/full detail layouts. Screen capture is unavailable when the host denies access to the Codex window.
- Desktop Stop-button behavior in the active app host; HTTP-disconnect cleanup is already covered by a real process test.
- Broader Codex-version and CLI-version compatibility matrix, including Linux live acceptance.
- Live acceptance for additional configured CLIs; Claude Code is the tested external runtime. Custom JSONL drivers have deterministic protocol tests.
- Automated catalog refresh after upstream model changes. Current refresh is an explicit configure operation.
- Strong OS confinement for deployments that require it, plus log rotation, retention policy, packaging and signed releases.

These items do not require modifying the official Codex executable. They must be completed before claiming full visual/lifecycle acceptance or hardened multi-user deployment.

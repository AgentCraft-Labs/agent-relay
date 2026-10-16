AgentRelay is a Rust command-line companion and local background service for the unchanged official Codex app.

Version 0.3.3 prevents a missing local provider registration from leaving Codex chats disconnected indefinitely. Ordinary OpenAI chats configured with AgentRelay also depend on its local listener. The existing provider LaunchAgent restarts exited processes, but cannot restart a job removed with `launchctl bootout`. A second independent guardian now checks every 10 seconds and restores the registration. Healthy services and workers are left intact; no model inference is used by recovery.

Service installation writes both service files before stopping the provider, registers the guardian first, and checks readiness after startup. Use `codex service-ensure` for immediate manual recovery. To intentionally stop the integration, unload the guardian before the provider and restore normal provider settings before disabling permanently. Existing chats can retain their provider.

Update with `./agentrelay codex upgrade --wait-idle`. The upgrade waits for active external turns and preserves task history, session bindings and the native model profiles introduced in 0.3.2. Codex itself is not rebuilt or patched. Recovery depends on macOS scheduling, valid files and a free port; it does not guarantee upstream network availability or kill a hung running worker.

Download the `macos-arm64` archive for Apple Silicon or `macos-x86_64` for Intel. Verify its `.sha256` file before extraction. Each archive contains the binary, README, license, documentation and sanitized acceptance results. See the README for user installation, profile configuration and background service startup.

The binaries are built and tested on native GitHub-hosted macOS runners. They are not Developer ID signed or notarized. This release supplies a terminal/background companion, without a separate graphical window. Claude Code, Codex and any other agent CLIs are installed and authenticated separately.

Rendered desktop layout and the Codex Stop button remain outside the automated acceptance claims. Applied CLI effort is not independently reported by the provider; requested effort and observed model are recorded separately.

AgentRelay is a Rust command-line companion and local background service for the unchanged official Codex app.

Version 0.3.4 fixes a second availability failure: the provider could remain registered and listening while SQLite connection open/close mutex waits blocked the asynchronous HTTP executor. Connection lifetimes are now serialized with same-thread nesting, and database/native-worker operations execute outside the parent HTTP executor. Health and ordinary OpenAI requests remain serviceable during database contention.

The macOS guardian now handles registered hangs as well as missing registrations. It requires three failures of the same verified PID/start time, at least 10 seconds apart, rechecks readiness, validates the executable path and restarts only that provider process. A healthy process is never restarted. Concurrent checks are excluded; failures reset after success or a process change. Forced recovery may interrupt active external turns; persisted CLI session IDs and assignments remain available for explicit follow-up, without automatic task repetition.

Update with `./agentrelay codex upgrade --wait-idle`. The guardian uses no model inference. Codex itself is not rebuilt or patched. Recovery depends on macOS scheduling, valid service files and a free port, and does not guarantee upstream availability.

Download the `macos-arm64` archive for Apple Silicon or `macos-x86_64` for Intel. Verify its `.sha256` file before extraction. Each archive contains the binary, README, license, documentation and sanitized acceptance results. See the README for user installation, profile configuration and background service startup.

The binaries are built and tested on native GitHub-hosted macOS runners. They are not Developer ID signed or notarized. This release supplies a terminal/background companion, without a separate graphical window. Claude Code, Codex and any other agent CLIs are installed and authenticated separately.

Rendered desktop layout and the Codex Stop button remain outside the automated acceptance claims. Applied CLI effort is not independently reported by the provider; requested effort and observed model are recorded separately.

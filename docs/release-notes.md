AgentRelay is a Rust command-line companion and local background service for the unchanged official Codex app.

This patch release aligns explicit external model/effort choices with native agent profiles. The macOS upgrade command installs Haiku 5.5 high, Sonnet 5.5 high and Opus 5.5 high profiles. Missing profiles fail before delegation instead of showing a generic low/Light label. Existing workers retain frozen native bindings and resumable CLI sessions when profiles are added. Worker entries remain hidden from the main model picker.

Quiet CLI execution now emits a real SSE progress event every 15 seconds to avoid Codex's idle-stream timeout, without fabricating assistant text or token usage. Initial receipts explicitly describe a snapshot; final results retain observed model details.

Update with `./agentrelay codex upgrade --wait-idle`, then reload Codex before creating agents with the new profiles. The upgrade waits for active external turns and verifies the running provider version after restart. Existing agents keep their historical native labels; create a new agent to use a dedicated profile. Codex itself is not rebuilt or patched.

Download the `macos-arm64` archive for Apple Silicon or `macos-x86_64` for Intel. Verify its `.sha256` file before extraction. Each archive contains the binary, README, license, documentation and sanitized acceptance results. See the README for user installation, profile configuration and background service startup.

The binaries are built and tested on native GitHub-hosted macOS runners. They are not Developer ID signed or notarized. This release supplies a terminal/background companion, without a separate graphical window. Claude Code, Codex and any other agent CLIs are installed and authenticated separately.

Rendered desktop layout and the Codex Stop button remain outside the automated acceptance claims. Applied CLI effort is not independently reported by the provider; requested effort and observed model are recorded separately.

AgentRelay is a Rust command-line companion and local background service for the unchanged official Codex app.

This patch release hides AgentRelay runtime aliases and model profiles from the main Codex model picker and removes inherited OpenAI new-model announcements and upgrade recommendations. Explicit native-agent delegation, profile metadata and same-session follow-ups remain available. Existing OpenAI model entries are preserved.

After updating, rerun `agentrelay codex configure`, then fully quit and reopen Codex to reload the catalog. The fix does not require rebuilding or patching Codex. External model/effort selection and native child history continue to work without an intermediate Codex model.

Download the `macos-arm64` archive for Apple Silicon or `macos-x86_64` for Intel. Verify its `.sha256` file before extraction. Each archive contains the binary, README, license, documentation and sanitized acceptance results. See the README for user installation, profile configuration and background service startup.

The binaries are built and tested on native GitHub-hosted macOS runners. They are not Developer ID signed or notarized. This release supplies a terminal/background companion, without a separate graphical window. Claude Code, Codex and any other agent CLIs are installed and authenticated separately.

Rendered desktop layout and the Codex Stop button remain outside the automated acceptance claims. Applied CLI effort is not independently reported by the provider; requested effort and observed model are recorded separately.

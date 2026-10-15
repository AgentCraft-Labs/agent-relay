AgentRelay is a Rust command-line companion and local background service for the unchanged official Codex app.

This release adds explicit external model/effort selection and immutable native model profiles, including Claude Haiku 5.5 high. Native child history records the assigned task, selected/observed model and actual CLI result. No intermediate Codex model executes the delegated task.

Download the `macos-arm64` archive for Apple Silicon or `macos-x86_64` for Intel. Verify its `.sha256` file before extraction. Each archive contains the binary, README, license, documentation and sanitized acceptance results. See the README for user installation, profile configuration and background service startup.

The binaries are built and tested on native GitHub-hosted macOS runners. They are not Developer ID signed or notarized. This release supplies a terminal/background companion, without a separate graphical window. Claude Code, Codex and any other agent CLIs are installed and authenticated separately.

Rendered desktop layout and the Codex Stop button remain outside the automated acceptance claims. Applied CLI effort is not independently reported by the provider; requested effort and observed model are recorded separately.

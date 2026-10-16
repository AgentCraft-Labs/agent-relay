# macOS release automation

`.github/workflows/release-macos.yml` builds and publishes the Rust companion when a version tag such as `v0.3.2` is pushed. It also supports manual dispatch with an existing version tag. The tag's Cargo package version must match, and every build job checks out the same resolved commit.

The matrix uses native Apple Silicon (`macos-15`, `aarch64-apple-darwin`) and Intel (`macos-15-intel`, `x86_64-apple-darwin`) runners. Each job runs formatting, ordinary tests and clippy, builds with Cargo.lock, packages its exact binary, checks its Mach-O architecture, and smoke-tests the executable extracted from the resulting archive. Live model tests remain opt-in and do not run in release CI.

The release contains:

- `agentrelay-vVERSION-macos-arm64.tar.gz`
- `agentrelay-vVERSION-macos-x86_64.tar.gz`
- A `.sha256` file for each archive
- Combined `SHA256SUMS`

Archives include only the binary, README, license, documentation and sanitized aggregate evidence. Prompts, sessions, credentials, local configuration, private transcripts and registries are not copied. `scripts/package-macos.sh` can reproduce the packaging step on macOS. Build deployment target is 11.0; native CI acceptance runs on macOS 15, so older macOS versions are not independently validated.

Build jobs have read-only repository permissions. Only the final publish job receives `contents: write` through the standard ephemeral GitHub token. Actions are pinned to commit IDs. The publish job verifies checksums, uploads to a draft and publishes only after both builds succeed. Published releases are immutable in this workflow; use a new version tag for changes. A failed draft may be retried before publication.

## Publish a new version

Update Cargo.toml/Cargo.lock, the documented download version and release notes, then commit and push. Create the matching tag:

```bash
git tag v0.3.2
git push origin v0.3.2
```

The workflow creates the release; no local binary upload is required. Alternatively run the workflow manually with the same tag after it exists. Follow the Actions run and confirm the release assets/checksums before distributing the download link.

## Installation and updates

Download the archive for the machine's architecture, verify its SHA-256 file, and extract it. Run `./agentrelay codex install --user`, then use the copied user executable to register any profiles, configure Codex and install the LaunchAgent. The [README](../readme.md#download-and-run-on-macos) contains complete commands. No Rust toolchain is needed by users.

For an update, run the new archive's executable with `codex upgrade --wait-idle`. It waits for active external turns, migrates native bindings before adding profiles, installs the user integration/default profiles, configures the catalog and restarts the owned service. Without `--wait-idle`, an active worker causes an explicit error before installation changes. User-edited files are protected and registries are preserved. New Codex conversations reload the configured provider/roles; restart the app if its catalog remains cached. The `/version` endpoint reports the running provider version and heartbeat interval.

This distribution provides the command-line companion/background service, without a separate GUI or a modification to Codex. Apple Developer ID signing and notarization are not configured. Users approve any macOS execution prompt through normal system controls; no protection bypass is part of the installer.

Version 0.3.3 also installs an independent macOS guardian before restarting the provider. The guardian restores missing launchd registrations every 10 seconds; a healthy provider is not restarted. `codex service-ensure` performs the same recovery immediately. See the README before intentionally disabling either service.

Version 0.3.4 extends the guardian to verified process hangs and isolates serialized database/native-worker execution from parent HTTP handling. Recovery preserves persistent sessions but can interrupt a turn; follow-up remains explicit.

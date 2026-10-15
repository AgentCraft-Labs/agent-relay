#!/usr/bin/env bash
set -euo pipefail

relay_version=${1:?Pass the release version}
relay_arch=${2:?Pass arm64 or x86_64}
relay_binary=${3:?Pass the compiled binary}
relay_out=${4:-dist}
[[ "$relay_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]
[[ "$(uname -s)" == Darwin ]]
case "$relay_arch" in arm64|x86_64) ;; *) exit 1 ;; esac
[[ "$(lipo -archs "$relay_binary")" == "$relay_arch" ]]
[[ "$("$relay_binary" --version)" == "agentrelay $relay_version" ]]

relay_stage=$(mktemp -d "${TMPDIR:-/tmp}/agentrelay-package.XXXXXX")
trap 'rm -rf "$relay_stage"' EXIT
relay_name="agentrelay-v${relay_version}-macos-${relay_arch}"
mkdir -p "$relay_stage/$relay_name/evidence" "$relay_out"
cp "$relay_binary" "$relay_stage/$relay_name/agentrelay"
chmod 755 "$relay_stage/$relay_name/agentrelay"
cp readme.md LICENSE "$relay_stage/$relay_name/"
cp -R docs "$relay_stage/$relay_name/docs"
cp evidence/native-provider-results.json "$relay_stage/$relay_name/evidence/"
COPYFILE_DISABLE=1 tar -C "$relay_stage" -czf "$relay_out/$relay_name.tar.gz" "$relay_name"
(cd "$relay_out" && shasum -a 256 "$relay_name.tar.gz" > "$relay_name.tar.gz.sha256" && shasum -a 256 -c "$relay_name.tar.gz.sha256")

# Validate what users will extract, not only the build directory executable.
mkdir "$relay_stage/extracted"
tar -C "$relay_stage/extracted" -xzf "$relay_out/$relay_name.tar.gz"
[[ "$("$relay_stage/extracted/$relay_name/agentrelay" --version)" == "agentrelay $relay_version" ]]
"$relay_stage/extracted/$relay_name/agentrelay" --help >/dev/null

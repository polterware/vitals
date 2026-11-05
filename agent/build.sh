#!/usr/bin/env bash
# Builds the read-only vitals agent for every host Vitals connects to, into agent/dist/
# (set VITALS_AGENT_OUT to write elsewhere). Linux (static musl) and Windows go through
# cargo-zigbuild (`brew install zig` and `cargo install cargo-zigbuild`); macOS builds
# natively, so it needs a Mac. The `agent` profile builds for size.
set -euo pipefail

cd "$(dirname "$0")"
out_dir="$(mkdir -p "${VITALS_AGENT_OUT:-dist}" && cd "${VITALS_AGENT_OUT:-dist}" && pwd)"
cd ..

# name:rust target
cross=(
  "linux-amd64:x86_64-unknown-linux-musl"
  "linux-arm64:aarch64-unknown-linux-musl"
  "windows-amd64.exe:x86_64-pc-windows-gnu"
  "windows-arm64.exe:aarch64-pc-windows-gnullvm"
)
native=(
  "darwin-amd64:x86_64-apple-darwin"
  "darwin-arm64:aarch64-apple-darwin"
)

build() {
  local tool="$1" name="$2" target="$3"
  rustup target add "${target}" >/dev/null 2>&1
  cargo ${tool} --quiet --locked -p vitals-agent --profile agent --target "${target}"
  local exe="vitals-agent"
  [[ "${name}" == *.exe ]] && exe="vitals-agent.exe"
  cp "target/${target}/agent/${exe}" "${out_dir}/vitals-agent-${name}"
}

if command -v cargo-zigbuild >/dev/null && command -v zig >/dev/null; then
  for entry in "${cross[@]}"; do
    build zigbuild "${entry%%:*}" "${entry#*:}"
  done
else
  echo "build.sh: skipping linux and windows agents (needs zig and cargo-zigbuild)" >&2
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
  for entry in "${native[@]}"; do
    build build "${entry%%:*}" "${entry#*:}"
  done
else
  echo "build.sh: skipping darwin agents (they build on a macOS host)" >&2
fi

ls -l "${out_dir}"

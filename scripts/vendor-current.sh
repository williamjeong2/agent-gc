#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

cargo build --release

platform="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m)"

# Normalize uname values across macOS/Linux/Windows(Git Bash/MSYS)
case "${platform}" in
  mingw*|msys*|cygwin*) platform="windows" ;;
esac

case "${platform}-${arch}" in
  darwin-arm64) name="agent-gc-darwin-arm64" ;;
  darwin-x86_64) name="agent-gc-darwin-x64" ;;
  linux-x86_64) name="agent-gc-linux-x64" ;;
  linux-aarch64|linux-arm64) name="agent-gc-linux-arm64" ;;
  windows-x86_64|windows-amd64) name="agent-gc-win32-x64.exe" ;;
  windows-aarch64|windows-arm64) name="agent-gc-win32-arm64.exe" ;;
  *)
    echo "unsupported platform: ${platform}-${arch}" >&2
    echo "copy target/release/agent-gc manually into vendor/agent-gc-<platform>" >&2
    exit 1
    ;;
esac

src="target/release/agent-gc"
if [[ "${name}" == *.exe ]]; then
  if [[ -f "target/release/agent-gc.exe" ]]; then
    src="target/release/agent-gc.exe"
  fi
fi

if [[ ! -f "${src}" ]]; then
  echo "missing release binary: ${src}" >&2
  exit 1
fi

mkdir -p vendor
cp "${src}" "vendor/${name}"
chmod 755 "vendor/${name}" 2>/dev/null || true
echo "wrote vendor/${name}"

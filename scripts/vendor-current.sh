#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

cargo build --release

platform="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m)"

case "${platform}-${arch}" in
  darwin-arm64) name="agent-gc-darwin-arm64" ;;
  darwin-x86_64) name="agent-gc-darwin-x64" ;;
  linux-x86_64) name="agent-gc-linux-x64" ;;
  linux-aarch64|linux-arm64) name="agent-gc-linux-arm64" ;;
  *)
    echo "unsupported platform: ${platform}-${arch}" >&2
    exit 1
    ;;
esac

mkdir -p vendor
cp "target/release/agent-gc" "vendor/${name}"
chmod 755 "vendor/${name}"
echo "wrote vendor/${name}"

#!/usr/bin/env bash
# 실제 바이너리(또는 npm 래퍼)로 scan → dry-run → 실제 삭제까지 끝까지 돌려 본다.
# usage: scripts/smoke-test.sh <command...>
#   scripts/smoke-test.sh target/release/agent-gc
#   scripts/smoke-test.sh node bin/agent-gc.js
#   scripts/smoke-test.sh npx --no-install agent-gc
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: $0 <agent-gc command...>" >&2
  exit 2
fi

cmd=("$@")
fixture="$(mktemp -d)"
trap 'rm -rf "${fixture}"' EXIT

project="${fixture}/app"
mkdir -p "${project}/node_modules/pkg/lib"
echo '{"name":"app"}' > "${project}/package.json"
echo 'module.exports = 1;' > "${project}/node_modules/pkg/index.js"
echo 'x' > "${project}/node_modules/pkg/lib/readonly.js"
chmod a-w "${project}/node_modules/pkg/lib/readonly.js"

fail() {
  echo "smoke test failed: $*" >&2
  exit 1
}

echo "==> version"
"${cmd[@]}" --version

echo "==> scan --json"
scan_output="$("${cmd[@]}" scan --json "${fixture}")"
echo "${scan_output}"
[[ "${scan_output}" == *node_modules* ]] || fail "scan did not report node_modules"
[[ "${scan_output}" == *'"risk_level": "SAFE"'* ]] || fail "scan did not classify node_modules as SAFE"

echo "==> clean --dry-run"
"${cmd[@]}" clean --dry-run --preset safe "${fixture}"
[[ -d "${project}/node_modules" ]] || fail "dry-run deleted files"

echo "==> clean without --yes on non-TTY must be refused"
if "${cmd[@]}" clean --preset safe "${fixture}" < /dev/null; then
  fail "non-interactive clean without --yes was not refused"
fi
[[ -d "${project}/node_modules" ]] || fail "refused clean deleted files"

echo "==> clean --yes"
"${cmd[@]}" clean --preset safe --yes "${fixture}"
[[ ! -e "${project}/node_modules" ]] || fail "node_modules still exists after clean"
[[ -f "${project}/package.json" ]] || fail "clean removed project files"

echo "smoke test passed"

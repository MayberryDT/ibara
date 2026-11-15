#!/usr/bin/env bash
# Runs every check of the agent friction changes (November 2026) and keeps
# their output in ARTIFACT_DIR: the controller and Cua tests, the operator
# and PKGBUILD tests, journal retention with its time limits, operator status
# through real daemons with and without a build identity, and the page reader
# in headless Chromium.
#
#   scripts/check-agent-friction.sh ARTIFACT_DIR
#
# Needs what `cargo test` needs (docs/development.md), Node 22 or later and
# Chromium. Exits non-zero when any check fails; summary.json says which.
set -uo pipefail
[[ $# -eq 1 ]] || { echo "usage: $0 ARTIFACT_DIR" >&2; exit 2; }
core=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
mkdir -p "$1" && out=$(cd -- "$1" && pwd -P)
cd "$core" || exit 2
commit=$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
steps=()
run() {
  local name=$1 code
  shift
  echo "== $name: $*" >&2
  "$@" >"$out/$name.log" 2>&1
  code=$?
  echo "   exit $code ($out/$name.log)" >&2
  steps+=("{\"name\":\"$name\",\"exit\":$code,\"log\":\"$name.log\"}")
}
run controller cargo test --locked --lib -- controller::tests::friction controller::tests::reconnect desktop::cua::tests::an_element_placed_outside
run operator-and-packaging cargo test --locked --test operator --test build_identity
# With their time limits, which assume a computer not busy with other work.
run retention env IBARA_E2E_TIMING=1 IBARA_E2E_ARTIFACTS="$out/retention" cargo test --locked --test server_entries -- retention ended_task_attention --test-threads=1
run status-without-build env -u IBARA_BUILD IBARA_E2E_EVIDENCE="$out/status-without-build" cargo test --locked --test everyday -- a_computer_is_looked_after
run status-with-build env IBARA_BUILD="$commit-check" IBARA_E2E_EVIDENCE="$out/status-with-build" cargo test --locked --test everyday -- a_computer_is_looked_after
run page-reader node e2e/browser/reader.mjs "$out"
failed=0
for step in "${steps[@]}"; do [[ $step == *'"exit":0,'* ]] || failed=1; done
printf '{"commit":"%s","at":"%s","host":"%s","ok":%s,"steps":[%s]}\n' "$commit" "$(date -u +%FT%TZ)" "$(hostname)" \
  "$([[ $failed == 0 ]] && echo true || echo false)" "$(IFS=,; echo "${steps[*]}")" >"$out/summary.json"
cat "$out/summary.json"
exit $failed

#!/bin/bash
# Send the real `claude` on one short visit against the mock platform: the
# usage meter (trust question included, since the meter folder is new), one
# world turn, homecoming, and the visit's end. This DOES run a few small model
# turns on your Claude subscription.
#
#   dev/visit-check.sh [port]
#
# Like live-check.sh it touches nothing outside its scratch directory, apart
# from Claude recording that it trusts the scratch meter folder.

set -euo pipefail

PORT="${1:-8802}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$(dirname "$HERE")"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/daycare-visit-XXXXXX")"
BIN="$CRATE/target/debug/daycare-runner"
CLAUDE_BIN="${CLAUDE_BIN:-claude}"

cleanup() {
  [ -f "$SCRATCH/server.pid" ] && kill "$(cat "$SCRATCH/server.pid")" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> building"
(cd "$CRATE" && cargo build --offline)

echo "==> starting mock platform on 127.0.0.1:$PORT"
DAYCARE_MOCK_VISIT=1 python3 "$HERE/mock-platform.py" "$PORT" "$SCRATCH/state.json" > "$SCRATCH/server.log" 2>&1 &
echo $! > "$SCRATCH/server.pid"
sleep 1

export DAYCARE_HOME="$SCRATCH/home"
export DAYCARE_TOKEN_FILE="$SCRATCH/tokens.json"

echo "==> enroll"
"$BIN" enroll --url "http://127.0.0.1:$PORT" --code VISIT-TEST --device-name visit-check

echo "==> usage meter"
"$BIN" usage --claude-bin "$CLAUDE_BIN"

echo "==> one-turn visit"
"$BIN" visit start --foreground --turns 1 --interval 1 --weekly-percent 2 \
  --instructions "Look around and say hello." --claude-bin "$CLAUDE_BIN" --json \
  | tee "$SCRATCH/visit.json"

echo "==> what the platform saw"
python3 - "$SCRATCH/state.json" "$SCRATCH/visit.json" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
visit = json.loads(open(sys.argv[2]).read().strip().splitlines()[-1])
print("opened:", state["visits"])
print("turns:", [(c["path"], c["report"]["status"]) for c in state["completions"]])
print("ended:", state["visit_ends"])
print("delivered:", state["visit_reports"])
print("weekly usage:", visit.get("weekly_usage"))

assert visit["ok"] is True, visit
assert state["visits"] and state["visits"][0].get("budget_usage_pct") == 2.0
assert any(c["report"]["status"] == "completed" for c in state["completions"])
assert state["visit_ends"], "the visit end was never reported"
assert "report" in state["visit_reports"], "the day report was never delivered"
print("\nOK: metered, one refereed turn, home, and ended.")
PY
echo "(scratch dir: $SCRATCH)"

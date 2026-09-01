#!/bin/bash
# Prove the whole companion loop against a mock platform, using the real
# `claude` on this machine. This DOES run two small model turns on your Claude
# subscription — it is the only way to prove a turn actually works.
#
#   tools/daycare-runner/dev/live-check.sh [port]
#
# It touches nothing outside its own scratch directory: DAYCARE_HOME and the
# token file are both under /tmp, so your real ~/.claude-daycare and your
# keychain are untouched.

set -euo pipefail

PORT="${1:-8801}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$(dirname "$HERE")"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/daycare-live-XXXXXX")"
BIN="$CRATE/target/debug/daycare-runner"

cleanup() {
  [ -f "$SCRATCH/server.pid" ] && kill "$(cat "$SCRATCH/server.pid")" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> building"
(cd "$CRATE" && cargo build --offline)

echo "==> starting mock platform on 127.0.0.1:$PORT"
python3 "$HERE/mock-platform.py" "$PORT" "$SCRATCH/state.json" > "$SCRATCH/server.log" 2>&1 &
echo $! > "$SCRATCH/server.pid"
sleep 1

export DAYCARE_HOME="$SCRATCH/home"
export DAYCARE_TOKEN_FILE="$SCRATCH/tokens.json"

echo "==> enroll"
"$BIN" enroll --url "http://127.0.0.1:$PORT" --code LIVE-TEST --device-name live-check

echo "==> turn 1 (new session)"
"$BIN" run-once --timeout 240

echo "==> turn 2 (resumes the same Claude)"
"$BIN" run-once --timeout 240

echo "==> queue empty"
"$BIN" run-once

echo "==> status"
"$BIN" status

echo "==> what the character did"
python3 - "$SCRATCH/state.json" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
print("tool calls:", [c["name"] for c in state["tool_calls"]])
for action in state["actions"]:
    print("action:", action)
for index, memory in enumerate(state["memories"], 1):
    print(f"memory {index}:", memory["text"])
sessions = {c["report"]["claude_session_id"] for c in state["completions"]}
print("sessions used:", sessions)

assert len(sessions) == 1, "turn 2 did not resume turn 1's session"
assert len(state["memories"]) == 2, "both turns should have saved a memory"
assert all(c["report"]["status"] == "completed" for c in state["completions"])

# Contract rehearsal, not just transport: the model must have used the argument
# vocabulary the real server pins (mcpTools.ts — action/detail, memory), and the
# tools it reached for must be the five that exist.
KNOWN = {
    "daycare_identity_get",
    "daycare_world_snapshot",
    "daycare_action_propose",
    "daycare_memory_save",
    "daycare_memory_list",
}
for call in state["tool_calls"]:
    assert call["name"] in KNOWN, f"unknown tool called: {call['name']}"
    keys = set(call["arguments"])
    if call["name"] == "daycare_action_propose":
        assert keys <= {"action", "detail"}, f"wrong propose args: {keys}"
        assert call["arguments"]["action"] in ("look", "say", "note", "rest")
    if call["name"] == "daycare_memory_save":
        assert keys == {"memory"}, f"wrong memory args: {keys}"

applied = [e for e in state["events"] if e["type"] == "daycare.action_resolved"]
assert any("refused" not in e["summary"] for e in applied), "every action was refused"
print(f"events recorded: {[e['type'] for e in state['events']]}")

print("\nOK: two refereed turns, one session, memory carried over,")
print("    and every tool call matched the real server's vocabulary.")
PY

echo "==> talk to the same Claude yourself:"
"$BIN" open
echo "(scratch dir: $SCRATCH)"

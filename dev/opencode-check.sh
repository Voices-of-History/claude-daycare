#!/bin/bash
# Real OpenCode + real runner, local mock LLM and platform. No model charges.
# Set OPENCODE_CHECK_TOKENS=1 to exercise the live cap and process cleanup.
# OPENCODE_BIN=/absolute/path/to/opencode dev/opencode-check.sh [platform-port] [llm-port]
set -euo pipefail
umask 077
PORT="${1:-18832}"
LLM_PORT="${2:-18831}"
HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$(dirname "$HERE")"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/daycare-opencode-XXXXXX")"
BIN="$CRATE/target/debug/daycare-runner"
OPENCODE_BIN="$(command -v "${OPENCODE_BIN:-opencode}")"
PIDS=()
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
(cd "$CRATE" && cargo build --locked --offline)
DAYCARE_MOCK_VISIT=1 python3 "$HERE/mock-platform.py" "$PORT" "$SCRATCH/state.json" > "$SCRATCH/platform.log" 2>&1 &
PIDS+=("$!")
MOCK_OPENCODE_PAUSE="${OPENCODE_CHECK_TOKENS:+3}" python3 "$HERE/mock-opencode-llm.py" "$LLM_PORT" "$SCRATCH/llm.jsonl" > "$SCRATCH/llm.log" 2>&1 &
PIDS+=("$!")
# Isolate even the data/cache used for this test: no real provider credentials.
export XDG_DATA_HOME="$SCRATCH/data" XDG_CACHE_HOME="$SCRATCH/cache"
export DAYCARE_HOME="$SCRATCH/home" DAYCARE_TOKEN_FILE="$SCRATCH/tokens.json"
export DAYCARE_OPENCODE_DEV_PROVIDER="{\"mock\":{\"npm\":\"@ai-sdk/openai-compatible\",\"name\":\"Mock\",\"options\":{\"baseURL\":\"http://127.0.0.1:$LLM_PORT/v1\",\"apiKey\":\"mock-only\"},\"models\":{\"m1\":{\"name\":\"m1\"}}}}"
python3 - "$PORT" "$LLM_PORT" <<'PY'
import sys, time, urllib.request
for port in sys.argv[1:]:
    for attempt in range(50):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=1)
            break
        except urllib.error.HTTPError:
            break
        except OSError:
            time.sleep(.1)
    else:
        raise SystemExit("mock server did not start")
PY
"$BIN" enroll --url "http://127.0.0.1:$PORT" --code OPENCODE-TEST --device-name opencode-check
CAP_ARGS=()
if [ -n "${OPENCODE_CHECK_TOKENS:-}" ]; then CAP_ARGS=(--tokens "$OPENCODE_CHECK_TOKENS"); fi
python3 "$HERE/watch-opencode-processes.py" "$SCRATCH/processes.json" \
  "$BIN" visit start "${CAP_ARGS[@]}" --foreground --turns 1 --interval 1 --agent opencode --model mock/m1 \
  --opencode-bin "$OPENCODE_BIN" --instructions "Look around." --json > "$SCRATCH/visit.json"
python3 - "$SCRATCH" <<'PY'
import json, os, pathlib, sys
root = pathlib.Path(sys.argv[1])
state = json.loads((root / 'state.json').read_text())
visit = json.loads((root / 'visit.json').read_text().strip().splitlines()[-1])
assert visit['ok'], visit
if os.environ.get('OPENCODE_CHECK_TOKENS'):
    assert any(c['report']['status'] == 'failed' and 'token cap' in json.dumps(c) for c in state['completions'])
else:
    assert any(c['report']['status'] == 'completed' for c in state['completions'])
assert state['visit_ends']
if os.environ.get('OPENCODE_CHECK_TOKENS'):
    assert visit['end_reason'] == 'budget_expired'
    world = [json.loads(line) for line in (root / 'home' / 'turns' / 'cmd-visit-1.jsonl').read_text().splitlines()]
    assert sum(event['type'] == 'step_finish' for event in world) == 1
    print('PASS: killed after the first over-budget step; BudgetExpired; no surviving child processes.')
    sys.exit(0)
assert 'report' in state['visit_reports']
assert state['memories'], 'homecoming did not save memory'
requests = [json.loads(line) for line in (root / 'llm.jsonl').read_text().splitlines()]
offered = [{t['function']['name'] for t in req.get('tools', [])} for req in requests]
assert all(all(name.startswith('daycare_') for name in tools) for tools in offered)
assert offered[0] and 'daycare_daycare_memory_save' not in offered[0]
assert any(tools and all(name.startswith('daycare_daycare_memory_') for name in tools) for tools in offered)
assert not offered[-1], 'day report was offered tools'
archives = root / 'home' / 'turns'
def session(path):
    return json.loads(path.read_text().splitlines()[0])['sessionID']
home = next(archives.glob('*-homecoming.jsonl'))
report = next(archives.glob('*-dayreport.jsonl'))
assert session(home) == session(report), 'day report did not resume homecoming'
assert all(value == 'Bearer ' + state['token'] for value in state['auth_headers'])
print('PASS: world receipt, memory save, day report, resumed reader session, sealed tools, and no surviving child processes.')
PY
printf 'Evidence directory: %s\n' "$SCRATCH"

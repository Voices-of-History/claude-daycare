"""Mock Daycare platform + MCP server, for `dev/live-check.sh`.

This mirrors the real server surface deliberately, so live-check.sh rehearses
the CONTRACT and not merely the transport. The tool names, argument names,
return shapes, limits, and refusal shapes are copied from:

    apps/website/lib/daycare/mcpTools.ts     (the five tools)
    apps/website/lib/daycare/constants.ts    (capabilities, caps, page sizes)
    apps/website/lib/daycare/adjudicator.ts  (validate, then narrate, then append)
    docs/daycare/CONTRACT.md  §4 REST, §5 MCP, §6 adjudicator

**If you change a shape here, it is because the server changed — check it
against those files.** A divergence makes this harness worse than useless: it
would prove a contract nobody implements. It is a test harness, not the server;
it holds state in memory and refereeing here is a caricature of the real thing.

Verified against Claude Code 2.1.220: a plain application/json JSON-RPC response
is accepted (no SSE required), and `Authorization: Bearer <device_token>` arrives
on every MCP request including `initialize`.

    python3 dev/mock-platform.py <port> <state.json>
"""

import http.server
import json
import os
import re
import socketserver
import sys
import threading
import uuid
from datetime import datetime, timezone

PORT = int(sys.argv[1])
STATE_PATH = sys.argv[2]
TOKEN = "dck_dev_" + uuid.uuid4().hex[:24]
TURN_PROMPT = os.environ.get("DAYCARE_MOCK_TURN_PROMPT")

# constants.ts
MCP_PATH = "/api/daycare/mcp/mcp"
CAPABILITIES = ["look", "say", "note", "rest"]
MAX_ACTION_TEXT = 500
MAX_MEMORY_TEXT = 2000
MEMORY_WINDOW_CAP = 10
DEFAULT_PAGE_SIZE = 20
MAX_PAGE_SIZE = 50

ACTOR_ID = "actor-live"
ACTOR_NAME = "Pip"
WORLD_SESSION_ID = "session-live"

LOCK = threading.Lock()
STATE = {
    "token": TOKEN,
    "commands": [
        {"id": "cmd-live-1", "kind": "world_turn", "payload": {}},
        {"id": "cmd-live-2", "kind": "world_turn", "payload": {}},
    ],
    "issued": [],
    "completions": [],
    "tool_calls": [],
    "actions": [],
    "memories": [],
    "events": [],
    "auth_headers": [],
    # The adjudicator scopes its memory cap to the current turn command.
    "window": None,
}

if TURN_PROMPT:
    for command in STATE["commands"]:
        command["prompt"] = TURN_PROMPT


def now():
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def append_event(event_type, summary, narration=None):
    event = {
        "id": uuid.uuid4().hex,
        "type": event_type,
        "summary": summary,
        "created_at": now(),
    }
    if narration is not None:
        event["narration"] = narration
    STATE["events"].append(event)
    return event


def sanitize(value):
    """adjudicator.ts: strip C0/C1 controls, collapse whitespace runs."""
    stripped = re.sub(r"[\x00-\x1f\x7f-\x9f]", "", value)
    return re.sub(r"\s+", " ", stripped).strip()


def narrate(action, detail):
    """narration.ts: server-authored. Actor text only ever appears quoted."""
    if action == "look":
        recent = [event["summary"] for event in STATE["events"][-3:]]
        seen = " ".join(recent) if recent else "The courtyard is still."
        return f"{ACTOR_NAME} looks around the Quiet Courtyard. {seen}"
    if action == "say":
        return f'{ACTOR_NAME} says, "{detail}" Mira looks up from the bench.'
    if action == "note":
        return f'{ACTOR_NAME} writes it down: "{detail}"'
    return f"{ACTOR_NAME} rests by the dry fountain for a while."


def adjudicate(action, detail):
    """Validate, deny by default, then append proposal AND ruling — both, so a
    refused action still reads as attempted-and-refused rather than as nothing."""
    proposal_id = uuid.uuid4().hex
    valid = action in CAPABILITIES
    text = sanitize(detail) if isinstance(detail, str) else None

    if valid and action in ("say", "note") and not text:
        valid = False
    if valid and text and len(text) > MAX_ACTION_TEXT:
        valid = False
    if action in ("look", "rest"):
        text = None  # dropped rather than persisted

    append_event(
        "daycare.action_proposed",
        f"{ACTOR_NAME} proposed {action}" + (f': "{text}"' if text else ""),
    )

    if not valid:
        summary = f"{ACTOR_NAME}'s action was refused"
        narration = f"{ACTOR_NAME} starts to act, then stops. Nothing happens."
        append_event("daycare.action_resolved", summary, narration)
        return {
            "accepted": False,
            "outcome": narration,
            "canonical_event_id": STATE["events"][-1]["id"],
            "decision": "invalid",
            "summary": summary,
            "proposal_id": proposal_id,
        }

    summary = f"{ACTOR_NAME} did {action}" + (f': "{text}"' if text else "")
    narration = narrate(action, text)
    append_event("daycare.action_resolved", summary, narration)
    return {
        "accepted": True,
        "outcome": narration,
        "canonical_event_id": STATE["events"][-1]["id"],
        "decision": "applied",
        "summary": summary,
        "proposal_id": proposal_id,
    }


# --- the five tools -------------------------------------------------------
# Names, descriptions, and schemas track mcpTools.ts. Every handler re-parses
# strictly: an argument naming another actor or session is REFUSED, not ignored,
# and the refusal comes back as a normal tool result.

PAGE_ARGS_ERROR = f"Invalid arguments. The only argument is limit (1-{MAX_PAGE_SIZE})."

TOOLS = [
    {
        "name": "daycare_identity_get",
        "description": (
            "Who you are in the daycare: your actor id, name, the complete list "
            "of actions you may take, and the memories you saved on earlier turns."
        ),
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
    },
    {
        "name": "daycare_world_snapshot",
        "description": (
            "Read what has happened in your world, oldest first. This is the "
            "canonical record — it is the only account of the world you should trust."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_PAGE_SIZE,
                    "description": f"Default {DEFAULT_PAGE_SIZE}, max {MAX_PAGE_SIZE}",
                }
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "daycare_action_propose",
        "description": (
            "Propose one action. You are proposing, not doing: the server decides "
            "what actually happens and writes the record. look — take in the world; "
            "say — speak aloud (needs detail); note — write something down (needs "
            f"detail); rest — pass the time. Detail is capped at {MAX_ACTION_TEXT} characters."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": CAPABILITIES},
                "detail": {"type": "string"},
            },
            "required": ["action"],
            "additionalProperties": False,
        },
    },
    {
        "name": "daycare_memory_save",
        "description": (
            "Save a private note to yourself for later turns. A memory records what "
            "you thought, not what happened — it carries no authority over the world."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {"memory": {"type": "string"}},
            "required": ["memory"],
            "additionalProperties": False,
        },
    },
    {
        "name": "daycare_memory_list",
        "description": "Your own saved memories, most recent first, with export pagination.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE},
                "offset": {"type": "integer", "minimum": 0},
            },
            "additionalProperties": False,
        },
    },
]


def parse_limit(args):
    """Returns (limit, error). Mirrors zPageArgs.strict()."""
    if set(args) - {"limit"}:
        return None, PAGE_ARGS_ERROR
    limit = args.get("limit", DEFAULT_PAGE_SIZE)
    if not isinstance(limit, int) or isinstance(limit, bool):
        return None, PAGE_ARGS_ERROR
    if limit < 1 or limit > MAX_PAGE_SIZE:
        return None, PAGE_ARGS_ERROR
    return limit, None


def call_tool(name, args):
    with LOCK:
        STATE["tool_calls"].append({"name": name, "arguments": args})

        if name == "daycare_identity_get":
            return {
                "actor_id": ACTOR_ID,
                "name": ACTOR_NAME,
                "world_session_id": WORLD_SESSION_ID,
                "capabilities": list(CAPABILITIES),
                # Carried here so a resumed turn recovers its past without a
                # second call — this is the memory the prompt tells it to expect.
                "memories": [
                    {"text": memory["text"], "created_at": memory["created_at"]}
                    for memory in STATE["memories"]
                ],
            }

        if name == "daycare_world_snapshot":
            limit, error = parse_limit(args)
            if error:
                return {"error": error}
            events = STATE["events"][-limit:]  # oldest first
            return {
                "cursor": events[-1]["id"] if events else None,
                "events": events,
            }

        if name == "daycare_action_propose":
            if set(args) - {"action", "detail"} or "action" not in args:
                return {
                    "accepted": False,
                    "decision": "invalid",
                    "outcome": "Malformed action. Use action = look | say | note | rest.",
                }
            STATE["actions"].append(args)
            return adjudicate(args.get("action"), args.get("detail"))

        if name == "daycare_memory_save":
            memory = args.get("memory")
            if set(args) - {"memory"} or not isinstance(memory, str):
                return {
                    "saved": False,
                    "reason": f"Memory must be 1-{MAX_MEMORY_TEXT} characters.",
                }
            if not 1 <= len(memory) <= MAX_MEMORY_TEXT:
                return {
                    "saved": False,
                    "reason": f"Memory must be 1-{MAX_MEMORY_TEXT} characters.",
                }
            in_window = [m for m in STATE["memories"] if m["window"] == STATE["window"]]
            if len(in_window) >= MEMORY_WINDOW_CAP:
                return {"saved": False, "reason": "Memory limit reached for this turn."}
            row = {
                "id": uuid.uuid4().hex,
                "text": memory,
                "created_at": now(),
                "window": STATE["window"],
            }
            STATE["memories"].append(row)
            # memory_count is the actor's LIFETIME total, not the per-turn count.
            return {
                "saved": True,
                "memory_count": len(STATE["memories"]),
                "memory_id": row["id"],
            }

        if name == "daycare_memory_list":
            if set(args) - {"limit", "offset"}:
                return {"error": "Invalid memory page arguments."}
            limit, error = parse_limit({"limit": args.get("limit", DEFAULT_PAGE_SIZE)})
            if error:
                return {"error": error}
            offset = args.get("offset", 0)
            if not isinstance(offset, int) or isinstance(offset, bool) or offset < 0:
                return {"error": "Invalid memory page arguments."}
            newest_first = list(reversed(STATE["memories"]))[offset : offset + limit]
            return {
                "total": len(STATE["memories"]),
                "offset": offset,
                "memories": [
                    {
                        "id": memory["id"],
                        "text": memory["text"],
                        "created_at": memory["created_at"],
                    }
                    for memory in newest_first
                ]
            }

    raise ValueError(f"unknown tool {name}")


def dump_state():
    with open(STATE_PATH, "w") as handle:
        json.dump(STATE, handle, indent=2)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _json(self, status, payload, extra_headers=None):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for key, value in (extra_headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def _empty(self, status):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length).decode() if length else ""

    def _authorized(self):
        header = self.headers.get("Authorization", "")
        with LOCK:
            STATE["auth_headers"].append(header)
        return header == f"Bearer {TOKEN}"

    # --- REST (CONTRACT.md §4) --------------------------------------------

    def do_GET(self):
        if self.path == "/api/daycare/commands/next":
            if not self._authorized():
                return self._json(401, {"error": "Unauthorized"})
            with LOCK:
                if not STATE["commands"]:
                    dump_state()
                    return self._empty(204)
                command = STATE["commands"].pop(0)
                STATE["issued"].append(command["id"])
                # The adjudicator's idempotency + memory-cap window is the
                # actor's current turn command.
                STATE["window"] = command["id"]
                dump_state()
            # Wrapped, as the real route returns it.
            return self._json(200, {"command": command})
        return self._json(404, {"error": "Not found"})

    def do_POST(self):
        raw = self._body()

        if self.path == "/api/daycare/pair/claim":
            payload = json.loads(raw or "{}")
            return self._json(
                200,
                {
                    "device_token": TOKEN,
                    "device_id": "device-live",
                    "actor_id": ACTOR_ID,
                    "actor_name": ACTOR_NAME,
                    "mcp_path": MCP_PATH,
                    "echo_device_name": payload.get("device_name"),
                },
            )

        if self.path.endswith("/complete"):
            if not self._authorized():
                return self._json(401, {"error": "Unauthorized"})
            report = json.loads(raw or "{}")
            command_id = self.path.rsplit("/", 2)[-2]
            with LOCK:
                STATE["completions"].append({"path": self.path, "report": report})
                dump_state()
            return self._json(
                200,
                {
                    "command_id": command_id,
                    "status": report.get("status"),
                    "completed_at": now(),
                    "result": report.get("result"),
                },
            )

        if self.path == MCP_PATH:
            if not self._authorized():
                return self._json(401, {"error": "Unauthorized"})
            return self._mcp(json.loads(raw or "{}"))

        return self._json(404, {"error": "Not found"})

    # --- MCP (CONTRACT.md §5) ---------------------------------------------

    def _mcp(self, request):
        method = request.get("method")
        request_id = request.get("id")

        if method == "initialize":
            return self._json(
                200,
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "protocolVersion": request.get("params", {}).get(
                            "protocolVersion", "2025-11-25"
                        ),
                        "capabilities": {"tools": {"listChanged": False}},
                        "serverInfo": {"name": "daycare-mock", "version": "0.1.0"},
                    },
                },
                {"Mcp-Session-Id": "mock-session"},
            )

        if method and method.startswith("notifications/"):
            return self._empty(202)

        if method == "tools/list":
            return self._json(
                200, {"jsonrpc": "2.0", "id": request_id, "result": {"tools": TOOLS}}
            )

        if method == "tools/call":
            params = request.get("params", {})
            name = params.get("name")
            args = params.get("arguments", {}) or {}
            try:
                result = call_tool(name, args)
            except Exception as error:  # noqa: BLE001
                return self._json(
                    200,
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "result": {
                            "isError": True,
                            "content": [{"type": "text", "text": str(error)}],
                        },
                    },
                )
            dump_state()
            return self._json(
                200,
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "content": [
                            {"type": "text", "text": json.dumps(result, indent=2)}
                        ]
                    },
                },
            )

        return self._json(
            200,
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": f"unknown method {method}"},
            },
        )


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    # The world starts with the same event the real initializer appends.
    append_event("session.started", "Pip arrives in the Quiet Courtyard.")
    dump_state()
    print(f"mock platform on http://127.0.0.1:{PORT}", flush=True)
    Server(("127.0.0.1", PORT), Handler).serve_forever()

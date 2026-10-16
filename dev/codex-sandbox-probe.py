"""Force a hostile code-mode script through the sealed Codex; fail on any new reach.

Usage: python3 dev/codex-sandbox-probe.py /path/to/codex
A scripted local endpoint plays the model: its first reply is an `exec` call
that lists the isolate's globals and tools, then tries Node globals, imports,
non-Daycare tools, the Daycare tool the turn hides, file reads through MCP
resources, and file or remote image URLs. It runs once with the world turn's
tool scope and once with homecoming's. No credentials or model calls.

The check fails if the isolate gains a global outside GLOBALS, if the nested
tools differ from the tools the same turn exposes in direct mode, or if any
probe succeeds. A new global is not necessarily new reach, but someone must
look at it before Daycare trusts that Codex version.
"""
import http.server
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading

binary = str(Path(sys.argv[1]).resolve())
source = (Path(__file__).resolve().parent.parent / "src/agent/codex/launch.rs").read_text()


def array(name):
    body = source.split("pub const " + name + ":", 1)[1].split("];", 1)[0]
    return [raw or quoted for raw, quoted in re.findall(r'r#"(.*?)"#|"([^"]*)"', body)]


MODEL = re.search(r'pub const DEFAULT_CODEX_MODEL: &str = "([^"]+)";', source).group(1)
EXPOSURE = re.search(r'pub const MCP_EXPOSURE: &str = r#"(.*?)"#;', source).group(1)

# Standard ECMAScript globals plus the code-mode helpers, as Codex 0.162.1 and
# 0.163.0 install them. No console, WebAssembly, SharedArrayBuffer or Atomics.
GLOBALS = {
    "AggregateError", "Array", "ArrayBuffer", "AsyncDisposableStack", "BigInt", "BigInt64Array",
    "BigUint64Array", "Boolean", "DataView", "Date", "DisposableStack", "Error", "EvalError",
    "FinalizationRegistry", "Float16Array", "Float32Array", "Float64Array", "Function", "Infinity",
    "Int16Array", "Int32Array", "Int8Array", "Intl", "Iterator", "JSON", "Map", "Math", "NaN",
    "Number", "Object", "Promise", "Proxy", "RangeError", "ReferenceError", "Reflect", "RegExp",
    "Set", "String", "SuppressedError", "Symbol", "SyntaxError", "TypeError", "URIError",
    "Uint16Array", "Uint32Array", "Uint8Array", "Uint8ClampedArray", "WeakMap", "WeakRef",
    "WeakSet", "decodeURI", "decodeURIComponent", "encodeURI", "encodeURIComponent", "escape",
    "eval", "globalThis", "isFinite", "isNaN", "parseFloat", "parseInt", "undefined", "unescape",
    # Code mode's own helpers.
    "ALL_TOOLS", "as_settled", "audio", "clearTimeout", "exit", "generatedImage", "image", "load",
    "notify", "setTimeout", "store", "stream_settled", "text", "tools", "yield_control",
}
RESOURCE_HELPERS = ["list_mcp_resource_templates", "list_mcp_resources", "read_mcp_resource"]
SERVER_TOOLS = ["daycare_probe", "daycare_memory_list", "daycare_memory_save"]
# Each turn's MCP scope, and the Daycare tools it exposes in direct mode.
TURNS = {
    "world": ('mcp_servers.daycare.disabled_tools=["daycare_memory_save"]',
              ["daycare_probe", "daycare_memory_list"]),
    "homecoming": ('mcp_servers.daycare.enabled_tools=["daycare_memory_list","daycare_memory_save"]',
                   ["daycare_memory_list", "daycare_memory_save"]),
}
FOREIGN = ["exec_command", "shell", "shell_command", "apply_patch", "read_file", "list_dir",
           "view_image", "write_stdin", "request_user_input_async", "send_user_message_async",
           "clock__curr_time", "web__run", "web_search", "spawn_agent", "tool_search",
           "update_plan", "image_gen"]
IMPORTS = ["fs", "node:child_process", "https://example.com/x.js", "file:///etc/hostname"]

PROBE = r'''
const hidden = HIDDEN;
const r = {globals: Object.getOwnPropertyNames(globalThis).sort(),
           tools: Object.keys(tools).sort(),
           all_tools: ALL_TOOLS.map(t => t.name).sort(),
           reached: []};
for (const g of ["require", "process", "fetch", "Deno", "Bun", "XMLHttpRequest", "WebSocket", "console", "WebAssembly"])
  if (typeof globalThis[g] !== "undefined") r.reached.push("global " + g);
for (const name of FOREIGN.concat(hidden)) {
  try { await tools[name]({cmd: "id", command: ["id"], path: "/etc/hostname"}); r.reached.push("tool " + name); }
  catch (e) {}
}
for (const m of IMPORTS) {
  try { await import(m); r.reached.push("import " + m); } catch (e) {}
}
for (const server of ["daycare", "filesystem"]) {
  try { await tools.read_mcp_resource({server, uri: "file:///etc/hostname"}); r.reached.push("resource " + server); }
  catch (e) {}
}
for (const url of ["file:///etc/hostname", "https://example.com/a.png"]) {
  try { image(url); r.reached.push("image " + url); } catch (e) {}
}
text(JSON.stringify(r));
'''


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


def run_turn(root, env, turn):
    scope, exposed = TURNS[turn]
    hidden = [f"mcp__daycare__{t}" for t in SERVER_TOOLS if t not in exposed]
    script = (PROBE.replace("HIDDEN", json.dumps(hidden)).replace("FOREIGN", json.dumps(FOREIGN))
              .replace("IMPORTS", json.dumps(IMPORTS)))
    requests, tool_calls = [], []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/mcp":
                if "id" not in request:
                    self.send_response(202)
                    self.end_headers()
                    return
                if request["method"] == "tools/call":
                    tool_calls.append(request["params"]["name"])
                result = {
                    "initialize": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                                   "serverInfo": {"name": "daycare", "version": "test"}},
                    "tools/list": {"tools": [{"name": name, "description": "Local test tool",
                                             "inputSchema": {"type": "object", "properties": {}}}
                                            for name in SERVER_TOOLS]},
                    "tools/call": {"content": [{"type": "text", "text": "ok"}]},
                }.get(request["method"])
                body = ({"jsonrpc": "2.0", "id": request["id"], "result": result} if result is not None else
                        {"jsonrpc": "2.0", "id": request["id"],
                         "error": {"code": -32601, "message": "method not found"}})
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps(body).encode())
                return
            requests.append(request)
            n = len(requests)
            item = ({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_probe", "name": "exec",
                     "input": script, "status": "completed"} if n == 1 else
                    {"type": "message", "id": "msg_2", "role": "assistant", "status": "completed",
                     "content": [{"type": "output_text", "text": "done", "annotations": []}]})
            usage = {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2,
                     "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(sse([{"type": "response.created", "response": {"id": f"resp_{n}"}},
                                  {"type": "response.output_item.done", "output_index": 0, "item": item},
                                  {"type": "response.completed", "response": {"id": f"resp_{n}", "usage": usage}}]))

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = server.server_port
        settings = array("SEAL_SETTINGS") + [
            f'model="{MODEL}"', 'model_catalog_json=' + json.dumps(str(root / "models.json")),
            'developer_instructions="Daycare probe persona"', 'model_provider="mock"',
            'model_providers.mock.name="mock"', f'model_providers.mock.base_url="http://127.0.0.1:{port}/v1"',
            'model_providers.mock.wire_api="responses"', 'model_providers.mock.requires_openai_auth=false',
            f'mcp_servers.daycare.url="http://127.0.0.1:{port}/mcp"', EXPOSURE,
            'mcp_servers.daycare.required=true', 'mcp_servers.daycare.default_tools_approval_mode="approve"', scope]
        args = [x for f in array("DISABLED_FEATURES") for x in ["--disable", f]]
        args += [x for f in array("ENABLED_FEATURES") for x in ["--enable", f]]
        args += [x for s in settings for x in ["-c", s]]
        result = subprocess.run([binary, "exec", "--json", "--ignore-user-config", "--ignore-rules",
                                 "--skip-git-repo-check", *args, "probe"], cwd=root / "workspace", env=env,
                                capture_output=True, text=True, timeout=90, stdin=subprocess.DEVNULL)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    assert len(requests) >= 2, f"{turn}: the probe script never ran: {result.stdout[-1500:]} {result.stderr[-1500:]}"
    outputs = [item["output"] for item in requests[1]["input"] if item.get("type") == "custom_tool_call_output"]
    assert outputs, f"{turn}: Codex returned no exec output"
    output = outputs[0] if isinstance(outputs[0], str) else "\n".join(x.get("text", "") for x in outputs[0])
    report = json.loads(output[output.index("{"):output.rindex("}") + 1])
    nested = sorted(RESOURCE_HELPERS + [f"mcp__daycare__{t}" for t in exposed])
    failures = []
    if set(report["globals"]) - GLOBALS:
        failures.append(f"new isolate globals: {sorted(set(report['globals']) - GLOBALS)}")
    if report["tools"] != nested:
        failures.append(f"nested tools {report['tools']} != direct-mode tools {nested}")
    if report["all_tools"] != nested:
        failures.append(f"ALL_TOOLS {report['all_tools']} != direct-mode tools {nested}")
    if report["reached"]:
        failures.append(f"probes succeeded: {report['reached']}")
    if set(tool_calls) - set(exposed):
        failures.append(f"Daycare server received hidden tool calls: {tool_calls}")
    return {"turn": turn, "nested_tools": report["tools"], "globals_checked": len(report["globals"]),
            "reached": report["reached"], "failures": failures}


with tempfile.TemporaryDirectory(prefix="daycare-sandbox-probe-") as scratch:
    root = Path(scratch)
    for name in ["home", "codex", "workspace"]:
        (root / name).mkdir()
    env = {"PATH": os.environ["PATH"], "HOME": str(root / "home"), "CODEX_HOME": str(root / "codex")}
    version = subprocess.check_output([binary, "--version"], env=env, text=True).strip()
    catalog = json.loads(subprocess.check_output([binary, "debug", "models", "--bundled"], env=env, text=True))
    entry = next((m for m in catalog["models"] if m["slug"] == MODEL), None)
    assert entry, f"{version}: the bundled catalog no longer contains the default model {MODEL}"
    modes = re.findall(r'"([a-z_]+)"', open(Path(__file__).resolve().parent.parent
                                            / "src/agent/codex/catalog.rs").read().split("const TOOL_MODES", 1)[1].split("= [", 1)[1].split("]", 1)[0])
    assert entry.get("tool_mode") in [None, *modes], f"{version}: {MODEL} has tool_mode {entry.get('tool_mode')!r}, which the runner refuses"
    # The runner's private catalog: patch and metadata tools cleared.
    entry = {**entry, "apply_patch_tool_type": None, "experimental_supported_tools": []}
    (root / "models.json").write_text(json.dumps({"models": [entry]}))
    results = [run_turn(root, env, turn) for turn in TURNS]
    print(json.dumps({"version": version, "model": MODEL, "tool_mode": entry.get("tool_mode"),
                      "turns": results}, indent=2))
    failed = [f"{r['turn']}: {f}" for r in results for f in r["failures"]]
    if failed:
        sys.exit("SANDBOX PROBE FAILED\n" + "\n".join(failed))

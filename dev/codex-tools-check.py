"""Capture Codex's actual native/MCP catalog without credentials or model calls.

Usage: python3 dev/codex-tools-check.py /path/to/codex
The local endpoint returns HTTP 400 after capturing the model request.
A synthetic multi-agent capability supplies a collaboration positive control.
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


with tempfile.TemporaryDirectory(prefix="daycare-tools-check-") as scratch:
    root = Path(scratch)
    for name in ["home", "codex", "workspace"]:
        (root / name).mkdir()
    env = {"PATH": os.environ["PATH"], "HOME": str(root / "home"), "CODEX_HOME": str(root / "codex")}
    version = subprocess.check_output([binary, "--version"], env=env, text=True).strip()
    catalog = json.loads(subprocess.check_output([binary, "debug", "models", "--bundled"], env=env, text=True))
    model = next(m for m in catalog["models"] if m["slug"] == "gpt-5.5")
    model["multi_agent_version"] = "v2"
    requests = []

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
                result = {
                    "initialize": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                                   "serverInfo": {"name": "daycare", "version": "test"}},
                    "tools/list": {"tools": [{"name": name, "description": "Local test tool",
                                             "inputSchema": {"type": "object", "properties": {}}}
                                            for name in ["daycare_probe", "daycare_memory_list", "daycare_memory_save"]]},
                }.get(request["method"], {})
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}).encode())
            else:
                requests.append(request)
                self.send_response(400)
                self.end_headers()
                self.wfile.write(b'{"error":{"message":"local capture only","type":"invalid_request_error"}}')

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    settings = [s for s in array("SEAL_SETTINGS") if s != "agents.enabled=false"] + [
        'model="gpt-5.5"', 'model_provider="capture"',
        'model_catalog_json=' + json.dumps(str(root / "models.json")),
        'model_providers.capture.name="Local capture"',
        f'model_providers.capture.base_url="http://127.0.0.1:{server.server_port}/v1"',
        'model_providers.capture.wire_api="responses"',
        'model_providers.capture.requires_openai_auth=false',
    ]
    args = [x for feature in array("DISABLED_FEATURES") for x in ["--disable", feature]]
    args += [x for setting in settings for x in ["-c", setting]]
    exposure = re.search(r'pub const MCP_EXPOSURE: &str = r#"(.*?)"#;', source).group(1)
    mcp = ["-c", f'mcp_servers.daycare.url="http://127.0.0.1:{server.server_port}/mcp"',
           "-c", exposure,
           "-c", "mcp_servers.daycare.required=true", "-c", 'mcp_servers.daycare.default_tools_approval_mode="approve"']
    names = {}
    resume_id = None
    try:
        for name, patch, agents, use_mcp in [
            ("patch_control", "freeform", False, True),
            ("collaboration_control", None, True, True),
            ("sealed_new", None, False, True),
            ("sealed_resume", None, False, True),
            ("sealed_homecoming", None, False, True),
            ("sealed_day_report", None, False, False),
        ]:
            requests.clear()
            model["apply_patch_tool_type"] = patch
            (root / "models.json").write_text(json.dumps({"models": [model]}))
            resume = ["resume", resume_id] if name in ["sealed_resume", "sealed_homecoming", "sealed_day_report"] else []
            scope = ('mcp_servers.daycare.enabled_tools=["daycare_memory_list","daycare_memory_save"]'
                     if name == "sealed_homecoming" else 'mcp_servers.daycare.disabled_tools=["daycare_memory_save"]')
            result = subprocess.run([binary, "exec", *resume, "--json", "--ignore-user-config", "--ignore-rules",
                                     "--skip-git-repo-check", *args, "-c", f"agents.enabled={str(agents).lower()}",
                                     *(mcp + ["-c", scope] if use_mcp else []), "Capture tool definitions only."],
                                    cwd=root / "workspace", env=env, capture_output=True, text=True, timeout=25)
            assert requests, f"Codex never reached the local capture endpoint: {result.stderr[-1000:]}"
            names[name] = [t.get("name", t["type"]) for t in requests[0]["tools"]]
            assert ("apply_patch" in names[name]) == (patch is not None), names
            assert ("collaboration" in names[name]) == agents, names
            for tool in requests[0]["tools"]:
                if tool.get("name") == "mcp__daycare":
                    expected = (["daycare_memory_list", "daycare_memory_save"] if name == "sealed_homecoming"
                                else ["daycare_probe", "daycare_memory_list"])
                    assert {t["name"] for t in tool["tools"]} == set(expected), tool
            daycare = [n for n in names[name] if n.startswith("mcp__daycare")]
            assert bool(daycare) == use_mcp, requests[0]["tools"]
            if name.startswith("sealed"):
                assert set(names[name]) == set(daycare + (["list_mcp_resources", "list_mcp_resource_templates", "read_mcp_resource"] if use_mcp else [])), names
            if name == "sealed_new":
                events = [json.loads(line) for line in result.stdout.splitlines()]
                resume_id = next(e["thread_id"] for e in events if e["type"] == "thread.started")
        prompt = subprocess.check_output([binary, "debug", "prompt-input", *args,
                                          "-c", "agents.enabled=false", "-c", 'developer_instructions="Daycare test persona"',
                                          "DAYCARE-SEAL-PROBE"], cwd=root / "workspace", env=env, text=True)
        kinds = [kind for item in json.loads(prompt)
                 for kind in item.get("internal_chat_message_metadata_passthrough", {}).get("content_item_kinds", [])]
        assert kinds == ["generic.developer_instructions", "user.text"], kinds
        names["prompt_kinds"] = kinds
        print(json.dumps({"version": version, "synthetic_multi_agent_version": "v2", **names}, indent=2))
    finally:
        server.shutdown()
        server.server_close()
        thread.join()

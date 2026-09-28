"""Prove the Codex collaboration switch with a local request capture.

Usage: python3 dev/codex-tools-check.py /path/to/codex
No credentials, paid model calls, or external model endpoints are used.
A synthetic catalog advertises multi-agent v2 on gpt-5.5 so the positive
control cannot pass merely because today's model lacks collaboration.
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
    # No inherited auth, instructions, telemetry configuration, or provider.
    env = {"PATH": os.environ["PATH"], "HOME": str(root / "home"), "CODEX_HOME": str(root / "codex")}
    version = subprocess.check_output([binary, "--version"], env=env, text=True).strip()
    catalog = json.loads(subprocess.check_output([binary, "debug", "models", "--bundled"], env=env, text=True))
    model = next(m for m in catalog["models"] if m["slug"] == "gpt-5.5")
    model["multi_agent_version"] = "v2"
    (root / "models.json").write_text(json.dumps({"models": [model]}))
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            # A permanent error stops exec immediately after it exposes tools.
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
    names = {}
    try:
        for enabled in [True, False]:
            requests.clear()
            subprocess.run([binary, "exec", "--json", "--ignore-user-config", "--ignore-rules",
                            "--skip-git-repo-check", *args, "-c", f"agents.enabled={str(enabled).lower()}",
                            "Capture tool definitions only."], cwd=root / "workspace", env=env,
                           capture_output=True, text=True, timeout=25)
            assert requests, "Codex never reached the local capture endpoint"
            tools = requests[0]["tools"]
            names[enabled] = [t.get("name", t["type"]) for t in tools]
            assert ("collaboration" in names[enabled]) == enabled, names
        print(json.dumps({"version": version, "synthetic_multi_agent_version": "v2",
                          "agents_enabled_true": names[True], "agents_enabled_false": names[False]}, indent=2))
    finally:
        server.shutdown()
        server.server_close()
        thread.join()

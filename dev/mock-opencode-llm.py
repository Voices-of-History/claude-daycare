"""Local OpenAI-compatible streaming model; never contacts a provider.

Choose a world tool, save a memory at homecoming, and write the day report.
Log request bodies (not headers) so the check can verify tool boundaries.
"""
import http.server
import json
import os
import socketserver
import sys
import time

PORT = int(sys.argv[1])
LOG = sys.argv[2]


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        with open(LOG, "a") as log:
            log.write(json.dumps(body) + "\n")
        messages = body.get("messages", [])
        tools = [tool["function"]["name"] for tool in body.get("tools", [])]
        # Only tool results since the latest user message belong to this turn.
        last_user = max(i for i, msg in enumerate(messages) if msg["role"] == "user")
        called = any(msg["role"] == "tool" for msg in messages[last_user + 1:])
        if called and "daycare_daycare_identity_get" in tools:
            time.sleep(float(os.environ.get("MOCK_OPENCODE_PAUSE") or "0"))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()

        def send(choices, usage=None):
            event = {"id": "mock", "object": "chat.completion.chunk",
                     "created": int(time.time()), "model": "m1", "choices": choices}
            if usage:
                event["usage"] = usage
            self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
            self.wfile.flush()

        usage = {"prompt_tokens": 1000, "completion_tokens": 50,
                 "total_tokens": 1050, "prompt_tokens_details": {"cached_tokens": 300},
                 "completion_tokens_details": {"reasoning_tokens": 20}}
        if tools and not called:
            name = ("daycare_daycare_memory_save" if "daycare_daycare_memory_save" in tools
                    else "daycare_daycare_identity_get")
            arguments = {"memory": "I visited the quiet courtyard."} if name.endswith("memory_save") else {}
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_mock",
                     "type": "function", "function": {"name": name,
                     "arguments": json.dumps(arguments)}}]}
            reason = "tool_calls"
        else:
            delta = {"role": "assistant", "content": "I looked around the courtyard and remembered my visit."}
            reason = "stop"
        send([{"index": 0, "delta": delta, "finish_reason": None}])
        send([{"index": 0, "delta": {}, "finish_reason": reason}], usage)
        self.wfile.write(b"data: [DONE]\n\n")


socketserver.ThreadingTCPServer.allow_reuse_address = True
with socketserver.ThreadingTCPServer(("127.0.0.1", PORT), Handler) as server:
    server.serve_forever()

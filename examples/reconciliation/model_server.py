#!/usr/bin/env python3
"""A tiny offline scripted model, in pure Python standard library.

This is the same idea as the repository's `salvor-demo-model` binary: a hermetic
HTTP server that speaks just enough of the Messages API for a Salvor agent to
drive, with no API key and no network. The stock `salvor-demo-model` is scripted
for the nine-write demo research vocabulary, so this example ships its own
one-write script instead. `agent.toml` routes model calls here through
`base_url_env = "SALVOR_DEMO_BASE_URL"`, exactly the offline hook the demo uses.

It serves one route, POST /v1/messages, and selects a response by the number of
messages in the request body (turn k arrives with 2k - 1 messages, so selection
is stateless and replay-safe: a resume's first live turn carries the exact count
it would have had uninterrupted).

- 1 message  (turn 1): ask to call `commit_report` once. This is the one write.
- 3 messages (turn 2): a short final summary, end of turn. Served on resume,
  after the dangling write has been resolved, to complete the run.

Port comes from RECON_MODEL_PORT (default 8891).
"""

import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("RECON_MODEL_PORT", "8891"))

# The single line the agent commits. It appears in the model's tool call input
# and, therefore, as the one line the write appends to the report file.
REPORT_LINE = (
    "Write-ahead intents make a crash mid-write reconcilable: the intent is "
    "recorded before the write, so resume refuses to guess and a human resolves."
)


def tool_use_turn():
    return {
        "id": "msg_commit",
        "model": "salvor-offline-model",
        "role": "assistant",
        "content": [
            {
                "type": "tool_use",
                "id": "tu_commit_1",
                "name": "commit_report",
                "input": {"content": REPORT_LINE},
            }
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 120, "output_tokens": 30},
    }


def final_turn():
    return {
        "id": "msg_done",
        "model": "salvor-offline-model",
        "role": "assistant",
        "content": [
            {
                "type": "text",
                "text": "Report committed: one durable write, resolved by hand after the crash.",
            }
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 140, "output_tokens": 20},
    }


def response_for(message_count):
    if message_count == 1:
        return 200, tool_use_turn()
    if message_count == 3:
        return 200, final_turn()
    return 500, {
        "error": {
            "type": "offline_script",
            "message": f"no scripted response for {message_count} messages",
        }
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass  # keep the walkthrough output clean

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        if self.path == "/v1/messages":
            count = 0
            try:
                data = json.loads(body)
                messages = data.get("messages")
                if isinstance(messages, list):
                    count = len(messages)
            except (ValueError, TypeError):
                pass
            status, payload = response_for(count)
        else:
            status, payload = 404, {
                "error": {"type": "not_found", "message": self.path}
            }
        out = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    print(f"salvor-reconciliation-model listening on http://127.0.0.1:{PORT}", flush=True)
    server.serve_forever()

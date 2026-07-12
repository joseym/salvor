#!/usr/bin/env python3
"""A one-tool MCP write server, in pure Python standard library.

The whole point of this server is to be interruptible on purpose. Its single
tool, `commit_report`, is a genuine Write: it appends one line to a report file.
But before it returns, it performs the real write, flushes it to disk, and then
blocks (sleeps). That sleep is the controllable window the walkthrough kills in:
the runtime has already recorded the write's *intent* (write-ahead), the real
write has already landed on disk, and the completion has NOT been recorded yet.
A `kill -9` during the sleep leaves exactly that state, a dangling write intent,
which is what `salvor resume` refuses to guess about and a human resolves.

There is no dependency here. An MCP server over stdio is just a program that
reads newline-delimited JSON-RPC 2.0 requests on stdin and writes responses on
stdout, so the four methods a Salvor run needs (initialize, tools/list,
tools/call, plus ping) are handled by hand below. No `mcp` package, no venv, no
Salvor code: the host reaches this tool over the child's stdio.

Configuration comes from the environment (set by agent.toml):

- RECON_REPORT_PATH: the file the write appends to. This is the side-effect
  ledger the walkthrough inspects; one line per real `commit_report` execution.
- RECON_BLOCK_SECONDS: how long the tool sleeps after the write lands, holding
  the process in the killable window. A large default so the kill always wins.
"""

import json
import os
import sys
import time

REPORT_PATH = os.environ.get("RECON_REPORT_PATH", "report.txt")
BLOCK_SECONDS = float(os.environ.get("RECON_BLOCK_SECONDS", "3600"))

# One tool, deliberately carrying no read-only or idempotent annotation. Salvor's
# conservative default for an unannotated tool is Write, and agent.toml pins it to
# Write besides, so its intent is recorded before it runs.
TOOLS = [
    {
        "name": "commit_report",
        "description": "Commit the report by appending it durably to the report file. This is a write.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The one-line report to commit durably.",
                }
            },
            "required": ["content"],
        },
    }
]


def send(message):
    """Write one JSON-RPC message as a single line, then flush."""
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def commit_report(content):
    """The real write, then the block that makes the kill window controllable."""
    # 1. The genuine side effect: append one line and force it to disk. After
    #    this returns the write has happened, exactly once.
    with open(REPORT_PATH, "a", encoding="utf-8") as report:
        report.write(content + "\n")
        report.flush()
        os.fsync(report.fileno())
    # 2. Block. The write has landed but the runtime has not yet recorded this
    #    call's completion. A kill here is the dangling-write state. On a normal
    #    (unkilled) run the sleep simply elapses and the tool returns.
    time.sleep(BLOCK_SECONDS)
    return {
        "content": [{"type": "text", "text": f"committed report to {REPORT_PATH}"}],
        "isError": False,
    }


def handle(request):
    """Answer one request. Returns a response dict, or None for a notification."""
    method = request.get("method")
    request_id = request.get("id")

    if method == "initialize":
        # Echo the client's protocol version back so the two always agree.
        protocol_version = request.get("params", {}).get(
            "protocolVersion", "2025-06-18"
        )
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "protocolVersion": protocol_version,
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "salvor-reconciliation-writer",
                    "version": "0.1.0",
                },
            },
        }

    # A notification (no id): initialized handshake completion. No response.
    if method in ("notifications/initialized", "initialized"):
        return None

    if method == "ping":
        return {"jsonrpc": "2.0", "id": request_id, "result": {}}

    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": request_id, "result": {"tools": TOOLS}}

    if method == "tools/call":
        params = request.get("params", {})
        name = params.get("name")
        arguments = params.get("arguments", {})
        if name == "commit_report":
            result = commit_report(arguments.get("content", ""))
            return {"jsonrpc": "2.0", "id": request_id, "result": result}
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32602, "message": f"unknown tool: {name}"},
        }

    # Unknown method: error for a request, silence for a notification.
    if request_id is not None:
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": f"method not found: {method}"},
        }
    return None


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            continue
        response = handle(request)
        if response is not None:
            send(response)


if __name__ == "__main__":
    main()

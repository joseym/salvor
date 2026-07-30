#!/usr/bin/env python3
"""The disputes-desk MCP server: the three tools this example's graph runs on.

Pure Python standard library. An MCP server over stdio is a program that reads
newline-delimited JSON-RPC 2.0 requests on stdin and writes responses on stdout,
so the four methods a Salvor run needs (initialize, tools/list, tools/call, and
ping) are handled by hand below. No `mcp` package, no venv, no Salvor code.

Three tools, one per effect class, because the graph needs all three stories:

- `lookup_dispute` (Read, `readOnlyHint: true`): resolves a dispute id against
  the seed file. Re-running it re-reads the same record, so an interrupted call
  is freely retried.
- `issue_refund` (Write): appends ONE line to the refund ledger. It carries no
  annotation at all, so Salvor's conservative default already reads it as a
  Write, and `agents/*.toml` pins it besides. The ledger is the side-effect
  ledger the durability proof counts lines in: one line per real execution,
  forever.
- `send_notice` (Idempotent, `idempotentHint: true`): upserts one entry keyed by
  dispute id and rewrites the whole notices file. Calling it three times with
  the same message leaves the file byte-identical to calling it once, which is
  what idempotent means here, so the hint is honest and no override contradicts
  it.

Every tool answers with `structuredContent` as well as text. Salvor records the
WHOLE tool result as the node's output, so `structuredContent` is what the graph
document's branch expression reads (`structuredContent.amount_usd >= 250.0`) and
what a downstream node sees as its input. Text alone would leave a branch with
nothing typed to route on.

Configuration comes from the environment, set by the agent definitions:

- SALVOR_DISPUTES_FILE: the seed dispute records to read. Defaults to
  `disputes.json` beside this file, so the server works with no configuration.
- SALVOR_DISPUTES_LEDGER: the refund ledger `issue_refund` appends to. This is
  runtime state, never committed; it defaults to `refund-ledger.txt` under the
  working directory and the example points it at a scratch path.
- SALVOR_DISPUTES_NOTICES: the notices file `send_notice` upserts into, same
  defaulting story (`customer-notices.json`).
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DISPUTES_FILE = os.environ.get(
    "SALVOR_DISPUTES_FILE", os.path.join(HERE, "disputes.json")
)
LEDGER_PATH = os.environ.get("SALVOR_DISPUTES_LEDGER", "refund-ledger.txt")
NOTICES_PATH = os.environ.get("SALVOR_DISPUTES_NOTICES", "customer-notices.json")

TOOLS = [
    {
        "name": "lookup_dispute",
        "description": (
            "Look up one invoice dispute by id, returning the invoice, the "
            "customer, the disputed amount and the stated reason."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "dispute_id": {
                    "type": "string",
                    "description": "The dispute id, e.g. `DSP-4471`.",
                }
            },
            "required": ["dispute_id"],
        },
        # Reading the seed file changes nothing, so the hint is true and Salvor
        # classifies this Read with no operator override needed.
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "issue_refund",
        "description": (
            "Issue the refund for a dispute by appending one durable line to "
            "the refund ledger. This is a write and the customer is refunded."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "dispute_id": {"type": "string"},
                "amount_usd": {"type": "number"},
                "approver": {
                    "type": "string",
                    "description": "Who authorised this refund.",
                },
                "note": {"type": "string", "description": "Why it was allowed."},
            },
            "required": ["dispute_id", "amount_usd", "approver"],
        },
        # Deliberately no annotation. An unannotated tool is a Write under
        # Salvor's conservative default, which is the correct reading of an
        # append: retrying an interrupted call would refund twice.
    },
    {
        "name": "send_notice",
        "description": (
            "Send the customer the notice for a dispute. Upserts one entry "
            "keyed by dispute id, so re-sending replaces rather than repeats."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "dispute_id": {"type": "string"},
                "message": {"type": "string"},
            },
            "required": ["dispute_id", "message"],
        },
        # The upsert really is idempotent, so trusting the hint is correct here.
        "annotations": {"idempotentHint": True},
    },
]


def send(message):
    """Write one JSON-RPC message as a single line, then flush."""
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def ok(payload, text):
    """A successful tool result: the text a model reads, plus the structured
    object a graph branch or a downstream node reads."""
    return {
        "content": [{"type": "text", "text": text}],
        "structuredContent": payload,
        "isError": False,
    }


def failed(text):
    """A tool-reported failure. Salvor surfaces `isError` as a handler error."""
    return {"content": [{"type": "text", "text": text}], "isError": True}


def load_disputes():
    with open(DISPUTES_FILE, encoding="utf-8") as handle:
        return json.load(handle)


def lookup_dispute(arguments):
    dispute_id = arguments.get("dispute_id", "")
    record = load_disputes().get(dispute_id)
    if record is None:
        return failed(f"no dispute {dispute_id!r} on file")
    return ok(record, f"{dispute_id}: {record['customer']} disputes "
              f"${record['amount_usd']:.2f} on {record['invoice_id']} "
              f"({record['reason']})")


def issue_refund(arguments):
    """The one genuine side effect in this example: append and fsync."""
    dispute_id = arguments.get("dispute_id", "")
    amount = arguments.get("amount_usd", 0)
    approver = arguments.get("approver", "unknown")
    note = arguments.get("note", "")
    line = json.dumps(
        {
            "dispute_id": dispute_id,
            "amount_usd": amount,
            "approver": approver,
            "note": note,
        },
        sort_keys=True,
    )
    # Append and force to disk. Once this returns the money is out the door,
    # exactly once. Nothing here retries and nothing here is undone.
    with open(LEDGER_PATH, "a", encoding="utf-8") as ledger:
        ledger.write(line + "\n")
        ledger.flush()
        os.fsync(ledger.fileno())
    payload = {
        "dispute_id": dispute_id,
        "amount_usd": amount,
        "approver": approver,
        "ledger_path": LEDGER_PATH,
    }
    return ok(payload, f"refunded ${amount} on {dispute_id}, approved by {approver}")


def send_notice(arguments):
    """Upsert one notice keyed by dispute id, then rewrite the whole file."""
    dispute_id = arguments.get("dispute_id", "")
    message = arguments.get("message", "")
    notices = {}
    if os.path.exists(NOTICES_PATH):
        with open(NOTICES_PATH, encoding="utf-8") as handle:
            notices = json.load(handle)
    notices[dispute_id] = message
    with open(NOTICES_PATH, "w", encoding="utf-8") as handle:
        json.dump(notices, handle, indent=2, sort_keys=True)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    return ok(
        {"dispute_id": dispute_id, "notices_path": NOTICES_PATH},
        f"notice sent to the customer on {dispute_id}",
    )


HANDLERS = {
    "lookup_dispute": lookup_dispute,
    "issue_refund": issue_refund,
    "send_notice": send_notice,
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
                "serverInfo": {"name": "salvor-disputes-desk", "version": "0.1.0"},
            },
        }

    # A notification (no id): the handshake's completion. No response.
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
        handler = HANDLERS.get(name)
        if handler is None:
            return {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32602, "message": f"unknown tool: {name}"},
            }
        return {"jsonrpc": "2.0", "id": request_id, "result": handler(arguments)}

    # Unknown method: an error for a request, silence for a notification.
    if request_id is not None:
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32601, "message": f"method not found: {method}"},
        }
    return None


def main():
    # Reading to EOF is also the shutdown path: when the salvor process that
    # spawned this one dies (including under `kill -9`), stdin closes and this
    # loop ends, so no server outlives the run that owns it.
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

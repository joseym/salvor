#!/usr/bin/env python3
"""The accounts-desk MCP server: the four tools this example's graph runs on.

Pure Python standard library. An MCP server over stdio is a program that reads
newline-delimited JSON-RPC 2.0 requests on stdin and writes responses on stdout,
so the four methods a Salvor run needs (initialize, tools/list, tools/call, and
ping) are handled by hand below. No `mcp` package, no venv, no Salvor code, and
no model anywhere in this example: the graph walks tool nodes only.

Four tools, one per step of a follow-up:

- `send_reminder` (Write): appends ONE line to the reminders ledger, keyed on
  the invoice id, so a second call for an invoice already reminded records
  nothing and says so. It carries no annotation, so Salvor's conservative
  default already reads it as a Write, and `agents/accounts-desk.toml` pins it
  besides. This is the step the timer proof cares about: it happens BEFORE the
  cool-off, and it must still have happened exactly once after the wake.
- `check_payment` (Read, `readOnlyHint: true`): reads the payments file and
  answers whether this invoice has been paid. Re-reading changes nothing, so an
  interrupted call is freely retried. This is the tool that reads the world, and
  the whole point of the example is WHEN it reads it: after the wake, not before
  the nap.
- `close_invoice` (Write): appends one line to the closed ledger. The paid arm.
- `escalate` (Write): appends one line to the escalations ledger. The unpaid
  arm.

A fifth tool, `await_payment_webhook`, is listed but is NOT in the graph and is
never called by `run.sh`. It is here to show the other half of the same idea: a
tool that parks the run it was called from on an EXTERNAL SIGNAL rather than on
a clock. MCP has no field for "park my caller", so the request rides in `_meta`,
the extension point the specification reserves on every result for metadata one
particular client understands; a host that is not Salvor sees an ordinary result
with an unfamiliar metadata key. Salvor reads `_meta.salvor.suspend`, records the
suspension with `kind: "signal"` (a wait nobody can answer by hand, so no
approval inbox lists it), and parks. The payment processor's webhook later
resumes the run with `{"paid": true}`, validated against the `input_schema` the
tool named. Swapping the graph's `cool_off` delay for this tool would trade "look
again in five days" for "continue the moment the money lands"; the example ships
the timer because a timer proves itself with no webhook to fire.

Every tool answers with `structuredContent` as well as text. Salvor records the
WHOLE tool result as the node's output, so `structuredContent` is what the
graph's branch expression reads (`structuredContent.paid == true`) and what the
next node sees as its input. Text alone would leave a branch with nothing typed
to route on.

A graph's `tool` node hands the node's input straight through as the tool call's
arguments, so every tool here reads its invoice id from either the bare
arguments (the graph input, at the first node) or from a `structuredContent`
object (the previous tool's whole result, at every node after it). `_invoice_id`
is that one rule, written once.

The run's input and every tool argument carry only that reference,
`invoice_id`. The customer name and the amount never travel through either:
they live in `LEDGER` below, this server's own record keyed by invoice id, and
every handler resolves them there instead of trusting them from the caller.
That is the practice ../../SECURITY.md asks for under "Pass references rather
than contents": everything a run records lands in a durable log that cannot be
edited or deleted, so personal data has to stay out of what gets recorded in
the first place, not be scrubbed from it afterward. `LEDGER` stands in for the
billing system a real accounts desk would look this up in.

Configuration comes from the environment, set by `run.sh` and inherited by this
process through the agent definition that spawns it:

- SALVOR_FOLLOWUP_PAYMENTS: the payments file `check_payment` reads. A JSON
  object keyed by invoice id. An absent file means nothing has been paid, which
  is what an accounts desk with no receipts on file actually knows.
- SALVOR_FOLLOWUP_REMINDERS: the reminders ledger `send_reminder` appends to.
- SALVOR_FOLLOWUP_CLOSED: the closed ledger `close_invoice` appends to.
- SALVOR_FOLLOWUP_ESCALATIONS: the escalations ledger `escalate` appends to.

All four default to plain names under the working directory, so the server runs
with no configuration at all; the example points every one of them at a scratch
path it owns, and nothing runtime ever lands in the repository.
"""

import json
import os
import sys

PAYMENTS_PATH = os.environ.get("SALVOR_FOLLOWUP_PAYMENTS", "payments.json")
REMINDERS_PATH = os.environ.get("SALVOR_FOLLOWUP_REMINDERS", "reminders.txt")
CLOSED_PATH = os.environ.get("SALVOR_FOLLOWUP_CLOSED", "closed-invoices.txt")
ESCALATIONS_PATH = os.environ.get("SALVOR_FOLLOWUP_ESCALATIONS", "escalations.txt")

INVOICE_ARG = {
    "type": "object",
    "properties": {
        "invoice_id": {"type": "string", "description": "The invoice id, e.g. `INV-2031`."},
    },
    "required": [],
}

# The accounts desk's own customer records, keyed by invoice id. A tool
# argument and the run's input carry only `invoice_id`; every handler below
# resolves the customer name and the amount here instead of trusting them from
# the caller, so neither ever has to cross into a tool argument or the run's
# input, and neither lands in Salvor's durable, unerasable log because of it.
LEDGER = {
    "INV-2031": {"customer": "Alder and Finch Joinery", "amount_cents": 128400},
}

TOOLS = [
    {
        "name": "send_reminder",
        "description": (
            "Send the customer the payment reminder for one invoice, by "
            "appending one durable line to the reminders ledger. Keyed on the "
            "invoice id: an invoice already reminded is not reminded twice."
        ),
        "inputSchema": INVOICE_ARG,
        # Deliberately no annotation. An unannotated tool is a Write under
        # Salvor's conservative default, which is the correct reading of a
        # notice going out to a customer.
    },
    {
        "name": "check_payment",
        "description": (
            "Read the payments file and report whether this invoice has been "
            "paid."
        ),
        "inputSchema": INVOICE_ARG,
        # Reading the payments file changes nothing, so the hint is true and
        # Salvor classifies this Read with no operator override needed.
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "close_invoice",
        "description": (
            "Close a paid invoice by appending one line to the closed ledger."
        ),
        "inputSchema": INVOICE_ARG,
    },
    {
        "name": "escalate",
        "description": (
            "Escalate an unpaid invoice to collections by appending one line "
            "to the escalations ledger."
        ),
        "inputSchema": INVOICE_ARG,
    },
    {
        "name": "await_payment_webhook",
        "description": (
            "Park the calling run until the payment processor's webhook "
            "reports on this invoice. Waits on a system, not a person."
        ),
        "inputSchema": INVOICE_ARG,
        # Nothing happens to the world here: the tool registers a wait and
        # returns. `readOnlyHint` is the honest annotation, and it also means a
        # call interrupted before its result was recorded is freely re-driven.
        "annotations": {"readOnlyHint": True},
    },
]

# What the webhook must send to resume a run parked by `await_payment_webhook`.
# Salvor records this schema in the log and validates the resume input against
# it, so a webhook that posts something else is refused rather than believed.
WEBHOOK_SCHEMA = {
    "type": "object",
    "properties": {
        "paid": {
            "type": "boolean",
            "description": "Whether the processor settled this invoice.",
        },
    },
    "required": ["paid"],
}


def send(message):
    """Write one JSON-RPC message as a single line, then flush."""
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def ok(payload, text):
    """A successful tool result: the text a reader reads, plus the structured
    object a graph branch or a downstream node reads."""
    return {
        "content": [{"type": "text", "text": text}],
        "structuredContent": payload,
        "isError": False,
    }


def failed(text):
    """A tool-reported failure. Salvor surfaces `isError` as a handler error."""
    return {"content": [{"type": "text", "text": text}], "isError": True}


def _invoice_id(arguments):
    """The invoice id this call concerns, and nothing else.

    A `tool` node's input is whatever the node before it produced, and for a
    tool node that is the previous tool's WHOLE result, so `invoice_id` sits
    one level down under `structuredContent`. At the first node the input is
    the graph input itself and `invoice_id` is right there. Prefer the nested
    object when it names an invoice, fall back to the bare arguments
    otherwise, so every tool here is fed the same way wherever it sits in the
    walk. This is the only field ever read off a call; look up `LEDGER` for
    everything else a handler needs.
    """
    structured = arguments.get("structuredContent")
    if isinstance(structured, dict) and structured.get("invoice_id"):
        return structured["invoice_id"]
    return arguments.get("invoice_id", "")


def _append(path, row):
    """Append one JSON line and force it to disk. Once this returns, the line
    is durable: nothing here retries and nothing here is undone."""
    line = json.dumps(row, sort_keys=True)
    with open(path, "a", encoding="utf-8") as ledger:
        ledger.write(line + "\n")
        ledger.flush()
        os.fsync(ledger.fileno())


def _holds(path, invoice_id):
    """Whether `path` already carries a line for this invoice. A missing file
    holds nothing, and an unparseable line is skipped rather than fatal."""
    if not os.path.exists(path):
        return False
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if row.get("invoice_id") == invoice_id:
                return True
    return False


def load_payments():
    """The payments on file. An absent or empty file means none."""
    if not os.path.exists(PAYMENTS_PATH):
        return {}
    with open(PAYMENTS_PATH, encoding="utf-8") as handle:
        text = handle.read()
    if not text.strip():
        return {}
    try:
        loaded = json.loads(text)
    except json.JSONDecodeError:
        return {}
    return loaded if isinstance(loaded, dict) else {}


def send_reminder(arguments):
    invoice_id = _invoice_id(arguments)
    if not invoice_id:
        return failed("no invoice_id in the arguments")
    info = LEDGER.get(invoice_id, {})
    if _holds(REMINDERS_PATH, invoice_id):
        # Keyed, so a re-drive that somehow reached this tool a second time
        # cannot send a second reminder. The recorded log already prevents
        # that; this is the tool refusing on its own account as well.
        return ok(
            {
                "invoice_id": invoice_id,
                "reminder_sent": False,
                "already_reminded": True,
                "ledger_path": REMINDERS_PATH,
            },
            f"{invoice_id} was already reminded; nothing sent",
        )
    # The reminders ledger is this desk's own record of what it sent, not
    # Salvor's log, so it carries what a human reading it needs: the name and
    # the amount, resolved from LEDGER rather than from the call.
    _append(
        REMINDERS_PATH,
        {
            "invoice_id": invoice_id,
            "customer": info.get("customer", ""),
            "amount_cents": info.get("amount_cents", 0),
        },
    )
    # The RETURNED result is what Salvor records, so it carries the reference
    # and a status only, never the name: see LEDGER's comment above.
    return ok(
        {
            "invoice_id": invoice_id,
            "reminder_sent": True,
            "already_reminded": False,
            "ledger_path": REMINDERS_PATH,
        },
        f"reminder sent on {invoice_id}",
    )


def check_payment(arguments):
    invoice_id = _invoice_id(arguments)
    if not invoice_id:
        return failed("no invoice_id in the arguments")
    payment = load_payments().get(invoice_id)
    paid = isinstance(payment, dict) and bool(payment.get("paid"))
    payload = {
        "invoice_id": invoice_id,
        "paid": paid,
        "payments_path": PAYMENTS_PATH,
    }
    verdict = "paid" if paid else "still unpaid"
    return ok(payload, f"{invoice_id} is {verdict} as of this reading")


def close_invoice(arguments):
    invoice_id = _invoice_id(arguments)
    if not invoice_id:
        return failed("no invoice_id in the arguments")
    info = LEDGER.get(invoice_id, {})
    if not _holds(CLOSED_PATH, invoice_id):
        _append(
            CLOSED_PATH,
            {
                "invoice_id": invoice_id,
                "customer": info.get("customer", ""),
                "amount_cents": info.get("amount_cents", 0),
                "outcome": "closed",
            },
        )
    return ok(
        {"invoice_id": invoice_id, "outcome": "closed", "ledger_path": CLOSED_PATH},
        f"{invoice_id} closed: payment received",
    )


def escalate(arguments):
    invoice_id = _invoice_id(arguments)
    if not invoice_id:
        return failed("no invoice_id in the arguments")
    info = LEDGER.get(invoice_id, {})
    if not _holds(ESCALATIONS_PATH, invoice_id):
        _append(
            ESCALATIONS_PATH,
            {
                "invoice_id": invoice_id,
                "customer": info.get("customer", ""),
                "amount_cents": info.get("amount_cents", 0),
                "outcome": "escalated",
            },
        )
    return ok(
        {
            "invoice_id": invoice_id,
            "outcome": "escalated",
            "ledger_path": ESCALATIONS_PATH,
        },
        f"{invoice_id} escalated to collections: no payment after the cool-off",
    )


def await_payment_webhook(arguments):
    """Park the calling run until the payment processor reports back.

    The tool does no work and changes nothing. What it returns is a request:
    an ordinary result, plus `_meta.salvor.suspend` saying why the run should
    park, what input will resume it, and that a SYSTEM rather than a person
    owes that input (`kind: "signal"`). Salvor records the tool call's
    completion first and the suspension second, so the call is settled and its
    idempotency claim released before the wait begins; a run parked here for a
    week blocks nothing and holds no process.

    Not in `invoice-follow-up.json` and not called by `run.sh`. It is here to
    be read, and to be swapped in by anyone who has a webhook to fire.
    """
    invoice_id = _invoice_id(arguments)
    if not invoice_id:
        return failed("no invoice_id in the arguments")
    result = ok(
        {"invoice_id": invoice_id, "awaiting": "payment_webhook"},
        f"{invoice_id}: waiting on the payment processor to report back",
    )
    result["_meta"] = {
        "salvor": {
            "suspend": {
                "reason": f"waiting on the payment webhook for {invoice_id}",
                "input_schema": WEBHOOK_SCHEMA,
                # Omit this key and the park is a human gate, which is what an
                # unnamed kind has always meant. Naming it is what keeps a wait
                # nobody can answer out of an approval inbox.
                "kind": "signal",
            }
        }
    }
    return result


HANDLERS = {
    "send_reminder": send_reminder,
    "check_payment": check_payment,
    "close_invoice": close_invoice,
    "escalate": escalate,
    "await_payment_webhook": await_payment_webhook,
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
                "serverInfo": {"name": "salvor-accounts-desk", "version": "0.1.0"},
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
    # spawned this one exits, stdin closes and this loop ends, so no server
    # outlives the run that owns it. That is why a sleeping run holds no
    # process: the CLI returns at the park and takes this child with it.
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

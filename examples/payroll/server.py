#!/usr/bin/env python3
"""The payroll-desk MCP server: the tools this example's graph runs on.

Pure Python standard library. An MCP server over stdio is a program that reads
newline-delimited JSON-RPC 2.0 requests on stdin and writes responses on stdout,
so the four methods a Salvor run needs (initialize, tools/list, tools/call, and
ping) are handled by hand below. No `mcp` package, no venv, no Salvor code.

Four tools:

- `pull_roster` (Read, `readOnlyHint: true`): resolves a pay-period id to the
  roster for that period: twelve employees, each with an id, a name, and a gross
  amount in cents. It stamps the period onto every row and reports the median,
  which the next tool uses to spot outliers. Re-running it re-reads the same
  file, so an interrupted lookup is freely retried.
- `flag_exceptions` (Read, `readOnlyHint: true`): scans the roster for amounts
  that are wildly off the median (more than five times it, or less than a fifth
  of it) and returns a structured review: the clean count, the flagged rows with
  a reason each, and the roster carried through unchanged. It computes, it never
  writes, so it is a Read.
- `pay_employee` (Idempotent, `idempotentHint: true`): the side effect. It
  appends ONE line to the pay ledger for one employee, keyed by
  `pay_period:employee_id`. Before appending it checks whether that key is
  already in the ledger, and if so it appends nothing and returns the same
  answer. That makes it exactly-once even when a crash re-drives the call: the
  engine re-executes a not-yet-completed idempotent call on resume (MCP does not
  carry the engine's key over the wire, so the tool owns the collapse), and this
  check is what stops a second charge landing.
- `send_summary` (Write): appends ONE payslip-run notice to the notices file.
  It carries no annotation, so Salvor's conservative default reads the append as
  a Write, and the agent pins it besides. The notice text is whatever the caller
  passes, so it stays true for any pay period.

Every tool answers with `structuredContent` as well as text. Salvor records the
WHOLE tool result as the node's output, so `structuredContent` is what the graph
document's branch expression reads (`structuredContent.flagged_count`), what the
map's `over` reference resolves against (`structuredContent.roster`), and what a
downstream node sees as its input. Text alone would leave a branch with nothing
typed to route on.

Configuration comes from the environment, set by the run script:

- SALVOR_PAYROLL_ROSTER: the seed roster file to read. Defaults to `roster.json`
  beside this file, so the server works with no configuration.
- SALVOR_PAYROLL_LEDGER: the pay ledger `pay_employee` appends to. Runtime state,
  never committed; it defaults to `pay-ledger.txt` under the working directory
  and the example points it at a scratch path.
- SALVOR_PAYROLL_NOTICES: the notices file `send_summary` upserts into, same
  defaulting story (`payslip-notices.json`).
- SALVOR_PAYROLL_PAY_DELAY_MS: an optional per-payment delay, so the run script
  can catch the batch part-way through and kill it there. Defaults to 0.
"""

import fcntl
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROSTER_FILE = os.environ.get("SALVOR_PAYROLL_ROSTER", os.path.join(HERE, "roster.json"))
LEDGER_PATH = os.environ.get("SALVOR_PAYROLL_LEDGER", "pay-ledger.txt")
NOTICES_PATH = os.environ.get("SALVOR_PAYROLL_NOTICES", "payslip-notices.json")
PAY_DELAY_MS = int(os.environ.get("SALVOR_PAYROLL_PAY_DELAY_MS", "0"))

# Outlier thresholds against the median, in multiples: above the high one or
# below the low one is flagged for a human. Desk policy, kept in one place.
HIGH_MULTIPLE = 5.0
LOW_DIVISOR = 5.0

TOOLS = [
    {
        "name": "pull_roster",
        "description": (
            "Resolve a pay-period id to its roster: twelve employees, each with "
            "an id, a name, and a gross amount in cents, plus the median amount."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "pay_period": {
                    "type": "string",
                    "description": "The pay-period id, e.g. `2025-11-B`.",
                }
            },
            "required": ["pay_period"],
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "flag_exceptions",
        "description": (
            "Review a roster for amounts far off the median and return a "
            "structured verdict: the clean count, the flagged rows with a reason "
            "each, and the roster carried through unchanged."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "structuredContent": {
                    "type": "object",
                    "description": "The pull_roster result this node received.",
                }
            },
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "pay_employee",
        "description": (
            "Pay one employee by appending one durable line to the pay ledger, "
            "keyed by pay_period:id. Re-running the same key appends nothing."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "name": {"type": "string"},
                "amount_cents": {"type": "integer"},
                "pay_period": {"type": "string"},
            },
            "required": ["id", "amount_cents"],
        },
        # The upsert-by-key really is idempotent, so the hint is honest and the
        # per-employee ledger key is what makes a re-executed call collapse.
        "annotations": {"idempotentHint": True},
    },
    {
        "name": "send_summary",
        "description": (
            "Append one payslip-run summary notice to the notices file. The "
            "message is recorded verbatim."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {"type": "string"},
            },
            "required": ["message"],
        },
        # Deliberately no annotation. An append is a Write under the conservative
        # default, which is the correct reading: re-running it would add a second
        # notice line.
    },
]


def send(message):
    """Write one JSON-RPC message as a single line, then flush."""
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def ok(payload, text):
    """A successful tool result: the text a reader sees, plus the structured
    object a graph branch, a map `over`, or a downstream node reads."""
    return {
        "content": [{"type": "text", "text": text}],
        "structuredContent": payload,
        "isError": False,
    }


def failed(text):
    """A tool-reported failure. Salvor surfaces `isError` as a handler error."""
    return {"content": [{"type": "text", "text": text}], "isError": True}


def load_roster_file():
    with open(ROSTER_FILE, encoding="utf-8") as handle:
        return json.load(handle)


def median_cents(rows):
    amounts = sorted(row["amount_cents"] for row in rows)
    count = len(amounts)
    if count == 0:
        return 0
    middle = count // 2
    if count % 2 == 1:
        return amounts[middle]
    return (amounts[middle - 1] + amounts[middle]) // 2


def pull_roster(arguments):
    pay_period = arguments.get("pay_period", "")
    periods = load_roster_file()
    rows = periods.get(pay_period)
    if rows is None:
        return failed(f"no roster for pay period {pay_period!r}")
    # Stamp the period onto every row so a single roster row, once the map hands
    # it to `pay_employee` on its own, still carries the key it is paid under.
    roster = [
        {"id": r["id"], "name": r["name"], "amount_cents": r["amount_cents"], "pay_period": pay_period}
        for r in rows
    ]
    median = median_cents(roster)
    payload = {
        "pay_period": pay_period,
        "roster": roster,
        "median_cents": median,
        "count": len(roster),
    }
    return ok(payload, f"roster for {pay_period}: {len(roster)} employees, median ${median / 100:.2f}")


def _roster_from(arguments):
    """The roster the review runs over. The node's input is the pull_roster
    result, so the roster is at `structuredContent.roster`; fall back to a bare
    `roster` and to a re-read by period so the tool is robust to how it is fed."""
    structured = arguments.get("structuredContent")
    if isinstance(structured, dict) and isinstance(structured.get("roster"), list):
        return structured["roster"], structured.get("pay_period", "")
    if isinstance(arguments.get("roster"), list):
        return arguments["roster"], arguments.get("pay_period", "")
    pay_period = arguments.get("pay_period", "")
    periods = load_roster_file()
    rows = periods.get(pay_period, [])
    roster = [
        {"id": r["id"], "name": r["name"], "amount_cents": r["amount_cents"], "pay_period": pay_period}
        for r in rows
    ]
    return roster, pay_period


def flag_exceptions(arguments):
    roster, pay_period = _roster_from(arguments)
    median = median_cents(roster)
    high = median * HIGH_MULTIPLE
    low = median / LOW_DIVISOR
    flagged = []
    for row in roster:
        amount = row["amount_cents"]
        if amount > high:
            reason = (
                f"amount ${amount / 100:.2f} is more than {HIGH_MULTIPLE:g}x the "
                f"median ${median / 100:.2f}"
            )
        elif amount < low:
            reason = (
                f"amount ${amount / 100:.2f} is less than a {LOW_DIVISOR:g}th of the "
                f"median ${median / 100:.2f}"
            )
        else:
            continue
        flagged.append(
            {"id": row["id"], "name": row["name"], "amount_cents": amount, "reason": reason}
        )
    payload = {
        "pay_period": pay_period,
        "roster": roster,
        "median_cents": median,
        "clean_count": len(roster) - len(flagged),
        "flagged_count": len(flagged),
        "flagged": flagged,
    }
    if flagged:
        text = f"{len(flagged)} of {len(roster)} rows flagged for review"
    else:
        text = f"all {len(roster)} rows within range, none flagged"
    return ok(payload, text)


def _key_present(handle, key):
    handle.seek(0)
    for line in handle:
        line = line.strip()
        if line and json.loads(line).get("key") == key:
            return True
    return False


def pay_employee(arguments):
    """Pay one employee, exactly once, keyed by pay_period:id.

    The check for the key and the append happen under one exclusive file lock, so
    two processes that both reach this employee (a resume re-driving a
    not-yet-completed iteration while the crashed process's orphaned tool child is
    still finishing its own write) cannot both slip past the check and append
    twice. Whoever takes the lock first writes; the other then sees the key and
    charges nothing more. That is what makes the payment exactly-once even under a
    kill that leaves a write in flight.
    """
    employee_id = arguments.get("id", "")
    amount = arguments.get("amount_cents", 0)
    name = arguments.get("name", "")
    pay_period = arguments.get("pay_period", "")
    key = f"{pay_period}:{employee_id}"

    # The delay (used by the run script to widen the crash window) is taken before
    # the lock so it does not hold the lock, and the exactly-once check is done
    # inside the lock, after the delay, right before the append.
    if PAY_DELAY_MS:
        time.sleep(PAY_DELAY_MS / 1000.0)

    with open(LEDGER_PATH, "a+", encoding="utf-8") as ledger:
        fcntl.flock(ledger.fileno(), fcntl.LOCK_EX)
        try:
            if _key_present(ledger, key):
                return ok(
                    {"id": employee_id, "amount_cents": amount, "key": key, "charged": False},
                    f"{employee_id} already paid under {key}",
                )
            line = json.dumps(
                {"id": employee_id, "name": name, "amount_cents": amount, "key": key},
                sort_keys=True,
            )
            # Append and force to disk. Once this returns the payment is out the
            # door, exactly once under this key.
            ledger.seek(0, os.SEEK_END)
            ledger.write(line + "\n")
            ledger.flush()
            os.fsync(ledger.fileno())
        finally:
            fcntl.flock(ledger.fileno(), fcntl.LOCK_UN)
    return ok(
        {"id": employee_id, "amount_cents": amount, "key": key, "charged": True},
        f"paid {employee_id} ${amount / 100:.2f} under {key}",
    )


def send_summary(arguments):
    """Append one payslip-run notice line to the notices file."""
    message = arguments.get("message", "")
    line = json.dumps({"message": message}, sort_keys=True)
    with open(NOTICES_PATH, "a", encoding="utf-8") as handle:
        handle.write(line + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    return ok(
        {"notices_path": NOTICES_PATH, "message": message},
        "summary notice appended",
    )


HANDLERS = {
    "pull_roster": pull_roster,
    "flag_exceptions": flag_exceptions,
    "pay_employee": pay_employee,
    "send_summary": send_summary,
}


def handle(request):
    """Answer one request. Returns a response dict, or None for a notification."""
    method = request.get("method")
    request_id = request.get("id")

    if method == "initialize":
        protocol_version = request.get("params", {}).get("protocolVersion", "2025-06-18")
        return {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "protocolVersion": protocol_version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "salvor-payroll-desk", "version": "0.1.0"},
            },
        }

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

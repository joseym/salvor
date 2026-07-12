"""A support-desk MCP server, in about a hundred lines of Python.

Same idea as `examples/python-tools/server.py`: an MCP server is a plain
program that advertises a few named functions ("tools") over stdin/stdout. A
host (a Salvor agent) launches it as a child process, asks what tools it has,
and calls them on the model's behalf. There is no Salvor package imported
here, no binding, no SDK beyond `mcp` itself.

The scenario is a support-ticket triage desk with four tools:

- `lookup_ticket` and `get_order_status` are reads: they only ever look
  something up.
- `post_reply` and `set_ticket_status` are both writes in the everyday sense
  (they change what the desk knows), but they behave differently under a
  retry, and that difference is the point of this example. See the comments
  on each tool, and `README.md`, for why one is pinned to `write` in
  `agent.toml` and the other is trusted as `idempotent` with no override.

Two kinds of file live here:

- `tickets.json` and `orders.json` are seed data: static, committed, and
  never written by this server. They stand in for a ticket system and an
  order system a support desk would normally reach over an internal API.
- `activity.jsonl` and `status_overrides.json` are runtime state, created on
  first write and not committed (see `.gitignore`). They hold everything the
  agent actually does to a ticket.
"""

import json
import os
from datetime import datetime, timezone
from pathlib import Path

from mcp.server.fastmcp import FastMCP
from mcp.types import ToolAnnotations

HERE = Path(__file__).parent
TICKETS_FILE = Path(os.environ.get("TICKETS_FILE", HERE / "tickets.json"))
ORDERS_FILE = Path(os.environ.get("ORDERS_FILE", HERE / "orders.json"))
ACTIVITY_LOG = Path(os.environ.get("ACTIVITY_LOG", HERE / "activity.jsonl"))
STATUS_STORE = Path(os.environ.get("STATUS_STORE", HERE / "status_overrides.json"))

VALID_STATUSES = {"open", "pending", "resolved", "escalated"}

mcp = FastMCP("support-desk")


def _load_json(path: Path) -> dict:
    if not path.exists():
        return {}
    return json.loads(path.read_text())


@mcp.tool(
    # A pure read: it looks at the seed ticket plus whatever runtime state
    # exists for it and reports the current picture. readOnlyHint=True lets
    # Salvor classify it as Read on its own; no override needed.
    annotations=ToolAnnotations(readOnlyHint=True),
)
def lookup_ticket(ticket_id: str) -> str:
    """Look up a support ticket: customer, subject, order, current status,
    the customer's original message, and any replies posted so far."""
    tickets = _load_json(TICKETS_FILE)
    ticket = tickets.get(ticket_id)
    if ticket is None:
        return f"No ticket found with id {ticket_id}."

    status = _load_json(STATUS_STORE).get(ticket_id, ticket["status"])

    replies = []
    if ACTIVITY_LOG.exists():
        for raw in ACTIVITY_LOG.read_text().splitlines():
            if not raw.strip():
                continue
            event = json.loads(raw)
            if event["ticket_id"] == ticket_id:
                replies.append(event["message"])

    lines = [
        f"Ticket {ticket_id}: {ticket['subject']}",
        f"Customer: {ticket['customer']} <{ticket['email']}>",
        f"Order: {ticket['order_id'] or 'none attached'}",
        f"Status: {status}",
        f"Opened: {ticket['opened']}",
        f"Customer message: {ticket['body']}",
    ]
    if replies:
        lines.append(f"Replies posted so far ({len(replies)}):")
        lines.extend(f"  - {message}" for message in replies)
    else:
        lines.append("No replies posted yet.")
    return "\n".join(lines)


@mcp.tool(
    # Also a pure read: orders.json is seed data this server never writes.
    annotations=ToolAnnotations(readOnlyHint=True),
)
def get_order_status(order_id: str) -> str:
    """Look up an order: items, shipment status, tracking number, and total."""
    orders = _load_json(ORDERS_FILE)
    order = orders.get(order_id)
    if order is None:
        return f"No order found with id {order_id}."
    items = ", ".join(order["items"])
    return (
        f"Order {order_id}: {items}\n"
        f"Shipment status: {order['shipment_status']}\n"
        f"Tracking: {order['tracking']}\n"
        f"Total: ${order['total']:.2f}\n"
        f"Note: {order['note']}"
    )


@mcp.tool(
    # idempotentHint is set true on purpose, and it is WRONG: this is the same
    # hazard `examples/python-tools/server.py` walks through. Read literally,
    # the hint says a retry under the same key is harmless. But the tool
    # APPENDS a line to activity.jsonl, so retrying an interrupted call does
    # not restore the same state; it posts the reply a second time. A customer
    # would see the same message twice. agent.toml overrides this tool to
    # `write` rather than trusting the hint; see README.md.
    annotations=ToolAnnotations(idempotentHint=True),
)
def post_reply(ticket_id: str, message: str) -> str:
    """Post a reply to a ticket's thread. The customer sees this message."""
    tickets = _load_json(TICKETS_FILE)
    if ticket_id not in tickets:
        return f"No ticket found with id {ticket_id}; reply not posted."
    event = {
        "ticket_id": ticket_id,
        "message": message,
        "ts": datetime.now(timezone.utc).isoformat(),
    }
    with ACTIVITY_LOG.open("a") as log:
        log.write(json.dumps(event) + "\n")
    return f"Posted reply to {ticket_id}."


@mcp.tool(
    # idempotentHint is also set true here, and this time it is RIGHT. The
    # tool does not append anything: it upserts one key in a dict, keyed by
    # ticket_id, and rewrites the whole file. Calling set_ticket_status(T,
    # "resolved") three times in a row leaves status_overrides.json in exactly
    # the state one call would: {"T": "resolved"}. A retry of an interrupted
    # call restores the same state rather than duplicating anything, which is
    # the actual definition of idempotent, not just a hint about it. Salvor's
    # default mapping reads idempotentHint=True as Idempotent, and agent.toml
    # takes no override here, because that default is correct for this tool.
    # Contrast with post_reply above, which carries the identical hint over an
    # append and is overridden precisely because the hint lies about it.
    annotations=ToolAnnotations(idempotentHint=True),
)
def set_ticket_status(ticket_id: str, status: str) -> str:
    """Set a ticket's status to one of: open, pending, resolved, escalated."""
    if status not in VALID_STATUSES:
        return f"'{status}' is not a valid status; use one of {sorted(VALID_STATUSES)}."
    tickets = _load_json(TICKETS_FILE)
    if ticket_id not in tickets:
        return f"No ticket found with id {ticket_id}; status not changed."
    overrides = _load_json(STATUS_STORE)
    overrides[ticket_id] = status
    STATUS_STORE.write_text(json.dumps(overrides, indent=2) + "\n")
    return f"Set {ticket_id} to {status}."


if __name__ == "__main__":
    # Serve over stdio: read requests on stdin, write responses on stdout.
    # This is the transport a Salvor agent spawns the server with.
    mcp.run()

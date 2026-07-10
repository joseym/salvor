"""An expense-tracker MCP server, in about eighty lines of Python.

If you have never touched the Model Context Protocol before, here is the whole
idea: an MCP server is a plain program that advertises a few named functions
("tools") over stdin/stdout. A host (here, a Salvor agent) launches this program
as a child process, asks it what tools it has, and calls them on the model's
behalf. Your Python function *is* the tool. There is no Salvor library to import,
no binding to compile, no SDK to learn beyond `mcp` itself. That is the point of
this example: a Python developer extends a Salvor agent purely by writing one of
these.

`FastMCP` does the protocol plumbing. You decorate a function with `@mcp.tool`,
and its type hints become the JSON Schema the model sees, its docstring becomes
the tool description, and its return value becomes the tool result.

The `annotations` on each tool are hints about side effects. Salvor reads them to
classify a tool as Read / Idempotent / Write, which decides how a call is
retried and resumed. They are only hints, though: a host may override them, and
this example's `agent.toml` does exactly that. See the README.
"""

import json
import os
from pathlib import Path

from mcp.server.fastmcp import FastMCP
from mcp.types import ToolAnnotations

# The ledger is one JSON object per line (JSONL). The path comes from an
# environment variable so the host controls where state lands; agent.toml sets
# it. A sensible default keeps the server runnable on its own for a quick poke.
LEDGER = Path(os.environ.get("EXPENSE_LEDGER", "ledger.jsonl"))

mcp = FastMCP("expenses")


@mcp.tool(
    # readOnlyHint is left off and idempotentHint is set true on purpose, to
    # show the hazard the README walks through: appending a line is NOT actually
    # idempotent (a retry writes a second, duplicate line), so an operator who
    # knows the tool pins it to Write in agent.toml rather than trusting this.
    annotations=ToolAnnotations(idempotentHint=True),
)
def add_expense(amount: float, category: str, note: str = "") -> str:
    """Record one expense. Appends a line to the ledger and returns a confirmation."""
    entry = {"amount": round(amount, 2), "category": category, "note": note}
    # Append mode: every call adds a line. This is the genuine write.
    with LEDGER.open("a") as ledger:
        ledger.write(json.dumps(entry) + "\n")
    return f"Recorded ${entry['amount']:.2f} in {category}."


@mcp.tool(
    # A pure read: it only ever reads the ledger file. readOnlyHint=True lets
    # Salvor classify it as Read on its own, with no override needed.
    annotations=ToolAnnotations(readOnlyHint=True),
)
def list_expenses() -> str:
    """List every recorded expense, one per line, as `amount  category  note`."""
    if not LEDGER.exists():
        return "No expenses recorded yet."
    lines = []
    for raw in LEDGER.read_text().splitlines():
        if not raw.strip():
            continue
        e = json.loads(raw)
        lines.append(f"${e['amount']:.2f}\t{e['category']}\t{e.get('note', '')}".rstrip())
    return "\n".join(lines) if lines else "No expenses recorded yet."


@mcp.tool(
    annotations=ToolAnnotations(readOnlyHint=True),
)
def total_by_category() -> str:
    """Sum spending per category, as `category: $total` lines, highest first."""
    if not LEDGER.exists():
        return "No expenses recorded yet."
    totals: dict[str, float] = {}
    for raw in LEDGER.read_text().splitlines():
        if not raw.strip():
            continue
        e = json.loads(raw)
        totals[e["category"]] = totals.get(e["category"], 0.0) + e["amount"]
    ranked = sorted(totals.items(), key=lambda kv: kv[1], reverse=True)
    return "\n".join(f"{cat}: ${total:.2f}" for cat, total in ranked)


if __name__ == "__main__":
    # Serve over stdio: read requests on stdin, write responses on stdout. This
    # is the transport a Salvor agent spawns the server with.
    mcp.run()

#!/usr/bin/env python3
"""A support desk, in Python, made durable by one middleware.

The agent is an ordinary LangChain ``create_agent``: a model, three tools, and a
thread id. The only salvor-shaped line in it is the middleware in
``create_agent``'s ``middleware`` list. Everything else here is what any
LangChain app already has, plus the printing this example's ``run.sh`` reads its
proofs out of.

``app.ts`` next door is the same desk, tool for tool and line for line, so a
reader can hold the two side by side and see that the durability is the
middleware's and not the language's.

It is driven by ``run.sh``, which passes everything as flags::

    python3 app.py --server http://127.0.0.1:18402 \\
                   --thread orders-7781 \\
                   --ask "Refund ORD-7781, the item arrived damaged."

Flags:
  --server URL         the control plane to record against (required)
  --thread ID          the LangGraph thread id, which is also the run
  --ask TEXT           the customer's question
  --crash-in TOOL      die with exit 9 inside TOOL, after its ledger write and
                       before it returns: a crash between a call happening and
                       salvor hearing about it
  --slow-tool TOOL=N   make TOOL take N seconds, so a second copy of this app
                       can try the same thread while this one holds it
  --finish             close the thread with ``finish_thread`` and exit

Ledgers land under SALVOR_EXAMPLE_SCRATCH (or the system temp directory). They
are this desk's own records, on the desk's side of the reference: the refund
identifiers and amounts live there, not in salvor's log.

No API key is needed. The model is a scripted stand-in that reads the
conversation so far and answers the way a real one would for this desk. Set
ANTHROPIC_API_KEY and it uses ``ChatAnthropic`` instead, with nothing else in
this file changing.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import tempfile
import time
from typing import Any, Optional

from langchain.agents import create_agent
from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import AIMessage
from langchain_core.outputs import ChatGeneration, ChatResult
from langchain_core.tools import tool

from salvor import Client
from salvor.langchain import (
    SalvorMiddleware,
    ToolNeedsResolution,
    current_tool_call,
    finish_thread,
    run_id_for_thread,
    salvor_error,
)

# --- the desk's flags -------------------------------------------------------

parser = argparse.ArgumentParser(description="a support desk recorded by salvor")
parser.add_argument("--server", default=os.environ.get("SALVOR_LC_SERVER", "http://127.0.0.1:18402"))
parser.add_argument("--thread", required=True)
parser.add_argument("--ask", default="")
parser.add_argument("--crash-in", dest="crash_in", default=None)
parser.add_argument("--slow-tool", dest="slow_tool", default=None, help="TOOL=SECONDS")
parser.add_argument("--finish", action="store_true")
OPTIONS = parser.parse_args()

SLOW_TOOL: Optional[str] = None
SLOW_SECONDS = 0.0
if OPTIONS.slow_tool:
    name, _, seconds = OPTIONS.slow_tool.partition("=")
    SLOW_TOOL = name
    SLOW_SECONDS = float(seconds or 5)


def say(line: str) -> None:
    """One line of the desk's own narration."""
    print("[desk] " + line, flush=True)


# --- the desk's ledgers -----------------------------------------------------
#
# Ordinary files, appended to by the tool bodies, exactly as `follow-up`'s MCP
# server keeps its reminders. They are the billing system's records rather than
# salvor's, which is the point: salvor holds the log of what was asked for and
# what came back, and the money lives on the far side of that reference.

SCRATCH = os.environ.get("SALVOR_EXAMPLE_SCRATCH") or tempfile.gettempdir()
os.makedirs(SCRATCH, exist_ok=True)

LEDGERS = {
    "lookups": os.path.join(SCRATCH, "salvor-langchain-py-lookups.jsonl"),
    "refunds": os.path.join(SCRATCH, "salvor-langchain-py-refunds.jsonl"),
    "large": os.path.join(SCRATCH, "salvor-langchain-py-large-refunds.jsonl"),
}


def append(path: str, row: dict) -> None:
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(row) + "\n")


def rows(path: str) -> list:
    """Every row a ledger holds, oldest first. A missing ledger holds nothing."""
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


# --- the desk's order book --------------------------------------------------
#
# The stand-in for a real order system. Every tool resolves the amount here,
# keyed by the order id, rather than trusting an amount from the caller.

ORDER_BOOK = {
    "ORD-7781": {"status": "paid", "total_cents": 4200},
    "ORD-8120": {"status": "paid", "total_cents": 15900},
    "ORD-3050": {"status": "paid", "total_cents": 2500},
    "ORD-9002": {"status": "paid", "total_cents": 1500},
    "ORD-4400": {"status": "paid", "total_cents": 240000},
    "ORD-5150": {"status": "paid", "total_cents": 3300},
}

# The desk's own limit, matching the `maximum` on `refund-order.toml` and the
# `minimum` on `refund-large.toml`. The model routes by it; the operator's
# schemas are what actually enforce it.
LARGE_REFUND_CENTS = 100_000


def dollars(cents: int) -> str:
    return "${:.2f}".format(cents / 100)


# --- the tools --------------------------------------------------------------

#: How many tool bodies this process actually ran. Replay leaves it at zero.
TOOL_BODIES = 0


def maybe_slow(name: str) -> None:
    """The slow-tool flag, so `run.sh` can hold a thread long enough to contest it."""
    if SLOW_TOOL != name:
        return
    say("SLOW TOOL: {} is holding the thread for {:g}s".format(name, SLOW_SECONDS))
    time.sleep(SLOW_SECONDS)


@tool
def lookup_order(order_id: str) -> dict:
    """Look up an order that has already been placed."""
    global TOOL_BODIES
    TOOL_BODIES += 1
    maybe_slow("lookup_order")
    order = ORDER_BOOK.get(order_id)
    if order is None:
        raise ValueError("no order named " + order_id)
    append(LEDGERS["lookups"], dict(order, order_id=order_id))
    say("lookup_order ran: {} is {}, {} cents".format(order_id, order["status"], order["total_cents"]))
    return {"order_id": order_id, "status": order["status"], "total_cents": order["total_cents"]}


def perform_refund(tool_name: str, ledger: str, order_id: str, amount_cents: int) -> dict:
    """The money, for both refund tools.

    The idempotency key comes from salvor: ``current_tool_call()`` hands back the
    key it derived for this call, and what it derived it from is the operator's
    choice, not the desk's. ``refund_large`` names no key fields, so its key is
    positional, a hash of ``(run, seq, tool)``: an attempt identifier, the same
    string on every attempt at that one call. ``refund_order`` declares
    ``idempotency_key = ["order_id"]``, so its key is a hash of
    ``(run, tool, order_id)`` with no position in it, and the same order refunded
    twice in one run derives one key both times.

    A real desk passes that key to its payment provider as the provider's own
    idempotency token. This one has no provider, so the ledger IS the provider: a
    key already on file returns the refund that key produced, and no second line
    is written. That is what makes the crash proof in ``run.sh`` cost one refund
    rather than two.
    """
    global TOOL_BODIES
    TOOL_BODIES += 1
    maybe_slow(tool_name)

    call = current_tool_call()
    key = call.key if call else "no-key"

    on_file = next((row for row in rows(ledger) if row.get("idempotency_key") == key), None)
    if on_file is not None:
        say("{}: key {}... is already on file; no second refund".format(tool_name, key[:20]))
        return {
            "order_id": on_file["order_id"],
            "amount_cents": on_file["amount_cents"],
            "refund_id": on_file["refund_id"],
            "status": "succeeded",
        }

    refund = {
        "order_id": order_id,
        "amount_cents": amount_cents,
        "refund_id": "re_" + key[-12:],
        "status": "succeeded",
    }
    append(ledger, dict(refund, tool=tool_name, idempotency_key=key))
    say(
        "{} moved money: {} on {} as {}".format(
            tool_name, dollars(amount_cents), order_id, refund["refund_id"]
        )
    )

    # The crash the whole design is for: the refund has happened and the ledger
    # says so, and this process dies before salvor is told. The log is left
    # ending at this call's intent. `os._exit` rather than `sys.exit`, because a
    # raised SystemExit would be caught and reported like any other failure.
    if OPTIONS.crash_in == tool_name:
        say("crashing inside {}, after the money moved and before salvor heard".format(tool_name))
        sys.stdout.flush()
        os._exit(9)

    return refund


@tool
def refund_order(order_id: str, amount_cents: int) -> dict:
    """Refund an order in full, up to the desk's own limit."""
    return perform_refund("refund_order", LEDGERS["refunds"], order_id, amount_cents)


@tool
def refund_large(order_id: str, amount_cents: int) -> dict:
    """Refund an order too large for the desk to close on its own say-so."""
    return perform_refund("refund_large", LEDGERS["large"], order_id, amount_cents)


# --- the model --------------------------------------------------------------

#: How many times this process actually called a model. Replay leaves it at zero.
MODEL_CALLS = 0


def last_tool_result(messages: list) -> dict:
    """The last tool result in the conversation, parsed back from its message."""
    for message in reversed(messages):
        if message.type == "tool":
            return json.loads(str(message.content))
    return {}


def next_turn(messages: list) -> dict:
    """What the desk's model says next, decided entirely by the conversation so far.

    Turn 0 looks the order up. Turn 1 reads that lookup out of the tool message
    and either refunds (through the tool the amount calls for) or answers. Turn 2
    reads the refund out of its tool message and closes out. A real model would
    decide the same three things from the same three inputs, which is why
    swapping ``ChatAnthropic`` in below changes nothing else.

    One question takes a shorter path: a ticket that names its own amount and
    says the refund is on it twice. There is nothing to look up, and a model
    reading a duplicated line item asks for the refund twice in the one turn.
    That is the shape ``refund_order``'s declared ``idempotency_key`` exists for.
    """
    question = next((str(m.content) for m in messages if m.type == "human"), "")
    found = re.search(r"ORD-\d+", question)
    order_id = found.group(0) if found else "ORD-0000"
    wants_refund = "refund" in question.lower()
    listed_twice = "twice" in question.lower()
    stated = re.search(r"(\d+) cents", question)
    stated_cents = int(stated.group(1)) if stated else 0
    turn = len([m for m in messages if m.type == "ai"])

    if turn == 0 and wants_refund and listed_twice and stated_cents > 0:
        args = {"order_id": order_id, "amount_cents": stated_cents}
        return {
            "content": "Refunding {}; the ticket lists it twice.".format(order_id),
            "tool_calls": [
                {"name": "refund_order", "args": args, "id": "call-refund-first"},
                # The same arguments, a second time. The two calls need distinct
                # ids because that is how LangChain tells one tool call from
                # another, and how the middleware ranks them within the turn.
                {"name": "refund_order", "args": dict(args), "id": "call-refund-again"},
            ],
        }

    if turn == 0:
        return {
            "content": "Looking up {}.".format(order_id),
            "tool_calls": [
                {"name": "lookup_order", "args": {"order_id": order_id}, "id": "call-lookup"}
            ],
        }

    if turn == 1 and not listed_twice:
        order = last_tool_result(messages)
        total = int(order.get("total_cents", 0))
        if not wants_refund:
            return {
                "content": "{} is {}, {}. Nothing to refund.".format(
                    order_id, order.get("status"), dollars(total)
                )
            }
        name = "refund_large" if total >= LARGE_REFUND_CENTS else "refund_order"
        return {
            "content": "Refunding {}.".format(order_id),
            "tool_calls": [
                {
                    "name": name,
                    "args": {"order_id": order_id, "amount_cents": total},
                    "id": "call-refund",
                }
            ],
        }

    refund = last_tool_result(messages)
    return {
        "content": "Refunded {} on {}; the provider has it as {}.".format(
            dollars(int(refund.get("amount_cents", 0))), order_id, refund.get("refund_id")
        )
    }


class ScriptedModel(BaseChatModel):
    """A hand-rolled model, not one of the fakes in
    langchain_core.language_models.fake_chat_models: those cannot script a
    multi-turn tool-calling agent, and a bind_tools that rebuilds the model
    drops anything attached to the instance it replaces."""

    @property
    def _llm_type(self) -> str:
        return "scripted"

    def bind_tools(self, tools, **kwargs):
        return self

    def _generate(self, messages, stop=None, run_manager=None, **kwargs) -> ChatResult:
        global MODEL_CALLS
        MODEL_CALLS += 1
        step = next_turn(messages)
        message = AIMessage(
            content=step["content"],
            tool_calls=[dict(call, type="tool_call") for call in step.get("tool_calls", [])],
        )
        return ChatResult(generations=[ChatGeneration(message=message)])


def choose_model():
    """The real provider, when there is a key for one.

    Nothing else in this file changes: the tools, the middleware, the thread id
    and every proof ``run.sh`` makes are the same, because salvor records the
    call and never the provider.
    """
    if not os.environ.get("ANTHROPIC_API_KEY"):
        return ScriptedModel(), "scripted (no ANTHROPIC_API_KEY set)"
    from langchain_anthropic import ChatAnthropic

    name = os.environ.get("SALVOR_LC_MODEL", "claude-opus-5")
    return ChatAnthropic(model=name), "ChatAnthropic " + name


# --- what a message says about itself ---------------------------------------


def marker_of(message: Any) -> str:
    """The marker the middleware puts on every AI message it returns.

    ``replayed`` when the answer came out of the log, ``live`` when this invoke
    really called the model on a path the log still agrees with, and ``forked``
    from the point the invoke left the recorded path onward.
    """
    mark = (getattr(message, "response_metadata", None) or {}).get("salvor")
    if not mark:
        return "none"
    if mark.get("replayed"):
        return "replayed@{}".format(mark["seq"])
    if mark.get("live"):
        return "live@{}".format(mark["seq"])
    if mark.get("forked"):
        return "forked@{}".format(mark["forked"]["at"])
    return "unknown"


# --- the run ----------------------------------------------------------------

FORKS = 0
MARKERS: list = []


def print_counts() -> None:
    """The counts every path prints, so a refused invoke says what it did not do."""
    calls = "unavailable (real provider)" if os.environ.get("ANTHROPIC_API_KEY") else str(MODEL_CALLS)
    print("MODEL CALLS: " + calls, flush=True)
    print("TOOL BODIES: {}".format(TOOL_BODIES), flush=True)
    print("MARKERS: " + (",".join(MARKERS) or "none"), flush=True)
    print("FORKS: {}".format(FORKS), flush=True)


def one_line(text: str) -> str:
    """A message flattened to one line, so a shell can grep a sentence out of it."""
    return " ".join(str(text).split())


def on_fork(notice) -> None:
    # A fork is not an error: the invoke carries on and appends to the log. This
    # is where an application routes the notice; the default logs a warning.
    global FORKS
    FORKS += 1
    say("FORK at seq {}: {}".format(notice.at, one_line(notice.message)))


def main() -> int:
    global MARKERS

    client = Client(OPTIONS.server)
    print("RUN: " + run_id_for_thread(OPTIONS.thread), flush=True)
    print("THREAD: " + OPTIONS.thread, flush=True)

    # Closing the thread out. A thread's run stays open until something says it
    # is over, because a task that looks finished today may get one more turn
    # tomorrow.
    if OPTIONS.finish:
        finished = finish_thread(client, OPTIONS.thread)
        print("FINISHED: run={} seq={}".format(finished.run_id, finished.seq), flush=True)
        return 0

    model, name = choose_model()
    say("model: " + name)

    agent = create_agent(
        model=model,
        tools=[lookup_order, refund_order, refund_large],
        middleware=[SalvorMiddleware(client, on_fork=on_fork)],
    )

    try:
        answer = agent.invoke(
            {"messages": [{"role": "user", "content": OPTIONS.ask}]},
            {"configurable": {"thread_id": OPTIONS.thread}},
        )
    except Exception as error:  # noqa: BLE001 - every refusal comes through here
        refusal = salvor_error(error)
        if refusal is None:
            raise  # the app's own error, unchanged

        if isinstance(refusal, ToolNeedsResolution):
            # A `trust_completion = false` tool ran and salvor will not take this
            # process's word for what it did. The run holds the intent; a person
            # confirms the refund and records it.
            print_counts()
            print(
                "NEEDS RESOLUTION: "
                + json.dumps(
                    {
                        "run": refusal.run_id,
                        "seq": refusal.seq,
                        "tool": refusal.tool,
                        "key": refusal.key,
                        "output": refusal.output,
                    }
                ),
                flush=True,
            )
            say(one_line(refusal.message))
            return 4

        print_counts()
        print("REFUSED {}: {}".format(refusal.code, one_line(refusal.message)), flush=True)
        if refusal.lapses_in_seconds is not None:
            print("LAPSES IN: {}".format(refusal.lapses_in_seconds), flush=True)
        return 3

    MARKERS = [marker_of(m) for m in answer["messages"] if m.type == "ai"]
    print_counts()
    print("ANSWER: " + one_line(answer["messages"][-1].content), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())

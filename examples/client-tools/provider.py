#!/usr/bin/env python3
"""A stand-in payment provider, owned by this example and run by the CLIENT.

Nothing in salvor knows this file exists. It is never named in a declaration,
never passed to `salvor serve`, and never spawned by the control plane. The
refund desk (`desk.py`) runs it as a subprocess of itself, in its own process
tree, with a credential it reads from its own environment.

Two operations, both writes:

    provider.py refund --idempotency-key K --order ORD-7781 \
        --amount-cents 4200 --currency USD [--reason "..."]
    provider.py payout --idempotency-key K --payee "****4417" \
        --amount-cents 240000 --currency USD [--reference "..."]

Each prints one JSON object on stdout: what a real provider's API client would
hand back to the code that called it.

The part that matters for the example is the ledger. Every call is filed under
its idempotency key, and a second call presenting a key already on file gets
back the SAME refund or transfer rather than a new one. That is what makes the
key salvor derives load-bearing instead of decorative: the desk can lose a
response, retry, and not pay twice, but only because it retried under the key
it was given rather than under a fresh one it made up.

The ledger lives at $SALVOR_EXAMPLE_PROVIDER_LEDGER (one JSON object per line)
and is the example's proof: `created` lines are money moving, `replayed` lines
are money not moving.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path
from typing import Any, NoReturn, Optional

# The credential. A real provider client would sign requests with it. Here it
# only has to be present, because its presence is the claim being made: this
# secret lives in the desk's process, is inherited by this subprocess, and is
# never exported into the environment `salvor serve` was started in.
CREDENTIAL_ENV = "REFUND_PROVIDER_API_KEY"

# Where the ledger is kept. The desk and `run.sh` both read it; nothing else
# writes it.
LEDGER_ENV = "SALVOR_EXAMPLE_PROVIDER_LEDGER"
DEFAULT_LEDGER = Path(os.environ.get("TMPDIR", "/tmp")) / "salvor-client-tools-provider.jsonl"


def fail(message: str, code: int = 2) -> NoReturn:
    """Stop with a message on stderr, the way a CLI a real script shells out to
    would, rather than a traceback the caller has to parse."""
    print(f"[provider] {message}", file=sys.stderr)
    sys.exit(code)


def ledger_path() -> Path:
    return Path(os.environ.get(LEDGER_ENV) or DEFAULT_LEDGER)


def read_ledger() -> list[dict[str, Any]]:
    """Every entry on file, oldest first. A missing file is an empty ledger."""
    path = ledger_path()
    if not path.exists():
        return []
    entries = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            entries.append(json.loads(line))
    return entries


def append_ledger(entry: dict[str, Any]) -> None:
    path = ledger_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as handle:
        handle.write(json.dumps(entry) + "\n")
        handle.flush()
        os.fsync(handle.fileno())


def existing(action: str, key: str) -> Optional[dict[str, Any]]:
    """The first entry that created something under `key` for `action`.

    Only `created` entries are searched. A `replayed` entry is a record that a
    duplicate arrived, not a second thing to collapse onto.
    """
    for entry in read_ledger():
        if entry.get("action") == action and entry.get("idempotency_key") == key:
            if entry.get("outcome") == "created":
                return entry
    return None


def mint(prefix: str, key: str) -> str:
    """A plausible provider identifier, derived from the key so a reader can see
    for themselves that the same key and the same id go together."""
    digest = hashlib.sha256(key.encode()).hexdigest()
    return f"{prefix}_{digest[:12]}"


def perform(action: str, prefix: str, key: str, amount_cents: int, detail: dict[str, Any]) -> None:
    """The whole provider, for both operations.

    A key already on file returns what it returned the first time. Anything else
    creates one thing, files it, and returns it.
    """
    if not os.environ.get(CREDENTIAL_ENV):
        fail(
            f"{CREDENTIAL_ENV} is not set in this process. This script needs the "
            "payment credential, which is exactly why it runs here and not inside "
            "salvor."
        )
    if not key:
        fail("--idempotency-key is required; the desk gets it from salvor, not from itself")

    id_field = f"provider_{'refund' if action == 'refund' else 'transfer'}_id"
    prior = existing(action, key)
    if prior is not None:
        identifier = prior[id_field]
        entries = read_ledger()
        append_ledger(
            {
                "n": len(entries) + 1,
                "action": action,
                "outcome": "replayed",
                "idempotency_key": key,
                id_field: identifier,
                "amount_cents": prior["amount_cents"],
                "note": "same key already on file; no second movement of money",
                **detail,
            }
        )
        print(
            json.dumps(
                {
                    id_field: identifier,
                    "status": "succeeded",
                    "amount_cents": prior["amount_cents"],
                    "replayed": True,
                }
            )
        )
        return

    identifier = mint(prefix, key)
    entries = read_ledger()
    append_ledger(
        {
            "n": len(entries) + 1,
            "action": action,
            "outcome": "created",
            "idempotency_key": key,
            id_field: identifier,
            "amount_cents": amount_cents,
            **detail,
        }
    )
    print(
        json.dumps(
            {
                id_field: identifier,
                "status": "succeeded",
                "amount_cents": amount_cents,
                "replayed": False,
            }
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser(description="a stand-in payment provider the client runs")
    sub = parser.add_subparsers(dest="command", required=True)

    refund = sub.add_parser("refund", help="refund a card charge")
    refund.add_argument("--idempotency-key", required=True)
    refund.add_argument("--order", required=True)
    refund.add_argument("--amount-cents", type=int, required=True)
    refund.add_argument("--currency", required=True)
    refund.add_argument("--reason", default="")

    payout = sub.add_parser("payout", help="send a bank transfer")
    payout.add_argument("--idempotency-key", required=True)
    payout.add_argument("--payee", required=True)
    payout.add_argument("--amount-cents", type=int, required=True)
    payout.add_argument("--currency", required=True)
    payout.add_argument("--reference", default="")

    args = parser.parse_args()

    if args.command == "refund":
        perform(
            "refund",
            "re",
            args.idempotency_key,
            args.amount_cents,
            {"order_id": args.order, "currency": args.currency, "reason": args.reason},
        )
    else:
        perform(
            "payout",
            "tr",
            args.idempotency_key,
            args.amount_cents,
            {"payee": args.payee, "currency": args.currency, "reference": args.reference},
        )


if __name__ == "__main__":
    main()

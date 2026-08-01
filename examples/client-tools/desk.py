"""A refund desk whose money moves in this process, recorded by a salvor it
never gives the payment code to.

The desk owns the loop (a client-driven run), so it decides when to call the
model and when to call the provider. What it does not own is any of the four
things salvor keeps: the effect class of a refund, the schema its input must
satisfy, the schema its result must carry, and the idempotency key it has to
perform under. Those come from the operator's declarations, fetched here over
`GET /v1/client-tools` and handed to the model verbatim as function
definitions, so the desk never keeps a second copy of a schema the server is
validating against.

`provider.py` is this example's stand-in payment provider. The desk runs it as
its own subprocess with a credential from its own environment. Salvor is never
told it exists.

Three cases, each its own run:

1. happy path: fetch, ask the model, open the intent, charge under the derived
   key, record the completion carrying the provider's id.
2. missing evidence: the desk reports a result with no provider id in it. The
   completion is refused and nothing is recorded, so the run sits on an
   unsettled write. The desk then retries the same call, under the same derived
   key, and the provider returns the same refund rather than a second one.
3. untrusted tool: a second tool declared `trust_completion = false`. The
   client's completion is refused whatever it carries, the run parks at
   NeedsReconciliation, and a person settles it through resolve.

Bring the offline stack up first (see ../README.md or ../run.sh), then:

    python desk.py http://127.0.0.1:18964
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any, NoReturn, Optional

import httpx

from salvor import Client, ClientToolDecl
from salvor.client_runs import ClientRunDriver
from salvor.errors import SalvorAPIError

EXAMPLE_DIR = Path(__file__).resolve().parent
PROVIDER = EXAMPLE_DIR / "provider.py"

# The desk's own agent identity. A client-driven run's `RunStarted` carries
# whatever hash the client puts there; nothing is registered for it, because
# this desk's loop is written here rather than described in an agent file.
AGENT = "sha256:client-tools-refund-desk"

MODEL = "salvor-demo-model"


def say(text: str = "") -> None:
    print(f"[desk] {text}" if text else "[desk]")


def fail(message: str) -> NoReturn:
    print(f"[desk] ERROR: {message}", file=sys.stderr)
    sys.exit(1)


# -- the provider, in this process ------------------------------------------


def call_provider(action: str, key: str, **fields: Any) -> dict[str, Any]:
    """Run `provider.py` as a subprocess of this desk, under `key`.

    This is the seam the whole example is about. The command below is built
    here, runs here, and inherits this process's environment, including the
    payment credential. No part of it is known to salvor: not the path, not the
    arguments, not the credential, not the output before this function reports
    it.
    """
    argv = [sys.executable, str(PROVIDER), action, "--idempotency-key", key]
    for name, value in fields.items():
        argv += [f"--{name.replace('_', '-')}", str(value)]
    printable = " ".join(["provider.py", action, "--idempotency-key", key[:19] + "..."])
    say(f"  running in this process: {printable} ...")
    result = subprocess.run(argv, capture_output=True, text=True)
    if result.returncode != 0:
        fail(f"the provider failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


# -- driving one run ---------------------------------------------------------


def function_definitions(decls: list[ClientToolDecl]) -> list[dict[str, Any]]:
    """The model's tools, built from what the operator declared.

    A declaration's `input_schema` IS the parameter schema, so this is a
    rename and nothing else. The desk holds no schema of its own to drift.
    """
    return [
        {
            "name": decl.name,
            "description": decl.input_schema.get("description", decl.name),
            "input_schema": decl.input_schema,
        }
        for decl in decls
    ]


def model_request(system: str, tools: list[dict[str, Any]], messages: list[Any]) -> dict[str, Any]:
    return {
        "model": MODEL,
        "max_tokens": 512,
        "system": system,
        "tools": tools,
        "messages": messages,
    }


def tool_use_block(response: Any) -> dict[str, Any]:
    for block in response.get("content", []):
        if block.get("type") == "tool_use":
            return block
    fail(f"the model asked for no tool: {json.dumps(response)[:200]}")


def text_of(response: Any) -> str:
    return "".join(
        block.get("text", "") for block in response.get("content", []) if block.get("type") == "text"
    )


def start_run(client: Client, case: str, task: dict[str, Any]) -> ClientRunDriver:
    run = client.open_client_run()
    run.append([run.envelope(0, "RunStarted", agent_def_hash=AGENT, input=task, labels={"case": case})])
    say(f"  run {run.run_id}")
    return run


def print_log(run: ClientRunDriver) -> None:
    for event in run.log():
        detail = ""
        if event.kind == "ToolCallRequested":
            payload = event.payload
            detail = (
                f"  {payload.get('tool')} [{payload.get('effect')}]"
                f" performed_by={payload.get('performed_by')}"
                f" key={str(payload.get('idempotency_key'))[:19]}..."
            )
        elif event.kind == "ToolCallCompleted":
            detail = f"  {json.dumps(event.output)}"
        elif event.kind == "ModelCallCompleted":
            usage = event.usage
            detail = f"  usage in {usage.input_tokens} out {usage.output_tokens}" if usage else ""
        say(f"    seq {event.seq}  {event.kind}{detail}")


def state_of(client: Client, run: ClientRunDriver) -> str:
    return client.get_run(run.run_id).status.state


def finish(run: ClientRunDriver, request: dict[str, Any], tool_use: dict[str, Any], output: Any) -> str:
    """The second model turn and the run's own completion event.

    The tool result goes back to the model the ordinary way, as a
    `tool_result` block, so the transcript the model sees is the transcript
    salvor recorded.
    """
    messages = request["messages"] + [
        {"role": "assistant", "content": [tool_use]},
        {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": tool_use["id"],
                    "content": json.dumps(output),
                }
            ],
        },
    ]
    closing = run.model_step(5, {**request, "messages": messages})
    summary = text_of(closing.response)
    run.append([run.envelope(7, "RunCompleted", output={"summary": summary})])
    return summary


# -- case 1: the happy path --------------------------------------------------


def happy_path(client: Client, tools: list[dict[str, Any]]) -> None:
    say("CASE 1: a refund the client performs and salvor records")
    system = (
        "You are Northwind's refund desk. Decide the one refund action this case "
        "needs and call the declared tool. Case file: happy-path."
    )
    run = start_run(client, "happy-path", {"ticket": "RT-3391", "order": "ORD-7781"})

    request = model_request(
        system,
        tools,
        [{"role": "user", "content": "ORD-7781 arrived damaged. The customer kept the replacement."}],
    )
    step = run.model_step(1, request)
    call = tool_use_block(step.response)
    say(f"  the model asked for {call['name']}: {json.dumps(call['input'])}")

    opened = run.client_tool_intent(3, call["name"], call["input"])
    say(f"  intent recorded at seq {opened.seq}, effect {opened.effect} (the operator's, not ours)")
    say(f"  the server derived the key: {opened.idempotency_key}")

    charged = call_provider(
        "refund",
        opened.idempotency_key,
        order=call["input"]["order_id"],
        amount_cents=call["input"]["amount_cents"],
        currency=call["input"]["currency"],
        reason=call["input"].get("reason", ""),
    )
    say(f"  the provider answered: {json.dumps(charged)}")

    run.client_tool_completion(
        3,
        {
            "provider_refund_id": charged["provider_refund_id"],
            "status": charged["status"],
            "amount_cents": charged["amount_cents"],
        },
    )
    say("  completion recorded")

    summary = finish(run, request, call, charged)
    say(f"  the model closed out: {summary}")
    say(f"  run state: {state_of(client, run)}")
    print_log(run)
    say()


# -- case 2: a completion carrying no evidence -------------------------------


def missing_evidence(client: Client, tools: list[dict[str, Any]]) -> None:
    say("CASE 2: a completion with no provider id in it")
    system = (
        "You are Northwind's refund desk. Decide the one refund action this case "
        "needs and call the declared tool. Case file: missing-evidence."
    )
    run = start_run(client, "missing-evidence", {"ticket": "RT-3402", "order": "ORD-8120"})

    request = model_request(
        system,
        tools,
        [{"role": "user", "content": "ORD-8120 was charged twice for one renewal."}],
    )
    step = run.model_step(1, request)
    call = tool_use_block(step.response)
    say(f"  the model asked for {call['name']}: {json.dumps(call['input'])}")

    opened = run.client_tool_intent(3, call["name"], call["input"])
    say(f"  the server derived the key: {opened.idempotency_key}")

    charged = call_provider(
        "refund",
        opened.idempotency_key,
        order=call["input"]["order_id"],
        amount_cents=call["input"]["amount_cents"],
        currency=call["input"]["currency"],
        reason=call["input"].get("reason", ""),
    )
    say(f"  the provider answered: {json.dumps(charged)}")

    # What a desk reports when its own wrapper summarised the call and dropped
    # the one field it could not have invented. Shape-wise it looks fine.
    thin = {"status": charged["status"], "amount_cents": charged["amount_cents"]}
    say(f"  reporting a completion with no provider id: {json.dumps(thin)}")
    try:
        run.client_tool_completion(3, thin)
    except SalvorAPIError as error:
        say(f"  REFUSED {error.code}: {error.message}")
    else:
        fail("the completion carrying no provider id was recorded; it should not have been")

    say("  the log after the refusal (nothing was written):")
    print_log(run)
    say(f"  run state: {state_of(client, run)}")

    # The retry. The intent is re-posted at the same position, byte for byte,
    # which re-derives the same key and writes nothing; the provider then
    # returns the refund it already made rather than making a second one.
    again = run.client_tool_intent(3, call["name"], call["input"])
    say(f"  re-posted the intent at seq 3; same key: {again.idempotency_key == opened.idempotency_key}")
    retried = call_provider(
        "refund",
        again.idempotency_key,
        order=call["input"]["order_id"],
        amount_cents=call["input"]["amount_cents"],
        currency=call["input"]["currency"],
        reason=call["input"].get("reason", ""),
    )
    say(f"  the provider answered: {json.dumps(retried)}")
    if retried["provider_refund_id"] != charged["provider_refund_id"]:
        fail(
            "the retry under the same key produced a different refund "
            f"({retried['provider_refund_id']} != {charged['provider_refund_id']}); "
            "the customer was refunded twice"
        )
    say(f"  same refund as the first attempt: {retried['provider_refund_id']}")

    run.client_tool_completion(
        3,
        {
            "provider_refund_id": retried["provider_refund_id"],
            "status": retried["status"],
            "amount_cents": retried["amount_cents"],
        },
    )
    say("  completion recorded, now that it carries the provider's id")

    summary = finish(run, request, call, retried)
    say(f"  the model closed out: {summary}")
    say(f"  run state: {state_of(client, run)}")
    print_log(run)
    say()


# -- case 3: trust_completion = false ----------------------------------------


def untrusted_payout(client: Client, tools: list[dict[str, Any]]) -> None:
    say("CASE 3: a tool declared trust_completion = false")
    system = (
        "You are Northwind's refund desk. Decide the one refund action this case "
        "needs and call the declared tool. Case file: untrusted-payout."
    )
    run = start_run(client, "untrusted-payout", {"ticket": "RT-3417", "order": "ORD-6650"})

    request = model_request(
        system,
        tools,
        [
            {
                "role": "user",
                "content": "ORD-6650's card expired before we could refund it. Pay the customer out.",
            }
        ],
    )
    step = run.model_step(1, request)
    call = tool_use_block(step.response)
    say(f"  the model asked for {call['name']}: {json.dumps(call['input'])}")

    opened = run.client_tool_intent(3, call["name"], call["input"])
    say(f"  intent recorded at seq {opened.seq}, effect {opened.effect}")
    say(f"  the server derived the key: {opened.idempotency_key}")

    sent = call_provider(
        "payout",
        opened.idempotency_key,
        payee=call["input"]["payee_account"],
        amount_cents=call["input"]["amount_cents"],
        currency=call["input"]["currency"],
        reference=call["input"].get("reference", ""),
    )
    say(f"  the provider answered: {json.dumps(sent)}")

    output = {
        "provider_transfer_id": sent["provider_transfer_id"],
        "status": sent["status"],
        "amount_cents": sent["amount_cents"],
    }
    say(f"  reporting a completion that satisfies the output schema: {json.dumps(output)}")
    try:
        run.client_tool_completion(3, output)
    except SalvorAPIError as error:
        say(f"  REFUSED {error.code}: {error.message}")
    else:
        fail("the client closed a trust_completion = false call; it should not have been able to")

    say("  the log after the refusal:")
    print_log(run)
    say(f"  run state: {state_of(client, run)}")

    # The part a person does. Someone in payments looks the transfer up at the
    # provider, sees it, and records what they saw. This example reads the
    # provider's own ledger to stand in for that lookup, because the point is
    # that the answer comes from outside the run.
    say("  a person verifies the transfer at the provider, outside salvor:")
    verified = look_up_transfer(sent["provider_transfer_id"])
    if verified is None:
        fail(f"no transfer {sent['provider_transfer_id']} on the provider's ledger")
    say(f"    found on the provider's ledger: {json.dumps(verified)}")

    run.resolve(output)
    say("  settled by hand through resolve")
    say(f"  run state: {state_of(client, run)}")

    summary = finish(run, request, call, output)
    say(f"  the model closed out: {summary}")
    say(f"  run state: {state_of(client, run)}")
    print_log(run)
    say()


def look_up_transfer(transfer_id: str) -> Optional[dict[str, Any]]:
    """Read the provider's ledger for one transfer, the way a person checking a
    bank would read a statement. Nothing salvor holds is consulted here."""
    ledger = Path(
        os.environ.get("SALVOR_EXAMPLE_PROVIDER_LEDGER")
        or Path(os.environ.get("TMPDIR", "/tmp")) / "salvor-client-tools-provider.jsonl"
    )
    if not ledger.exists():
        return None
    for line in ledger.read_text().splitlines():
        if not line.strip():
            continue
        entry = json.loads(line)
        if entry.get("provider_transfer_id") == transfer_id and entry.get("outcome") == "created":
            return entry
    return None


# -- entry point -------------------------------------------------------------


def main() -> None:
    if len(sys.argv) < 2:
        fail("usage: desk.py <base_url>, for example http://127.0.0.1:18964")
    base_url = sys.argv[1]
    if not os.environ.get("REFUND_PROVIDER_API_KEY"):
        fail("REFUND_PROVIDER_API_KEY is not set for this desk; run.sh sets it for this process only")

    try:
        with Client(base_url) as client:
            run_desk(client)
    except (httpx.HTTPError, ConnectionError) as exc:
        fail(f"could not reach the control plane at {base_url}: {exc}")
    except SalvorAPIError as exc:
        fail(f"the control plane refused a request: {exc.code}: {exc.message}")


def run_desk(client: Client) -> None:
    say("what the operator declared (GET /v1/client-tools):")
    decls = client.list_client_tools()
    if not decls:
        fail("this server holds no client-tool declarations; start it with --client-tool")
    for decl in decls:
        say(
            f"  {decl.name}: effect={decl.effect} trust_completion={decl.trust_completion} "
            f"input requires {decl.input_schema.get('required')} "
            f"output requires {(decl.output_schema or {}).get('required')}"
        )
    say("  the payment credential is set in this process, and salvor was started without it")

    tools = function_definitions(decls)
    say(
        f"  the model gets {len(tools)} function definitions built from those input schemas: "
        f"{[tool['name'] for tool in tools]}"
    )
    say("  the desk keeps no schema of its own, so it has none to let drift")
    say()

    happy_path(client, tools)
    missing_evidence(client, tools)
    untrusted_payout(client, tools)
    say("done")


if __name__ == "__main__":
    main()

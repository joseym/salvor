"""Drive the disputes-desk graph over HTTP, from Python, against a running
``salvor serve``.

This is the Python leg of the three-language story ../README.md tells: an
invoice dispute arrives at an application, the application submits a graph
document, starts a run, answers a gate when the run parks there, and reads the
result back. The deciding, the parking, and the recording all live on the
server; this script does only the four things a real caller would do.

The server used here is STOCK: no ``--demo-tools``, no tool registry, nothing
a reader would need to write Rust to reproduce. That is why the graph has no
``tool`` node and both side-effecting steps live inside ``agent`` nodes, whose
MCP tools the control plane reaches by spawning the agent's own child process.
See ../README.md for the full explanation.

Bring the offline stack up first (see ../README.md or ../run.sh), then:

    python service.py http://127.0.0.1:18962
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, NoReturn

import httpx

from salvor import BranchCase, Client, GraphBuilder, expression
from salvor.errors import SalvorAPIError

# This file lives at examples/graph-clients/python/service.py; every other
# path this script touches (the document, the agent TOMLs, the two disputes)
# sits one directory up, in examples/graph-clients/. Deriving that from
# __file__ means the script runs correctly no matter what directory it was
# launched from, rather than only working by accident from the repository
# root the way a bare relative path would.
EXAMPLE_DIR = Path(__file__).resolve().parent.parent


def fail(message: str) -> NoReturn:
    """Stop with a clear, actionable message instead of a traceback.

    Every predictable failure this script can hit, an unreachable server, a
    hash that does not match, a run that lands somewhere other than the state
    the story expects, comes through here rather than an unguarded exception.
    """
    print(f"[python] ERROR: {message}", file=sys.stderr)
    sys.exit(1)


def agent_hash_for(document: dict[str, Any], node_id: str) -> str:
    """The ``agent_hash`` the document names for one agent node.

    Reading this back out of the loaded document, rather than writing the hash
    a second time as a literal in this file, is what makes the rebuild in
    section 1 an honest test: the builder-built graph names the exact same two
    agents the on-disk document does, so a hash mismatch could only mean the
    builder produced a structurally different document.
    """
    for node in document.get("nodes", []):
        payload = node.get("payload", {})
        if payload.get("id") == node_id:
            agent_hash = payload.get("agent_hash")
            if not agent_hash:
                fail(f"node `{node_id}` in the document has no agent_hash")
            return agent_hash
    fail(f"no agent node `{node_id}` found in the document")


def stream_to_rest(client: Client, run_id: str, from_seq: int = 0) -> int:
    """Stream events from a cursor until the run rests; return the next seq.

    A graph run's stream carries the same events an agent run's does, plus
    ``BranchTaken`` and ``NodeSkipped``. Streaming is preferred over polling
    for the reason a real service would prefer it: the run drives on the
    server whether or not anyone is watching, and a stream shows each step as
    it lands rather than a client guessing how often to ask. The server ends
    the stream with one terminal frame the moment the run rests, so this
    always returns promptly instead of hanging past a suspend or a
    completion.
    """
    stream = client.stream_events(run_id, from_seq=from_seq)
    next_seq = from_seq
    for event in stream:
        next_seq = event.seq + 1
        detail = ""
        if event.kind == "BranchTaken":
            detail = f"  (case={event.payload.get('case')})"
        elif event.tool:
            detail = f"  ({event.tool})"
        print(f"[python]   seq {event.seq:>2}  {event.kind}{detail}")
    end = stream.end
    resting = end.status.state if end and end.status else "unknown"
    print(f"[python]   -- stream closed; run is {resting}")
    return next_seq


def main() -> None:
    if len(sys.argv) < 2:
        fail("usage: service.py <base_url>, for example http://127.0.0.1:18962")
    base_url = sys.argv[1]

    try:
        with Client(base_url) as client:
            run_story(client)
    except (httpx.HTTPError, ConnectionError) as exc:
        fail(f"could not reach the control plane at {base_url}: {exc}")
    except SalvorAPIError as exc:
        fail(f"the control plane refused a request: {exc.code}: {exc.message}")


def run_story(client: Client) -> None:
    print("[python] 1. the document, and its content address")

    doc_path = EXAMPLE_DIR / "dispute-refund.json"
    document = json.loads(doc_path.read_text())
    from_disk = client.submit_graph(document)
    print(f"[python]   submitted from disk: {from_disk.graph} (created={from_disk.created})")

    # Rebuild the identical graph with the typed builder. The two hashes below
    # are read out of the same on-disk document rather than written a second
    # time, so this rebuild is provably the same two agent references, not
    # just similarly-named ones.
    settle_hash = agent_hash_for(document, "settle_and_notify")
    claims_hash = agent_hash_for(document, "small_claims")

    approval_schema = {
        "type": "object",
        "properties": {
            "dispute_id": {"type": "string"},
            "amount_usd": {"type": "number"},
            "approver": {"type": "string"},
            "note": {"type": "string"},
        },
        "required": ["dispute_id", "amount_usd", "approver"],
    }
    gate_prompt = (
        "This dispute is at or above the review threshold. Answer with the "
        "refund to issue: dispute_id, amount_usd, approver, and an optional "
        "note. The run stays parked until you do."
    )

    # A graph document IS its content hash: the server parses the submitted
    # bytes into strict typed nodes and edges and hashes THAT canonical form,
    # never the raw bytes. So the JSON key order on disk (id, cases; or id,
    # prompt, approval_schema) and whatever order this builder happens to
    # populate its dataclasses' fields in are both irrelevant to the answer;
    # only the decoded structure, meaning which nodes, which fields and which
    # edges, can change the hash. That is what the assertion below is actually
    # testing: not that this script wrote the same bytes twice, but that two
    # independently-produced encodings of the same graph are the same graph.
    built = (
        GraphBuilder()
        .branch(
            "route_by_amount",
            [
                BranchCase("escalate", expression("amount_usd >= 250.0")),
                BranchCase("auto_settle", expression("amount_usd < 250.0")),
            ],
        )
        .gate("approve_refund", approval_schema, prompt=gate_prompt)
        .agent("settle_and_notify", settle_hash)
        .agent("small_claims", claims_hash)
        .edge("route_by_amount", "approve_refund", label="escalate")
        .edge("approve_refund", "settle_and_notify")
        .edge("route_by_amount", "small_claims", label="auto_settle")
        .build()
    )
    from_builder = client.submit_graph(built)
    print(
        f"[python]   submitted from builder: {from_builder.graph} "
        f"(created={from_builder.created})"
    )

    if from_disk.graph != from_builder.graph:
        fail(
            "the on-disk document and the builder-built document hashed "
            f"differently: {from_disk.graph} != {from_builder.graph}. A graph "
            "is supposed to be content addressed; this mismatch means the "
            "rebuild is not actually the same graph."
        )
    print(f"[python]   the two hashes match: {from_disk.graph}")
    graph_hash = from_disk.graph

    print("[python] 2. registering the two agents")
    settle_toml = (EXAMPLE_DIR / "agents" / "settle-and-notify.toml").read_text()
    claims_toml = (EXAMPLE_DIR / "agents" / "small-claims.toml").read_text()

    registered_settle = client.register_agent(settle_toml)
    print(f"[python]   registered settle-and-notify: {registered_settle}")
    if registered_settle != settle_hash:
        fail(
            "agent hash mismatch for settle_and_notify: the document names "
            f"{settle_hash}, the server computed {registered_settle}"
        )

    registered_claims = client.register_agent(claims_toml)
    print(f"[python]   registered small-claims: {registered_claims}")
    if registered_claims != claims_hash:
        fail(
            "agent hash mismatch for small_claims: the document names "
            f"{claims_hash}, the server computed {registered_claims}"
        )
    print("[python]   both agent hashes match the document")

    print("[python] 3. the escalated dispute: DSP-4471, $512.00")
    escalated_input = json.loads((EXAMPLE_DIR / "input.json").read_text())
    run_id = client.start_graph_run(graph_hash, escalated_input, labels={"desk": "disputes"})
    print(f"[python]   started run {run_id}")
    next_seq = stream_to_rest(client, run_id)

    state = client.get_run(run_id)
    if state.status.state != "suspended":
        fail(
            f"expected run {run_id} to suspend at the approve_refund gate; "
            f"got state {state.status.state!r} instead"
        )
    print(f"[python]   run id: {run_id}")
    print(f"[python]   gate prompt: {state.status.reason}")
    print("[python]   gate answer schema:")
    print(json.dumps(state.status.input_schema, indent=2))

    print("[python] 4. answering the gate")
    approval = {
        "dispute_id": "DSP-4471",
        "amount_usd": 512.0,
        "approver": "j.okafor",
        "note": "Duplicate charge confirmed against INV-90210.",
    }
    result = client.resume(run_id, approval)
    print(f"[python]   resume outcome: {result.outcome}")
    stream_to_rest(client, run_id, from_seq=next_seq)

    escalated_final = client.get_run(run_id)
    if not escalated_final.status.is_completed:
        fail(
            f"expected run {run_id} to complete after the gate was answered; "
            f"got state {escalated_final.status.state!r} instead"
        )
    print(f"[python]   final output: {escalated_final.status.output}")

    print("[python] 5. the auto-settle dispute: DSP-3312, $47.25")
    small_input = json.loads((EXAMPLE_DIR / "input-small.json").read_text())
    small_run_id = client.start_graph_run(graph_hash, small_input, labels={"desk": "disputes"})
    print(f"[python]   started run {small_run_id}")
    stream_to_rest(client, small_run_id)

    small_final = client.get_run(small_run_id)
    if not small_final.status.is_completed:
        fail(
            f"expected run {small_run_id} to complete without ever suspending; "
            f"got state {small_final.status.state!r} instead"
        )
    print(f"[python]   final output: {small_final.status.output}")

    projection = client.get_run_graph(small_run_id)
    skipped = [node for node in projection.nodes if node.state == "skipped"]
    if not skipped:
        fail(
            f"expected run {small_run_id} to skip the escalated arm (the gate "
            "and settle_and_notify), but the per-node projection shows nothing "
            "skipped"
        )
    print("[python]   never suspended; the skipped nodes are the evidence:")
    for node in skipped:
        print(f"[python]     {node.node}: {node.reason}")

    print("[python] done")
    print(f"[python]   escalated run   {run_id}: {escalated_final.status.state}")
    print(f"[python]   auto-settle run {small_run_id}: {small_final.status.state}")


if __name__ == "__main__":
    main()

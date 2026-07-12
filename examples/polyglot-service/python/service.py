"""Drive a running Salvor control plane from Python, end to end.

This is a small service-style script showing the "durable backend in any
language" story: one durable Rust process (`salvor serve`) does all the work,
and this thin Python client registers an agent, starts a run, streams its
events live, handles a human-in-the-loop park, and streams to completion. Its
TypeScript twin in ../typescript/service.ts does the identical flow.

The run parks on purpose. The agent (../agent.toml) declares a low step budget,
so after eight model calls the run stops as `budget_exceeded` and waits. That
is a real suspension held on the server: the process would sit there across a
restart. This script reads the park over HTTP, grants a budget extension, and
resumes, all over the same control plane.

Bring the offline stack up first (see ../README.md or ../run.sh), then:

    python service.py http://127.0.0.1:8080
"""

import sys
from pathlib import Path

from salvor import Client

BASE_URL = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"

# The budget extension we grant when the run parks: eight more steps than the
# 20-model-call run needs, so it drives cleanly to completion after the resume.
# This is exactly the shape POST /v1/runs/{id}/resume accepts for a parked
# budget: {"input": {"extend": {"steps": N}}}. The SDK wraps it under "input".
STEP_EXTENSION = 40


def stream_to_rest(client: Client, run_id: str, from_seq: int = 0) -> int:
    """Stream events from a cursor until the run rests; return the next seq.

    Prints one line per event. The server replays the log from `from_seq`, then
    tails live events, then closes at the terminal end frame; the resting status
    is on `stream.end` afterwards.
    """
    stream = client.stream_events(run_id, from_seq=from_seq)
    next_seq = from_seq
    for event in stream:
        next_seq = event.seq + 1
        detail = ""
        if event.tool:
            detail = f"  ({event.tool})"
        print(f"  seq {event.seq:>2}  {event.kind}{detail}")
    end = stream.end
    resting = end.status.state if end and end.status else "unknown"
    print(f"  -- stream closed; run is {resting}")
    return next_seq


def main() -> None:
    agent_toml = (Path(__file__).parent.parent / "agent.toml").read_text()

    with Client(BASE_URL) as client:
        # 1. Register the definition once. The server validates it (builds the
        #    agent) and returns a content hash; runs reference it by that hash.
        agent = client.register_agent(agent_toml)
        print(f"registered agent {agent}")

        # 2. Start a run. Returns at once with the run id; the run then drives
        #    in the background on the server.
        run_id = client.start_run(agent, {"topic": "durable execution for AI agents"})
        print(f"started run {run_id}")

        # 3. Stream the first leg. The low step budget parks the run partway.
        print("streaming until the run rests:")
        next_seq = stream_to_rest(client, run_id)

        # 4. Inspect the resting state. A budget park reports which dimension
        #    was crossed, its effective limit, and the observed value.
        state = client.get_run(run_id)
        if state.status.state != "budget_exceeded":
            print(f"run did not park on budget (state: {state.status.state}); stopping")
            return
        budget = state.status.raw.get("budget", {})
        observed = state.status.raw.get("observed")
        print(
            f"\nrun parked: budget_exceeded on {budget.get('kind')} "
            f"(limit {budget.get('limit')}, observed {observed})"
        )

        # 5. The human-in-the-loop decision: grant more budget and resume. The
        #    server validates the extension against the recorded budget shape
        #    before recording anything, then drives the run in the background.
        extension = {"extend": {"steps": STEP_EXTENSION}}
        print(f"resuming with budget extension {extension}")
        result = client.resume(run_id, extension)
        print(f"resume outcome: {result.outcome}")

        # 6. Stream the continuation from where the first leg stopped. No event
        #    is missed or repeated: the cursor picks up at the next sequence.
        print("streaming the continuation:")
        stream_to_rest(client, run_id, from_seq=next_seq)

        # 7. Read the final folded state.
        final = client.get_run(run_id)
        print(f"\nfinal state: {final.status.state}")
        if final.status.is_completed:
            print(f"summary: {final.status.output}")
        if final.usage:
            print(
                f"usage: {final.usage.input_tokens} in / "
                f"{final.usage.output_tokens} out"
            )


if __name__ == "__main__":
    main()

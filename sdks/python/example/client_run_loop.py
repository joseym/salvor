"""Drive a client-driven run from Python: the second of Salvor's two modes.

Where example/answer.py hands the loop to the server (server-driven mode), this
script owns the loop itself and streams the events it produces to the server,
which owns the durable log and guards every append. The client appends its own
control and context events (`RunStarted`, `NowObserved`, `RunCompleted`), asks
the server to perform the one side-effecting model step (the server holds the
key), then re-opens a run and re-drives it from the fetched log to show that a
recorded run replays with zero live calls.

The model step is the only leg that needs a model the server can reach.
`salvor serve`'s client-driven model executor reads `ANTHROPIC_API_KEY` for its
credential and honors `SALVOR_MODEL_BASE_URL` to target a local or offline
endpoint speaking the same wire protocol, so the whole script runs offline and
keyless against the scripted demo model:

    target/debug/salvor-demo-model --port 8893 --delay-ms 0 &
    SALVOR_MODEL_BASE_URL=http://127.0.0.1:8893 \
        target/debug/salvor serve --bind 127.0.0.1:8080 --store /tmp/loop.db &
    python example/client_run_loop.py http://127.0.0.1:8080

or against the public endpoint with a key on the server's environment
(`ANTHROPIC_API_KEY=sk-ant-...` and no `SALVOR_MODEL_BASE_URL`). With neither,
the control loop and the replay still run end to end (they are the durable
surface) and the model leg reports that the server has no reachable model.
"""

import sys

from salvor import Client
from salvor.errors import SalvorAPIError

BASE_URL = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"

AGENT = "sha256:example-client-run"
REQUEST = {
    "model": "claude-sonnet-4-5",
    "max_tokens": 256,
    "messages": [{"role": "user", "content": "In one word, name a durable-agent property."}],
}


def control_loop_then_replay(client: Client) -> None:
    """Open a run, drive a control-only loop, then re-open and replay it.

    Every event here rides the generic guarded append, which holds no secret and
    no side effect, so this whole leg runs offline with no model at all. It is
    the durability guarantee in miniature: the server folds the log on every
    append, and a re-open hands back the recorded log a client re-drives from.
    """
    run = client.open_client_run()
    print(f"opened run {run.run_id}")
    run.append(
        [
            run.envelope(0, "RunStarted", agent_def_hash=AGENT, input={"topic": "otters"}),
            run.envelope(1, "NowObserved", now="2026-07-11T12:00:00Z"),
            run.envelope(2, "RunCompleted", output={"done": True}),
        ]
    )
    print("appended RunStarted, NowObserved, RunCompleted")

    # Re-open: a fresh lease plus every recorded envelope, ready to rebuild a
    # cursor. Re-driving from this log pays nothing; each step replays.
    reopened = client.open_client_run(run_id=run.run_id)
    print(f"re-opened {reopened.run_id}; replaying {len(reopened.log_envelopes)} events:")
    for event in reopened.log_envelopes:
        print(f"  seq {event.seq:>2}  {event.kind}")
    print("replayed from the log; zero live calls\n")


def model_step_leg(client: Client) -> None:
    """Reserve a model intent and ask the server to perform the call.

    The server records the intent write-ahead, calls the provider with the key
    it holds, records the completion, and returns it. A streaming variant paints
    a live ticker; here we take the unary form. When the server has no reachable
    model, this reports the gap and leaves the run at its dangling model intent,
    which a retry against a reachable model would re-issue safely.
    """
    run = client.open_client_run(record_prompts=True)
    print(f"opened run {run.run_id} for the model step")
    run.append([run.envelope(0, "RunStarted", agent_def_hash=AGENT, input={})])
    try:
        result = run.model_step(1, REQUEST)
    except SalvorAPIError as error:
        if error.code in ("model_executor_unavailable", "model_execution"):
            print(f"model step skipped: server has no reachable model ({error.code})")
            return
        raise
    usage = result.usage
    print(f"model step recorded; usage {usage.input_tokens} in / {usage.output_tokens} out")
    run.append([run.envelope(3, "RunCompleted", output={"answered": True})])
    print("completed the run after the model step")


def main() -> None:
    with Client(BASE_URL) as client:
        print("== control loop and replay (always offline) ==")
        control_loop_then_replay(client)
        print("== model step (the one side-effecting leg) ==")
        model_step_leg(client)


if __name__ == "__main__":
    main()

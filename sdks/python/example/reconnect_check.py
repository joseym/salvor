"""Reconnect check: drop the event stream mid-tail and confirm it resumes from
the cursor with no gaps and no duplicates.

We force a transport drop after the first two frames on the first connection.
The client's stream loop catches it, reconnects with ?from_seq set to the next
sequence it expects, and skips anything already seen. The assertions confirm
the merged stream is contiguous and duplicate-free, and reaches the end frame.
"""

import sys
import time

import httpx

from salvor import Client

BASE_URL = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8087"

AGENT_TOML = """
model = "claude-haiku-4-5"
max_response_tokens = 300
system_prompt = "Answer the question in one short sentence, then stop."
[llm]
api_key_env = "ANTHROPIC_API_KEY"
[budgets]
steps = 4
tokens = 20000
"""


def main() -> None:
    client = Client(BASE_URL)
    agent = client.register_agent(AGENT_TOML)
    run_id = client.start_run(agent, {"question": "What is a write-ahead log?"})

    # Let the run finish so the whole log is present to replay across the drop.
    for _ in range(60):
        if client.get_run(run_id).status.state in ("completed", "failed"):
            break
        time.sleep(0.25)

    # Wrap the per-connection frame reader so the first connection dies after
    # two frames, exactly as a dropped socket would.
    original = client._read_frames
    state = {"connects": 0}

    def flaky(rid, from_seq):
        state["connects"] += 1
        this_connect = state["connects"]
        delivered = 0
        for frame in original(rid, from_seq):
            yield frame
            delivered += 1
            if this_connect == 1 and delivered == 2:
                raise httpx.RemoteProtocolError("simulated mid-stream drop")

    client._read_frames = flaky  # type: ignore[assignment]

    seqs: list[int] = []
    stream = client.stream_events(run_id, from_seq=0)
    for event in stream:
        seqs.append(event.seq)

    print(f"connections used: {state['connects']}  (1 = drop, 2 = resume)")
    print(f"delivered seqs:   {seqs}")

    assert state["connects"] >= 2, "the stream did not reconnect"
    assert seqs == sorted(set(seqs)), "duplicate or out-of-order sequences"
    assert seqs == list(range(seqs[0], seqs[-1] + 1)), "a gap in the sequence"
    assert stream.end is not None, "no terminal end frame"
    print(f"reconnect OK: contiguous, duplicate-free, ended at "
          f"{stream.end.status.state}")
    client.close()


if __name__ == "__main__":
    main()

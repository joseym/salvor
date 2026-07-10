"""Register a model-only agent, start a run, stream it to completion, print the
final state. The Python mirror of the examples/web-research walkthrough, driven
over the control plane instead of the CLI.

Run a server first (with a key on its environment):

    ANTHROPIC_API_KEY=sk-ant-... \
        target/debug/salvor serve --bind 127.0.0.1:8080 --store /tmp/answer.db

then:

    python example/answer.py http://127.0.0.1:8080
"""

import sys
from pathlib import Path

from salvor import Client

BASE_URL = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"
QUESTION = "In one sentence, what problem does durable execution solve for agents?"


def main() -> None:
    agent_toml = (Path(__file__).parent / "agent.toml").read_text()

    with Client(BASE_URL) as client:
        # 1. Register the definition once. The server validates it and returns a
        #    content hash; every run references the agent by that hash.
        agent = client.register_agent(agent_toml)
        print(f"registered agent {agent}")

        # 2. Start a run. This returns at once with the run id; the run then
        #    drives in the background on the server.
        run_id = client.start_run(agent, {"question": QUESTION})
        print(f"started run {run_id}")

        # 3. Stream events in order until the run rests. The stream replays the
        #    log, tails new events live, and stops at the terminal end frame.
        count = 0
        stream = client.stream_events(run_id)
        for event in stream:
            count += 1
            print(f"  seq {event.seq:>2}  {event.kind}")

        # 4. Read the resting status the end frame carried, and the folded state.
        state = client.get_run(run_id)
        print(f"\nevents: {count}")
        print(f"final state: {state.status.state}")
        if state.status.is_completed:
            print(f"answer: {state.status.output}")
        if state.usage:
            usd = (
                state.usage.input_tokens * 1.0 + state.usage.output_tokens * 5.0
            ) / 1_000_000
            print(
                f"usage: {state.usage.input_tokens} in / "
                f"{state.usage.output_tokens} out  (~${usd:.5f})"
            )


if __name__ == "__main__":
    main()

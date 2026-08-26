"""Proves the ASYNC client-driven run driver against a real ``salvor serve``.

The synchronous driver is proven against the real binary in
``tests/test_client_runs.py``; this is its twin, awaited. The point of running it
against the real control plane rather than a stub is that the driver's rules are
only worth anything if the server agrees with them: the append-guard, the model
step's retry identity, the effect table, and the durable timer's "the log
decides first" are all the server's, and a driver that had quietly diverged from
them would pass a stub and fail here.

One server serves the whole class, started with the two flags the client-driven
surface needs and a stock server does not have: ``--demo-tools`` for a tool step
to reach (a plain ``salvor serve`` wires the registry empty) and
``--client-tool`` for the client-performed pair. Its model executor points at a
local dual-mode endpoint, so the model step is a genuine server-performed
provider call, keyless and offline, and that endpoint's own hit count is what
the no-re-pay proof counts.

Standard library only (``unittest``, ``asyncio``, ``http.server``,
``subprocess``), plus the SDK's own dependency ``httpx``. Run it with

    .venv/bin/python -m unittest tests.test_async_client_runs
"""

from __future__ import annotations

import asyncio
import json
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

try:
    import httpx
except ImportError:
    raise unittest.SkipTest(
        "httpx is not installed; the client-run driver tests need the SDK's "
        "one dependency (pip install -e sdks/python)"
    ) from None

from salvor import AsyncClient, AsyncClientRunDriver
from salvor.errors import DivergenceError, LeaseHeldError, SalvorAPIError

REPO_ROOT = Path(__file__).resolve().parents[3]
SALVOR = REPO_ROOT / "target" / "debug" / "salvor"
CLIENT_TOOL = REPO_ROOT / "examples" / "client-tools" / "refund-card.toml"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_until_up(base: str) -> bool:
    """Poll the control plane until it answers, within a bounded deadline."""
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            httpx.get(f"{base}/v1/agents", timeout=0.5)
            return True
        except httpx.HTTPError:
            time.sleep(0.1)
    return False


def sse_frame(event: str, data: dict) -> str:
    return f"event: {event}\ndata: {json.dumps(data)}\n\n"


def messages_sse_body() -> str:
    """A Messages SSE body folding to the same response as the JSON one, with
    the text arriving as two deltas so delta delivery is exercised."""
    return (
        sse_frame(
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": "msg_async",
                    "type": "message",
                    "model": "test-model",
                    "role": "assistant",
                    "content": [],
                    "stop_reason": None,
                    "usage": {"input_tokens": 10, "output_tokens": 0},
                },
            },
        )
        + sse_frame(
            "content_block_start",
            {"type": "content_block_start", "index": 0,
             "content_block": {"type": "text", "text": ""}},
        )
        + sse_frame(
            "content_block_delta",
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "text_delta", "text": "the plan: "}},
        )
        + sse_frame(
            "content_block_delta",
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "text_delta", "text": "study otters"}},
        )
        + sse_frame("content_block_stop", {"type": "content_block_stop", "index": 0})
        + sse_frame(
            "message_delta",
            {"type": "message_delta",
             "delta": {"stop_reason": "end_turn", "stop_sequence": None},
             "usage": {"output_tokens": 5}},
        )
        + sse_frame("message_stop", {"type": "message_stop"})
    )


class DualModeModel(BaseHTTPRequestHandler):
    """A local model endpoint: JSON for a plain request, Messages SSE for a
    ``stream: true`` one, both folding to the same response."""

    def log_message(self, *args: object) -> None:
        pass

    def do_POST(self) -> None:  # noqa: N802 (stdlib naming)
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        self.server.hits.append(self.path)  # type: ignore[attr-defined]
        if body.get("stream") is True:
            payload = messages_sse_body().encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
        else:
            payload = json.dumps(
                {
                    "id": "msg_async",
                    "model": "test-model",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "the plan: study otters"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 5},
                }
            ).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


REQUEST = {
    "model": "test-model",
    "max_tokens": 256,
    "messages": [{"role": "user", "content": "draft a plan"}],
}


class AsyncDriverRealServer(unittest.TestCase):
    """The whole async driver surface against the real control-plane binary."""

    proc: subprocess.Popen
    endpoint: ThreadingHTTPServer
    base: str
    #: This class's own directory under the system temp dir, holding the store
    #: and nothing else, removed however the class ends.
    workspace: str

    @classmethod
    def setUpClass(cls) -> None:
        if not SALVOR.exists():
            raise unittest.SkipTest(f"build the binary first (cargo build): {SALVOR}")
        cls.endpoint = ThreadingHTTPServer(("127.0.0.1", 0), DualModeModel)
        cls.endpoint.hits = []  # type: ignore[attr-defined]
        threading.Thread(target=cls.endpoint.serve_forever, daemon=True).start()
        model_port = cls.endpoint.server_address[1]
        serve_port = free_port()
        cls.base = f"http://127.0.0.1:{serve_port}"
        cls.workspace = tempfile.mkdtemp(prefix="salvor-py-")
        store = str(Path(cls.workspace) / "async-driver.db")
        cls.proc = subprocess.Popen(
            [
                str(SALVOR), "--store", store, "serve",
                "--bind", f"127.0.0.1:{serve_port}",
                # A stock server has an empty tool registry and no client-tool
                # declarations, so both tool surfaces would be `unknown_tool`.
                "--demo-tools",
                "--client-tool", str(CLIENT_TOOL),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={
                "PATH": "/usr/bin:/bin",
                "SALVOR_MODEL_BASE_URL": f"http://127.0.0.1:{model_port}",
            },
        )
        if not wait_until_up(cls.base):
            cls.tearDownClass()
            raise unittest.SkipTest("salvor serve did not come up")

    @classmethod
    def tearDownClass(cls) -> None:
        proc = getattr(cls, "proc", None)
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        workspace = getattr(cls, "workspace", None)
        if workspace is not None:
            shutil.rmtree(workspace, ignore_errors=True)
        endpoint = getattr(cls, "endpoint", None)
        if endpoint is not None:
            endpoint.shutdown()
            endpoint.server_close()

    # -- harness --------------------------------------------------------------

    def drive(self, scenario) -> None:
        """Run one scenario on its own event loop, closing what it opened.

        Every driver opened through :meth:`open` is registered for close on the
        same loop that opened it, which is what an async connection pool needs.
        """
        async def main() -> None:
            self._opened: list = []
            try:
                await scenario()
            finally:
                for handle in self._opened:
                    await handle.close()

        asyncio.run(main())

    async def open(self, **kwargs) -> AsyncClientRunDriver:
        run = await AsyncClientRunDriver.open(self.base, **kwargs)
        self._opened.append(run)
        return run

    def hits(self) -> int:
        """How many model requests the local endpoint has served."""
        return len(self.endpoint.hits)  # type: ignore[attr-defined]

    @staticmethod
    def started(run: AsyncClientRunDriver, seq: int = 0) -> dict:
        return run.envelope(
            seq, "RunStarted", agent_def_hash="sha256:agent", input={"topic": "otters"}
        )

    # -- the control loop -----------------------------------------------------

    def test_full_control_loop_and_replay(self) -> None:
        async def scenario() -> None:
            run = await self.open()

            # Open mints a fresh run with an empty log and a lease.
            self.assertEqual(run.log_envelopes, [])
            self.assertTrue(run.drive_token)

            appended = await run.append(
                [
                    self.started(run),
                    run.envelope(1, "NowObserved", now="2026-07-11T12:00:00Z"),
                    run.envelope(2, "RandomObserved", value=7),
                    run.envelope(3, "RunCompleted", output={"done": True}),
                ]
            )
            self.assertEqual(appended, [0, 1, 2, 3])

            log = await run.log()
            self.assertEqual(
                [e.kind for e in log],
                ["RunStarted", "NowObserved", "RandomObserved", "RunCompleted"],
            )

            # A from_seq read trims the prefix.
            tail = await run.log(from_seq=2)
            self.assertEqual([e.seq for e in tail], [2, 3])

        self.drive(scenario)

    def test_reopen_with_the_held_token_keeps_the_lease(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])
            old_token = run.drive_token

            # A bare re-open, with no token, is refused while the lease is
            # current: the rule is not "newest caller wins".
            with self.assertRaises(LeaseHeldError) as caught:
                await self.open(run_id=run.run_id)
            self.assertEqual(caught.exception.code, "lease_held")
            self.assertGreater(caught.exception.lapses_in_seconds, 0)

            # Presenting the held lease's own token re-opens under the SAME
            # token, so the driver that already holds the run never gives it up.
            reopened = await self.open(run_id=run.run_id, drive_token=old_token)
            self.assertEqual([e.kind for e in reopened.log_envelopes], ["RunStarted"])
            self.assertEqual(reopened.drive_token, old_token)

            # And that token still drives the run afterwards.
            await reopened.append(
                [reopened.envelope(1, "NowObserved", now="2026-07-11T12:00:00Z")]
            )

        self.drive(scenario)

    def test_idempotent_reappend_is_a_no_op_and_divergent_bytes_are_refused(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            started = self.started(run)
            self.assertEqual(await run.append([started]), [0])
            # Byte-identical re-append at the recorded seq is a 200 no-op.
            self.assertEqual(await run.append([started]), [0])
            self.assertEqual(len(await run.log()), 1)

            with self.assertRaises(DivergenceError):
                await run.append(
                    [run.envelope(0, "RunStarted", agent_def_hash="sha256:OTHER", input={})]
                )

        self.drive(scenario)

    # -- the model step -------------------------------------------------------

    def test_model_step_records_returns_and_never_re_pays(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])

            before = self.hits()
            result = await run.model_step(1, REQUEST)
            self.assertEqual(result.usage.input_tokens, 10)
            self.assertEqual(result.usage.output_tokens, 5)
            self.assertEqual(
                result.response["content"][0]["text"], "the plan: study otters"
            )
            self.assertEqual(self.hits(), before + 1, "one live provider call")

            log = await run.log()
            self.assertEqual(
                [e.kind for e in log],
                ["RunStarted", "ModelCallRequested", "ModelCallCompleted"],
            )

            # The same step again: the recorded completion comes back verbatim.
            retry = await run.model_step(1, REQUEST)
            self.assertEqual(retry.raw, result.raw, "the recorded completion, verbatim")
            self.assertEqual(self.hits(), before + 1, "retry paid nothing")
            self.assertEqual(len(await run.log()), 3, "no growth")

            # A streaming retry of a completed step streams one complete frame
            # carrying the recorded completion, with no deltas and no hit.
            stream = run.model_step_stream(1, REQUEST)
            deltas = [delta async for delta in stream]
            self.assertEqual(deltas, [], "a replayed step has no live deltas")
            self.assertEqual(stream.completion.response, result.response)
            self.assertEqual(self.hits(), before + 1, "streaming replay paid nothing")

        self.drive(scenario)

    def test_model_step_stream_yields_deltas_then_the_completion(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])
            before = self.hits()

            stream = run.model_step_stream(1, REQUEST)
            deltas = [delta async for delta in stream]
            text = "".join(d["text"] for d in deltas if d["type"] == "text_delta")
            self.assertEqual(text, "the plan: study otters")
            self.assertTrue(any(d["type"] == "usage" for d in deltas))
            self.assertEqual(stream.completion.usage.input_tokens, 10)
            self.assertEqual(stream.completion.usage.output_tokens, 5)
            self.assertEqual(self.hits(), before + 1, "one live provider call")

            self.assertEqual(
                [e.kind for e in await run.log()],
                ["RunStarted", "ModelCallRequested", "ModelCallCompleted"],
            )

            # Streaming and unary share one retry identity, (seq, request_hash).
            retry = await run.model_step(1, REQUEST)
            self.assertEqual(
                retry.response["content"][0]["text"], "the plan: study otters"
            )
            self.assertEqual(self.hits(), before + 1, "retry paid nothing")

        self.drive(scenario)

    # -- the tool steps -------------------------------------------------------

    def test_tool_step_performs_a_registered_tool_and_records_it(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])

            output = await run.tool_step(1, "lookup_invoice", {"invoice_id": "inv_1001"})
            self.assertEqual(output["invoice_id"], "inv_1001")
            self.assertEqual(output["amount_usd"], 128.50)
            self.assertEqual(
                [e.kind for e in await run.log()],
                ["RunStarted", "ToolCallRequested", "ToolCallCompleted"],
            )

            with self.assertRaises(SalvorAPIError) as caught:
                await run.tool_step(3, "ghost", {})
            self.assertEqual(caught.exception.code, "unknown_tool")

        self.drive(scenario)

    def test_client_tool_intent_and_completion_settle_a_client_performed_call(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])
            refund = {"order_id": "ORD-7781", "amount_cents": 5000, "currency": "USD"}

            intent = await run.client_tool_intent(1, "refund_card", refund)
            self.assertEqual(intent.seq, 1)
            self.assertEqual(intent.effect, "write")
            self.assertFalse(intent.settled, "nothing has reported on it yet")
            # The key is the server's, derived from (run, seq, tool).
            self.assertTrue(intent.idempotency_key.startswith("sha256:"))

            # A re-post before the completion hands back the same key.
            again = await run.client_tool_intent(1, "refund_card", refund)
            self.assertEqual(again.idempotency_key, intent.idempotency_key)
            self.assertFalse(again.settled)

            await run.client_tool_completion(
                1,
                {"provider_refund_id": "re_1", "status": "succeeded", "amount_cents": 5000},
            )
            self.assertEqual(
                [e.kind for e in await run.log()],
                ["RunStarted", "ToolCallRequested", "ToolCallCompleted"],
            )

            # Now the same intent reports itself settled, which is how a
            # payments caller tells "already done" from "safe to perform".
            settled = await run.client_tool_intent(1, "refund_card", refund)
            self.assertTrue(settled.settled)
            self.assertEqual(settled.idempotency_key, intent.idempotency_key)
            self.assertEqual(
                settled.output,
                {"provider_refund_id": "re_1", "status": "succeeded", "amount_cents": 5000},
                "a settled answer carries the recorded completion's output",
            )

        self.drive(scenario)

    def test_client_tool_failure_records_the_sentinel_and_settles_the_call(
        self,
    ) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])
            refund = {"order_id": "ORD-9002", "amount_cents": 1200, "currency": "USD"}

            intent = await run.client_tool_intent(1, "refund_card", refund)
            self.assertFalse(intent.settled)

            await run.client_tool_failure(1, "the provider timed out")
            self.assertEqual(
                [e.kind for e in await run.log()],
                ["RunStarted", "ToolCallRequested", "ToolCallCompleted"],
            )

            # The failure settles the call exactly as a reported output would:
            # a re-post of the same intent comes back settled, carrying the
            # sentinel a native tool's exhausted retries would have written.
            settled = await run.client_tool_intent(1, "refund_card", refund)
            self.assertTrue(settled.settled)
            self.assertEqual(
                settled.output,
                {
                    "__salvor_error": {
                        "is_error": True,
                        "kind": "handler",
                        "message": "the provider timed out",
                        "attempts": 1,
                    }
                },
            )

        self.drive(scenario)

    def test_client_tool_intent_refuses_an_undeclared_tool(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])
            with self.assertRaises(SalvorAPIError) as caught:
                await run.client_tool_intent(1, "ghost", {})
            self.assertEqual(caught.exception.code, "unknown_tool")
            self.assertEqual(len(await run.log()), 1, "the refusal wrote nothing")

        self.drive(scenario)

    def test_client_model_intent_and_completion_settle_a_client_performed_call(
        self,
    ) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])

            intent = await run.client_model_intent(1, "sha256:the-request")
            self.assertEqual(intent.seq, 1)
            self.assertFalse(intent.settled, "nothing has reported on it yet")
            self.assertIsNone(intent.response)

            await run.client_model_completion(
                1,
                {"content": [{"type": "text", "text": "the plan"}]},
                {"input_tokens": 10, "output_tokens": 5},
            )
            self.assertEqual(
                [e.kind for e in await run.log()],
                ["RunStarted", "ModelCallRequested", "ModelCallCompleted"],
            )

            # A re-post of the same hash now reports itself settled, carrying
            # the recorded response and usage back without a second log read.
            settled = await run.client_model_intent(1, "sha256:the-request")
            self.assertTrue(settled.settled)
            self.assertEqual(settled.response["content"][0]["text"], "the plan")
            self.assertEqual(settled.usage.input_tokens, 10)
            self.assertEqual(settled.usage.output_tokens, 5)

            # A different hash at the same recorded position is a divergence:
            # the client's cursor and the log disagree about what was sent.
            with self.assertRaises(DivergenceError):
                await run.client_model_intent(1, "sha256:a-different-request")

        self.drive(scenario)

    # -- durable timers -------------------------------------------------------

    def test_sleep_parks_the_run_and_only_the_deadline_wakes_it(self) -> None:
        """Four drives over one run: park, come back too soon, come back late,
        then replay the closed pair.

        Each drive is a fresh driver re-opened on the same run id, which is what
        a later drive actually is: a process holding only the recorded log and
        its own clock. The second one is the point of the whole feature. It runs
        the identical code with a clock ten minutes on, and appends nothing,
        because the deadline it compares against comes from the log rather than
        from how long this process has been awake.
        """
        started_at = datetime(2026, 7, 11, 12, 0, tzinfo=timezone.utc)

        async def scenario() -> None:
            # Drive one: park on a timer an hour out, derived from a recorded
            # reading of this drive's clock.
            first = await self.open()
            first.clock = lambda: started_at
            await first.append([self.started(first)])
            wake_at = await first.sleep_for(1, timedelta(hours=1))
            self.assertEqual(wake_at, started_at + timedelta(hours=1))
            self.assertEqual(
                [e.kind for e in await first.log()],
                ["RunStarted", "NowObserved", "SleepStarted"],
            )
            parked = await first.await_wake(3)
            self.assertFalse(parked.woken, "the deadline is an hour away")
            self.assertEqual(parked.wake_at, wake_at)
            self.assertEqual(len(await first.log()), 3, "asking appended nothing")

            run_id = first.run_id

            # Drive two, ten minutes later: the replayed instants are the
            # recorded ones, so the deadline has not moved. `first` never went
            # quiet, so its lease is still current, and this drive presents its
            # token to re-open under it rather than being refused `lease_held`.
            early = await self.open(run_id=run_id, drive_token=first.drive_token)
            early.clock = lambda: started_at + timedelta(minutes=10)
            replayed = await early.sleep_for(1, timedelta(hours=1))
            self.assertEqual(replayed, wake_at, "the wake instant reproduces on replay")
            still_asleep = await early.await_wake(3)
            self.assertFalse(still_asleep.woken, "driving early does not wake a run")
            self.assertEqual(len(await early.log()), 3, "and appends nothing at all")

            # Drive three, two hours later: the deadline has passed, so this
            # drive closes the pair itself and the run carries on. Same story:
            # the lease is still current (the token `early` just re-opened
            # under), so this drive presents it too.
            late = await self.open(run_id=run_id, drive_token=early.drive_token)
            late.clock = lambda: started_at + timedelta(hours=2)
            self.assertEqual(await late.sleep_for(1, timedelta(hours=1)), wake_at)
            woken = await late.await_wake(3)
            self.assertTrue(woken.woken, "the deadline passed, so the sleep is over")
            self.assertEqual(woken.wake_at, wake_at)
            await late.append([late.envelope(4, "RunCompleted", output={"slept": True})])
            self.assertEqual(
                [e.kind for e in await late.log()],
                [
                    "RunStarted",
                    "NowObserved",
                    "SleepStarted",
                    "SleepCompleted",
                    "RunCompleted",
                ],
            )

            # A fourth drive replays the closed pair: nothing appends however
            # early this drive's clock reads.
            after = await self.open(run_id=run_id)
            after.clock = lambda: started_at
            self.assertTrue((await after.await_wake(3)).woken, "a recorded wake replays")
            self.assertEqual(len(await after.log()), 5, "and the log did not grow")

        self.drive(scenario)

    def test_a_sleep_completion_with_no_sleep_is_refused(self) -> None:
        async def scenario() -> None:
            run = await self.open()
            await run.append([self.started(run)])
            with self.assertRaises(DivergenceError):
                await run.append([run.envelope(1, "SleepCompleted")])
            self.assertEqual(len(await run.log()), 1, "the refusal wrote nothing")

        self.drive(scenario)

    # -- the event tail, against the real server's own stream ------------------
    #
    # tests/test_async_client.py proves the tail's cursor and end frame against
    # a stub, which controls exactly where the frame boundaries fall. These two
    # prove it against the real thing: the server's own chunking, its own
    # keep-alive comments, and its own terminal frame.

    def test_stream_events_tails_a_real_run_to_its_end_frame(self) -> None:
        async def scenario() -> None:
            async with AsyncClient(self.base, timeout=10.0) as client:
                run = await client.open_client_run()
                await run.append([self.started(run)])
                await run.append(
                    [run.envelope(1, "RunCompleted", output={"ok": True})]
                )

                stream = client.stream_events(run.run_id)
                seen = [(event.seq, event.kind) async for event in stream]
                self.assertEqual(
                    seen, [(0, "RunStarted"), (1, "RunCompleted")]
                )
                self.assertIsNotNone(stream.end, "the tail stopped at the end frame")
                self.assertEqual(stream.end.status.state, "completed")

        self.drive(scenario)

    def test_stream_events_starts_from_the_cursor_it_is_given(self) -> None:
        async def scenario() -> None:
            async with AsyncClient(self.base, timeout=10.0) as client:
                run = await client.open_client_run()
                await run.append([self.started(run)])
                await run.append(
                    [run.envelope(1, "RunCompleted", output={"ok": True})]
                )

                stream = client.stream_events(run.run_id, from_seq=1)
                seen = [event.seq async for event in stream]
                self.assertEqual(seen, [1], "the prefix before the cursor is skipped")
                self.assertEqual(stream.end.status.state, "completed")

        self.drive(scenario)

    # -- opened over a shared AsyncClient --------------------------------------

    def test_open_client_run_shares_the_async_clients_pool(self) -> None:
        """``AsyncClient.open_client_run`` is awaited (opening a run is a
        request) and the driver it returns rides the client's own connection, so
        closing the client closes it."""

        async def scenario() -> None:
            async with AsyncClient(self.base, timeout=10.0) as client:
                run = await client.open_client_run()
                self.assertTrue(run.drive_token)
                await run.append([self.started(run)])
                await run.append([run.envelope(1, "RunCompleted", output={"ok": True})])
                self.assertEqual(
                    [e.kind for e in await run.log()], ["RunStarted", "RunCompleted"]
                )
                state = await client.get_run(run.run_id)
                self.assertEqual(state.status.state, "completed")

        self.drive(scenario)


if __name__ == "__main__":
    unittest.main()

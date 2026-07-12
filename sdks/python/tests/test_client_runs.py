"""Proves the client-driven run driver against a real ``salvor serve``.

Two layers, both offline and keyless:

- :class:`ClientRunLoopRealServer` drives the full control-and-context loop
  against the actual ``salvor serve`` binary over HTTP: open, the guarded
  generic append, the log read-back, re-open (resume) with a fresh lease, the
  byte-identical idempotent no-op, and the divergence refusal. This is the real
  durability surface, folded by the same runtime the CLI uses.

- :class:`DriverAgainstStub` exercises the four server-performed methods
  (``model_step`` unary and streaming, ``tool_step``, ``resolve``) against a
  small stdlib HTTP stub that speaks the documented wire shapes. ``salvor
  serve`` wires its client-driven model executor from ``salvor_llm::Client``,
  which reads only ``ANTHROPIC_API_KEY`` and targets the public endpoint (there
  is no base-URL override), and it wires an empty tool registry, so those two
  side-effecting steps cannot be exercised offline through ``salvor serve``
  itself. The stub stands in for a host that injects a local model executor and
  a tool registry (the composition pattern the design intends), and the wire
  shapes it returns are the ones the server test suites in
  ``crates/salvor-server/tests`` prove.

Standard library only (``unittest``, ``http.server``, ``subprocess``), plus the
SDK's own dependency ``httpx``. Run it with

    .venv/bin/python -m unittest tests.test_client_runs
"""

from __future__ import annotations

import json
import socket
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import httpx

from salvor import ClientRunDriver
from salvor.errors import DivergenceError, NeedsReconciliationError, SalvorAPIError

REPO_ROOT = Path(__file__).resolve().parents[3]
SALVOR = REPO_ROOT / "target" / "debug" / "salvor"
DEMO_MODEL = REPO_ROOT / "target" / "debug" / "salvor-demo-model"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class ClientRunLoopRealServer(unittest.TestCase):
    """The full control-and-context loop against the real control-plane binary."""

    proc: subprocess.Popen
    model: subprocess.Popen
    base: str

    @classmethod
    def setUpClass(cls) -> None:
        if not SALVOR.exists() or not DEMO_MODEL.exists():
            raise unittest.SkipTest(
                f"build the binaries first (cargo build): {SALVOR}, {DEMO_MODEL}"
            )
        model_port = free_port()
        serve_port = free_port()
        cls.base = f"http://127.0.0.1:{serve_port}"
        store = f"/tmp/salvor-driver-test-{serve_port}.db"
        Path(store).unlink(missing_ok=True)
        cls.model = subprocess.Popen(
            [str(DEMO_MODEL), "--port", str(model_port), "--delay-ms", "0"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        env = {
            "PATH": "/usr/bin:/bin",
            "SALVOR_DEMO_BASE_URL": f"http://127.0.0.1:{model_port}",
        }
        cls.proc = subprocess.Popen(
            [str(SALVOR), "--store", store, "serve", "--bind", f"127.0.0.1:{serve_port}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                httpx.get(f"{cls.base}/v1/agents", timeout=0.5)
                break
            except httpx.HTTPError:
                time.sleep(0.1)
        else:
            cls.tearDownClass()
            raise unittest.SkipTest("salvor serve did not come up")

    @classmethod
    def tearDownClass(cls) -> None:
        for proc in (getattr(cls, "proc", None), getattr(cls, "model", None)):
            if proc is not None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()

    def started(self, run: ClientRunDriver, seq: int) -> dict:
        return run.envelope(
            seq, "RunStarted", agent_def_hash="sha256:agent", input={"topic": "otters"}
        )

    def test_full_control_loop_and_replay(self) -> None:
        run = ClientRunDriver.open(self.base)
        self.addCleanup(run.close)

        # Open mints a fresh run with an empty log and a lease.
        self.assertEqual(run.log_envelopes, [])
        self.assertTrue(run.drive_token)

        # The guarded generic append records a legal sequence and reports its seqs.
        appended = run.append(
            [
                self.started(run, 0),
                run.envelope(1, "NowObserved", now="2026-07-11T12:00:00Z"),
                run.envelope(2, "RandomObserved", value=7),
                run.envelope(3, "RunCompleted", output={"done": True}),
            ]
        )
        self.assertEqual(appended, [0, 1, 2, 3])

        # The log reads back as the four recorded events, in order.
        log = run.log()
        self.assertEqual([e.kind for e in log], [
            "RunStarted",
            "NowObserved",
            "RandomObserved",
            "RunCompleted",
        ])

        # A from_seq read trims the prefix.
        tail = run.log(from_seq=2)
        self.assertEqual([e.seq for e in tail], [2, 3])

    def test_reopen_supersedes_lease_and_returns_log(self) -> None:
        run = ClientRunDriver.open(self.base)
        self.addCleanup(run.close)
        run.append([self.started(run, 0)])
        old_token = run.drive_token

        # Re-opening the same run returns its recorded log and a fresh lease.
        reopened = ClientRunDriver.open(self.base, run_id=run.run_id)
        self.addCleanup(reopened.close)
        self.assertEqual([e.kind for e in reopened.log_envelopes], ["RunStarted"])
        self.assertNotEqual(reopened.drive_token, old_token)

        # The superseded lease no longer drives the run.
        run.drive_token = old_token
        with self.assertRaises(SalvorAPIError) as caught:
            run.append([run.envelope(1, "NowObserved", now="2026-07-11T12:00:00Z")])
        self.assertEqual(caught.exception.code, "invalid_drive_token")

    def test_idempotent_reappend_is_a_no_op(self) -> None:
        run = ClientRunDriver.open(self.base)
        self.addCleanup(run.close)
        started = self.started(run, 0)
        self.assertEqual(run.append([started]), [0])
        # Byte-identical re-append at the recorded seq is a 200 no-op reporting
        # the seq; the log does not grow.
        self.assertEqual(run.append([started]), [0])
        self.assertEqual(len(run.log()), 1)

    def test_divergent_bytes_is_a_divergence_error(self) -> None:
        run = ClientRunDriver.open(self.base)
        self.addCleanup(run.close)
        run.append([self.started(run, 0)])
        with self.assertRaises(DivergenceError):
            run.append(
                [run.envelope(0, "RunStarted", agent_def_hash="sha256:OTHER", input={})]
            )

    def test_model_event_on_generic_append_is_refused(self) -> None:
        run = ClientRunDriver.open(self.base)
        self.addCleanup(run.close)
        run.append([self.started(run, 0)])
        # A model event carries its own `seq` payload field, so build the
        # envelope directly; the generic append must refuse it whatever its shape.
        model_event = {
            "run_id": run.run_id,
            "seq": 1,
            "schema_version": 1,
            "recorded_at": "1970-01-01T00:00:00Z",
            "event": {
                "kind": "ModelCallRequested",
                "payload": {"seq": 1, "request_hash": "sha256:x", "request_body": None},
            },
        }
        with self.assertRaises(SalvorAPIError) as caught:
            run.append([model_event])
        self.assertEqual(caught.exception.code, "unsupported_event_kind")

    def test_model_step_reaches_executor_or_reports_the_gap(self) -> None:
        run = ClientRunDriver.open(self.base)
        self.addCleanup(run.close)
        run.append([self.started(run, 0)])
        request = {
            "model": "test-model",
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "draft a plan"}],
        }
        try:
            result = run.model_step(1, request)
        except SalvorAPIError as error:
            if error.code in ("model_executor_unavailable", "model_execution"):
                self.skipTest(
                    "salvor serve's client-driven model executor targets the public "
                    "endpoint (Client::from_env has no base-URL override), so the "
                    "model step cannot reach the offline demo model; see the "
                    "stub-backed DriverAgainstStub tests for the driver's model-step "
                    f"logic. server said: {error.code}"
                )
            raise
        # A reachable executor: assert the usage folds and the retry is a no-op
        # returning the recorded completion (no re-pay).
        self.assertIsNotNone(result.usage)
        retry = run.model_step(1, request)
        self.assertEqual(retry.response, result.response)
        self.assertEqual(len(run.log()), 3)


class Stub(BaseHTTPRequestHandler):
    """A minimal control plane speaking the client-driven wire shapes."""

    # Set per-test on the server instance.
    def log_message(self, *args: object) -> None:  # silence the default logging
        pass

    def _send(self, status: int, obj: dict) -> None:
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:  # noqa: N802 (stdlib naming)
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        self.server.requests.append((self.path, dict(self.headers), body))  # type: ignore[attr-defined]
        handler = self.server.routes.get(self.path)  # type: ignore[attr-defined]
        if handler is None:
            self._send(404, {"error": {"code": "unknown_run", "message": self.path}})
            return
        handler(self, body)


class DriverAgainstStub(unittest.TestCase):
    """The four server-performed methods against a wire-shape stub."""

    def make_driver(self, routes: dict) -> tuple[ClientRunDriver, ThreadingHTTPServer]:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Stub)
        server.routes = routes  # type: ignore[attr-defined]
        server.requests = []  # type: ignore[attr-defined]
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{server.server_address[1]}"
        http = httpx.Client(base_url=base, timeout=5.0)
        run = ClientRunDriver(
            http,
            run_id="11111111-1111-1111-1111-111111111111",
            drive_token="dt_test",
            log=[],
            owns_http=True,
            stream_timeout=httpx.Timeout(5.0, read=None),
        )
        self.addCleanup(run.close)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return run, server

    def test_model_step_unary_sends_request_and_parses_usage(self) -> None:
        completion = {
            "response": {"content": [{"type": "text", "text": "the plan"}]},
            "usage": {"input_tokens": 10, "output_tokens": 5},
        }
        run, server = self.make_driver(
            {"/v1/client-runs/11111111-1111-1111-1111-111111111111/model-step":
             lambda h, body: h._send(200, completion)}
        )
        result = run.model_step(3, {"model": "m", "messages": []})
        self.assertEqual(result.usage.input_tokens, 10)
        self.assertEqual(result.usage.output_tokens, 5)
        self.assertEqual(result.response["content"][0]["text"], "the plan")
        # The driver sent the reserved seq, the request, and the lease header.
        path, headers, sent = server.requests[-1]
        self.assertEqual(sent, {"seq": 3, "request": {"model": "m", "messages": []}})
        self.assertEqual(headers.get("X-Drive-Token"), "dt_test")

    def test_model_step_stream_yields_deltas_then_completion(self) -> None:
        def stream(h: Stub, body: dict) -> None:
            frames = (
                'event: delta\ndata: {"type":"text_delta","index":0,"text":"the "}\n\n'
                'event: delta\ndata: {"type":"text_delta","index":0,"text":"plan"}\n\n'
                'event: delta\ndata: {"type":"usage","output_tokens":5}\n\n'
                'event: complete\ndata: {"response":{"content":[{"type":"text",'
                '"text":"the plan"}]},"usage":{"input_tokens":10,"output_tokens":5}}\n\n'
            ).encode()
            h.send_response(200)
            h.send_header("content-type", "text/event-stream")
            h.send_header("content-length", str(len(frames)))
            h.end_headers()
            h.wfile.write(frames)

        run, _ = self.make_driver(
            {"/v1/client-runs/11111111-1111-1111-1111-111111111111/model-step": stream}
        )
        stream_handle = run.model_step_stream(3, {"model": "m", "messages": []})
        deltas = list(stream_handle)
        text = "".join(d["text"] for d in deltas if d["type"] == "text_delta")
        self.assertEqual(text, "the plan")
        self.assertTrue(any(d["type"] == "usage" for d in deltas))
        self.assertIsNotNone(stream_handle.completion)
        self.assertEqual(stream_handle.completion.usage.output_tokens, 5)

    def test_tool_step_returns_output(self) -> None:
        run, server = self.make_driver(
            {"/v1/client-runs/11111111-1111-1111-1111-111111111111/tool-step":
             lambda h, body: h._send(200, {"output": {"echo": body["input"]}})}
        )
        out = run.tool_step(5, "render", {"doc": "a.typ"}, idempotency_key="k-1")
        self.assertEqual(out, {"echo": {"doc": "a.typ"}})
        _, _, sent = server.requests[-1]
        self.assertEqual(sent["tool"], "render")
        self.assertEqual(sent["idempotency_key"], "k-1")

    def test_tool_step_dangling_write_raises_needs_reconciliation(self) -> None:
        intent = {"kind": "tool", "seq": 1, "tool": "render", "effect": "write"}
        run, _ = self.make_driver(
            {"/v1/client-runs/11111111-1111-1111-1111-111111111111/tool-step":
             lambda h, body: h._send(409, {"error": {
                 "code": "needs_reconciliation", "message": "dangling write",
                 "details": {"intent": intent}}})}
        )
        with self.assertRaises(NeedsReconciliationError) as caught:
            run.tool_step(1, "render", {"doc": "a.typ"})
        self.assertEqual(caught.exception.intent["tool"], "render")
        self.assertEqual(caught.exception.intent["effect"], "write")

    def test_resolve_posts_output(self) -> None:
        run, server = self.make_driver(
            {"/v1/client-runs/11111111-1111-1111-1111-111111111111/resolve":
             lambda h, body: h._send(200, {"run": "r", "resolved": True})}
        )
        run.resolve({"pdf": "a.pdf"})
        _, headers, sent = server.requests[-1]
        self.assertEqual(sent, {"output": {"pdf": "a.pdf"}})
        self.assertEqual(headers.get("X-Drive-Token"), "dt_test")


class LazyImport(unittest.TestCase):
    """Importing the package for graph authoring must not import httpx."""

    def test_driver_is_lazy(self) -> None:
        # A subprocess with a clean import graph proves httpx is untouched until
        # the client or the driver is reached for.
        code = (
            "import sys, salvor;"
            "assert 'salvor.client' not in sys.modules;"
            "assert 'salvor.client_runs' not in sys.modules;"
            "salvor.GraphBuilder;"
            "assert 'httpx' not in sys.modules;"
            "salvor.ClientRunDriver;"
            "assert 'salvor.client_runs' in sys.modules;"
            "print('ok')"
        )
        out = subprocess.run(
            [sys.executable, "-c", code],
            cwd=str(REPO_ROOT / "sdks" / "python"),
            capture_output=True,
            text=True,
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout.strip(), "ok")


if __name__ == "__main__":
    unittest.main()

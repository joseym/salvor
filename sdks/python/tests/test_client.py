"""Proves ``salvor.Client``'s ``labels`` surface, offline and keyless, against a
small stdlib HTTP stub speaking the documented ``POST /v1/runs`` and
``GET /v1/runs`` wire shapes (see ``crates/salvor-server/API.md``).

``start_run`` is proven two ways: labels present in the body when the caller
passes them, and the ``labels`` key entirely absent from the body when the
caller does not (mirroring the server's additive-optional contract: omitting
it is byte-identical to a caller that predates this parameter). ``list_runs``
is proven to decode ``labels`` off a run entry when the server sends it, and
to leave it ``None`` (not ``{}``) when the server omits it.

Standard library only (``unittest``, ``http.server``), plus the SDK's one
dependency ``httpx``, mirroring ``tests/test_client_runs.py``'s stub layer.
Run it with

    .venv/bin/python -m unittest tests.test_client
"""

from __future__ import annotations

import json
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

try:
    import httpx  # noqa: F401
except ImportError:
    raise unittest.SkipTest(
        "httpx is not installed; the client tests need the SDK's one "
        "dependency (pip install -e sdks/python)"
    ) from None

from salvor import Client


class Stub(BaseHTTPRequestHandler):
    """A minimal control plane speaking the ``/v1/runs`` wire shapes."""

    def log_message(self, *args: object) -> None:  # silence default logging
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
        self.server.requests.append((self.path, body))  # type: ignore[attr-defined]
        handler = self.server.routes.get(self.path)  # type: ignore[attr-defined]
        if handler is None:
            self._send(404, {"error": {"code": "unknown_run", "message": self.path}})
            return
        handler(self, body)

    def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
        self.server.requests.append((self.path, {}))  # type: ignore[attr-defined]
        handler = self.server.routes.get(self.path)  # type: ignore[attr-defined]
        if handler is None:
            self._send(404, {"error": {"code": "unknown_run", "message": self.path}})
            return
        handler(self, {})


class ClientAgainstStub(unittest.TestCase):
    """``start_run`` and ``list_runs`` against a wire-shape stub."""

    def make_client(self, routes: dict) -> tuple[Client, ThreadingHTTPServer]:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Stub)
        server.routes = routes  # type: ignore[attr-defined]
        server.requests = []  # type: ignore[attr-defined]
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{server.server_address[1]}"
        client = Client(base, timeout=5.0)
        self.addCleanup(client.close)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return client, server

    def test_start_run_sends_labels_when_given(self) -> None:
        client, server = self.make_client(
            {"/v1/runs": lambda h, body: h._send(201, {"run": "6f", "status": "running"})}
        )
        run_id = client.start_run(
            "sha256:agent", {"q": "otters"}, labels={"build": "42", "env": "prod"}
        )
        self.assertEqual(run_id, "6f")
        _, sent = server.requests[-1]
        self.assertEqual(
            sent,
            {
                "agent": "sha256:agent",
                "input": {"q": "otters"},
                "labels": {"build": "42", "env": "prod"},
            },
        )

    def test_start_run_omits_labels_when_not_given(self) -> None:
        client, server = self.make_client(
            {"/v1/runs": lambda h, body: h._send(201, {"run": "6f", "status": "running"})}
        )
        client.start_run("sha256:agent", {"q": "otters"})
        _, sent = server.requests[-1]
        self.assertNotIn("labels", sent, "no labels argument means no labels key at all")

    def test_list_client_tools_decodes_declarations_with_schemas_intact(self) -> None:
        client_tools_body = {
            "client_tools": [
                {
                    "name": "charge_card",
                    "effect": "write",
                    "input_schema": {"type": "object", "required": ["amount_cents"]},
                    "output_schema": {"type": "object", "required": ["charge_id"]},
                    "trust_completion": False,
                },
                {
                    "name": "lookup_invoice",
                    "effect": "read",
                    "input_schema": {"type": "object"},
                    "trust_completion": True,
                },
            ]
        }
        client, _ = self.make_client(
            {"/v1/client-tools": lambda h, body: h._send(200, client_tools_body)}
        )
        decls = client.list_client_tools()
        self.assertEqual(len(decls), 2)
        charge = next(d for d in decls if d.name == "charge_card")
        self.assertEqual(charge.effect, "write")
        self.assertFalse(charge.trust_completion)
        self.assertEqual(charge.input_schema, {"type": "object", "required": ["amount_cents"]})
        self.assertEqual(charge.output_schema, {"type": "object", "required": ["charge_id"]})
        lookup = next(d for d in decls if d.name == "lookup_invoice")
        self.assertIsNone(lookup.output_schema)
        self.assertTrue(lookup.trust_completion)

    def test_list_runs_decodes_labels_when_present_and_none_otherwise(self) -> None:
        runs_body = {
            "runs": [
                {
                    "run": "labeled",
                    "status": {"state": "completed", "output": "ok"},
                    "event_count": 3,
                    "labels": {"build": "42"},
                },
                {
                    "run": "unlabeled",
                    "status": {"state": "completed", "output": "ok"},
                    "event_count": 3,
                },
            ]
        }
        client, _ = self.make_client(
            {"/v1/runs": lambda h, body: h._send(200, runs_body)}
        )
        runs = client.list_runs()
        labeled = next(r for r in runs if r.run == "labeled")
        unlabeled = next(r for r in runs if r.run == "unlabeled")
        self.assertEqual(labeled.labels, {"build": "42"})
        self.assertIsNone(unlabeled.labels)


if __name__ == "__main__":
    unittest.main()

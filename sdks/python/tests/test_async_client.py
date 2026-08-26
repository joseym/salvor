"""Proves ``salvor.AsyncClient`` against the same offline stub scenarios
``salvor.Client`` is held to, by running one test body through both.

Every scenario below is written once and run twice: once through the
synchronous transport and once through the asynchronous one. That is the whole
point of the file. The two clients share a sans-IO core (:mod:`salvor._core`),
and a shared core is only worth having if something notices when a transport
stops agreeing with it, so the parity is asserted rather than assumed. A
scenario reaches its client through :func:`call`, which awaits a coroutine and
passes a plain value straight back, so a body reads the same whichever transport
is underneath it.

Standard library only (``unittest``, ``asyncio``, ``http.server``), plus the
SDK's one dependency ``httpx``, mirroring ``tests/test_client.py``'s stub layer.
Run it with

    .venv/bin/python -m unittest tests.test_async_client
"""

from __future__ import annotations

import asyncio
import inspect
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

from salvor import AsyncClient, Client, GraphBuilder, SalvorAPIError

# The same minimal two-node document ``tests/test_client_graphs.py`` submits.
DOC = (
    GraphBuilder()
    .tool("fetch", "lookup_invoice")
    .gate("approve", {"type": "object"})
    .edge("fetch", "approve")
    .build()
)


async def call(method, *args, **kwargs):
    """Call one client method, whichever transport it belongs to.

    A coroutine is awaited; a plain return value is handed back as it is. This
    is what lets a scenario be written once: ``await call(client.list_runs)``
    reads the same against both clients, and the difference stays here.
    """
    result = method(*args, **kwargs)
    if inspect.isawaitable(result):
        return await result
    return result


async def drain(stream):
    """Collect an event stream to a list, sync iterator or async iterator."""
    if hasattr(stream, "__aiter__"):
        return [event async for event in stream]
    return list(stream)


class Stub(BaseHTTPRequestHandler):
    """A minimal control plane speaking the documented wire shapes."""

    def log_message(self, *args: object) -> None:  # silence default logging
        pass

    def _send(self, status: int, obj: dict) -> None:
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse(self, text: str) -> None:
        body = text.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _dispatch(self, body: dict) -> None:
        path = self.path.split("?")[0]
        self.server.requests.append((path, body))  # type: ignore[attr-defined]
        handler = self.server.routes.get(path)  # type: ignore[attr-defined]
        if handler is None:
            self._send(404, {"error": {"code": "unknown_run", "message": self.path}})
            return
        handler(self, body)

    def do_POST(self) -> None:  # noqa: N802 (stdlib naming)
        length = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(length) or b"{}"
        try:
            body = json.loads(raw)
        except ValueError:
            # `register_agent` posts TOML; record the bytes rather than refuse.
            body = {"__raw__": raw.decode("utf-8", "replace")}
        self._dispatch(body)

    def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
        self._dispatch({})


class TransportScenarios:
    """The scenarios, written once. Two subclasses bind the two transports.

    A mixin rather than a ``TestCase`` on purpose: unittest would otherwise
    collect and run the bodies a third time with no client bound.
    """

    #: Bound by each subclass: the class under test.
    CLIENT: type = Client

    def stub(self, routes: dict) -> ThreadingHTTPServer:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Stub)
        server.routes = routes  # type: ignore[attr-defined]
        server.requests = []  # type: ignore[attr-defined]
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server

    def drive(self, routes: dict, scenario) -> None:
        """Run one scenario against a fresh stub, through this class's client.

        The whole body runs inside a single ``asyncio.run``, so an async
        client's pool lives and dies on one event loop.
        """
        server = self.stub(routes)
        base = f"http://127.0.0.1:{server.server_address[1]}"

        async def main() -> None:
            client = self.CLIENT(base, timeout=5.0)
            try:
                await scenario(client, server)
            finally:
                await call(client.close)

        asyncio.run(main())

    # -- agents ---------------------------------------------------------------

    def test_register_agent_posts_toml_and_returns_the_hash(self) -> None:
        async def scenario(client, server):
            agent = await call(client.register_agent, "name = 'demo'\n")
            self.assertEqual(agent, "sha256:34e0")
            self.assertEqual(server.requests[-1][1]["__raw__"], "name = 'demo'\n")
            agent = await call(client.register_agent, {"name": "demo"})
            self.assertEqual(server.requests[-1][1], {"name": "demo"})

        self.drive(
            {"/v1/agents": lambda h, body: h._send(201, {"agent": "sha256:34e0"})},
            scenario,
        )

    def test_list_agents_decodes_the_hashes(self) -> None:
        async def scenario(client, server):
            self.assertEqual(
                await call(client.list_agents), ["sha256:aa", "sha256:bb"]
            )

        self.drive(
            {
                "/v1/agents": lambda h, body: h._send(
                    200, {"agents": [{"agent": "sha256:aa"}, {"agent": "sha256:bb"}]}
                )
            },
            scenario,
        )

    # -- runs -----------------------------------------------------------------

    def test_start_run_sends_labels_when_given(self) -> None:
        async def scenario(client, server):
            run_id = await call(
                client.start_run,
                "sha256:agent",
                {"q": "otters"},
                labels={"build": "42", "env": "prod"},
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

        self.drive(
            {"/v1/runs": lambda h, body: h._send(201, {"run": "6f", "status": "running"})},
            scenario,
        )

    def test_start_run_omits_labels_when_not_given(self) -> None:
        async def scenario(client, server):
            await call(client.start_run, "sha256:agent", {"q": "otters"})
            _, sent = server.requests[-1]
            self.assertNotIn("labels", sent, "no labels argument means no labels key")

        self.drive(
            {"/v1/runs": lambda h, body: h._send(201, {"run": "6f", "status": "running"})},
            scenario,
        )

    def test_list_runs_decodes_labels_and_overdue(self) -> None:
        runs_body = {
            "runs": [
                {
                    "run": "labeled",
                    "status": {
                        "state": "sleeping",
                        "wake_at": "2026-07-11T13:00:00Z",
                        "overdue": True,
                        "overdue_seconds": 90,
                    },
                    "event_count": 3,
                    "labels": {"build": "42"},
                },
                {
                    "run": "unlabeled",
                    "status": {"state": "sleeping", "wake_at": "2026-07-11T13:00:00Z"},
                    "event_count": 3,
                },
            ]
        }

        async def scenario(client, server):
            runs = await call(client.list_runs)
            labeled = next(r for r in runs if r.run == "labeled")
            unlabeled = next(r for r in runs if r.run == "unlabeled")
            self.assertEqual(labeled.labels, {"build": "42"})
            self.assertIsNone(unlabeled.labels)
            self.assertTrue(labeled.status.overdue)
            self.assertEqual(labeled.status.overdue_seconds, 90)
            self.assertFalse(unlabeled.status.overdue)
            self.assertIsNone(unlabeled.status.overdue_seconds)

        self.drive({"/v1/runs": lambda h, body: h._send(200, runs_body)}, scenario)

    def test_get_run_and_replay_decode_the_derived_state(self) -> None:
        state = {
            "run": "r1",
            "status": {"state": "completed", "output": {"answer": 42}},
            "event_count": 4,
        }

        async def scenario(client, server):
            run = await call(client.get_run, "r1")
            self.assertEqual(run.status.state, "completed")
            self.assertEqual(run.event_count, 4)
            projected = await call(client.replay, "r1")
            self.assertEqual(projected.status.state, "completed")

        self.drive(
            {
                "/v1/runs/r1": lambda h, body: h._send(200, state),
                "/v1/runs/r1/replay": lambda h, body: h._send(200, state),
            },
            scenario,
        )

    def test_resume_sends_input_only_when_given(self) -> None:
        async def scenario(client, server):
            await call(client.resume, "r1")
            self.assertEqual(server.requests[-1][1], {}, "a bare resume sends no input")
            await call(client.resume, "r1", {"approved": True})
            self.assertEqual(server.requests[-1][1], {"input": {"approved": True}})

        self.drive(
            {
                "/v1/runs/r1/resume": lambda h, body: h._send(
                    200, {"run": "r1", "resumed": True, "status": {"state": "running"}}
                )
            },
            scenario,
        )

    def test_resolve_reshapes_the_nested_status_into_a_run_state(self) -> None:
        async def scenario(client, server):
            state = await call(client.resolve, "r1", {"pdf": "a.pdf"})
            self.assertEqual(state.run, "r1")
            self.assertEqual(state.status.state, "running")
            self.assertEqual(server.requests[-1][1], {"output": {"pdf": "a.pdf"}})

        self.drive(
            {
                "/v1/runs/r1/resolve": lambda h, body: h._send(
                    200, {"run": "r1", "status": {"state": "running"}, "event_count": 6}
                )
            },
            scenario,
        )

    def test_abandon_sends_reason_only_when_given(self) -> None:
        async def scenario(client, server):
            result = await call(client.abandon, "r1")
            self.assertEqual(server.requests[-1][1], {}, "a bare abandon sends no reason")
            self.assertEqual(result.run, "r1")
            self.assertEqual(result.status.state, "abandoned")
            self.assertEqual(result.appended_seq, 7)

            await call(client.abandon, "r1", "husk is dead forever")
            self.assertEqual(
                server.requests[-1][1], {"reason": "husk is dead forever"}
            )

        self.drive(
            {
                "/v1/runs/r1/abandon": lambda h, body: h._send(
                    200,
                    {
                        "run": "r1",
                        "abandoned": True,
                        "appended_seq": 7,
                        "status": {"state": "abandoned"},
                    },
                )
            },
            scenario,
        )

    def test_abandon_surfaces_the_unresolved_write_of_a_dangling_run(self) -> None:
        async def scenario(client, server):
            result = await call(client.abandon, "r1", "husk is dead forever")
            self.assertEqual(result.status.state, "abandoned")
            self.assertEqual(
                result.status.unresolved_write, {"seq": 4, "tool": "charge"}
            )

        self.drive(
            {
                "/v1/runs/r1/abandon": lambda h, body: h._send(
                    200,
                    {
                        "run": "r1",
                        "abandoned": True,
                        "appended_seq": 5,
                        "status": {
                            "state": "abandoned",
                            "unresolved_write": {"seq": 4, "tool": "charge"},
                        },
                    },
                )
            },
            scenario,
        )

    def test_a_refusal_decodes_into_the_typed_error(self) -> None:
        async def scenario(client, server):
            with self.assertRaises(SalvorAPIError) as caught:
                await call(client.get_run, "ghost")
            self.assertEqual(caught.exception.code, "unknown_run")
            self.assertEqual(caught.exception.status, 404)

        self.drive(
            {
                "/v1/runs/ghost": lambda h, body: h._send(
                    404, {"error": {"code": "unknown_run", "message": "no run `ghost`"}}
                )
            },
            scenario,
        )

    # -- graphs ---------------------------------------------------------------

    def test_submit_graph_posts_the_document_verbatim(self) -> None:
        async def scenario(client, server):
            submitted = await call(client.submit_graph, DOC)
            self.assertEqual(submitted.graph, "sha256:aa")
            self.assertTrue(submitted.created)
            # the document goes on the wire verbatim: the bytes ARE the identity
            self.assertEqual(server.requests[-1], ("/v1/graphs", DOC.to_dict()))

        self.drive(
            {
                "/v1/graphs": lambda h, body: h._send(
                    201, {"graph": "sha256:aa", "created": True}
                )
            },
            scenario,
        )

    def test_submit_graph_raises_invalid_graph_with_the_error_list(self) -> None:
        errors = [
            {
                "code": "dangling_edge",
                "message": "edge `approve` -> `ghost` references unknown node id",
                "edge": {"from": "approve", "to": "ghost"},
            }
        ]

        async def scenario(client, server):
            with self.assertRaises(SalvorAPIError) as caught:
                await call(client.submit_graph, DOC)
            self.assertEqual(caught.exception.code, "invalid_graph")
            self.assertEqual(caught.exception.details["errors"], errors)

        self.drive(
            {
                "/v1/graphs": lambda h, body: h._send(
                    400,
                    {
                        "error": {
                            "code": "invalid_graph",
                            "message": "the graph document has 1 validation error(s)",
                            "details": {"errors": errors},
                        }
                    },
                )
            },
            scenario,
        )

    def test_validate_graph_answers_invalid_rather_than_raising(self) -> None:
        async def scenario(client, server):
            report = await call(client.validate_graph, DOC.to_dict())
            self.assertFalse(report.valid)
            self.assertIsNone(report.graph, "an invalid document has no hash")
            self.assertEqual(report.errors[0].code, "duplicate_id")
            self.assertEqual(report.errors[0].node, "fetch")

        self.drive(
            {
                "/v1/graphs/validate": lambda h, body: h._send(
                    200,
                    {
                        "valid": False,
                        "errors": [
                            {
                                "code": "duplicate_id",
                                "message": "node id `fetch` is declared twice",
                                "node": "fetch",
                            }
                        ],
                    },
                )
            },
            scenario,
        )

    def test_list_and_get_graph_decode_the_shape_and_the_document(self) -> None:
        async def scenario(client, server):
            graphs = await call(client.list_graphs)
            self.assertEqual(graphs[0].graph, "sha256:aa")
            self.assertEqual(graphs[0].shape.node_count, 2)
            self.assertEqual(graphs[0].shape.entry_nodes, ["fetch"])
            stored = await call(client.get_graph, "sha256:aa")
            self.assertEqual(stored.document, DOC.to_dict())

        self.drive(
            {
                "/v1/graphs": lambda h, body: h._send(
                    200,
                    {
                        "graphs": [
                            {
                                "graph": "sha256:aa",
                                "node_count": 2,
                                "edge_count": 1,
                                "entry_nodes": ["fetch"],
                                "terminal_nodes": ["approve"],
                            }
                        ]
                    },
                ),
                "/v1/graphs/sha256:aa": lambda h, body: h._send(
                    200, {"graph": "sha256:aa", "document": DOC.to_dict()}
                ),
            },
            scenario,
        )

    def test_start_graph_run_sends_hash_input_and_labels(self) -> None:
        async def scenario(client, server):
            run_id = await call(
                client.start_graph_run, "sha256:aa", {"q": 1}, labels={"env": "prod"}
            )
            self.assertEqual(run_id, "gr1")
            self.assertEqual(
                server.requests[-1][1],
                {"graph_hash": "sha256:aa", "input": {"q": 1}, "labels": {"env": "prod"}},
            )
            await call(client.start_graph_run, "sha256:aa")
            self.assertNotIn("labels", server.requests[-1][1])

        self.drive(
            {"/v1/graph-runs": lambda h, body: h._send(201, {"run": "gr1"})}, scenario
        )

    def test_get_run_graph_decodes_the_walk(self) -> None:
        async def scenario(client, server):
            projection = await call(client.get_run_graph, "gr1")
            self.assertEqual(projection.graph_hash, "sha256:aa")
            self.assertEqual(projection.current_node, "approve")
            self.assertEqual(projection.nodes[0].node, "fetch")
            self.assertEqual(projection.nodes[1].reason, "no live inbound edge")
            self.assertIsNone(projection.forked_from, "an unforked run has no origin")

        self.drive(
            {
                "/v1/runs/gr1/graph": lambda h, body: h._send(
                    200,
                    {
                        "graph_hash": "sha256:aa",
                        "current_node": "approve",
                        "nodes": [
                            {"node": "fetch", "state": "exited"},
                            {
                                "node": "reject",
                                "state": "skipped",
                                "reason": "no live inbound edge",
                            },
                        ],
                    },
                )
            },
            scenario,
        )

    def test_fork_and_preview_share_one_endpoint_and_differ_by_dry_run(self) -> None:
        async def scenario(client, server):
            forked = await call(
                client.fork_run, "gr1", "approve", acknowledge_writes=[3]
            )
            self.assertEqual(forked.run, "child")
            self.assertEqual(forked.forked_from.run_id, "gr1")
            self.assertEqual(forked.forked_from.acknowledged_writes, [3])
            self.assertEqual(
                server.requests[-1][1],
                {"from_node": "approve", "acknowledge_writes": [3]},
            )
            self.assertNotIn("dry_run", server.requests[-1][1], "a real fork is not dry")

            preview = await call(client.preview_fork, "gr1", "approve")
            self.assertFalse(preview.would_proceed)
            self.assertEqual(preview.unacknowledged_writes, [3])
            self.assertEqual(preview.writes[0].tool, "issue_refund")
            self.assertEqual(
                server.requests[-1][1], {"from_node": "approve", "dry_run": True}
            )

        def fork(h, body):
            if body.get("dry_run"):
                h._send(
                    200,
                    {
                        "dry_run": True,
                        "origin": "gr1",
                        "from_node": "approve",
                        "through_seq": 3,
                        "graph_hash": "sha256:aa",
                        "prefix_event_count": 4,
                        "writes": [
                            {"seq": 3, "tool": "issue_refund", "input": {"amount": 12}}
                        ],
                        "unacknowledged_writes": [3],
                        "would_proceed": False,
                    },
                )
            else:
                h._send(
                    201,
                    {
                        "run": "child",
                        "status": "running",
                        "forked_from": {
                            "run_id": "gr1",
                            "through_seq": 3,
                            "from_node": "approve",
                            "graph_hash": "sha256:aa",
                            "acknowledged_writes": body.get("acknowledge_writes", []),
                        },
                    },
                )

        self.drive({"/v1/runs/gr1/fork": fork}, scenario)

    def test_fork_run_raises_write_replay_hazard_with_the_writes(self) -> None:
        writes = [{"seq": 3, "tool": "issue_refund", "effect": "write"}]

        async def scenario(client, server):
            with self.assertRaises(SalvorAPIError) as caught:
                await call(client.fork_run, "gr1", "approve")
            self.assertEqual(caught.exception.code, "write_replay_hazard")
            self.assertEqual(caught.exception.details["writes"], writes)

        self.drive(
            {
                "/v1/runs/gr1/fork": lambda h, body: h._send(
                    409,
                    {
                        "error": {
                            "code": "write_replay_hazard",
                            "message": "1 recorded write would re-fire",
                            "details": {"writes": writes},
                        }
                    },
                )
            },
            scenario,
        )

    def test_list_forks_decodes_the_derived_index(self) -> None:
        async def scenario(client, server):
            index = await call(client.list_forks, "gr1")
            self.assertEqual(index.run, "gr1")
            self.assertTrue(index.derived, "the server says out loud that this is a scan")
            self.assertEqual(index.forks[0].run, "child")
            self.assertEqual(index.forks[0].acknowledged_writes, [4])

        self.drive(
            {
                "/v1/runs/gr1/forks": lambda h, body: h._send(
                    200,
                    {
                        "run": "gr1",
                        "derived": True,
                        "forks": [
                            {
                                "run": "child",
                                "from_node": "approve",
                                "through_seq": 3,
                                "acknowledged_writes": [4],
                            }
                        ],
                    },
                )
            },
            scenario,
        )

    # -- client-performed tools ----------------------------------------------

    def test_list_client_tools_decodes_declarations_with_schemas_intact(self) -> None:
        declared = {
            "client_tools": [
                {
                    "name": "charge_card",
                    "effect": "write",
                    "input_schema": {"type": "object", "required": ["amount_cents"]},
                    "output_schema": {"type": "object", "required": ["charge_id"]},
                    "trust_completion": False,
                    "require_equal": ["amount_cents"],
                    "idempotency_key": ["order_id", "amount_cents"],
                },
                {
                    "name": "lookup_invoice",
                    "effect": "read",
                    "input_schema": {"type": "object"},
                    "trust_completion": True,
                },
            ]
        }

        async def scenario(client, server):
            decls = await call(client.list_client_tools)
            self.assertEqual(len(decls), 2)
            charge = next(d for d in decls if d.name == "charge_card")
            self.assertEqual(charge.effect, "write")
            self.assertFalse(charge.trust_completion)
            self.assertEqual(charge.require_equal, ["amount_cents"])
            self.assertEqual(charge.idempotency_key, ["order_id", "amount_cents"])
            lookup = next(d for d in decls if d.name == "lookup_invoice")
            self.assertIsNone(lookup.output_schema)
            self.assertEqual(lookup.require_equal, [])
            self.assertEqual(lookup.idempotency_key, [])

        self.drive(
            {"/v1/client-tools": lambda h, body: h._send(200, declared)}, scenario
        )

    # -- the event stream -----------------------------------------------------

    def test_stream_events_yields_the_log_then_stops_at_the_end_frame(self) -> None:
        def envelope(seq: int, kind: str) -> str:
            payload = {
                "run_id": "r1",
                "seq": seq,
                "schema_version": 1,
                "recorded_at": "2026-07-11T12:00:00Z",
                "event": {"kind": kind, "payload": {}},
            }
            return f"id: {seq}\nevent: event\ndata: {json.dumps(payload)}\n\n"

        def events(h, body):
            h._sse(
                envelope(0, "RunStarted")
                + ": keep-alive\n\n"
                + envelope(1, "RunCompleted")
                + "event: end\n"
                + 'data: {"run": "r1", "status": {"state": "completed"}}\n\n'
            )

        async def scenario(client, server):
            stream = client.stream_events("r1")
            seen = await drain(stream)
            self.assertEqual([e.seq for e in seen], [0, 1])
            self.assertEqual([e.kind for e in seen], ["RunStarted", "RunCompleted"])
            self.assertIsNotNone(stream.end)
            self.assertEqual(stream.end.status.state, "completed")

        self.drive({"/v1/runs/r1/events": events}, scenario)

    def test_stream_events_starts_from_the_cursor_it_is_given(self) -> None:
        def events(h, body):
            h.server.query.append(h.path)  # type: ignore[attr-defined]
            h._sse('event: end\ndata: {"run": "r1", "status": {"state": "completed"}}\n\n')

        async def scenario(client, server):
            server.query = []  # type: ignore[attr-defined]
            stream = client.stream_events("r1", from_seq=7)
            self.assertEqual(await drain(stream), [])
            self.assertIn("from_seq=7", server.query[0])  # type: ignore[attr-defined]

        self.drive({"/v1/runs/r1/events": events}, scenario)


class SyncTransport(TransportScenarios, unittest.TestCase):
    """Every scenario through ``salvor.Client``."""

    CLIENT = Client


class AsyncTransport(TransportScenarios, unittest.TestCase):
    """Every scenario through ``salvor.AsyncClient``, awaited.

    Identical bodies to :class:`SyncTransport`, which is the assertion: a
    behaviour that drifted between the transports would fail here and pass
    there.
    """

    CLIENT = AsyncClient


if __name__ == "__main__":
    unittest.main()

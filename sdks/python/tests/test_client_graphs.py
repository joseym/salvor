"""Proves ``salvor.Client``'s graph surface, offline and keyless, against a
small stdlib HTTP stub speaking the documented graph wire shapes (see
``crates/salvor-server/API.md``, "Graphs and graph runs"): submit, list, read
one back, validate without storing, start a run, read the run's per-node
projection, fork, preview a fork, and list the derived forks index.

Two server facts a caller WILL hit are proven here rather than left to a
docstring: a graph hash that no longer resolves comes back as a plain
``unknown_graph`` refusal (the store is in memory, so a restart drops it), and
a ``tool`` node on a stock server comes back as ``unknown_tool`` (a default
``salvor serve`` wires the tool registry empty). Both surface as the ordinary
typed :class:`~salvor.errors.SalvorAPIError`, so a caller matches a stable code
rather than parsing a sentence.

Standard library only (``unittest``, ``http.server``), plus the SDK's one
dependency ``httpx``, mirroring ``tests/test_client.py``'s stub layer. Run it
with

    .venv/bin/python -m unittest tests.test_client_graphs
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

from salvor import Client, GraphBuilder, SalvorAPIError

# A minimal two-node document, the shape the builder emits.
DOC = (
    GraphBuilder()
    .tool("fetch", "lookup_invoice")
    .gate("approve", {"type": "object"})
    .edge("fetch", "approve")
    .build()
)


class Stub(BaseHTTPRequestHandler):
    """A minimal control plane speaking the graph wire shapes."""

    def log_message(self, *args: object) -> None:  # silence default logging
        pass

    def _send(self, status: int, obj: dict) -> None:
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _dispatch(self, body: dict) -> None:
        self.server.requests.append((self.path, body))  # type: ignore[attr-defined]
        handler = self.server.routes.get(self.path)  # type: ignore[attr-defined]
        if handler is None:
            self._send(404, {"error": {"code": "unknown_graph", "message": self.path}})
            return
        handler(self, body)

    def do_POST(self) -> None:  # noqa: N802 (stdlib naming)
        length = int(self.headers.get("content-length", 0))
        self._dispatch(json.loads(self.rfile.read(length) or b"{}"))

    def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
        self._dispatch({})


class GraphsAgainstStub(unittest.TestCase):
    """The graph and graph-run verbs against a wire-shape stub."""

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

    def test_submit_graph_posts_the_document_and_decodes_the_hash(self) -> None:
        client, server = self.make_client(
            {"/v1/graphs": lambda h, body: h._send(201, {"graph": "sha256:aa", "created": True})}
        )
        submitted = client.submit_graph(DOC)
        self.assertEqual(submitted.graph, "sha256:aa")
        self.assertTrue(submitted.created)
        # the document goes on the wire verbatim: the bytes ARE the identity
        self.assertEqual(server.requests[-1], ("/v1/graphs", DOC.to_dict()))

    def test_submit_graph_accepts_a_plain_dict_as_well_as_a_built_graph(self) -> None:
        client, server = self.make_client(
            {"/v1/graphs": lambda h, body: h._send(201, {"graph": "sha256:aa", "created": False})}
        )
        submitted = client.submit_graph(DOC.to_dict())
        self.assertEqual(submitted.graph, "sha256:aa")
        self.assertFalse(
            submitted.created, "re-submitting an identical document is idempotent"
        )
        self.assertEqual(server.requests[-1][1], DOC.to_dict())

    def test_submit_graph_raises_invalid_graph_with_the_complete_error_list(self) -> None:
        errors = [
            {
                "code": "dangling_edge",
                "message": "edge `approve` -> `ghost` references unknown node id `ghost`",
                "edge": {"from": "approve", "to": "ghost"},
            }
        ]
        client, _ = self.make_client(
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
            }
        )
        with self.assertRaises(SalvorAPIError) as caught:
            client.submit_graph(DOC)
        self.assertEqual(caught.exception.code, "invalid_graph")
        self.assertEqual(caught.exception.details["errors"], errors)

    def test_list_graphs_and_get_graph_decode_the_shape_and_the_document(self) -> None:
        client, _ = self.make_client(
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
            }
        )
        graphs = client.list_graphs()
        self.assertEqual(len(graphs), 1)
        self.assertEqual(graphs[0].graph, "sha256:aa")
        self.assertEqual(graphs[0].shape.node_count, 2)
        self.assertEqual(graphs[0].shape.edge_count, 1)
        self.assertEqual(graphs[0].shape.entry_nodes, ["fetch"])
        self.assertEqual(graphs[0].shape.terminal_nodes, ["approve"])
        self.assertEqual(client.get_graph("sha256:aa").document, DOC.to_dict())

    def test_get_graph_raises_unknown_graph_for_a_hash_the_server_dropped(self) -> None:
        # The graph store is in memory, so a hash from before a restart resolves
        # to nothing. The refusal is a plain typed code, not a special case.
        client, _ = self.make_client({})
        with self.assertRaises(SalvorAPIError) as caught:
            client.get_graph("sha256:gone")
        self.assertEqual(caught.exception.code, "unknown_graph")
        self.assertEqual(caught.exception.status, 404)

    def test_validate_graph_answers_invalid_rather_than_raising(self) -> None:
        client, _ = self.make_client(
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
            }
        )
        result = client.validate_graph(DOC)
        self.assertFalse(result.valid)
        self.assertIsNone(result.graph, "an invalid document has no hash to report")
        self.assertIsNone(result.shape)
        self.assertEqual(len(result.errors), 1)
        self.assertEqual(result.errors[0].code, "duplicate_id")
        self.assertEqual(result.errors[0].node, "fetch")
        self.assertIsNone(result.errors[0].edge, "a node error names no edge")

    def test_validate_graph_on_a_valid_document_reports_hash_and_shape(self) -> None:
        client, _ = self.make_client(
            {
                "/v1/graphs/validate": lambda h, body: h._send(
                    200,
                    {
                        "valid": True,
                        "graph": "sha256:aa",
                        "summary": {
                            "node_count": 2,
                            "edge_count": 1,
                            "entry_nodes": ["fetch"],
                            "terminal_nodes": ["approve"],
                        },
                    },
                )
            }
        )
        result = client.validate_graph(DOC)
        self.assertTrue(result.valid)
        self.assertEqual(result.graph, "sha256:aa")
        self.assertIsNotNone(result.shape)
        self.assertEqual(result.shape.node_count, 2)  # type: ignore[union-attr]
        self.assertEqual(result.errors, [])

    def test_start_graph_run_sends_hash_and_input_and_returns_the_run_id(self) -> None:
        client, server = self.make_client(
            {"/v1/graph-runs": lambda h, body: h._send(201, {"run": "6f", "status": "running"})}
        )
        run_id = client.start_graph_run("sha256:aa", {"invoice": "INV-1"})
        self.assertEqual(run_id, "6f")
        self.assertEqual(
            server.requests[-1],
            ("/v1/graph-runs", {"graph_hash": "sha256:aa", "input": {"invoice": "INV-1"}}),
        )

    def test_start_graph_run_sends_labels_when_given_and_omits_them_when_not(self) -> None:
        client, server = self.make_client(
            {"/v1/graph-runs": lambda h, body: h._send(201, {"run": "6f", "status": "running"})}
        )
        client.start_graph_run("sha256:aa", labels={"build": "42"})
        self.assertEqual(server.requests[-1][1]["labels"], {"build": "42"})
        client.start_graph_run("sha256:aa")
        sent = server.requests[-1][1]
        self.assertNotIn("labels", sent, "no labels argument means no labels key at all")
        self.assertIsNone(sent["input"], "input defaults to null, not absent")

    def test_start_graph_run_surfaces_the_empty_tool_registry_as_unknown_tool(self) -> None:
        # `salvor serve` wires the tool registry EMPTY, so a `tool` node resolves
        # to nothing on a default server. Resolution happens BEFORE the run is
        # spawned, so this is a refusal with no run id, never a run that fails
        # halfway.
        client, _ = self.make_client(
            {
                "/v1/graph-runs": lambda h, body: h._send(
                    404,
                    {
                        "error": {
                            "code": "unknown_tool",
                            "message": (
                                "node `fetch` names tool `lookup_invoice`, "
                                "which is not registered"
                            ),
                        }
                    },
                )
            }
        )
        with self.assertRaises(SalvorAPIError) as caught:
            client.start_graph_run("sha256:aa")
        self.assertEqual(caught.exception.code, "unknown_tool")
        self.assertIn("fetch", caught.exception.message, "the refusal names the node")

    def test_get_run_graph_decodes_the_walk_absent_vs_present(self) -> None:
        client, _ = self.make_client(
            {
                "/v1/runs/6f/graph": lambda h, body: h._send(
                    200,
                    {
                        "graph_hash": "sha256:aa",
                        "current_node": "approve",
                        "nodes": [
                            {"node": "fetch", "state": "exited"},
                            {"node": "approve", "state": "entered"},
                            {
                                "node": "reject",
                                "state": "skipped",
                                "reason": "no live inbound edge",
                            },
                        ],
                    },
                )
            }
        )
        projection = client.get_run_graph("6f")
        self.assertEqual(projection.graph_hash, "sha256:aa")
        self.assertEqual(projection.current_node, "approve")
        self.assertEqual(len(projection.nodes), 3)
        self.assertIsNone(projection.nodes[0].reason, "an exited node has no reason")
        self.assertEqual(projection.nodes[2].reason, "no live inbound edge")
        self.assertIsNone(projection.forked_from, "an unforked run records no origin")

    def test_fork_run_acknowledges_writes_and_decodes_the_recorded_origin(self) -> None:
        def fork(h: Stub, body: dict) -> None:
            h._send(
                201,
                {
                    "run": "child",
                    "status": "running",
                    "forked_from": {
                        "run_id": "6f",
                        "through_seq": 3,
                        "from_node": "approve",
                        "graph_hash": "sha256:aa",
                        "acknowledged_writes": body.get("acknowledge_writes", []),
                    },
                },
            )

        client, server = self.make_client({"/v1/runs/6f/fork": fork})
        result = client.fork_run("6f", "approve", acknowledge_writes=[4])
        self.assertEqual(result.run, "child")
        self.assertIsNotNone(result.forked_from)
        self.assertEqual(result.forked_from.run_id, "6f")  # type: ignore[union-attr]
        self.assertEqual(result.forked_from.through_seq, 3)  # type: ignore[union-attr]
        self.assertEqual(
            result.forked_from.acknowledged_writes, [4]  # type: ignore[union-attr]
        )
        self.assertEqual(
            server.requests[-1][1], {"from_node": "approve", "acknowledge_writes": [4]}
        )
        self.assertNotIn("dry_run", server.requests[-1][1], "a real fork sends no dry_run")

    def test_fork_run_raises_write_replay_hazard_with_the_outstanding_writes(self) -> None:
        writes = [
            {"seq": 4, "tool": "issue_refund", "input": {"amount": 12}, "idempotency_key": None}
        ]
        client, _ = self.make_client(
            {
                "/v1/runs/6f/fork": lambda h, body: h._send(
                    409,
                    {
                        "error": {
                            "code": "write_replay_hazard",
                            "message": "forking run 6f would re-execute 1 recorded write(s)",
                            "details": {"writes": writes},
                        }
                    },
                )
            }
        )
        with self.assertRaises(SalvorAPIError) as caught:
            client.fork_run("6f", "approve")
        self.assertEqual(caught.exception.code, "write_replay_hazard")
        self.assertEqual(caught.exception.status, 409)
        self.assertEqual(caught.exception.details["writes"], writes)

    def test_preview_fork_sends_dry_run_and_decodes_the_preview(self) -> None:
        client, server = self.make_client(
            {
                "/v1/runs/6f/fork": lambda h, body: h._send(
                    200,
                    {
                        "dry_run": True,
                        "origin": "6f",
                        "from_node": "approve",
                        "through_seq": 3,
                        "graph_hash": "sha256:aa",
                        "prefix_event_count": 4,
                        "writes": [
                            {
                                "seq": 4,
                                "tool": "issue_refund",
                                "input": {"amount": 12},
                                "idempotency_key": None,
                            },
                            {
                                "seq": 6,
                                "tool": "notify",
                                "input": {},
                                "idempotency_key": "graph:aa:notify:0",
                            },
                        ],
                        "unacknowledged_writes": [4],
                        "would_proceed": False,
                    },
                )
            }
        )
        preview = client.preview_fork("6f", "approve")
        self.assertTrue(server.requests[-1][1]["dry_run"])
        self.assertFalse(preview.would_proceed)
        self.assertEqual(preview.prefix_event_count, 4)
        self.assertEqual(preview.unacknowledged_writes, [4])
        self.assertIsNone(
            preview.writes[0].idempotency_key, "an explicit null key is absence"
        )
        self.assertEqual(preview.writes[1].idempotency_key, "graph:aa:notify:0")

    def test_list_forks_decodes_the_derived_index(self) -> None:
        client, _ = self.make_client(
            {
                "/v1/runs/6f/forks": lambda h, body: h._send(
                    200,
                    {
                        "run": "6f",
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
            }
        )
        index = client.list_forks("6f")
        self.assertEqual(index.run, "6f")
        self.assertTrue(index.derived, "the server says out loud that this is a scan")
        self.assertEqual(index.forks[0].run, "child")
        self.assertEqual(index.forks[0].acknowledged_writes, [4])


if __name__ == "__main__":
    unittest.main()

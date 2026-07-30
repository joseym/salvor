"""Proves the Python builder emits the canonical graph document.

The single cross-language fixture is
``examples/graphs/research-review-publish.json``. This test builds the same
flow with :class:`salvor.GraphBuilder`, then asserts the emitted document
equals the fixture. Comparison is on parsed values (Python dicts and lists), so
key order does not matter; what matters is that the structure and every value
agree, and that no optional field leaked onto the wire as ``null``.

Standard library only: ``unittest`` and ``json``. Run it with

    .venv/bin/python -m unittest tests.test_graph
"""

import json
import subprocess
import sys
import unittest
from pathlib import Path

from salvor import GraphBuilder
from salvor.graph import (
    BranchCase,
    best_by,
    expression,
    fold_node,
    map_node,
    model_decision,
)

CANONICAL = (
    Path(__file__).resolve().parents[3]
    / "examples"
    / "graphs"
    / "research-review-publish.json"
)

FOLD_FIXTURE = (
    Path(__file__).resolve().parents[3] / "examples" / "graphs" / "fold-refine.json"
)

BRANCH_MODEL_DECISION_FIXTURE = (
    Path(__file__).resolve().parents[3]
    / "examples"
    / "graphs"
    / "branch-model-decision.json"
)

DRAFT_SCHEMA = {
    "type": "object",
    "properties": {"draft": {"type": "string"}},
    "required": ["draft"],
}

SCORE_SCHEMA = {
    "type": "object",
    "properties": {"score": {"type": "number"}},
    "required": ["score"],
}


def build_fold_flow():
    return (
        GraphBuilder()
        .agent("tailor", f"sha256:{'3' * 64}", output_schema=SCORE_SCHEMA)
        .fold(
            "refine",
            fold_node("tailor"),
            3,
            "score >= 0.85",
            best_by("score"),
            name="Refine to threshold",
            accumulator_schema=SCORE_SCHEMA,
        )
        .build()
    )


def build_canonical_flow():
    return (
        GraphBuilder()
        .agent("research", f"sha256:{'1' * 64}", output_schema=DRAFT_SCHEMA)
        .agent(
            "review",
            f"sha256:{'2' * 64}",
            input_schema=DRAFT_SCHEMA,
            output_schema=DRAFT_SCHEMA,
        )
        .gate(
            "approve",
            {
                "type": "object",
                "properties": {"approved": {"type": "boolean"}},
                "required": ["approved"],
            },
            prompt="Approve this draft for publication?",
        )
        .tool(
            "publish",
            "http_post",
            input={"body": "approve.draft", "url": "config.publish_url"},
        )
        .edge("research", "review")
        .edge("review", "approve")
        .edge("approve", "publish")
        .build()
    )


def build_branch_model_decision_flow():
    return (
        GraphBuilder()
        .agent("research", f"sha256:{'1' * 64}")
        .tool("assess", "assess")
        .branch(
            "route",
            [
                BranchCase("high", expression("score >= 0.8")),
                BranchCase("review", model_decision()),
            ],
            on="assess.score",
            agent_hash=f"sha256:{'4' * 64}",
        )
        .gate(
            "approve",
            {
                "type": "object",
                "properties": {"approved": {"type": "boolean"}},
                "required": ["approved"],
            },
            prompt="Approve this high-scoring draft for publication?",
        )
        .tool("publish", "http_post")
        .tool("escalate", "notify")
        .edge("research", "assess")
        .edge("assess", "route")
        .edge("route", "approve", label="high")
        .edge("approve", "publish")
        .edge("route", "escalate", label="review")
        .build()
    )


class BuildsCanonicalDocument(unittest.TestCase):
    def test_authoring_does_not_require_httpx(self):
        # Importing the builder must not pull in httpx, the client's dependency:
        # authoring a graph is stdlib-only. Checked in a fresh interpreter so the
        # assertion is independent of whatever else the suite already imported
        # into this process (a driver test, for one, legitimately loads httpx).
        code = (
            "import sys, salvor;"
            "salvor.GraphBuilder;"
            "assert 'httpx' not in sys.modules, 'importing the graph builder imported httpx';"
            "print('ok')"
        )
        out = subprocess.run(
            [sys.executable, "-c", code],
            cwd=str(Path(__file__).resolve().parents[1]),
            capture_output=True,
            text=True,
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stdout.strip(), "ok")

    def test_matches_canonical_fixture(self):
        built = build_canonical_flow().to_dict()
        # Round trip through JSON so the comparison is against parsed values,
        # exactly what `salvor graph validate` would parse.
        built = json.loads(json.dumps(built))
        canonical = json.loads(CANONICAL.read_text())
        self.assertEqual(built, canonical)

    def test_matches_fold_fixture(self):
        built = json.loads(json.dumps(build_fold_flow().to_dict()))
        canonical = json.loads(FOLD_FIXTURE.read_text())
        self.assertEqual(built, canonical)

    def test_matches_branch_model_decision_fixture(self):
        built = json.loads(json.dumps(build_branch_model_decision_flow().to_dict()))
        canonical = json.loads(BRANCH_MODEL_DECISION_FIXTURE.read_text())
        self.assertEqual(built, canonical)

    def test_every_node_kind_accepts_an_optional_display_name(self):
        graph = (
            GraphBuilder()
            .agent(
                "research",
                f"sha256:{'1' * 64}",
                name="Research the topic",
            )
            .tool("publish", "http_post", name="Publish the draft")
            .gate("approve", {"type": "object"}, name="Approve the draft")
            .branch(
                "route",
                [BranchCase("high", expression("score > 0.8"))],
                name="Route on confidence",
            )
            .map(
                "fanout",
                "route.items",
                2,
                map_node("research"),
                name="Notify each watcher",
            )
            .edge("research", "publish")
            .build()
        )
        names = [node["payload"].get("name") for node in graph.nodes]
        self.assertEqual(
            names,
            [
                "Research the topic",
                "Publish the draft",
                "Approve the draft",
                "Route on confidence",
                "Notify each watcher",
            ],
        )

        # Unset stays entirely off the wire, never a defaulted ``None`` value.
        unnamed = GraphBuilder().gate("approve", {"type": "object"}).build()
        self.assertEqual(
            list(unnamed.nodes[0]["payload"].keys()), ["id", "approval_schema"]
        )


if __name__ == "__main__":
    unittest.main()

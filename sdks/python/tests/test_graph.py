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

CANONICAL = (
    Path(__file__).resolve().parents[3]
    / "examples"
    / "graphs"
    / "research-review-publish.json"
)

DRAFT_SCHEMA = {
    "type": "object",
    "properties": {"draft": {"type": "string"}},
    "required": ["draft"],
}


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


if __name__ == "__main__":
    unittest.main()

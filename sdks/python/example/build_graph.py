"""Authoring the research -> review -> gate -> publish flow with the typed
builder, then printing the canonical document.

Run it and pipe the output straight into the CLI:

    .venv/bin/python example/build_graph.py | salvor graph validate /dev/stdin

The builder gives you editor typing and completion; the semantic checks stay
with ``salvor graph validate``.
"""

from salvor import GraphBuilder

DRAFT_SCHEMA = {
    "type": "object",
    "properties": {"draft": {"type": "string"}},
    "required": ["draft"],
}

graph = (
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

print(graph.to_json(indent=2))

/**
 * Authoring the research -> review -> gate -> publish flow with the typed
 * builder, then printing the canonical document.
 *
 * Run it after building the package:
 *
 *     npm run build
 *     node dist-example/build_graph.js | salvor graph validate /dev/stdin
 *
 * (or write the output to a file and pass that path to `salvor graph validate`).
 * The builder gives you editor completion and compile-time typing; the semantic
 * checks stay with `salvor graph validate`.
 */

import { GraphBuilder } from "@salvor-run/client";

const draftSchema = {
  type: "object",
  properties: { draft: { type: "string" } },
  required: ["draft"],
};

const graph = new GraphBuilder()
  .agent("research", `sha256:${"1".repeat(64)}`, { outputSchema: draftSchema })
  .agent("review", `sha256:${"2".repeat(64)}`, {
    inputSchema: draftSchema,
    outputSchema: draftSchema,
  })
  .gate(
    "approve",
    {
      type: "object",
      properties: { approved: { type: "boolean" } },
      required: ["approved"],
    },
    { prompt: "Approve this draft for publication?" },
  )
  .tool("publish", "http_post", {
    input: { body: "approve.draft", url: "config.publish_url" },
  })
  .edge("research", "review")
  .edge("review", "approve")
  .edge("approve", "publish")
  .build();

console.log(JSON.stringify(graph, null, 2));

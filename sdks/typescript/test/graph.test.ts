/**
 * Proves the TypeScript builder emits the canonical graph document.
 *
 * The single cross-language fixture is
 * `examples/graphs/research-review-publish.json`. This test builds the same
 * flow with {@link GraphBuilder}, then asserts the emitted document is deeply
 * equal to the fixture. Both sides are normalized through a JSON round trip
 * first, which drops any `undefined` and makes the comparison purely
 * structural (object key order does not matter to `deepStrictEqual`).
 */

import { deepStrictEqual } from "node:assert";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { test } from "node:test";

import { GraphBuilder } from "../src/graph.ts";

const draftSchema = {
  type: "object",
  properties: { draft: { type: "string" } },
  required: ["draft"],
};

function buildCanonicalFlow() {
  return new GraphBuilder()
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
}

test("builder emits the canonical graph document", () => {
  const built = JSON.parse(JSON.stringify(buildCanonicalFlow()));
  const fixturePath = resolve(
    process.cwd(),
    "../../examples/graphs/research-review-publish.json",
  );
  const canonical = JSON.parse(readFileSync(fixturePath, "utf8"));
  deepStrictEqual(built, canonical);
});

test("every node kind accepts an optional display name, present only when set", () => {
  const graph = new GraphBuilder()
    .agent("research", `sha256:${"1".repeat(64)}`, { name: "Research the topic" })
    .tool("publish", "http_post", { name: "Publish the draft" })
    .gate("approve", { type: "object" }, { name: "Approve the draft" })
    .branch("route", [{ name: "high", when: { kind: "expression", value: "score > 0.8" } }], {
      name: "Route on confidence",
    })
    .map("fanout", "route.items", 2, { kind: "node", value: "research" }, {
      name: "Notify each watcher",
    })
    .edge("research", "publish")
    .build();

  deepStrictEqual(
    graph.nodes.map((n) => n.payload.name),
    [
      "Research the topic",
      "Publish the draft",
      "Approve the draft",
      "Route on confidence",
      "Notify each watcher",
    ],
  );

  // Unset stays entirely off the wire, never a defaulted `undefined` key.
  const unnamed = new GraphBuilder().gate("approve", { type: "object" }).build();
  deepStrictEqual(Object.keys(unnamed.nodes[0].payload), ["id", "approval_schema"]);
});

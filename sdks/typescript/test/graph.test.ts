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

import { deepStrictEqual, throws } from "node:assert";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { test } from "node:test";

import { GraphBuilder, type JsonValue } from "../src/graph.ts";

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

const scoreSchema = {
  type: "object",
  properties: { score: { type: "number" } },
  required: ["score"],
};

function buildFoldFlow() {
  return new GraphBuilder()
    .agent("tailor", `sha256:${"3".repeat(64)}`, { outputSchema: scoreSchema })
    .fold(
      "refine",
      { kind: "node", value: "tailor" },
      3,
      "score >= 0.85",
      { kind: "best_by", value: "score" },
      { name: "Refine to threshold", accumulatorSchema: scoreSchema },
    )
    .build();
}

test("builder emits the canonical fold document", () => {
  const built = JSON.parse(JSON.stringify(buildFoldFlow()));
  const fixturePath = resolve(process.cwd(), "../../examples/graphs/fold-refine.json");
  const canonical = JSON.parse(readFileSync(fixturePath, "utf8"));
  deepStrictEqual(built, canonical);
});

function buildBranchModelDecisionFlow() {
  return new GraphBuilder()
    .agent("research", `sha256:${"1".repeat(64)}`)
    .tool("assess", "assess")
    .branch(
      "route",
      [
        { name: "high", when: { kind: "expression", value: "score >= 0.8" } },
        { name: "review", when: { kind: "model_decision" } },
      ],
      { on: "assess.score", agentHash: `sha256:${"4".repeat(64)}` },
    )
    .gate(
      "approve",
      {
        type: "object",
        properties: { approved: { type: "boolean" } },
        required: ["approved"],
      },
      { prompt: "Approve this high-scoring draft for publication?" },
    )
    .tool("publish", "http_post")
    .tool("escalate", "notify")
    .edge("research", "assess")
    .edge("assess", "route")
    .edge("route", "approve", "high")
    .edge("approve", "publish")
    .edge("route", "escalate", "review")
    .build();
}

test("fold declares what a reached bound means, present only when set", () => {
  for (const onBound of ["join", "fail"] as const) {
    const graph = new GraphBuilder()
      .agent("tailor", `sha256:${"3".repeat(64)}`, { outputSchema: scoreSchema })
      .fold(
        "refine",
        { kind: "node", value: "tailor" },
        3,
        "score >= 0.85",
        { kind: "best_by", value: "score" },
        { onBound },
      )
      .build();
    deepStrictEqual(graph.nodes[1].payload.on_bound, onBound);
  }

  // Unset stays entirely off the wire, so a fold's bytes are what they were
  // before the field existed.
  const quiet = buildFoldFlow();
  deepStrictEqual(Object.keys(quiet.nodes[1].payload), [
    "id",
    "body",
    "max_iterations",
    "stop_when",
    "join",
    "name",
    "accumulator_schema",
  ]);
});

test("builder emits the canonical branch model-decision document", () => {
  const built = JSON.parse(JSON.stringify(buildBranchModelDecisionFlow()));
  const fixturePath = resolve(
    process.cwd(),
    "../../examples/graphs/branch-model-decision.json",
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

test("gate accepts an intentionally empty schema", () => {
  const graph = new GraphBuilder().gate("approve", {}).build();
  deepStrictEqual(graph.nodes[0].payload.approval_schema, {});
});

test("gate rejects a non-object approval schema", () => {
  const builder = new GraphBuilder();
  throws(() => builder.gate("approve", "approved" as unknown as JsonValue), /plain JSON object/);
  throws(() => builder.gate("approve", null as unknown as JsonValue), /plain JSON object/);
  throws(
    () => builder.gate("approve", ["approved"] as unknown as JsonValue),
    /plain JSON object/,
  );
  throws(() => builder.gate("approve", 1 as unknown as JsonValue), /plain JSON object/);
  throws(() => builder.gate("approve", true as unknown as JsonValue), /plain JSON object/);
});

test("gate rejects the swapped-argument case: an options-shaped object as the schema", () => {
  const builder = new GraphBuilder();
  throws(
    () => builder.gate("approve", { prompt: "Approve this?" } as unknown as JsonValue),
    /GateOptions keys/,
  );
  throws(
    () =>
      builder.gate("approve", { name: "Approve the draft", prompt: "Approve this?" } as unknown as JsonValue),
    /GateOptions keys/,
  );
  // A schema that happens to use "name" as an ordinary JSON Schema property
  // key alongside "type"/"properties" is not ambiguous and must still pass.
  const graph = new GraphBuilder()
    .gate("approve", { type: "object", properties: { name: { type: "string" } } })
    .build();
  deepStrictEqual(graph.nodes[0].payload.approval_schema, {
    type: "object",
    properties: { name: { type: "string" } },
  });
});

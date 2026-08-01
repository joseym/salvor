/**
 * Proves `SalvorClient`'s graph surface, offline and keyless, against a small
 * `node:http` stub speaking the documented graph wire shapes (see
 * `crates/salvor-server/API.md`, "Graphs and graph runs"): submit, list, read
 * one back, validate without storing, start a run, read the run's per-node
 * projection, fork, preview a fork, and list the derived forks index.
 *
 * Two server facts a caller WILL hit are proven here rather than left to a
 * comment: a graph hash that no longer resolves comes back as a plain
 * `unknown_graph` refusal (the store is in memory, so a restart drops it), and
 * a `tool` node on a stock server comes back as `unknown_tool` (a default
 * `salvor serve` wires the tool registry empty). Both surface as the ordinary
 * typed `SalvorApiError`, so a caller matches a stable code rather than parsing
 * a sentence.
 */

import { deepStrictEqual, ok, rejects, strictEqual } from "node:assert";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { test } from "node:test";

// Imports the built output, exactly as client.test.ts does: the SDK's source
// uses `.js` NodeNext specifiers Node's type stripping does not resolve to
// `.ts`, so `dist` (rebuilt by the `pretest` hook) is the runnable entry.
import { SalvorApiError, SalvorClient } from "../dist/index.js";

type Route = (body: Record<string, unknown>) => { status: number; body: unknown };

/** A stub control plane. `requests` records every `[url, body]` for assertions. */
function stub(routes: Record<string, Route>): {
  server: Server;
  requests: [string, Record<string, unknown>][];
  ready: Promise<string>;
} {
  const requests: [string, Record<string, unknown>][] = [];
  const server = createServer((req, resp) => {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const text = Buffer.concat(chunks).toString("utf8");
      const body = text ? (JSON.parse(text) as Record<string, unknown>) : {};
      const url = req.url ?? "";
      requests.push([url, body]);
      const route = routes[url];
      if (!route) {
        resp.writeHead(404, { "content-type": "application/json" });
        resp.end(JSON.stringify({ error: { code: "unknown_graph", message: url } }));
        return;
      }
      const { status, body: respBody } = route(body);
      resp.writeHead(status, { "content-type": "application/json" });
      resp.end(JSON.stringify(respBody));
    });
  });
  const ready = new Promise<string>((resolveUrl) => {
    server.listen(0, "127.0.0.1", () => {
      const port = (server.address() as AddressInfo).port;
      resolveUrl(`http://127.0.0.1:${port}`);
    });
  });
  return { server, requests, ready };
}

/** A minimal two-node document, the shape the builder emits. */
const DOC = {
  schema_version: 1,
  nodes: [
    { kind: "tool" as const, payload: { id: "fetch", tool: "lookup_invoice" } },
    { kind: "gate" as const, payload: { id: "approve", approval_schema: { type: "object" } } },
  ],
  edges: [{ from: "fetch", to: "approve" }],
};

test("submitGraph posts the document and decodes the hash with created", async () => {
  const s = stub({
    "/v1/graphs": () => ({ status: 201, body: { graph: "sha256:aa", created: true } }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const first = await client.submitGraph(DOC);
    strictEqual(first.graph, "sha256:aa");
    strictEqual(first.created, true);
    // the document goes on the wire verbatim: the bytes ARE the identity
    deepStrictEqual(s.requests.at(-1), ["/v1/graphs", DOC]);
  } finally {
    s.server.close();
  }
});

test("submitGraph reports created: false on an idempotent re-submit", async () => {
  const s = stub({
    "/v1/graphs": () => ({ status: 201, body: { graph: "sha256:aa", created: false } }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const again = await client.submitGraph(DOC);
    strictEqual(again.graph, "sha256:aa", "the same document mints the same hash");
    strictEqual(again.created, false);
  } finally {
    s.server.close();
  }
});

test("submitGraph throws invalid_graph carrying the complete error list", async () => {
  const errors = [
    {
      code: "dangling_edge",
      message: "edge `approve` -> `ghost` references unknown node id `ghost`",
      edge: { from: "approve", to: "ghost" },
    },
  ];
  const s = stub({
    "/v1/graphs": () => ({
      status: 400,
      body: {
        error: {
          code: "invalid_graph",
          message: "the graph document has 1 validation error(s)",
          details: { errors },
        },
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    await rejects(
      () => client.submitGraph(DOC),
      (err: unknown) => {
        ok(err instanceof SalvorApiError);
        strictEqual(err.code, "invalid_graph");
        deepStrictEqual(err.details.errors, errors);
        return true;
      },
    );
  } finally {
    s.server.close();
  }
});

test("listGraphs and getGraph decode the shape summary and the document back", async () => {
  const s = stub({
    "/v1/graphs": () => ({
      status: 200,
      body: {
        graphs: [
          {
            graph: "sha256:aa",
            node_count: 2,
            edge_count: 1,
            entry_nodes: ["fetch"],
            terminal_nodes: ["approve"],
          },
        ],
      },
    }),
    "/v1/graphs/sha256:aa": () => ({
      status: 200,
      body: { graph: "sha256:aa", document: DOC },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const graphs = await client.listGraphs();
    strictEqual(graphs.length, 1);
    strictEqual(graphs[0].graph, "sha256:aa");
    strictEqual(graphs[0].nodeCount, 2);
    strictEqual(graphs[0].edgeCount, 1);
    deepStrictEqual(graphs[0].entryNodes, ["fetch"]);
    deepStrictEqual(graphs[0].terminalNodes, ["approve"]);
    const stored = await client.getGraph("sha256:aa");
    deepStrictEqual(stored.document, DOC);
  } finally {
    s.server.close();
  }
});

test("getGraph throws unknown_graph for a hash the server no longer holds", async () => {
  // The graph store is in memory, so a hash from before a restart resolves to
  // nothing. The refusal is a plain typed code, not a special case.
  const s = stub({});
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    await rejects(
      () => client.getGraph("sha256:gone"),
      (err: unknown) => {
        ok(err instanceof SalvorApiError);
        strictEqual(err.code, "unknown_graph");
        strictEqual(err.status, 404);
        return true;
      },
    );
  } finally {
    s.server.close();
  }
});

test("validateGraph resolves valid: false with the error list rather than throwing", async () => {
  const s = stub({
    "/v1/graphs/validate": () => ({
      status: 200,
      body: {
        valid: false,
        errors: [
          { code: "duplicate_id", message: "node id `fetch` is declared twice", node: "fetch" },
        ],
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const result = await client.validateGraph(DOC);
    strictEqual(result.valid, false);
    strictEqual(result.graph, undefined, "an invalid document has no hash to report");
    strictEqual(result.summary, undefined);
    strictEqual(result.errors.length, 1);
    strictEqual(result.errors[0].code, "duplicate_id");
    strictEqual(result.errors[0].node, "fetch");
    strictEqual(result.errors[0].edge, undefined, "a node error names no edge");
  } finally {
    s.server.close();
  }
});

test("validateGraph on a valid document reports the hash and the shape, storing nothing", async () => {
  const s = stub({
    "/v1/graphs/validate": () => ({
      status: 200,
      body: {
        valid: true,
        graph: "sha256:aa",
        summary: {
          node_count: 2,
          edge_count: 1,
          entry_nodes: ["fetch"],
          terminal_nodes: ["approve"],
        },
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const result = await client.validateGraph(DOC);
    strictEqual(result.valid, true);
    strictEqual(result.graph, "sha256:aa");
    strictEqual(result.summary?.nodeCount, 2);
    deepStrictEqual(result.errors, []);
  } finally {
    s.server.close();
  }
});

test("startGraphRun sends graph_hash and input, and returns the run id", async () => {
  const s = stub({
    "/v1/graph-runs": () => ({ status: 201, body: { run: "6f", status: "running" } }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const runId = await client.startGraphRun("sha256:aa", { invoice: "INV-1" });
    strictEqual(runId, "6f");
    deepStrictEqual(s.requests.at(-1), [
      "/v1/graph-runs",
      { graph_hash: "sha256:aa", input: { invoice: "INV-1" } },
    ]);
  } finally {
    s.server.close();
  }
});

test("startGraphRun sends labels when given and omits the key entirely when not", async () => {
  const s = stub({
    "/v1/graph-runs": () => ({ status: 201, body: { run: "6f", status: "running" } }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    await client.startGraphRun("sha256:aa", null, { labels: { build: "42" } });
    deepStrictEqual(s.requests.at(-1)?.[1].labels, { build: "42" });
    await client.startGraphRun("sha256:aa");
    ok(!("labels" in s.requests.at(-1)![1]), "no labels argument means no labels key at all");
    strictEqual(s.requests.at(-1)![1].input, null, "input defaults to null, not absent");
  } finally {
    s.server.close();
  }
});

test("startGraphRun surfaces the stock server's empty tool registry as unknown_tool", async () => {
  // `salvor serve` wires the tool registry EMPTY, so a `tool` node resolves to
  // nothing on a default server. Resolution happens BEFORE the run is spawned,
  // so this is a refusal with no run id, never a run that fails halfway.
  const s = stub({
    "/v1/graph-runs": () => ({
      status: 404,
      body: {
        error: {
          code: "unknown_tool",
          message: "node `fetch` names tool `lookup_invoice`, which is not registered",
        },
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    await rejects(
      () => client.startGraphRun("sha256:aa"),
      (err: unknown) => {
        ok(err instanceof SalvorApiError);
        strictEqual(err.code, "unknown_tool");
        ok(err.message.includes("fetch"), "the refusal names the offending node");
        return true;
      },
    );
  } finally {
    s.server.close();
  }
});

test("getRunGraph decodes the walk, absent-vs-present per node", async () => {
  const s = stub({
    "/v1/runs/6f/graph": () => ({
      status: 200,
      body: {
        graph_hash: "sha256:aa",
        current_node: "approve",
        nodes: [
          { node: "fetch", state: "exited" },
          { node: "approve", state: "entered" },
          { node: "reject", state: "skipped", reason: "no live inbound edge" },
        ],
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const projection = await client.getRunGraph("6f");
    strictEqual(projection.graphHash, "sha256:aa");
    strictEqual(projection.currentNode, "approve");
    strictEqual(projection.nodes.length, 3);
    strictEqual(projection.nodes[0].reason, undefined, "an exited node has no reason");
    strictEqual(projection.nodes[2].reason, "no live inbound edge");
    strictEqual(projection.forkedFrom, undefined, "an unforked run records no origin");
  } finally {
    s.server.close();
  }
});

test("forkRun acknowledges writes and decodes the recorded origin", async () => {
  const s = stub({
    "/v1/runs/6f/fork": (body) => ({
      status: 201,
      body: {
        run: "child",
        status: "running",
        forked_from: {
          run_id: "6f",
          through_seq: 3,
          from_node: "approve",
          graph_hash: "sha256:aa",
          acknowledged_writes: body.acknowledge_writes ?? [],
        },
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const result = await client.forkRun("6f", "approve", { acknowledgeWrites: [4] });
    strictEqual(result.run, "child");
    strictEqual(result.forkedFrom?.runId, "6f");
    strictEqual(result.forkedFrom?.throughSeq, 3);
    deepStrictEqual(result.forkedFrom?.acknowledgedWrites, [4]);
    deepStrictEqual(s.requests.at(-1)?.[1], { from_node: "approve", acknowledge_writes: [4] });
    ok(!("dry_run" in s.requests.at(-1)![1]), "a real fork sends no dry_run key");
  } finally {
    s.server.close();
  }
});

test("forkRun throws write_replay_hazard carrying the writes still needing acknowledgement", async () => {
  const writes = [
    { seq: 4, tool: "issue_refund", input: { amount: 12 }, idempotency_key: null },
  ];
  const s = stub({
    "/v1/runs/6f/fork": () => ({
      status: 409,
      body: {
        error: {
          code: "write_replay_hazard",
          message: "forking run 6f would re-execute 1 recorded write(s)",
          details: { writes },
        },
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    await rejects(
      () => client.forkRun("6f", "approve"),
      (err: unknown) => {
        ok(err instanceof SalvorApiError);
        strictEqual(err.code, "write_replay_hazard");
        strictEqual(err.status, 409);
        deepStrictEqual(err.details.writes, writes);
        return true;
      },
    );
  } finally {
    s.server.close();
  }
});

test("previewFork sends dry_run and decodes the preview, creating nothing", async () => {
  const s = stub({
    "/v1/runs/6f/fork": () => ({
      status: 200,
      body: {
        dry_run: true,
        origin: "6f",
        from_node: "approve",
        through_seq: 3,
        graph_hash: "sha256:aa",
        prefix_event_count: 4,
        writes: [
          { seq: 4, tool: "issue_refund", input: { amount: 12 }, idempotency_key: null },
          { seq: 6, tool: "notify", input: {}, idempotency_key: "graph:aa:notify:0" },
        ],
        unacknowledged_writes: [4],
        would_proceed: false,
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const preview = await client.previewFork("6f", "approve");
    strictEqual(s.requests.at(-1)?.[1].dry_run, true);
    strictEqual(preview.wouldProceed, false);
    strictEqual(preview.prefixEventCount, 4);
    deepStrictEqual(preview.unacknowledgedWrites, [4]);
    strictEqual(preview.writes[0].idempotencyKey, undefined, "an explicit null key is absence");
    strictEqual(preview.writes[1].idempotencyKey, "graph:aa:notify:0");
  } finally {
    s.server.close();
  }
});

test("listForks decodes the derived index", async () => {
  const s = stub({
    "/v1/runs/6f/forks": () => ({
      status: 200,
      body: {
        run: "6f",
        derived: true,
        forks: [
          { run: "child", from_node: "approve", through_seq: 3, acknowledged_writes: [4] },
        ],
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const index = await client.listForks("6f");
    strictEqual(index.run, "6f");
    strictEqual(index.derived, true, "the server says out loud that this is a scan, not a record");
    strictEqual(index.forks[0].run, "child");
    deepStrictEqual(index.forks[0].acknowledgedWrites, [4]);
  } finally {
    s.server.close();
  }
});

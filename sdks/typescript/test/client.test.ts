/**
 * Proves `SalvorClient`'s `labels` surface, offline and keyless, against a
 * small `node:http` stub speaking the documented `POST /v1/runs` and
 * `GET /v1/runs` wire shapes (see `crates/salvor-server/API.md`).
 *
 * `startRun` is proven two ways: labels present in the body when the caller
 * passes them, and the `labels` key entirely absent from the body when the
 * caller does not (mirroring the server's additive-optional contract:
 * omitting it is byte-identical to a client that predates this option).
 * `listRuns` is proven to decode `labels` off a run entry when the server
 * sends it, and to leave it `undefined` (not `{}`) when the server omits it.
 */

import { deepStrictEqual, ok, strictEqual } from "node:assert";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { test } from "node:test";

// Imports the built output, exactly as client_runs.test.ts does: the SDK's
// source uses `.js` NodeNext specifiers Node's type stripping does not
// resolve to `.ts`, so `dist` (rebuilt by the `pretest` hook) is the runnable
// entry.
import { SalvorClient } from "../dist/index.js";

type Route = (body: Record<string, unknown>) => { status: number; body: unknown };

/** A stub control plane. `requests` records every request body for assertions. */
function stub(routes: Record<string, Route>): {
  server: Server;
  requests: Record<string, unknown>[];
  ready: Promise<string>;
} {
  const requests: Record<string, unknown>[] = [];
  const server = createServer((req, resp) => {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const text = Buffer.concat(chunks).toString("utf8");
      const body = text ? (JSON.parse(text) as Record<string, unknown>) : {};
      requests.push(body);
      const route = routes[req.url ?? ""];
      if (!route) {
        resp.writeHead(404, { "content-type": "application/json" });
        resp.end(JSON.stringify({ error: { code: "unknown_run", message: req.url } }));
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

test("startRun sends labels in the body when given", async () => {
  const s = stub({
    "/v1/runs": () => ({ status: 201, body: { run: "6f", status: "running" } }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const runId = await client.startRun("sha256:agent", { q: "otters" }, {
      labels: { build: "42", env: "prod" },
    });
    strictEqual(runId, "6f");
    deepStrictEqual(s.requests.at(-1), {
      agent: "sha256:agent",
      input: { q: "otters" },
      labels: { build: "42", env: "prod" },
    });
  } finally {
    s.server.close();
  }
});

test("startRun omits labels from the body when not given", async () => {
  const s = stub({
    "/v1/runs": () => ({ status: 201, body: { run: "6f", status: "running" } }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    await client.startRun("sha256:agent", { q: "otters" });
    const sent = s.requests.at(-1)!;
    ok(!("labels" in sent), "no labels option means no labels key at all");
  } finally {
    s.server.close();
  }
});

test("listRuns decodes labels when the server sends them, and leaves it undefined otherwise", async () => {
  const s = stub({
    "/v1/runs": () => ({
      status: 200,
      body: {
        runs: [
          {
            run: "labeled",
            status: { state: "completed", output: "ok" },
            event_count: 3,
            labels: { build: "42" },
          },
          {
            run: "unlabeled",
            status: { state: "completed", output: "ok" },
            event_count: 3,
          },
        ],
      },
    }),
  });
  const base = await s.ready;
  try {
    const client = new SalvorClient(base);
    const runs = await client.listRuns();
    const labeled = runs.find((r) => r.run === "labeled")!;
    const unlabeled = runs.find((r) => r.run === "unlabeled")!;
    deepStrictEqual(labeled.labels, { build: "42" });
    strictEqual(unlabeled.labels, undefined);
  } finally {
    s.server.close();
  }
});

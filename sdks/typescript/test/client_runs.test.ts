/**
 * Proves the client-driven run driver, in two layers, both offline and keyless.
 *
 * The real-server layer drives the full control-and-context loop against the
 * actual `salvor serve` binary: open, the guarded generic append, the log
 * read-back, re-open (resume) with a fresh lease, the byte-identical idempotent
 * no-op, and the divergence refusal. It is skipped when the binaries are not
 * built.
 *
 * The stub layer exercises the four server-performed methods (`modelStep` unary
 * and streaming, `toolStep`, `resolve`) against a small `node:http` stub that
 * speaks the documented wire shapes. `salvor serve` wires its client-driven
 * model executor from `salvor_llm::Client` (public endpoint, no base-URL
 * override) and an empty tool registry, so those two side-effecting steps cannot
 * be exercised offline through `salvor serve` itself; the stub stands in for a
 * host that injects a local model executor and a tool registry, returning the
 * shapes the server test suites in `crates/salvor-server/tests` prove.
 *
 * `node:http` and `node:child_process` are test-only; the driver under test
 * (`src/client_runs.ts`) imports no Node builtin, which the grep-proof in the
 * README and the browser example rely on.
 */

import { deepStrictEqual, ok, rejects, strictEqual } from "node:assert";
import { spawn, type ChildProcess } from "node:child_process";
import { createServer, type Server } from "node:http";
import { createServer as netServer } from "node:net";
import type { AddressInfo } from "node:net";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { existsSync } from "node:fs";

// Imports the built output: the driver's source uses `.js` NodeNext specifiers
// for its sibling modules, which Node's type stripping does not resolve to
// `.ts`, so the barrel from `dist` (rebuilt by the `pretest` hook) is the
// runnable entry. The graph test imports its self-contained source directly.
import {
  ClientRunDriver,
  DivergenceError,
  NeedsReconciliationError,
  SalvorApiError,
  openClientRun,
} from "../dist/index.js";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..", "..", "..");
const SALVOR = resolve(repoRoot, "target", "debug", "salvor");
const DEMO_MODEL = resolve(repoRoot, "target", "debug", "salvor-demo-model");
const RUN_ID = "11111111-1111-1111-1111-111111111111";

type Route = (req: { body: Record<string, unknown>; headers: Record<string, string> }, res: Res) => void;
interface Res {
  json(status: number, obj: unknown): void;
  sse(frames: string): void;
}

function freePort(): Promise<number> {
  return new Promise((resolvePort) => {
    const srv = netServer();
    srv.listen(0, "127.0.0.1", () => {
      const port = (srv.address() as AddressInfo).port;
      srv.close(() => resolvePort(port));
    });
  });
}

/** A stub control plane. `requests` records every POST for assertions. */
function stub(routes: Record<string, Route>): {
  server: Server;
  requests: { path: string; headers: Record<string, string>; body: Record<string, unknown> }[];
  ready: Promise<string>;
} {
  const requests: { path: string; headers: Record<string, string>; body: Record<string, unknown> }[] = [];
  const server = createServer((req, resp) => {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const text = Buffer.concat(chunks).toString("utf8");
      const body = text ? (JSON.parse(text) as Record<string, unknown>) : {};
      const headers = req.headers as Record<string, string>;
      requests.push({ path: req.url ?? "", headers, body });
      const route = routes[req.url ?? ""];
      const res: Res = {
        json(status, obj) {
          const payload = JSON.stringify(obj);
          resp.writeHead(status, { "content-type": "application/json" });
          resp.end(payload);
        },
        sse(frames) {
          resp.writeHead(200, { "content-type": "text/event-stream" });
          resp.end(frames);
        },
      };
      if (route) route({ body, headers }, res);
      else res.json(404, { error: { code: "unknown_run", message: req.url } });
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

function driverAt(base: string): ClientRunDriver {
  return new ClientRunDriver(base, {}, 5000, {
    runId: RUN_ID,
    driveToken: "dt_test",
    log: [],
  });
}

// -- stub layer: the four server-performed methods --------------------------

test("modelStep sends the reserved seq and lease, parses usage", async () => {
  const completion = {
    response: { content: [{ type: "text", text: "the plan" }] },
    usage: { input_tokens: 10, output_tokens: 5 },
  };
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/model-step`]: (_req, res) => res.json(200, completion),
  });
  const base = await s.ready;
  try {
    const result = await driverAt(base).modelStep(3, { model: "m", messages: [] });
    strictEqual(result.usage?.inputTokens, 10);
    strictEqual(result.usage?.outputTokens, 5);
    deepStrictEqual((result.response as any).content[0].text, "the plan");
    const last = s.requests.at(-1)!;
    deepStrictEqual(last.body, { seq: 3, request: { model: "m", messages: [] } });
    strictEqual(last.headers["x-drive-token"], "dt_test");
  } finally {
    s.server.close();
  }
});

test("modelStepStream yields deltas then exposes the completion", async () => {
  const frames =
    'event: delta\ndata: {"type":"text_delta","index":0,"text":"the "}\n\n' +
    'event: delta\ndata: {"type":"text_delta","index":0,"text":"plan"}\n\n' +
    'event: delta\ndata: {"type":"usage","output_tokens":5}\n\n' +
    'event: complete\ndata: {"response":{"content":[{"type":"text","text":"the plan"}]},' +
    '"usage":{"input_tokens":10,"output_tokens":5}}\n\n';
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/model-step`]: (_req, res) => res.sse(frames),
  });
  const base = await s.ready;
  try {
    const stream = driverAt(base).modelStepStream(3, { model: "m", messages: [] });
    let text = "";
    let sawUsage = false;
    for await (const delta of stream) {
      if (delta.type === "text_delta") text += delta.text;
      if (delta.type === "usage") sawUsage = true;
    }
    strictEqual(text, "the plan");
    ok(sawUsage);
    strictEqual(stream.completion?.usage?.outputTokens, 5);
  } finally {
    s.server.close();
  }
});

test("toolStep returns the output and sends the idempotency key", async () => {
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/tool-step`]: (req, res) =>
      res.json(200, { output: { echo: req.body.input } }),
  });
  const base = await s.ready;
  try {
    const out = await driverAt(base).toolStep(5, "render", { doc: "a.typ" }, {
      idempotencyKey: "k-1",
    });
    deepStrictEqual(out, { echo: { doc: "a.typ" } });
    const last = s.requests.at(-1)!;
    strictEqual(last.body.tool, "render");
    strictEqual(last.body.idempotency_key, "k-1");
  } finally {
    s.server.close();
  }
});

test("toolStep on a dangling write throws NeedsReconciliationError with the intent", async () => {
  const intent = { kind: "tool", seq: 1, tool: "render", effect: "write" };
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/tool-step`]: (_req, res) =>
      res.json(409, {
        error: { code: "needs_reconciliation", message: "dangling write", details: { intent } },
      }),
  });
  const base = await s.ready;
  try {
    const driver = driverAt(base);
    await rejects(
      () => driver.toolStep(1, "render", { doc: "a.typ" }),
      (error: unknown) => {
        ok(error instanceof NeedsReconciliationError);
        strictEqual((error.intent as any).tool, "render");
        strictEqual((error.intent as any).effect, "write");
        return true;
      },
    );
  } finally {
    s.server.close();
  }
});

test("resolve posts the output under the lease", async () => {
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/resolve`]: (_req, res) =>
      res.json(200, { run: "r", resolved: true }),
  });
  const base = await s.ready;
  try {
    await driverAt(base).resolve({ pdf: "a.pdf" });
    const last = s.requests.at(-1)!;
    deepStrictEqual(last.body, { output: { pdf: "a.pdf" } });
    strictEqual(last.headers["x-drive-token"], "dt_test");
  } finally {
    s.server.close();
  }
});

test("append surfaces divergence as DivergenceError", async () => {
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/events`]: (_req, res) =>
      res.json(409, { error: { code: "divergence", message: "different bytes at seq 0" } }),
  });
  const base = await s.ready;
  try {
    const driver = driverAt(base);
    await rejects(
      () => driver.append([driver.envelope(0, "RunStarted", { agent_def_hash: "x", input: {} })]),
      (error: unknown) => error instanceof DivergenceError,
    );
  } finally {
    s.server.close();
  }
});

// -- real-server layer: the control-and-context loop ------------------------

let model: ChildProcess | undefined;
let serve: ChildProcess | undefined;
let base: string | undefined;

before(async () => {
  if (!existsSync(SALVOR) || !existsSync(DEMO_MODEL)) return;
  const modelPort = await freePort();
  const servePort = await freePort();
  base = `http://127.0.0.1:${servePort}`;
  model = spawn(DEMO_MODEL, ["--port", String(modelPort), "--delay-ms", "0"], {
    stdio: "ignore",
  });
  serve = spawn(SALVOR, ["--store", `/tmp/salvor-ts-driver-${servePort}.db`, "serve", "--bind", `127.0.0.1:${servePort}`], {
    stdio: "ignore",
    env: { PATH: "/usr/bin:/bin", SALVOR_DEMO_BASE_URL: `http://127.0.0.1:${modelPort}` },
  });
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const resp = await fetch(`${base}/v1/agents`);
      if (resp.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  base = undefined; // did not come up; the tests below skip
});

after(() => {
  serve?.kill();
  model?.kill();
});

test("full control loop, re-open, idempotency and divergence against salvor serve", async (t) => {
  if (!base) return t.skip("salvor serve not available (build with cargo build)");

  const run = await openClientRun(base);
  strictEqual(run.logEnvelopes.length, 0);
  ok(run.driveToken);

  const appended = await run.append([
    run.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: { topic: "otters" } }),
    run.envelope(1, "NowObserved", { now: "2026-07-11T12:00:00Z" }),
    run.envelope(2, "RunCompleted", { output: { done: true } }),
  ]);
  deepStrictEqual(appended, [0, 1, 2]);

  const log = await run.log();
  deepStrictEqual(log.map((e) => e.kind), ["RunStarted", "NowObserved", "RunCompleted"]);

  // Re-open mints a fresh lease and returns the recorded log.
  const oldToken = run.driveToken;
  const reopened = await openClientRun(base, { runId: run.runId });
  deepStrictEqual(reopened.logEnvelopes.map((e) => e.kind), ["RunStarted", "NowObserved", "RunCompleted"]);
  ok(reopened.driveToken !== oldToken);

  // Idempotent re-append of byte-identical events is a no-op reporting the seqs.
  const again = await reopened.append([
    reopened.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: { topic: "otters" } }),
  ]);
  deepStrictEqual(again, [0]);

  // Different bytes at a recorded seq diverge.
  await rejects(
    () => reopened.append([reopened.envelope(0, "RunStarted", { agent_def_hash: "sha256:OTHER", input: {} })]),
    (error: unknown) => error instanceof DivergenceError,
  );
});

test("a generic append refuses a model event", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  const run = await openClientRun(base);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: {} })]);
  const modelEvent = {
    run_id: run.runId,
    seq: 1,
    schema_version: 1,
    recorded_at: "1970-01-01T00:00:00Z",
    event: { kind: "ModelCallRequested", payload: { seq: 1, request_hash: "sha256:x", request_body: null } },
  };
  await rejects(
    () => run.append([modelEvent]),
    (error: unknown) => error instanceof SalvorApiError && error.code === "unsupported_event_kind",
  );
});

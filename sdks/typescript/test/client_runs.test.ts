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
import { tmpdir } from "node:os";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";
import { existsSync, mkdtempSync, rmSync } from "node:fs";

// Imports the built output: the driver's source uses `.js` NodeNext specifiers
// for its sibling modules, which Node's type stripping does not resolve to
// `.ts`, so the barrel from `dist` (rebuilt by the `pretest` hook) is the
// runnable entry. The graph test imports its self-contained source directly.
import {
  ClientRunDriver,
  DivergenceError,
  LeaseHeldError,
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

test("clientToolIntent sends seq/tool/input and surfaces the derived key", async () => {
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/client-tool-intent`]: (_req, res) =>
      res.json(200, {
        seq: 5,
        idempotency_key: "sha256:derived",
        effect: "write",
        settled: false,
      }),
  });
  const base = await s.ready;
  try {
    const result = await driverAt(base).clientToolIntent(5, "charge_card", {
      amount_cents: 500,
    });
    strictEqual(result.seq, 5);
    strictEqual(result.idempotencyKey, "sha256:derived");
    strictEqual(result.effect, "write");
    strictEqual(result.settled, false);
    const last = s.requests.at(-1)!;
    deepStrictEqual(last.body, { seq: 5, tool: "charge_card", input: { amount_cents: 500 } });
    strictEqual(last.headers["x-drive-token"], "dt_test");
  } finally {
    s.server.close();
  }
});

test("clientToolIntent surfaces settled true on a re-post", async () => {
  // A payments caller re-posting an intent after the completion already
  // landed must be able to tell the work is done from the response alone.
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/client-tool-intent`]: (_req, res) =>
      res.json(200, {
        seq: 5,
        idempotency_key: "sha256:derived",
        effect: "write",
        settled: true,
      }),
  });
  const base = await s.ready;
  try {
    const result = await driverAt(base).clientToolIntent(5, "charge_card", {
      amount_cents: 500,
    });
    strictEqual(result.settled, true);
  } finally {
    s.server.close();
  }
});

test("clientToolIntent on an undeclared tool throws SalvorApiError with code unknown_tool", async () => {
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/client-tool-intent`]: (_req, res) =>
      res.json(404, {
        error: { code: "unknown_tool", message: "no client-performed tool named `ghost`" },
      }),
  });
  const base = await s.ready;
  try {
    const driver = driverAt(base);
    await rejects(
      () => driver.clientToolIntent(5, "ghost", {}),
      (error: unknown) => error instanceof SalvorApiError && error.code === "unknown_tool",
    );
  } finally {
    s.server.close();
  }
});

test("clientToolCompletion sends seq and output under the lease", async () => {
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/client-tool-completion`]: (_req, res) =>
      res.json(200, { seq: 5, completed: true }),
  });
  const base = await s.ready;
  try {
    await driverAt(base).clientToolCompletion(5, { charge_id: "ch_1" });
    const last = s.requests.at(-1)!;
    deepStrictEqual(last.body, { seq: 5, output: { charge_id: "ch_1" } });
    strictEqual(last.headers["x-drive-token"], "dt_test");
  } finally {
    s.server.close();
  }
});

test("clientToolCompletion refused (trust_completion=false) throws SalvorApiError with code client_completion_refused", async () => {
  const s = stub({
    [`/v1/client-runs/${RUN_ID}/client-tool-completion`]: (_req, res) =>
      res.json(403, {
        error: {
          code: "client_completion_refused",
          message: "tool `charge_card` is declared with trust_completion = false",
        },
      }),
  });
  const base = await s.ready;
  try {
    const driver = driverAt(base);
    await rejects(
      () => driver.clientToolCompletion(5, { charge_id: "ch_1" }),
      (error: unknown) =>
        error instanceof SalvorApiError && error.code === "client_completion_refused",
    );
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
/**
 * This layer's own directory under the system temp dir, holding its store.
 * Made with `mkdtemp` and removed whole in `after`, pass or fail: a store named
 * after a port is world-readable, outlives the run, and leaves one more triple
 * of files behind every time the suite is run.
 */
let storeDir: string | undefined;
// The demo model logs one line per request to stderr; accumulating it is what
// lets the retry test count provider hits (the no-re-pay proof).
let modelLog = "";
const providerHits = () => (modelLog.match(/request #/g) ?? []).length;

before(async () => {
  if (!existsSync(SALVOR) || !existsSync(DEMO_MODEL)) return;
  storeDir = mkdtempSync(join(tmpdir(), "salvor-ts-"));
  const modelPort = await freePort();
  const servePort = await freePort();
  base = `http://127.0.0.1:${servePort}`;
  model = spawn(DEMO_MODEL, ["--port", String(modelPort), "--delay-ms", "0"], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  model.stderr?.on("data", (chunk: Buffer) => {
    modelLog += chunk.toString("utf8");
  });
  serve = spawn(SALVOR, ["--store", join(storeDir, "driver.db"), "serve", "--bind", `127.0.0.1:${servePort}`], {
    stdio: "ignore",
    env: {
      PATH: "/usr/bin:/bin",
      // The demo agent's own model route (server-driven runs).
      SALVOR_DEMO_BASE_URL: `http://127.0.0.1:${modelPort}`,
      // The client-driven model step's executor route: the same offline
      // scripted model, no key needed.
      SALVOR_MODEL_BASE_URL: `http://127.0.0.1:${modelPort}`,
    },
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
  if (storeDir) rmSync(storeDir, { recursive: true, force: true });
});

test("full control loop, re-open with the held lease, idempotency and divergence against salvor serve", async (t) => {
  if (!base) return t.skip("salvor serve not available (build with cargo build)");

  const run = await openClientRun(base);
  strictEqual(run.logEnvelopes.length, 0);
  ok(run.driveToken);

  const appended = await run.append([
    run.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: { topic: "otters" } }),
    run.envelope(1, "NowObserved", { now: "2026-07-11T12:00:00Z" }),
  ]);
  deepStrictEqual(appended, [0, 1]);

  const log = await run.log();
  deepStrictEqual(log.map((e) => e.kind), ["RunStarted", "NowObserved"]);

  // The lease is current, so a bare re-open (no token) is refused: the
  // driver that already has the run keeps it, and nothing is minted.
  const oldToken = run.driveToken;
  await rejects(
    () => openClientRun(base, { runId: run.runId }),
    (error: unknown) => error instanceof LeaseHeldError && error.lapsesInSeconds > 0,
  );

  // Presenting that lease's own token re-opens under the SAME token: the
  // run's own driver rebuilding its cursor, not a second writer.
  const reopened = await openClientRun(base, { runId: run.runId, driveToken: oldToken });
  deepStrictEqual(reopened.logEnvelopes.map((e) => e.kind), ["RunStarted", "NowObserved"]);
  strictEqual(reopened.driveToken, oldToken, "the same token comes back, not a fresh one");

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

  // A finished run is never held: once RunCompleted lands, anyone re-opens
  // it straight away, no token needed.
  await reopened.append([reopened.envelope(2, "RunCompleted", { output: { done: true } })]);
  const finished = await openClientRun(base, { runId: run.runId });
  deepStrictEqual(
    finished.logEnvelopes.map((e) => e.kind),
    ["RunStarted", "NowObserved", "RunCompleted"],
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

test("live model step records, returns, and never re-pays against salvor serve", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  // SALVOR_MODEL_BASE_URL points the serve binary's real executor at the
  // scripted demo model, so this is a genuine server-performed provider call,
  // keyless and offline. One user message reaches the demo model as its
  // scripted turn 1: a search_notes tool_use response with usage 200 in / 20 out.
  const run = await openClientRun(base);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: {} })]);
  const request = {
    model: "test-model",
    max_tokens: 256,
    messages: [{ role: "user", content: "draft a plan" }],
  };

  const before = providerHits();
  const result = await run.modelStep(1, request);
  strictEqual(result.usage?.inputTokens, 200);
  strictEqual(result.usage?.outputTokens, 20);
  const content = (result.response as any).content[0];
  strictEqual(content.type, "tool_use");
  strictEqual(content.name, "search_notes");
  strictEqual(providerHits(), before + 1, "one live provider call");

  const log = await run.log();
  deepStrictEqual(
    log.map((e) => e.kind),
    ["RunStarted", "ModelCallRequested", "ModelCallCompleted"],
  );

  // The same step again: the recorded completion comes back verbatim, the
  // provider is not hit again, and the log does not grow. No re-pay.
  const retry = await run.modelStep(1, request);
  deepStrictEqual(retry.raw, result.raw, "the recorded completion, verbatim");
  strictEqual(providerHits(), before + 1, "retry paid nothing");
  strictEqual((await run.log()).length, 3, "no growth");
});


test("a client-performed model call records, replays, and diverges on a different hash", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  // Nothing here calls a provider. That is the point: the middleware holds the
  // key and the model configuration, calls the provider itself, and hands
  // salvor the hash and the answer so a later drive replays the answer.
  const run = await openClientRun(base);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: {} })]);

  const before = providerHits();
  const hash = "sha256:client-request-1";
  const opened = await run.clientModelIntent(1, hash);
  strictEqual(opened.settled, false, "a fresh intent has to be performed");
  strictEqual(opened.response, undefined);

  // The client calls the provider here, in its own process.
  const answer = { content: [{ type: "text", text: "the plan" }] };
  await run.clientModelCompletion(1, answer, { inputTokens: 10, outputTokens: 5 });

  const log = await run.log();
  deepStrictEqual(
    log.map((e) => e.kind),
    ["RunStarted", "ModelCallRequested", "ModelCallCompleted"],
  );
  strictEqual(
    (log[1].payload as any).performed_by,
    "client",
    "the log says the client performed it",
  );

  // The replay: the same position and hash on a later drive answers with the
  // recorded completion, so the middleware short-circuits and pays nothing.
  const replayed = await run.clientModelIntent(1, hash);
  strictEqual(replayed.settled, true);
  deepStrictEqual(replayed.response, answer, "the recorded answer, verbatim");
  strictEqual(replayed.usage?.inputTokens, 10);
  strictEqual(replayed.usage?.outputTokens, 5);
  strictEqual((await run.log()).length, 3, "the replay wrote nothing");
  strictEqual(providerHits(), before, "no provider was called at any point");

  // A different hash at that position is the client disagreeing with its own log.
  await rejects(
    () => run.clientModelIntent(1, "sha256:a-different-request"),
    (error: unknown) => error instanceof DivergenceError,
  );

  // And a completion with nothing outstanding is refused.
  await rejects(
    () => run.clientModelCompletion(2, answer, { inputTokens: 1, outputTokens: 1 }),
    (error: unknown) => error instanceof DivergenceError,
  );
});

test("a sleep parks the run, refuses to wake early, and continues after the deadline", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  // Three drives over one run: park, come back too soon, come back late. Each
  // drive is a fresh driver re-opened on the same run id, which is what a later
  // drive actually is: a process holding only the recorded log and its own
  // clock. The second one is the point of the whole feature. It runs the
  // identical code with a clock ten minutes on and appends nothing, because the
  // deadline it compares against comes from the log rather than from how long
  // this process has been awake.
  const startedAt = new Date("2026-07-11T12:00:00.000Z");
  const hour = 60 * 60 * 1000;

  const first = await openClientRun(base);
  first.clock = () => startedAt;
  await first.append([first.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: {} })]);
  const wakeAt = await first.sleepFor(1, hour);
  strictEqual(wakeAt.getTime(), startedAt.getTime() + hour);
  deepStrictEqual(
    (await first.log()).map((e) => e.kind),
    ["RunStarted", "NowObserved", "SleepStarted"],
  );
  const parked = await first.awaitWake(3);
  strictEqual(parked.woken, false, "the deadline is an hour away");
  strictEqual(parked.wakeAt?.getTime(), wakeAt.getTime());
  strictEqual((await first.log()).length, 3, "asking appended nothing");

  // Ten minutes later: the replayed instants are the recorded ones, so the
  // deadline has not moved and the run stays asleep. The lease `first` opened
  // is still current at this point (nothing here waits a real minute, let
  // alone ten), so this drive is the SAME driver taking its own run up again,
  // presenting the token it already holds rather than a bare re-open, which
  // a run still held would refuse.
  const early = await openClientRun(base, { runId: first.runId, driveToken: first.driveToken });
  early.clock = () => new Date(startedAt.getTime() + 10 * 60 * 1000);
  const replayed = await early.sleepFor(1, hour);
  strictEqual(replayed.getTime(), wakeAt.getTime(), "the wake instant reproduces on replay");
  strictEqual((await early.awaitWake(3)).woken, false, "driving early does not wake a run");
  strictEqual((await early.log()).length, 3, "and appends nothing at all");

  // Two hours later: the deadline has passed, so this drive closes the pair
  // itself and the run carries on to its result. Same reasoning as `early`:
  // presenting the still-current lease's own token (which re-opening under a
  // held token always hands back unchanged, so it is still `first`'s) is what
  // lets this drive take the run up rather than being refused.
  const late = await openClientRun(base, { runId: first.runId, driveToken: early.driveToken });
  late.clock = () => new Date(startedAt.getTime() + 2 * hour);
  strictEqual((await late.sleepFor(1, hour)).getTime(), wakeAt.getTime());
  const woken = await late.awaitWake(3);
  strictEqual(woken.woken, true, "the deadline passed, so the sleep is over");
  await late.append([late.envelope(4, "RunCompleted", { output: { slept: true } })]);
  deepStrictEqual(
    (await late.log()).map((e) => e.kind),
    ["RunStarted", "NowObserved", "SleepStarted", "SleepCompleted", "RunCompleted"],
  );

  // A fourth drive replays the closed pair: the completion is recorded, so
  // nothing appends however early this drive's clock reads.
  const after = await openClientRun(base, { runId: first.runId });
  after.clock = () => startedAt;
  strictEqual((await after.awaitWake(3)).woken, true, "a recorded wake replays");
  strictEqual((await after.log()).length, 5, "and the log did not grow");
});

test("a sleep completion with no sleep started is refused", async (t) => {
  if (!base) return t.skip("salvor serve not available");
  // The server checks the pair order, so a driver that closes a sleep it never
  // opened is told so rather than writing a log that lies.
  const run = await openClientRun(base);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: "sha256:agent", input: {} })]);
  await rejects(
    () => run.append([run.envelope(1, "SleepCompleted")]),
    (error: unknown) => error instanceof DivergenceError,
  );
  strictEqual((await run.log()).length, 1, "the refusal wrote nothing");
});

/**
 * Drive a client-driven run from TypeScript: the second of Salvor's two modes.
 *
 * Where example/answer.ts hands the loop to the server (server-driven mode),
 * this script owns the loop itself and streams the events it produces to the
 * server, which owns the durable log and guards every append. The client
 * appends its own control and context events, asks the server to perform the one
 * side-effecting model step (the server holds the key), then re-opens a run and
 * re-drives it from the fetched log to show a recorded run replays with zero
 * live calls.
 *
 * The model step is the only leg that needs a model the server can reach.
 * `salvor serve`'s client-driven model executor reads `ANTHROPIC_API_KEY` for
 * its credential and honors `SALVOR_MODEL_BASE_URL` to target a local or
 * offline endpoint speaking the same wire protocol. This leg streams, so it
 * paints live against an endpoint that streams (the public one, with a key on
 * the server's environment):
 *
 *     ANTHROPIC_API_KEY=sk-ant-... \
 *         target/debug/salvor serve --bind 127.0.0.1:8080 --store /tmp/loop.db
 *
 * then build the package and run this file:
 *
 *     npm install && npm run build
 *     node --experimental-strip-types example/client_run_loop.ts http://127.0.0.1:8080
 *
 * With no reachable model, or one that does not stream (the scripted demo
 * model answers plain JSON only), the control loop and the replay still run
 * end to end and the model leg reports why it skipped, so this is runnable
 * offline as-is. examples/browser-client-run shows the unary-retry fallback
 * that completes the step against the demo model. In your own project you
 * would `import { SalvorClient } from "@salvor/client"`.
 */

import { SalvorApiError, SalvorClient, SalvorStreamError } from "../dist/index.js";
import type { ClientRunDriver } from "../dist/index.js";

const baseUrl = process.argv[2] ?? "http://127.0.0.1:8080";

const AGENT = "sha256:example-client-run";
const REQUEST = {
  model: "claude-sonnet-4-5",
  max_tokens: 256,
  messages: [{ role: "user", content: "In one word, name a durable-agent property." }],
};

const client = new SalvorClient(baseUrl);

/**
 * Open a run, drive a control-only loop, then re-open and replay it. Every event
 * rides the generic guarded append, which holds no secret and no side effect, so
 * this whole leg runs offline with no model at all.
 */
async function controlLoopThenReplay(): Promise<void> {
  const run = await client.openClientRun();
  console.log(`opened run ${run.runId}`);
  await run.append([
    run.envelope(0, "RunStarted", { agent_def_hash: AGENT, input: { topic: "otters" } }),
    run.envelope(1, "NowObserved", { now: "2026-07-11T12:00:00Z" }),
    run.envelope(2, "RunCompleted", { output: { done: true } }),
  ]);
  console.log("appended RunStarted, NowObserved, RunCompleted");

  // Re-open: a fresh lease plus every recorded envelope, ready to rebuild a
  // cursor. Re-driving from this log pays nothing; each step replays.
  const reopened = await client.openClientRun({ runId: run.runId });
  console.log(
    `re-opened ${reopened.runId}; replaying ${reopened.logEnvelopes.length} events:`,
  );
  for (const event of reopened.logEnvelopes) {
    console.log(`  seq ${String(event.seq).padStart(2)}  ${event.kind}`);
  }
  console.log("replayed from the log; zero live calls\n");
}

/**
 * Reserve a model intent and ask the server to perform the call, streaming a
 * live ticker. When the server has no reachable model, or its endpoint does
 * not stream, this reports why and leaves the run at its dangling model
 * intent, which a retry (streaming or unary) would re-issue safely.
 */
async function modelStepLeg(): Promise<void> {
  const run: ClientRunDriver = await client.openClientRun({ recordPrompts: true });
  console.log(`opened run ${run.runId} for the streaming model step`);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: AGENT, input: {} })]);
  try {
    const stream = run.modelStepStream(1, REQUEST);
    let ticker = "";
    for await (const delta of stream) {
      if (delta.type === "text_delta") ticker += delta.text;
    }
    process.stdout.write(`  ticker: ${ticker}\n`);
    const usage = stream.completion?.usage;
    console.log(`model step recorded; usage ${usage?.inputTokens} in / ${usage?.outputTokens} out`);
    await run.append([run.envelope(3, "RunCompleted", { output: { answered: true } })]);
    console.log("completed the run after the model step");
  } catch (error) {
    // A model-side failure: a pre-stream 5xx (SalvorApiError), or a mid-stream
    // failure delivered as an `error` frame (SalvorStreamError), which is also
    // what an endpoint that does not stream produces.
    const modelSide =
      (error instanceof SalvorApiError &&
        (error.code === "model_executor_unavailable" ||
          error.code === "model_execution")) ||
      error instanceof SalvorStreamError;
    if (modelSide) {
      console.log(
        "model step skipped: no reachable model, or the endpoint does not stream",
      );
      return;
    }
    throw error;
  }
}

console.log("== control loop and replay (always offline) ==");
await controlLoopThenReplay();
console.log("== streaming model step (the one side-effecting leg) ==");
await modelStepLeg();

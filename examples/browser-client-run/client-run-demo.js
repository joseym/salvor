/**
 * The client-driven run demo logic, with the DOM held behind a `sink` seam so
 * the exact same code runs in a browser tab and headless under Node.
 *
 * It imports the built `@salvor/client` SDK by relative path (the pattern
 * examples/polyglot-service uses), so no bundler is involved: a browser loads
 * this module with `<script type="module">` and it pulls the SDK's `dist` over
 * the same origin.
 *
 * The demo does three things, each a leg of Salvor's client-driven mode:
 *
 *   A. Control loop and replay. Open a run, append its own control and context
 *      events (RunStarted, NowObserved, RunCompleted) through the guarded
 *      append, then RE-OPEN the run and re-drive it from the fetched log. Every
 *      event replays from the log with zero live calls. This is the durable
 *      guarantee, and it runs offline with no model at all.
 *
 *   B. Streaming model step. On a second run, ask the server to perform a model
 *      call with the streaming variant, painting each ticker delta into the sink
 *      as it arrives. The server holds the key and records the completion. When
 *      the server has no reachable model (see the README on `salvor serve`'s
 *      executor) this reports the gap and the ticker stays empty.
 *
 *   C. Tool step. Attempt a server-performed tool call. `salvor serve` wires an
 *      empty tool registry, so this reports `unknown_tool`; a host that
 *      registers a tool (the composition pattern the design intends) would see
 *      it dispatched and recorded.
 */

import {
  openClientRun,
  SalvorApiError,
  SalvorStreamError,
} from "../../sdks/typescript/dist/index.js";

const AGENT = "sha256:browser-client-run";
const REQUEST = {
  model: "claude-sonnet-4-5",
  max_tokens: 256,
  messages: [{ role: "user", content: "In one short sentence, why record model calls?" }],
};

/**
 * Run the whole demo against `baseUrl`, reporting through `sink`.
 * @param {string} baseUrl the control plane, e.g. http://127.0.0.1:8080
 * @param {{ line(text: string): void, tick(text: string): void,
 *           section(text: string): void }} sink
 */
export async function runClientRunDemo(baseUrl, sink) {
  await controlLoopThenReplay(baseUrl, sink);
  await streamingModelStep(baseUrl, sink);
  await toolStep(baseUrl, sink);
  sink.section("done");
}

async function controlLoopThenReplay(baseUrl, sink) {
  sink.section("A. control loop and replay (always offline)");
  const run = await openClientRun(baseUrl);
  sink.line(`opened run ${run.runId}`);
  await run.append([
    run.envelope(0, "RunStarted", { agent_def_hash: AGENT, input: { topic: "otters" } }),
    run.envelope(1, "NowObserved", { now: "2026-07-11T12:00:00Z" }),
    run.envelope(2, "RunCompleted", { output: { done: true } }),
  ]);
  sink.line("appended RunStarted, NowObserved, RunCompleted");

  // Re-open: a fresh lease and every recorded envelope, ready to rebuild a
  // cursor. Re-driving from this log pays nothing; each step replays.
  const reopened = await openClientRun(baseUrl, { runId: run.runId });
  sink.line(`re-opened ${reopened.runId}; replaying ${reopened.logEnvelopes.length} events:`);
  for (const event of reopened.logEnvelopes) {
    sink.line(`  seq ${event.seq}  ${event.kind}`);
  }
  sink.line("replayed from the log; zero live calls");
}

async function streamingModelStep(baseUrl, sink) {
  sink.section("B. streaming model step (live ticker)");
  const run = await openClientRun(baseUrl, { recordPrompts: true });
  sink.line(`opened run ${run.runId}`);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: AGENT, input: {} })]);
  try {
    const stream = run.modelStepStream(1, REQUEST);
    let ticker = "";
    for await (const delta of stream) {
      if (delta.type === "text_delta") {
        ticker += delta.text;
        sink.tick(ticker); // paint the live ticker as tokens arrive
      }
    }
    const usage = stream.completion?.usage;
    sink.line(`model step recorded; usage ${usage?.inputTokens} in / ${usage?.outputTokens} out`);
    await run.append([run.envelope(3, "RunCompleted", { output: { answered: true } })]);
    sink.line("completed the run after the model step");
  } catch (error) {
    const noModel =
      (error instanceof SalvorApiError &&
        (error.code === "model_executor_unavailable" || error.code === "model_execution")) ||
      error instanceof SalvorStreamError;
    if (noModel) {
      sink.line("model step skipped: server has no reachable model (see README)");
      return;
    }
    throw error;
  }
}

async function toolStep(baseUrl, sink) {
  sink.section("C. tool step");
  const run = await openClientRun(baseUrl);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: AGENT, input: {} })]);
  try {
    const output = await run.toolStep(1, "render", { doc: "plan.typ" });
    sink.line(`tool step output: ${JSON.stringify(output)}`);
  } catch (error) {
    if (error instanceof SalvorApiError && error.code === "unknown_tool") {
      sink.line("tool step reported unknown_tool: salvor serve wires an empty registry");
      return;
    }
    throw error;
  }
}

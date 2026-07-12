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
 *   B. Model step. On a second run, ask the server to perform a model call with
 *      the streaming variant, painting each ticker delta into the sink as it
 *      arrives. The server holds the credential and records the completion.
 *      When the model endpoint cannot stream (the offline demo model answers
 *      plain JSON only), the failed stream leaves a dangling intent and the
 *      demo retries with the unary step, which re-issues that intent safely
 *      and completes; the assembled text then paints at once. The README shows
 *      pointing `salvor serve`'s executor at the offline model with
 *      `SALVOR_MODEL_BASE_URL`.
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
  sink.section("B. model step (streaming, with a unary retry fallback)");
  const run = await openClientRun(baseUrl, { recordPrompts: true });
  sink.line(`opened run ${run.runId}`);
  await run.append([run.envelope(0, "RunStarted", { agent_def_hash: AGENT, input: {} })]);

  // Prefer the streaming variant: each ticker delta paints as it arrives.
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
  } catch (error) {
    const modelSide =
      (error instanceof SalvorApiError &&
        (error.code === "model_executor_unavailable" || error.code === "model_execution")) ||
      error instanceof SalvorStreamError;
    if (!modelSide) throw error;
    // The offline demo model answers plain JSON only, so the stream fails and
    // leaves a dangling intent. Retry identity is (seq, request_hash): the
    // unary step at the same position re-issues that intent safely (an
    // unanswered model request has no external effect to double) and records
    // the completion. This is the crash story, driven on purpose.
    sink.line("stream unavailable from this model endpoint; retrying as a unary step");
    try {
      const result = await run.modelStep(1, REQUEST);
      const text = (result.response?.content ?? [])
        .map((block) =>
          block.type === "text" ? block.text : `[${block.type}: ${block.name ?? ""}]`,
        )
        .join("");
      sink.tick(text); // the assembled response paints at once
      const usage = result.usage;
      sink.line(`model step recorded; usage ${usage?.inputTokens} in / ${usage?.outputTokens} out`);
    } catch (retryError) {
      const noModel =
        retryError instanceof SalvorApiError &&
        (retryError.code === "model_executor_unavailable" ||
          retryError.code === "model_execution");
      if (!noModel) throw retryError;
      sink.line("model step skipped: server has no reachable model (see README)");
      return;
    }
  }
  await run.append([run.envelope(3, "RunCompleted", { output: { answered: true } })]);
  sink.line("completed the run after the model step");
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

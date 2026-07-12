/**
 * Drive a running Salvor control plane from TypeScript, end to end.
 *
 * This is the twin of ../python/service.py: the same "durable backend in any
 * language" story. One durable Rust process (`salvor serve`) does all the work,
 * and this thin async client registers an agent, starts a run, streams its
 * events live, handles a human-in-the-loop park, and streams to completion.
 *
 * The run parks on purpose. The agent (../agent.toml) declares a low step
 * budget, so after eight model calls the run stops as `budget_exceeded` and
 * waits. That is a real suspension held on the server: the process would sit
 * there across a restart. This script reads the park over HTTP, grants a budget
 * extension, and resumes, all over the same control plane.
 *
 * It imports the SDK's built output by relative path, so no install is needed
 * beyond `npm run build` in sdks/typescript. In your own project you would
 * `import { SalvorClient } from "@salvor/client"`.
 *
 * Bring the offline stack up first (see ../README.md or ../run.sh), then:
 *
 *     node --experimental-strip-types service.ts http://127.0.0.1:8080
 */

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { SalvorClient } from "../../../sdks/typescript/dist/index.js";
import type { EventStream } from "../../../sdks/typescript/dist/index.js";

const baseUrl = process.argv[2] ?? "http://127.0.0.1:8080";

// The budget extension we grant when the run parks: eight more steps than the
// 20-model-call run needs, so it drives cleanly to completion after the resume.
// This is exactly the shape POST /v1/runs/{id}/resume accepts for a parked
// budget: {"input": {"extend": {"steps": N}}}. The SDK wraps it under "input".
const STEP_EXTENSION = 40;

const here = dirname(fileURLToPath(import.meta.url));
const agentToml = readFileSync(join(here, "..", "agent.toml"), "utf8");

const client = new SalvorClient(baseUrl);

/**
 * Stream events from a cursor until the run rests; return the next seq. Prints
 * one line per event. The server replays the log from `fromSeq`, tails live
 * events, then closes at the terminal end frame with the resting status.
 */
async function streamToRest(runId: string, fromSeq = 0): Promise<number> {
  const stream: EventStream = client.streamEvents(runId, { fromSeq });
  let nextSeq = fromSeq;
  for await (const event of stream) {
    nextSeq = event.seq + 1;
    const detail =
      typeof event.payload.tool === "string" ? `  (${event.payload.tool})` : "";
    console.log(`  seq ${String(event.seq).padStart(2)}  ${event.kind}${detail}`);
  }
  const resting = stream.end?.status?.state ?? "unknown";
  console.log(`  -- stream closed; run is ${resting}`);
  return nextSeq;
}

// 1. Register the definition once. The server validates it (builds the agent)
//    and returns a content hash; runs reference it by that hash.
const agent = await client.registerAgent(agentToml);
console.log(`registered agent ${agent}`);

// 2. Start a run. Resolves at once with the run id; the run then drives in the
//    background on the server.
const runId = await client.startRun(agent, {
  topic: "durable execution for AI agents",
});
console.log(`started run ${runId}`);

// 3. Stream the first leg. The low step budget parks the run partway.
console.log("streaming until the run rests:");
const nextSeq = await streamToRest(runId);

// 4. Inspect the resting state. A budget park reports which dimension was
//    crossed, its effective limit, and the observed value.
const state = await client.getRun(runId);
if (state.status.state !== "budget_exceeded") {
  console.log(
    `run did not park on budget (state: ${state.status.state}); stopping`,
  );
  process.exit(0);
}
const budget = (state.status.raw.budget ?? {}) as Record<string, unknown>;
const observed = state.status.raw.observed;
console.log(
  `\nrun parked: budget_exceeded on ${budget.kind} ` +
    `(limit ${budget.limit}, observed ${observed})`,
);

// 5. The human-in-the-loop decision: grant more budget and resume. The server
//    validates the extension against the recorded budget shape before recording
//    anything, then drives the run in the background.
const extension = { extend: { steps: STEP_EXTENSION } };
console.log(`resuming with budget extension ${JSON.stringify(extension)}`);
const result = await client.resume(runId, extension);
console.log(`resume outcome: ${result.outcome}`);

// 6. Stream the continuation from where the first leg stopped. No event is
//    missed or repeated: the cursor picks up at the next sequence.
console.log("streaming the continuation:");
await streamToRest(runId, nextSeq);

// 7. Read the final folded state.
const final = await client.getRun(runId);
console.log(`\nfinal state: ${final.status.state}`);
if (final.status.state === "completed") {
  console.log(`summary: ${JSON.stringify(final.status.output)}`);
}
if (final.usage) {
  console.log(
    `usage: ${final.usage.inputTokens} in / ${final.usage.outputTokens} out`,
  );
}

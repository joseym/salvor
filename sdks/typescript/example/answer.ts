/**
 * Register a model-only agent, start a run, stream it to completion, print the
 * final state. The TypeScript mirror of the examples/web-research walkthrough,
 * driven over the control plane instead of the CLI.
 *
 * Run a server first (with a key on its environment):
 *
 *     ANTHROPIC_API_KEY=sk-ant-... \
 *         target/debug/salvor serve --bind 127.0.0.1:8080 --store /tmp/answer.db
 *
 * then build the package and run this file:
 *
 *     npm install && npm run build
 *     node --experimental-strip-types example/answer.ts http://127.0.0.1:8080
 *
 * In your own project you would `import { SalvorClient } from "@salvor/client"`;
 * this example imports the freshly built output next to it.
 */

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { SalvorClient } from "../dist/index.js";

const baseUrl = process.argv[2] ?? "http://127.0.0.1:8080";
const question =
  "In one sentence, what problem does durable execution solve for agents?";

const here = dirname(fileURLToPath(import.meta.url));
const agentToml = readFileSync(join(here, "agent.toml"), "utf8");

const client = new SalvorClient(baseUrl);

// 1. Register the definition once. The server validates it and returns a
//    content hash; every run references the agent by that hash.
const agent = await client.registerAgent(agentToml);
console.log(`registered agent ${agent}`);

// 2. Start a run. This resolves at once with the run id; the run then drives in
//    the background on the server.
const runId = await client.startRun(agent, { question });
console.log(`started run ${runId}`);

// 3. Stream events in order until the run rests. The stream replays the log,
//    tails new events live, and stops at the terminal end frame.
let count = 0;
const stream = client.streamEvents(runId);
for await (const event of stream) {
  count += 1;
  console.log(`  seq ${String(event.seq).padStart(2)}  ${event.kind}`);
}

// 4. Read the resting status the end frame carried, and the folded state.
const state = await client.getRun(runId);
console.log(`\nevents: ${count}`);
console.log(`final state: ${state.status.state}`);
if (state.status.state === "completed") {
  console.log(`answer: ${JSON.stringify(state.status.output)}`);
}
if (state.usage) {
  const usd =
    (state.usage.inputTokens * 1.0 + state.usage.outputTokens * 5.0) / 1_000_000;
  console.log(
    `usage: ${state.usage.inputTokens} in / ${state.usage.outputTokens} out  ` +
      `(~$${usd.toFixed(5)})`,
  );
}

/**
 * Drive the disputes-desk graph from TypeScript, over HTTP, against a stock
 * `salvor serve`, no flags.
 *
 * This is one of three apps that tell the same story: submit the graph
 * document, register the two agents it references, run the escalated dispute
 * to its human gate, answer the gate, then run the auto-settle dispute
 * straight through. See `../README.md` for the graph itself and why it has no
 * `tool` node; `../python/service.py` and `../rust/main.rs` tell the same
 * story in their own languages.
 *
 * Bring the offline stack up first (see ../README.md or ../run.sh), then:
 *
 *     node --experimental-strip-types service.ts http://127.0.0.1:18962
 *
 * ---------------------------------------------------------------------------
 * A NOTE ON THE IMPORT BELOW, BEFORE YOU COPY IT
 * ---------------------------------------------------------------------------
 * The import a few lines down reaches across the repository by relative path:
 *
 *     import { SalvorClient, GraphBuilder } from "../../../sdks/typescript/dist/index.js";
 *
 * THAT IS NOT HOW A NORMAL PROJECT IMPORTS THIS SDK. It is done only because
 * this example needs graph methods (`submitGraph`, `startGraphRun`,
 * `validateGraph`) that are newer than the latest published release, 0.7.0.
 * Depending on the published `@salvor-run/client` from npm would resolve to
 * that release and fail on a missing method before this app did anything
 * useful, so `run.sh` builds THIS CHECKOUT's SDK from `sdks/typescript` and
 * this file imports its built output directly, by path, in place of a normal
 * package dependency.
 *
 * In your own project, once a release carries the graph methods, this becomes
 * an ordinary dependency and an ordinary import:
 *
 *     import { Client } from "@salvor-run/client";
 *
 * (see `../../polyglot-service/typescript/service.ts` for exactly that shape,
 * dependency in `package.json` and all). Do not carry the relative path into
 * a real project; it exists here only to reach a checkout's unreleased build.
 * ---------------------------------------------------------------------------
 */

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  SalvorClient,
  GraphBuilder,
  SalvorApiError,
  type EventStream,
  type EndFrame,
  type Graph,
} from "../../../sdks/typescript/dist/index.js";

/** Raised for one of this app's own loud, predictable failures: a hash that does
 * not match, or a run that rested somewhere other than where the story expects
 * it to. Caught once at the bottom and printed without a stack trace. */
class StoryError extends Error {}

function fail(message: string): never {
  throw new StoryError(message);
}

const baseUrl = process.argv[2];
if (!baseUrl) {
  fail("usage: node --experimental-strip-types service.ts <base-url>");
}

// Paths are resolved from this file's own location, not from the process's
// working directory: `run.sh` happens to run this app from the repository
// root, but this file should not depend on that silently.
const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..", "..");
const exampleDir = join(root, "examples", "graph-clients");

function readJson(path: string): unknown {
  return JSON.parse(readFileSync(path, "utf8"));
}

const client = new SalvorClient(baseUrl);

/**
 * Stream a run's events from a cursor until it rests, printing one line per
 * event. Returns the next cursor and the terminal end frame, so a caller can
 * resume the same run later (after answering a gate) without missing or
 * repeating an event.
 *
 * This is the SDK's event stream, not a poll loop: the server replays the log
 * from `fromSeq`, tails live events, and closes at one terminal `end` frame
 * carrying the resting status. That is the shape a real service would watch a
 * run with, so it is what this app uses too.
 */
async function follow(
  runId: string,
  fromSeq = 0,
): Promise<{ nextSeq: number; end: EndFrame }> {
  const stream: EventStream = client.streamEvents(runId, { fromSeq });
  let nextSeq = fromSeq;
  for await (const event of stream) {
    nextSeq = event.seq + 1;
    let detail = "";
    if (event.kind === "BranchTaken") {
      detail = `  node=${event.payload.node} case=${event.payload.case}`;
    } else if (event.kind === "NodeSkipped") {
      detail = `  node=${event.payload.node} reason=${event.payload.reason}`;
    } else if (event.kind === "NodeEntered" || event.kind === "NodeExited") {
      detail = `  node=${event.payload.node}`;
    }
    console.log(`  seq ${String(event.seq).padStart(2)}  ${event.kind}${detail}`);
  }
  if (!stream.end) {
    fail(`event stream for run ${runId} closed with no terminal end frame`);
  }
  return { nextSeq, end: stream.end };
}

try {
  console.log("[typescript] 1. the document, and its content address");

  // The document as checked in. Read it, parse it, submit it exactly as it
  // sits on disk.
  const graphPath = join(exampleDir, "dispute-refund.json");
  const diskDoc = readJson(graphPath) as Graph;
  const fromDisk = await client.submitGraph(diskDoc);
  console.log(`  submitted from disk: ${fromDisk.graph}`);

  // The same graph, rebuilt with the typed builder rather than read off disk.
  //
  // Two of this rebuild's inputs are treated deliberately differently, and the
  // difference is what makes the comparison below mean anything.
  //
  // The two AGENT HASHES are read back out of the on-disk document. A hash is
  // an opaque 64-hex value with no meaning a reader could check by eye, so
  // retyping one here would only add a chance of a typo, and a typo would make
  // this rebuild reference some other (or no) agent. Pulling them from the
  // document is what makes the rebuild provably name the same two agents.
  //
  // The GATE's prompt and schema are written out as literals instead, a few
  // lines down. They are human-authored content, so this app can hold, and
  // state, its own belief about what the desk's gate says. Reading them back
  // out of the document would make that half of the comparison unable to fail:
  // a document whose gate said the wrong thing would be reproduced wrongly,
  // identically, and the hashes would still match.
  const nodeByOd = (id: string): Record<string, unknown> => {
    const nodes = diskDoc.nodes as unknown as { payload: { id: string } }[];
    const node = nodes.find((n) => n.payload.id === id);
    if (!node) fail(`the on-disk document has no node \`${id}\``);
    return (node as unknown as { payload: Record<string, unknown> }).payload;
  };
  const settleAgentHash = nodeByOd("settle_and_notify").agent_hash as string;
  const claimsAgentHash = nodeByOd("small_claims").agent_hash as string;

  // What this app believes the gate asks for, written independently of the
  // document. A discrepancy between this and what the desk's document actually
  // says changes the hash, and the assertion below catches it.
  const approvalSchema = {
    type: "object",
    properties: {
      dispute_id: { type: "string" },
      amount_usd: { type: "number" },
      approver: { type: "string" },
      note: { type: "string" },
    },
    required: ["dispute_id", "amount_usd", "approver"],
  };
  const gatePrompt =
    "This dispute is at or above the review threshold. Answer with the " +
    "refund to issue: dispute_id, amount_usd, approver, and an optional " +
    "note. The run stays parked until you do.";

  const built: Graph = new GraphBuilder()
    .branch("route_by_amount", [
      { name: "escalate", when: { kind: "expression", value: "amount_usd >= 250.0" } },
      { name: "auto_settle", when: { kind: "expression", value: "amount_usd < 250.0" } },
    ])
    .gate("approve_refund", approvalSchema, { prompt: gatePrompt })
    .agent("settle_and_notify", settleAgentHash)
    .agent("small_claims", claimsAgentHash)
    .edge("route_by_amount", "approve_refund", "escalate")
    .edge("approve_refund", "settle_and_notify")
    .edge("route_by_amount", "small_claims", "auto_settle")
    .build();

  // `built` is an independently written encoding of the same graph, gate
  // contents and all, in a different key order than the file on disk. It still
  // has to hash the same: the server parses the JSON into its own strict types
  // and canonicalizes that, never the request bytes. So the assertion below is
  // about this app's understanding of the gate, not about byte order.
  const fromBuilder = await client.submitGraph(built);
  console.log(`  submitted from the builder: ${fromBuilder.graph}`);

  if (fromDisk.graph !== fromBuilder.graph) {
    fail(
      "the on-disk document and the builder-built document hashed " +
        `differently (${fromDisk.graph} vs ${fromBuilder.graph}); a graph is ` +
        "supposed to be content addressed, so this means the two documents " +
        "are not actually the same graph",
    );
  }
  console.log(`  identical: both are ${fromDisk.graph}`);
  const graphHash = fromDisk.graph;

  console.log("[typescript] 2. registering the two agents");

  const settleTomlPath = join(exampleDir, "agents", "settle-and-notify.toml");
  const claimsTomlPath = join(exampleDir, "agents", "small-claims.toml");
  const settleToml = readFileSync(settleTomlPath, "utf8");
  const claimsToml = readFileSync(claimsTomlPath, "utf8");

  const registeredSettle = await client.registerAgent(settleToml);
  console.log(`  settle-and-notify -> ${registeredSettle}`);
  if (registeredSettle !== settleAgentHash) {
    fail(
      `settle-and-notify registered as ${registeredSettle}, but the graph ` +
        `document names it as ${settleAgentHash}. The TOML and the document ` +
        "have drifted apart.",
    );
  }

  const registeredClaims = await client.registerAgent(claimsToml);
  console.log(`  small-claims -> ${registeredClaims}`);
  if (registeredClaims !== claimsAgentHash) {
    fail(
      `small-claims registered as ${registeredClaims}, but the graph ` +
        `document names it as ${claimsAgentHash}. The TOML and the document ` +
        "have drifted apart.",
    );
  }
  console.log("  both match the document");

  console.log("[typescript] 3. the escalated dispute: DSP-4471, $512.00");

  const escalatedInput = readJson(join(exampleDir, "input.json"));
  const escalatedRunId = await client.startGraphRun(graphHash, escalatedInput, {
    labels: { desk: "disputes" },
  });
  console.log(`  started run ${escalatedRunId}`);
  console.log("  streaming until the run rests:");
  const { nextSeq: escalatedCursor, end: escalatedRest } = await follow(escalatedRunId);

  if (escalatedRest.status?.state !== "suspended") {
    fail(
      `run ${escalatedRunId} was expected to suspend at the gate but rested ` +
        `in state \`${escalatedRest.status?.state ?? "unknown"}\` instead`,
    );
  }
  console.log(`\n  run ${escalatedRunId} is suspended`);
  console.log(`  gate prompt: ${escalatedRest.status.reason}`);
  console.log(
    `  gate wants an answer matching this schema:\n` +
      JSON.stringify(escalatedRest.status.inputSchema, null, 2)
        .split("\n")
        .map((line) => `    ${line}`)
        .join("\n"),
  );

  console.log("[typescript] 4. answering the gate");

  const approval = {
    dispute_id: "DSP-4471",
    amount_usd: 512.0,
    approver: "j.okafor",
    note: "Duplicate charge confirmed against INV-90210.",
  };
  console.log(`  resuming with: ${JSON.stringify(approval)}`);
  const resumeResult = await client.resume(escalatedRunId, approval);
  console.log(`  resume outcome: ${resumeResult.outcome}`);
  console.log("  streaming the continuation:");
  const { end: escalatedFinal } = await follow(escalatedRunId, escalatedCursor);

  if (escalatedFinal.status?.state !== "completed") {
    fail(
      `run ${escalatedRunId} was expected to complete after the gate was ` +
        `answered but rested in state \`${escalatedFinal.status?.state ?? "unknown"}\` instead`,
    );
  }
  console.log(`\n  final output: ${JSON.stringify(escalatedFinal.status.output)}`);

  console.log("[typescript] 5. the auto-settle dispute: DSP-3312, $47.25");

  const smallInput = readJson(join(exampleDir, "input-small.json"));
  const smallRunId = await client.startGraphRun(graphHash, smallInput, {
    labels: { desk: "disputes" },
  });
  console.log(`  started run ${smallRunId}`);
  console.log("  streaming until the run rests:");
  const smallStream: EventStream = client.streamEvents(smallRunId);
  const skipped: { node: string; reason: string }[] = [];
  for await (const event of smallStream) {
    let detail = "";
    if (event.kind === "BranchTaken") {
      detail = `  node=${event.payload.node} case=${event.payload.case}`;
    } else if (event.kind === "NodeSkipped") {
      const node = event.payload.node as string;
      const reason = event.payload.reason as string;
      skipped.push({ node, reason });
      detail = `  node=${node} reason=${reason}`;
    } else if (event.kind === "NodeEntered" || event.kind === "NodeExited") {
      detail = `  node=${event.payload.node}`;
    }
    console.log(`  seq ${String(event.seq).padStart(2)}  ${event.kind}${detail}`);
  }
  const smallEnd = smallStream.end;
  if (!smallEnd) {
    fail(`event stream for run ${smallRunId} closed with no terminal end frame`);
  }

  if (smallEnd.status?.state !== "completed") {
    fail(
      `run ${smallRunId} (the auto-settle dispute) was expected to complete ` +
        `without ever suspending, but rested in state ` +
        `\`${smallEnd.status?.state ?? "unknown"}\` instead`,
    );
  }
  if (skipped.length === 0) {
    fail(
      `run ${smallRunId} completed but recorded no NodeSkipped events; the ` +
        "gate and the escalated arm's agent should have been skipped when " +
        "the branch routed the other way, and this run's own log is the " +
        "evidence that they were",
    );
  }
  console.log(`\n  run ${smallRunId} completed; it never suspended`);
  console.log("  evidence: nodes skipped because the branch routed the other way:");
  for (const node of skipped) {
    console.log(`    ${node.node}: ${node.reason}`);
  }
  console.log(`  final output: ${JSON.stringify(smallEnd.status.output)}`);

  console.log("[typescript] done");
  console.log(`  escalated run  ${escalatedRunId}  ->  ${escalatedFinal.status.state}`);
  console.log(`  auto-settle run  ${smallRunId}  ->  ${smallEnd.status.state}`);
} catch (err) {
  if (err instanceof StoryError) {
    console.error(`\n[typescript] FAILED: ${err.message}`);
  } else if (err instanceof SalvorApiError) {
    console.error(
      `\n[typescript] FAILED: the server refused a request (${err.code}): ${err.message}`,
    );
  } else if (err instanceof TypeError && /fetch/i.test(err.message)) {
    console.error(
      `\n[typescript] FAILED: could not reach salvor serve at ${baseUrl}. ` +
        "Is it running? (run.sh starts it, and passes its base URL as argv[2].)",
    );
  } else {
    const message = err instanceof Error ? err.message : String(err);
    console.error(`\n[typescript] FAILED: ${message}`);
  }
  process.exitCode = 1;
}

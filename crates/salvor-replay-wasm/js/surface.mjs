// Pins the exported .d.ts surface (types/index.d.ts) against the real wasm
// runtime output. wasm-pack's generated .d.ts covers the function signatures;
// this asserts the JSON SHAPE those functions carry — the RunStateJson union —
// still matches what types/index.d.ts documents, so the two cannot drift.
//
// It checks, across the committed fixtures: that every documented status `kind`
// is reachable, and that each folded state uses only the documented keys with
// the documented value types. The Rust surface_pin tests byte-pin the same
// shapes; this is the JS/TS-facing anchor for the hand-written declarations.
//
// Run:
//   wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_replay_wasm
//   node js/surface.mjs

import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const crateDir = join(here, "..");
const require = createRequire(import.meta.url);
const wasm = require(join(crateDir, "pkg-node", "salvor_replay_wasm.js"));

const logsDir = join(crateDir, "fixtures", "logs");

const DOCUMENTED_STATUS_KINDS = new Set([
  "NotStarted",
  "Running",
  "AwaitingModel",
  "AwaitingTool",
  "Suspended",
  "BudgetExceeded",
  "NeedsReconciliation",
  "Completed",
  "Failed",
]);
const DOCUMENTED_EFFECTS = new Set(["read", "idempotent", "write"]);

const errors = [];
const seenStatusKinds = new Set();

function checkState(where, s) {
  const topKeys = Object.keys(s).sort();
  for (const k of topKeys) {
    if (!["status", "next_seq", "usage", "pending_call"].includes(k)) {
      errors.push(`${where}: undocumented top-level key "${k}"`);
    }
  }
  if (typeof s.next_seq !== "number") errors.push(`${where}: next_seq not a number`);
  if (typeof s.usage?.input_tokens !== "number" || typeof s.usage?.output_tokens !== "number") {
    errors.push(`${where}: usage shape wrong`);
  }

  if (!s.status || typeof s.status.kind !== "string") {
    errors.push(`${where}: status missing kind`);
  } else {
    seenStatusKinds.add(s.status.kind);
    if (!DOCUMENTED_STATUS_KINDS.has(s.status.kind)) {
      errors.push(`${where}: undocumented status kind "${s.status.kind}"`);
    }
    if (s.status.kind === "BudgetExceeded") {
      if (typeof s.status.observed !== "number") errors.push(`${where}: observed not a number`);
      if (typeof s.status.budget?.limit !== "number") errors.push(`${where}: budget.limit not a number`);
    }
  }

  if (s.pending_call !== undefined) {
    const pc = s.pending_call;
    if (pc.kind === "Tool") {
      if (!DOCUMENTED_EFFECTS.has(pc.effect)) errors.push(`${where}: undocumented effect "${pc.effect}"`);
      if (typeof pc.seq !== "number") errors.push(`${where}: pending Tool seq not a number`);
    } else if (pc.kind === "Model") {
      if (typeof pc.request_hash !== "string") errors.push(`${where}: pending Model request_hash not a string`);
    } else {
      errors.push(`${where}: undocumented pending_call kind "${pc.kind}"`);
    }
  }
}

for (const file of readdirSync(logsDir).filter((f) => f.endsWith(".json"))) {
  const name = file.replace(/\.json$/, "");
  const logJson = readFileSync(join(logsDir, file), "utf8");
  const count = wasm.eventCount(logJson);
  for (let n = 0; n <= count; n++) {
    checkState(`${name}#${n}`, JSON.parse(wasm.deriveState(logJson, n)));
  }
}

// Every documented status kind must be reachable from the fixture set, or the
// surface documents a variant the proof never exercises.
for (const kind of DOCUMENTED_STATUS_KINDS) {
  if (!seenStatusKinds.has(kind)) errors.push(`documented status kind "${kind}" never appeared in fixtures`);
}

if (errors.length > 0) {
  console.error(`[surface] FAILED: ${errors.length} issue(s):`);
  for (const e of errors) console.error("  - " + e);
  process.exit(1);
}

console.log(
  `[surface] PASS: every folded state conforms to types/index.d.ts; ` +
    `all ${DOCUMENTED_STATUS_KINDS.size} documented status kinds exercised.`
);

// The scrub-latency measurement.
//
// A scrubber step folds the whole run log up to a prefix through the wasm
// boundary: deriveState(logJson, n) parses the log JSON, folds n events, and
// serializes the state back out. This measures that per-step cost on the
// ~1k-event fixture (large_1k), which is the realistic worst case for the
// inspector, plus the cost of a full sweep across every prefix.
//
// Budget: if a single scrub step exceeds ~10ms, boundary crossing dominates and
// the fallback (keep the folded-state diff inside wasm) becomes worth building.
// This script only measures and reports; it does not implement the fallback.
//
// Run:
//   wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_replay_wasm
//   node js/latency.mjs

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const crateDir = join(here, "..");
const require = createRequire(import.meta.url);
const wasm = require(join(crateDir, "pkg-node", "salvor_replay_wasm.js"));

const BUDGET_MS = 10;

const logJson = readFileSync(join(crateDir, "fixtures", "logs", "large_1k.json"), "utf8");
const count = wasm.eventCount(logJson);
console.log(`[latency] fixture large_1k: ${count} events, log JSON ${logJson.length} bytes`);

// Warm up (JIT + wasm instantiation paths).
for (let i = 0; i < 50; i++) wasm.deriveState(logJson, count);

// Single-step latency at the head prefix (the most work: fold the whole log).
const headSamples = [];
for (let i = 0; i < 200; i++) {
  const t0 = performance.now();
  wasm.deriveState(logJson, count);
  headSamples.push(performance.now() - t0);
}
headSamples.sort((a, b) => a - b);
const mean = headSamples.reduce((a, b) => a + b, 0) / headSamples.length;
const p50 = headSamples[Math.floor(headSamples.length * 0.5)];
const p95 = headSamples[Math.floor(headSamples.length * 0.95)];
const max = headSamples[headSamples.length - 1];

console.log(
  `[latency] single scrub step (fold full ${count}-event log):\n` +
    `          mean ${mean.toFixed(3)}ms  p50 ${p50.toFixed(3)}ms  ` +
    `p95 ${p95.toFixed(3)}ms  max ${max.toFixed(3)}ms`
);

// A full sweep: fold every prefix once, as dragging the scrubber end to end would.
const sweepStart = performance.now();
for (let n = 0; n <= count; n++) wasm.deriveState(logJson, n);
const sweepMs = performance.now() - sweepStart;
console.log(
  `[latency] full sweep of all ${count + 1} prefixes: ${sweepMs.toFixed(2)}ms total, ` +
    `${(sweepMs / (count + 1)).toFixed(3)}ms/step avg`
);

const verdict = p95 <= BUDGET_MS ? "WITHIN" : "OVER";
console.log(
  `[latency] budget ${BUDGET_MS}ms/step: p95 ${p95.toFixed(3)}ms is ${verdict} budget. ` +
    (p95 <= BUDGET_MS
      ? "String boundary is fine; the in-wasm state-diff fallback is unnecessary."
      : "Boundary dominates; consider the in-wasm state-diff fallback (standing risk #1).")
);

// The same-fold proof, wasm side. Runs in Node against the wasm-pack build.
//
// For every committed fixture log, folds every prefix (0, 1, …, len) through the
// wasm module's deriveState and asserts the result is byte-identical to the
// committed native fold in fixtures/expected/<name>.jsonl. The native test
// (tests/same_fold.rs) proves that committed-expected equals the live native
// fold, so this completes the chain: native == committed == wasm, byte for byte.
//
// It does the same for the budget check: every committed log against every
// declaration in fixtures/expected-budgets/<name>.jsonl, through the wasm
// module's checkBudgets. The declarations are listed here in the same order the
// native generator writes them, which is what makes the two files line up.
//
// Run:
//   wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_replay_wasm
//   node js/same-fold.mjs
//
// Exits non-zero on any mismatch, missing fixture, or wasm error.

import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const crateDir = join(here, "..");
const require = createRequire(import.meta.url);

const wasmPath = join(crateDir, "pkg-node", "salvor_replay_wasm.js");
let wasm;
try {
  wasm = require(wasmPath);
} catch (e) {
  console.error(
    `[same-fold] could not load the wasm module at ${wasmPath}\n` +
      `Build it first: wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_replay_wasm\n` +
      String(e)
  );
  process.exit(1);
}

const logsDir = join(crateDir, "fixtures", "logs");
const expectedDir = join(crateDir, "fixtures", "expected");
const budgetsDir = join(crateDir, "fixtures", "expected-budgets");

// The budget declarations, in the order REFERENCE_BUDGETS lists them in
// tests/same_fold.rs. One committed verdict line per entry, per log.
const BUDGET_DECLARATIONS = [
  ["none_declared", "{}"],
  ["steps_generous", '{"steps":1000}'],
  ["steps_zero", '{"steps":0}'],
  ["steps_one", '{"steps":1}'],
  ["tokens_tight", '{"tokens":10}'],
  [
    "cost_with_pricing",
    '{"cost_usd":0.0001,"pricing":{"input_per_mtok":3.0,"output_per_mtok":15.0}}',
  ],
  ["cost_without_pricing", '{"cost_usd":0.0001}'],
  ["wall_time", '{"wall_time_seconds":0.5}'],
  [
    "every_dimension",
    '{"steps":2,"tokens":50,"cost_usd":1.0,"wall_time_seconds":3600.0,"pricing":{"input_per_mtok":3.0,"output_per_mtok":15.0}}',
  ],
];

const logFiles = readdirSync(logsDir).filter((f) => f.endsWith(".json"));
if (logFiles.length === 0) {
  console.error(`[same-fold] no fixture logs in ${logsDir}`);
  process.exit(1);
}

let totalLogs = 0;
let totalPrefixes = 0;
let totalVerdicts = 0;
const failures = [];

for (const file of logFiles.sort()) {
  const name = file.replace(/\.json$/, "");
  const logJson = readFileSync(join(logsDir, file), "utf8");
  const expectedRaw = readFileSync(join(expectedDir, `${name}.jsonl`), "utf8");
  const expected = expectedRaw.split("\n").filter((line) => line.length > 0);

  const count = wasm.eventCount(logJson);
  if (count + 1 !== expected.length) {
    failures.push(
      `${name}: eventCount ${count} implies ${count + 1} prefixes, but expected has ${expected.length}`
    );
    continue;
  }

  for (let n = 0; n <= count; n++) {
    const got = wasm.deriveState(logJson, n);
    if (got !== expected[n]) {
      failures.push(
        `${name} prefix ${n}: wasm fold differs from native\n  wasm:   ${got}\n  native: ${expected[n]}`
      );
    }
    totalPrefixes++;
  }

  const budgetsRaw = readFileSync(join(budgetsDir, `${name}.jsonl`), "utf8");
  const budgets = budgetsRaw.split("\n").filter((line) => line.length > 0);
  if (budgets.length !== BUDGET_DECLARATIONS.length) {
    failures.push(
      `${name}: ${budgets.length} committed budget verdicts, but ${BUDGET_DECLARATIONS.length} declarations`
    );
  } else {
    for (let i = 0; i < BUDGET_DECLARATIONS.length; i++) {
      const [label, declaration] = BUDGET_DECLARATIONS[i];
      const got = wasm.checkBudgets(logJson, declaration);
      if (got !== budgets[i]) {
        failures.push(
          `${name}/${label}: wasm budget check differs from native\n  wasm:   ${got}\n  native: ${budgets[i]}`
        );
      }
      totalVerdicts++;
    }
  }
  totalLogs++;
}

if (failures.length > 0) {
  console.error(`[same-fold] FAILED with ${failures.length} mismatch(es):`);
  for (const f of failures) console.error("  - " + f);
  process.exit(1);
}

console.log(
  `[same-fold] PASS: ${totalLogs} logs, ${totalPrefixes} prefixes folded and ` +
    `${totalVerdicts} budget verdicts checked through wasm are byte-identical to the ` +
    `native results.`
);

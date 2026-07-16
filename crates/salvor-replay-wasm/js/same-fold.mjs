// The same-fold proof, wasm side. Runs in Node against the wasm-pack build.
//
// For every committed fixture log, folds every prefix (0, 1, …, len) through the
// wasm module's deriveState and asserts the result is byte-identical to the
// committed native fold in fixtures/expected/<name>.jsonl. The native test
// (tests/same_fold.rs) proves that committed-expected equals the live native
// fold, so this completes the chain: native == committed == wasm, byte for byte.
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

const logFiles = readdirSync(logsDir).filter((f) => f.endsWith(".json"));
if (logFiles.length === 0) {
  console.error(`[same-fold] no fixture logs in ${logsDir}`);
  process.exit(1);
}

let totalLogs = 0;
let totalPrefixes = 0;
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
  totalLogs++;
}

if (failures.length > 0) {
  console.error(`[same-fold] FAILED with ${failures.length} mismatch(es):`);
  for (const f of failures) console.error("  - " + f);
  process.exit(1);
}

console.log(
  `[same-fold] PASS: ${totalLogs} logs, ${totalPrefixes} prefixes folded through wasm ` +
    `are byte-identical to the native fold.`
);

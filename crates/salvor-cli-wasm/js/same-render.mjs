// The same-render proof, wasm side.
//
// `tests/same_render.rs` asserts that the native build's output equals what
// salvor-cli-core produces directly, and that both still equal the committed
// fixtures. This runs the SAME committed corpus through the actual compiled
// wasm module and asserts it equals that same committed expected, byte for
// byte. Together the two halves give native == committed == wasm, all three
// checked live, which is what makes a browser terminal unable to drift from
// the real CLI.
//
// Build the module first, then run this:
//
//   wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_cli_wasm
//   node js/same-render.mjs

import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  helpText,
  helpTextAnsi,
  parseArgv,
  renderList,
  renderListPlain,
} from "../pkg-node/salvor_cli_wasm.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixtures = join(here, "..", "fixtures");

const read = (...parts) => readFileSync(join(fixtures, ...parts), "utf8");
const names = (dir, suffix) =>
  readdirSync(join(fixtures, dir))
    .filter((file) => file.endsWith(suffix))
    .map((file) => file.slice(0, -suffix.length))
    .sort();

let checked = 0;
const failures = [];

function same(label, actual, expected) {
  checked += 1;
  if (actual !== expected) {
    failures.push(
      `${label}\n  wasm:     ${JSON.stringify(actual)}\n  expected: ${JSON.stringify(expected)}`,
    );
  }
}

// The list table, styled and plain, for every committed row set.
for (const name of names("rows", ".json")) {
  const rows = read("rows", `${name}.json`);
  same(`list ${name} (ansi)`, renderList(rows), read("expected/list", `${name}.ansi.txt`));
  same(
    `list ${name} (plain)`,
    renderListPlain(rows),
    read("expected/list", `${name}.plain.txt`),
  );
}

// Help, styled and plain, for every committed path. The fixture name is the
// path with its spaces written as dashes, which is how the Rust side names
// them; `root` is the empty path.
const helpPaths = {
  root: "",
  list: "list",
  run: "run",
  fork: "fork",
  serve: "serve",
  graph: "graph",
  "graph-validate": "graph validate",
  "graph-run": "graph run",
};
const helpNames = names("expected/help", ".plain.txt");
if (helpNames.length !== Object.keys(helpPaths).length) {
  failures.push(
    `the help path table here lists ${Object.keys(helpPaths).length} paths but ${helpNames.length} are committed; the two sides have drifted`,
  );
}
for (const name of helpNames) {
  const path = helpPaths[name];
  if (path === undefined) {
    failures.push(`no path in this file for the committed help fixture ${name}`);
    continue;
  }
  same(`help ${name} (plain)`, helpText(path), read("expected/help", `${name}.plain.txt`));
  same(`help ${name} (ansi)`, helpTextAnsi(path), read("expected/help", `${name}.ansi.txt`));
}

// The parse envelope for every committed argv. Compared as canonical JSON
// rather than as raw text: the Rust generator writes its fixtures through
// serde_json's Value, which sorts keys, so the bytes differ from the module's
// output while the value does not.
const canonical = (value) => {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, canonical(value[key])]),
    );
  }
  return value;
};
for (const name of names("argv", ".json")) {
  const argv = read("argv", `${name}.json`);
  same(
    `parse ${name}`,
    JSON.stringify(canonical(JSON.parse(parseArgv(argv)))),
    JSON.stringify(canonical(JSON.parse(read("expected/parse", `${name}.json`)))),
  );
}

if (failures.length > 0) {
  console.error(`same-render FAILED: ${failures.length} of ${checked} comparisons\n`);
  for (const failure of failures) console.error(failure);
  process.exit(1);
}

console.log(`same-render ok: ${checked} comparisons, wasm output identical to the committed native output`);

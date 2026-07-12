/**
 * Run the browser demo's logic headless, against the live offline stack.
 *
 * The demo logic lives in client-run-demo.js with the DOM held behind a `sink`
 * seam. This harness wires that seam to the console, so the exact code the
 * browser page runs is exercised end to end without a browser: open a run,
 * append control events, re-open and replay, then the model step (streaming,
 * with the unary retry fallback the offline demo model takes) and the tool
 * step (a clean refusal against `salvor serve`'s empty registry).
 *
 * Bring the offline stack up (see README.md), then:
 *
 *     node examples/browser-client-run/headless.mjs http://127.0.0.1:8080
 */

import { runClientRunDemo } from "./client-run-demo.js";

const baseUrl = process.argv[2] ?? "http://127.0.0.1:8080";

// A painted ticker line has no trailing newline (it repaints in place); the
// next full line closes it first so console output stays line-per-line.
let tickerOpen = false;
const closeTicker = () => {
  if (tickerOpen) {
    process.stdout.write("\n");
    tickerOpen = false;
  }
};

const sink = {
  section(text) {
    closeTicker();
    process.stdout.write(`\n== ${text} ==\n`);
  },
  line(text) {
    closeTicker();
    process.stdout.write(`${text}\n`);
  },
  tick(text) {
    // Overwrite the current line to mimic the live ticker the DOM paints.
    process.stdout.write(`\r  ticker: ${text}`);
    tickerOpen = true;
  },
};

await runClientRunDemo(baseUrl, sink);

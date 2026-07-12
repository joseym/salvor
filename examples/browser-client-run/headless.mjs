/**
 * Run the browser demo's logic headless, against the live offline stack.
 *
 * The demo logic lives in client-run-demo.js with the DOM held behind a `sink`
 * seam. This harness wires that seam to the console, so the exact code the
 * browser page runs is exercised end to end without a browser: open a run,
 * append control events, re-open and replay, then the streaming model step and
 * the tool step (which degrade with a clear message when `salvor serve` has no
 * reachable model and an empty tool registry).
 *
 * Bring the offline stack up (see README.md), then:
 *
 *     node examples/browser-client-run/headless.mjs http://127.0.0.1:8080
 */

import { runClientRunDemo } from "./client-run-demo.js";

const baseUrl = process.argv[2] ?? "http://127.0.0.1:8080";

const sink = {
  section(text) {
    process.stdout.write(`\n== ${text} ==\n`);
  },
  line(text) {
    process.stdout.write(`${text}\n`);
  },
  tick(text) {
    // Overwrite the current line to mimic the live ticker the DOM paints.
    process.stdout.write(`\r  ticker: ${text}`);
  },
};

await runClientRunDemo(baseUrl, sink);

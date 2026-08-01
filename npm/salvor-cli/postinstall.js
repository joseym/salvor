#!/usr/bin/env node
"use strict";

// Runs once, at `npm install` time. Downloads the platform's `salvor`
// binary, then (on POSIX) replaces bin/salvor's bytes with it, so every
// `salvor` invocation after this is the real binary, not a Node relay. See
// lib/binarylink.js for the full explanation and bin/salvor's fallback path.

const { getBinary } = require("./lib/binarylink");

async function main() {
  const bin = getBinary();
  if (!bin) return;

  await bin.install(false);

  if (process.platform === "win32") {
    // npm's generated .cmd/.ps1 shim always runs `node <target>` itself, so
    // bin/salvor has to stay a real Node script here.
    return;
  }

  try {
    bin.swapEntry();
    console.error("salvor: bin/salvor now execs the downloaded binary directly");
  } catch (err) {
    console.warn(
      `salvor: could not link the binary directly (${err.message}); ` +
        "falling back to the JS relay. `salvor` will still work, but " +
        "killing its PID may not stop an in-flight run promptly.",
    );
  }
}

main().catch((err) => {
  console.error(err && err.message ? err.message : String(err));
  process.exit(1);
});

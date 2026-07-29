# salvor-replay-wasm

A thin `wasm-bindgen` wrapper over the pure [`salvor-replay`](../salvor-replay)
crate. It compiles the runtime's own event fold to WebAssembly so the Bridge's
inspector can scrub a run's history in the browser, deriving the state at any
log prefix instantly, with no server round trip, from the same code the runtime
and server use. `salvor-replay` itself stays dependency-free; all the wasm
plumbing lives here.

This is the foundation of the Bridge build, and the architectural payoff of
the crate split: one fold implementation, no TypeScript port, no drift.

## API surface

Two exported functions (see `types/index.d.ts` for the full type surface,
including the JSON shape the state string parses into):

```ts
// Fold the first `prefixLen` events of a wire-JSON event log into the derived
// RunState, returned as canonical JSON. `logJson` is the exact wire form the
// store writes (a JSON array of EventEnvelope). Throws on a bad log or a prefix
// past the log's end.
function deriveState(logJson: string, prefixLen: number): string;

// Count the events in a log, so a caller can enumerate scrub positions.
function eventCount(logJson: string): number;
```

`deriveState(logJson, n)` is the scrubber's one operation: `n` in `0..=len`,
where `0` is the empty (not-started) prefix and `len` is the head.

## Boundary choice (measured, not assumed)

The log crosses in as the **exact wire JSON** the store already writes, and the
folded state crosses back as JSON, so only strings cross the boundary. This matches the
store's exact-wire-JSON posture and the existing SSE client, which already
deserializes each frame straight into `salvor_replay::EventEnvelope`.

The heavier `serde-wasm-bindgen` alternative was left unbuilt because the string
boundary already clears the scrub-latency budget by ~8x (see below). Numbers, not
taste: on a 1002-event log a full scrub step (parse the whole log, fold it,
serialize the state out) measured **mean 1.15ms / p95 1.32ms**, against a 10ms
budget. There is no boundary-cost problem to solve, so there is no reason to add a
second serialization path.

## The same-fold proof (this crate's reason to exist)

Every fixture log is folded at **every** prefix, natively **and** through the
wasm module, asserting byte-identical canonical JSON. The chain has two links:

1. `tests/same_fold.rs` (native, runs under `cargo test`) rebuilds the reference
   logs, asserts the committed `fixtures/logs` still equal them, then folds every
   committed log at every prefix natively and asserts it equals
   `fixtures/expected/*.jsonl` byte for byte. So the committed native side is
   live-verified on every test run.
2. `js/same-fold.mjs` (Node, runs the wasm build) folds every committed log at
   every prefix through `deriveState` and asserts it equals that same committed
   expected, byte for byte.

Together: **native == committed == wasm**, all three checked live. Latest run:
**13 logs, 1044 prefixes** verified on each side.

The reference logs deliberately touch every event kind and every derived status,
including the `f64` budget paths and the `u64` random/sequence paths, so the
cross-target number-formatting that is the real miscompilation risk.

## Building

```sh
# The npm package the Bridge consumes (--target web), output to pkg/:
wasm-pack build --target web --out-dir pkg --out-name salvor_replay_wasm

# The Node build the proof/latency harnesses drive (--target nodejs), pkg-node/:
wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_replay_wasm
```

`pkg/` and `pkg-node/` are wasm-pack outputs (each self-gitignored) and are not
committed. Latest `.wasm` size: **274 KB** optimized (before gzip).

## Running the proof and the measurement

```sh
# Native side (fast, no wasm toolchain):
cargo test -p salvor-replay-wasm

# Wasm side. Build the nodejs package first, then:
node js/same-fold.mjs   # the same-fold proof: wasm vs native, byte-identical
node js/surface.mjs     # pins the types/index.d.ts surface against runtime output
node js/latency.mjs      # the scrub-latency measurement on the 1k-event log
```

## Regenerating fixtures

The reference logs and their native folds are committed under `fixtures/`. To
rewrite them after changing the reference set:

```sh
REGEN_FIXTURES=1 cargo test -p salvor-replay-wasm --test same_fold -- --ignored regenerate
```

## Purity

The wasm build depends on `salvor-replay` with `default-features = false`, so the
`rng` feature (the one randomness-drawing constructor) is off and the module
draws no randomness, the same purity the CI `wasm32` build proves for
`salvor-replay`. The fold core is ordinary Rust that also builds natively, so
`cargo build/test --workspace` needs no wasm toolchain.

# Salvor

A durable execution runtime for AI agents, in Rust.

A salvor is the one who recovers a wrecked ship and its cargo. This runtime does the same for agent runs: when a process dies mid-flight, the durable log brings it back and finishes it from exactly where it stopped, with no work done twice.

Event-sourced runs, typed tool contracts with side-effect classification, crash-exact resume, and hard budgets, deployed as a single static binary with an embedded store.

![Salvor kills a research agent mid-run and resumes it to completion with no duplicate side effects](docs/demo.gif)

**Status:** pre-0.1, unpublished. The runtime, CLI, control plane, SDKs, and web dashboard all work end to end today; none of it is published anywhere yet (see [Quickstart](#quickstart)).

## Quickstart

Salvor is pre-0.1 and not published anywhere, so build the binary from source (a stable Rust toolchain is all you need):

```
cargo build            # produces target/debug/salvor
```

### A first run

An agent is a TOML file. Save this as `hello-agent.toml`:

```toml
model = "claude-opus-4-8"
system_prompt = "You are a concise assistant. Answer in one or two sentences."
```

Run it, with a key exported (Salvor talks to the public Anthropic endpoint by default):

```sh
export ANTHROPIC_API_KEY=sk-ant-...

./target/debug/salvor run --agent hello-agent.toml \
    --input '"What does it mean for a program to be durable?"'
```

That prints the run's id, then its answer, once the model responds:

```
run 2cfc5c00-4e7f-4ad9-942a-8c8e942f6051
"Durability means the run's state survives a crash: every event is written before the runtime acts on it, so a resume replays exactly what already happened and finishes the rest without repeating any side effect."
```

(The id is a fresh UUID and the wording is the model's own, so both vary run to run.) `salvor history <run-id>` shows what actually happened, as a durable event log rather than only the final answer:

```
   0  2026-07-14 02:44:30Z  RunStarted           agent sha256:abd8d6f… input "What does it mean for a program to be durable?"
   1  2026-07-14 02:44:30Z  NowObserved          2026-07-14 02:44:30Z
   2  2026-07-14 02:44:30Z  ModelCallRequested   request sha256:ff62b65…
   3  2026-07-14 02:44:30Z  ModelCallCompleted   usage in 24 out 41
   4  2026-07-14 02:44:30Z  RunCompleted         output "Durability means the run's state survives a crash: every event is written befor…
```

Every one of those five lines is a durably recorded event, written before the run moved past it. The rest of this Quickstart tests that property against a real crash.

### The kill and resume walkthrough

The headline workload lives in `demo/`: a research agent you can `kill -9` partway through and resume with an identical event history and zero repeated writes. From the repository root, with a key exported:

```sh
export ANTHROPIC_API_KEY=sk-ant-...

# 1. start a run in the background; it prints its run id first, before any
#    step executes, so a kill still leaves you an id to resume.
./target/debug/salvor run --agent demo/agent.toml --input @demo/input.json &

# 2. kill it dead, mid-run.
kill -9 $!

# 3. resume from the durable log: completed work is replayed from the log,
#    never re-executed, and the run finishes from the first unrecorded step.
./target/debug/salvor resume <run-id> --agent demo/agent.toml
```

The demo's MCP server appends one line to a findings file per real write, so `wc -l` before the kill and after the resume is the zero-duplicate proof. `salvor list` shows the crashed run and its id; `salvor history <run-id>` prints the event log. `demo/README.md` has the full walkthrough, including a hermetic mock-model mode that needs no key and no network (the same mode records the GIF above).

For a live version against real tools, `examples/web-research/` runs an agent over the official fetch and filesystem MCP servers and applies the same kill/resume story to real HTTP fetches and a real report write.

### Use it as a library

Salvor is also usable as a library, at two tiers you build against directly. The batteries-included tier is `Agent::builder()` plus a `Runtime`: you write typed tools and let the built-in loop drive them. The library-first tier is a hand-written async function over the public `RunCtx`, which gets the same durability and replay without the built-in loop. Each has a runnable example (and a `main.teach.md` walking through it) under `examples/todo-agent/` and `examples/approval-loop/`, wired into `salvor-runtime`'s `Cargo.toml` as out-of-package `[[example]]` targets:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p salvor-runtime --example todo_agent      # batteries-included: Agent::builder + native tools
cargo run -p salvor-runtime --example approval_loop   # library-first: your own loop over RunCtx
```

`todo_agent` prints a run id you can kill and recover with `RESUME_RUN_ID=<id>`; `approval_loop` parks awaiting approval on the first run and completes on a second run with `APPROVAL` set.

## Control plane

`salvor serve` puts the runtime on a network: an HTTP and server-sent-events server that owns one event store and drives runs in the background, so a client submits an agent definition and an input, then reads the run's events as they land.

```sh
./target/debug/salvor serve --bind 127.0.0.1:8080
```

An agent is data, so registration (`POST /v1/agents`, TOML or JSON body) hashes the definition and returns that hash; every start, resume, and recover after that references the agent by hash, not by re-sending it. From there: `POST /v1/runs` starts a run, `GET /v1/runs/{id}/events` streams it over SSE with resumable cursors, `POST /v1/runs/{id}/resume` continues a parked or crashed run, and `POST /v1/runs/{id}/resolve` records a dangling write's completion by hand after a human verifies it externally. Every guarantee the CLI has (exact replay, crash-safe resume, the write-ahead reconciliation rule) holds unchanged over HTTP, because the same runtime enforces it.

A second, additive surface under `/v1/client-runs` moves ownership of the agent loop to the client while the server keeps ownership of the durable log: a client (an SDK driver, or a browser folding its own log with a wasm `ReplayCursor`) drives its own loop and appends the events it produces, and the server re-folds the log on every append to confirm each one is the legal next event. The model and tool calls stay server-performed, since the server holds the key or the binary; everything else the client appends directly. The full contract, every route, status code, and event shape, is in [`crates/salvor-server/API.md`](crates/salvor-server/API.md).

Prompt recording is opt-in and off by default: a per-agent `record_prompts` TOML flag (or a `SALVOR_RECORD_PROMPTS` environment default, when the flag is unset) records the exact model request body on `ModelCallRequested`. Recorded bodies land only in the durable log, never in the progress stream or console output, so turn it on only where you accept that PII and secrets in the prompt get written to the store.

### Client SDKs

Thin Python and TypeScript clients over the control plane live under `sdks/` (`sdks/python`, `sdks/typescript`): register an agent, start a run, stream events, resume, all over HTTP. The durability stays in the one Rust process; each SDK is a few hundred lines. See each directory's README.

Both SDKs also drive the client-driven mode: a `ClientRunDriver` that opens or resumes a run, appends control events, and calls the model and tool steps directly against the server (see [Control plane](#control-plane) above). The model step is still performed by `salvor serve` itself, so pointing the server's own `SALVOR_MODEL_BASE_URL` at a local or offline endpoint (instead of the public Anthropic one) redirects every model step an SDK driver makes, with no change on the client side. `examples/browser-client-run` drives the same surface from a browser page.

## Workspace

| Crate | Purpose |
|---|---|
| `salvor-core` | Stable public surface over the event model, replay engine, budget enforcement, and deterministic context; re-exports `salvor-replay` |
| `salvor-replay` | Pure, IO-free event vocabulary, replay cursor, and state fold: the durability engine's core, wasm32-portable, shared by the runtime, the CLI's `replay`, and the dashboard |
| `salvor-store` | `EventStore` trait + SQLite (WAL) implementation |
| `salvor-store-conformance` | Store-agnostic conformance kit that proves an `EventStore` backend satisfies the trait contract |
| `salvor-llm` | Messages API client (Anthropic hosted and local endpoints) |
| `salvor-tools` | `ToolHandler` trait, effect classification, MCP client |
| `salvor-tools-macros` | The `#[derive(Tool)]` macro, re-exported by `salvor-tools` |
| `salvor-wasm` | Sandboxed WebAssembly component tools (wasmtime, WASI p2, deny-all capabilities) |
| `salvor-runtime` | The IO edge: `RunCtx`, the `Agent` builder, budget enforcement, the built-in agent loop |
| `salvor-graph` | Pure graph document model, versioned validation, and JSON Schema emission for the declarative graph-authoring format |
| `salvor-server` | The control plane: HTTP + server-sent-events over the durable runtime, server-driven and client-driven |
| `salvor-cli` | The `salvor` binary: `run`, `resume`, `resolve`, `list`, `history`, `replay`, `serve`, `graph` |
| `salvor` | The umbrella crate that holds the published name; re-exports the family once v0.1 ships |

`sdks/python` and `sdks/typescript` are the thin client SDKs described above; neither is a Rust crate, so neither is a workspace member. `dashboard/` is a client-side Leptos app compiled to wasm (`trunk build` / `trunk serve`) that talks to the control plane over `/v1` and folds event logs with the real `salvor-replay` code, so its run-inspector scrubber recomputes state in the browser rather than re-implementing the fold in JavaScript; it is excluded from the Cargo workspace because it targets `wasm32-unknown-unknown` and carries its own lockfile.

## Correctness

The kill demo is one crash at one boundary. The property suite behind it is the release gate: the same shape of run, killed at every one of its event boundaries, resumed through the full runtime, and checked for a byte-identical final log with zero duplicate writes at each one (`crates/salvor-runtime/tests/release_gate.rs`). Passing that suite is the release gate for v0.1.

## Development

Run this once after cloning (install cocogitto first with `brew install cocogitto` if you do not have it):

```
cog install-hook --all
```

Commit messages follow Conventional Commits, enforced by `cog verify` in the commit-msg hook. Releases are cut with `cog bump`; see [docs/RELEASING.md](docs/RELEASING.md) for the distribution pipeline and how a release becomes prebuilt binaries.

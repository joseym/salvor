# Salvor

A durable execution runtime for AI agents, in Rust.

Event-sourced runs, typed tool contracts with side-effect classification, crash-exact resume, and hard budgets, deployed as a single static binary with an embedded store.

![Salvor kills a research agent mid-run and resumes it to completion with no duplicate side effects](docs/demo.gif)

**Status:** early development, pre-0.1. Nothing here is usable yet.

## Quickstart

Salvor is pre-0.1 and not published anywhere, so build the binary from source (a stable Rust toolchain is all you need):

```
cargo build            # produces target/debug/salvor
```

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

## Workspace

| Crate | Purpose |
|---|---|
| `salvor-core` | Event model, replay engine, budget enforcement, deterministic context |
| `salvor-store` | `EventStore` trait + SQLite (WAL) implementation |
| `salvor-llm` | Messages API client (Anthropic hosted and local endpoints) |
| `salvor-tools` | `ToolHandler` trait, effect classification, MCP client |
| `salvor-cli` | The `salvor` binary: `run`, `resume`, `list`, `history`, `replay` |

## Correctness

The kill demo is one crash at one boundary. The property suite behind it is the release gate: the same shape of run, killed at every one of its event boundaries, resumed through the full runtime, and checked for a byte-identical final log with zero duplicate writes at each one (`crates/salvor-runtime/tests/release_gate.rs`). Passing that suite is the release gate for v0.1.

## Development

Run this once after cloning (install cocogitto first with `brew install cocogitto` if you do not have it):

```
cog install-hook --all
```

Commit messages follow Conventional Commits, enforced by `cog verify` in the commit-msg hook. Releases are cut with `cog bump`.

## License

MIT OR Apache-2.0

# salvor

A durable execution runtime for AI agents, in Rust. `kill -9` a run mid-flight, resume it, and
nothing happens twice.

A run is an append-only log of events. Every event is written before the runtime acts on it, and
state is a pure fold over the log, so a resume replays what already happened and re-executes none of
it. Tools declare an effect: reads replay for free, writes never replay blind, and a write left
dangling by a crash blocks the resume until a human reconciles it.

```sh
cargo add salvor
```

```rust
use salvor::prelude::*;
```

This crate is a facade over the `salvor-*` family and holds no logic of its own. The default
features carry the agent loop, the tool contract, the SQLite event store, and the event model:
enough to define an agent, run it, kill it, and resume it. `graph`, `engine`, `server`, `llm`,
`wasm` and `mcp` are opt-in, because each pulls a dependency tree worth choosing deliberately.

Depending on the individual crates instead is equally supported and gives a narrower build. Every
re-export is pinned to this crate's exact version, so the family cannot be mixed with versions it
was never tested against.

For the command-line runtime rather than the library, install `salvor-cli` (`cargo install
salvor-cli`, or `npm install -g @salvor-run/cli` for a prebuilt binary). The control-plane API, the
web UI, and the rest of the documentation are in the
[repository](https://github.com/joseym/salvor).

Licensed under MIT or Apache-2.0, at your option.

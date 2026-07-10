# Examples

Three worked examples, one per directory. Each shows Salvor from a different
entry point: the CLI over a real MCP-backed agent, and the two library tiers
you can build against directly.

| Directory | Shows | Run it |
|---|---|---|
| [`web-research/`](web-research/) | The `salvor` CLI driving a config-driven agent over real MCP servers (fetch + filesystem), with the kill/resume story against real HTTP fetches and a real file write. | `./target/debug/salvor --store /tmp/salvor-web.db run --agent examples/web-research/agent.toml --input @examples/web-research/input.json` (see [`web-research/README.md`](web-research/README.md) for prerequisites) |
| [`todo-agent/`](todo-agent/) | The batteries-included library tier: `Agent::builder()` plus a `Runtime`, with typed native tools and the built-in loop driving them. | `cargo run -p salvor-runtime --example todo_agent` |
| [`approval-loop/`](approval-loop/) | The library-first tier: a hand-written async function over the public `RunCtx`, with no built-in loop, and with the same durability, replay, and human-in-the-loop suspension. | `cargo run -p salvor-runtime --example approval_loop` |

`todo-agent` and `approval-loop` are ordinary Rust files (`main.rs`) wired
into `crates/salvor-runtime/Cargo.toml` as out-of-package `[[example]]`
targets, so `cargo build`/`cargo test --workspace` still compile-gate them
without running anything. Each carries a same-name `main.teach.md` walking
through what it demonstrates and why.

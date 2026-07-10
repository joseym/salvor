# Examples

Six worked examples, one per directory. Each shows Salvor from a different
entry point: the CLI over a real MCP-backed agent, the two library tiers you
can build against directly, the polyglot tool boundary from Python and
TypeScript, and untrusted code running as a sandboxed WebAssembly tool.

| Directory | Shows | Run it |
|---|---|---|
| [`web-research/`](web-research/) | The `salvor` CLI driving a config-driven agent over real MCP servers (fetch + filesystem), with the kill/resume story against real HTTP fetches and a real file write. | `./target/debug/salvor --store /tmp/salvor-web.db run --agent examples/web-research/agent.toml --input @examples/web-research/input.json` (see [`web-research/README.md`](web-research/README.md) for prerequisites) |
| [`python-tools/`](python-tools/) | Polyglot tools, no Salvor code: a Python MCP server (an expense tracker) is the agent's whole tool layer, reached over stdio. The polyglot story from Python. | `./target/debug/salvor --store /tmp/salvor-python.db run --agent examples/python-tools/agent.toml --input @examples/python-tools/input.json` (see [`python-tools/README.md`](python-tools/README.md) for the venv setup) |
| [`typescript-tools/`](typescript-tools/) | Polyglot tools, no Salvor code: a TypeScript/Node MCP server (a bookmarks manager) is the agent's whole tool layer, reached over stdio. The same story from TypeScript. | `./target/debug/salvor --store /tmp/salvor-typescript.db run --agent examples/typescript-tools/agent.toml --input @examples/typescript-tools/input.json` (see [`typescript-tools/README.md`](typescript-tools/README.md) for the npm setup) |
| [`wasm-tools/`](wasm-tools/) | Untrusted code as a tool: a WebAssembly component (the `salvor:tool@0.1.0` WIT world) runs in a wasmtime sandbox with operator-declared effect, limits, and grants; the binary's self-description is never used. Includes the Python (componentize-py) and JavaScript (jco) guest recipes. | Build the guest first (see [`wasm-tools/README.md`](wasm-tools/README.md)), then `./target/debug/salvor --store /tmp/salvor-wasm.db run --agent examples/wasm-tools/agent.toml --input @examples/wasm-tools/input.json` |
| [`todo-agent/`](todo-agent/) | The batteries-included library tier: `Agent::builder()` plus a `Runtime`, with typed native tools and the built-in loop driving them. | `cargo run -p salvor-runtime --example todo_agent` |
| [`approval-loop/`](approval-loop/) | The library-first tier: a hand-written async function over the public `RunCtx`, with no built-in loop, and with the same durability, replay, and human-in-the-loop suspension. | `cargo run -p salvor-runtime --example approval_loop` |

Every `[[mcp_servers]]` entry above spawns a local child process over stdio;
an entry can instead reach a server hosted elsewhere with `url` (plus
`bearer_token_env` for auth), shown commented-out in
[`web-research/agent.toml`](web-research/agent.toml) and explained in
[`web-research/README.md`](web-research/README.md#remote-mcp-servers).

`todo-agent` and `approval-loop` are ordinary Rust files (`main.rs`) wired
into `crates/salvor-runtime/Cargo.toml` as out-of-package `[[example]]`
targets, so `cargo build`/`cargo test --workspace` still compile-gate them
without running anything. Each carries a same-name `main.teach.md` walking
through what it demonstrates and why.

For driving the same runtime over HTTP instead of the CLI, the thin Python and
TypeScript client SDKs under [`../sdks/`](../sdks/) each ship a runnable
model-only example that mirrors this walkthrough against a `salvor serve`
control plane.

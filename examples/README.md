# Examples

Twelve worked examples, one per directory. Each shows Salvor from a different
entry point: the no-key local-model path, the CLI over real MCP-backed agents,
the durability guarantees on their own, the two library tiers, the polyglot
control plane over HTTP, and the v0.4 graph authoring surface. The ones marked
"no key" run for free against a local or scripted model.

| Directory | Shows | Run it |
|---|---|---|
| [`local-model/`](local-model/) | The no-key path: a config agent talking to a local model (Ollama or LM Studio) through `base_url`, so durability, replay, and budgets all work with no API key and no cost. | `salvor --store /tmp/salvor-local-model.db run --agent examples/local-model/agent.toml --input @examples/local-model/input.json` (needs a local model; see the README); no key |
| [`web-research/`](web-research/) | The `salvor` CLI driving a config-driven agent over real MCP servers (fetch + filesystem), with the kill/resume story against real HTTP fetches and a real file write. | `salvor --store /tmp/salvor-web.db run --agent examples/web-research/agent.toml --input @examples/web-research/input.json` (see [`web-research/README.md`](web-research/README.md)) |
| [`python-tools/`](python-tools/) | Polyglot tools, no Salvor code: a Python MCP server (an expense tracker) is the agent's whole tool layer, reached over stdio. The polyglot story from Python. | `salvor --store /tmp/salvor-python.db run --agent examples/python-tools/agent.toml --input @examples/python-tools/input.json` (see [`python-tools/README.md`](python-tools/README.md)) |
| [`typescript-tools/`](typescript-tools/) | Polyglot tools, no Salvor code: a TypeScript/Node MCP server (a bookmarks manager) is the agent's whole tool layer, reached over stdio. The same story from TypeScript. | `salvor --store /tmp/salvor-typescript.db run --agent examples/typescript-tools/agent.toml --input @examples/typescript-tools/input.json` (see [`typescript-tools/README.md`](typescript-tools/README.md)) |
| [`support-ops/`](support-ops/) | A product-shaped support-triage agent whose MCP tools show a real Read/Write/Idempotent effect mix. Two tools carry the same idempotency hint and get opposite operator treatment, because the effect system records the operator's determination of what a tool does, whatever the tool claims. Runs under a budget rail. | `salvor --store /tmp/salvor-support-ops.db run --agent examples/support-ops/agent.toml --input @examples/support-ops/input.json` (needs a model; see the README) |
| [`wasm-tools/`](wasm-tools/) | Untrusted code as a tool: a WebAssembly component (the `salvor:tool@0.1.0` WIT world) runs in a wasmtime sandbox with operator-declared effect, limits, and grants; the binary's self-description is never used. Includes the Python (componentize-py) and JavaScript (jco) guest recipes. | Build the guest first (see [`wasm-tools/README.md`](wasm-tools/README.md)), then `salvor --store /tmp/salvor-wasm.db run --agent examples/wasm-tools/agent.toml --input @examples/wasm-tools/input.json` |
| [`reconciliation/`](reconciliation/) | The write-safety guarantee on its own: a run killed mid-write leaves a dangling write intent, resume refuses with `NeedsReconciliation` and shows the recorded intent as evidence, and `salvor resolve` records the outcome by hand. The write happens exactly once. | `bash examples/reconciliation/run.sh`; no key |
| [`compliance/`](compliance/) | A compliance control in the library tier: a consequential Write is gated behind a mandatory, recorded human approval, and the append-only event log is the audit trail. Approve issues the action exactly once; reject records the decision and writes nothing. | `cargo run -p salvor-runtime --example compliance_gate` (see the README for the approve and reject steps); no key |
| [`todo-agent/`](todo-agent/) | The batteries-included library tier: `Agent::builder()` plus a `Runtime`, with typed native tools and the built-in loop driving them. | `cargo run -p salvor-runtime --example todo_agent` |
| [`approval-loop/`](approval-loop/) | The library-first tier: a hand-written async function over the public `RunCtx`, with no built-in loop, and with the same durability, replay, and human-in-the-loop suspension. | `cargo run -p salvor-runtime --example approval_loop` |
| [`polyglot-service/`](polyglot-service/) | The control plane over HTTP: a Python service and a TypeScript service each drive `salvor serve` through its SDK. Register an agent, start a run, stream events live, and resume a human-in-the-loop suspension, all against one durable Rust process. | `bash examples/polyglot-service/run.sh`; no key |
| [`graphs/`](graphs/) | The v0.4 graph authoring surface: canonical graph documents plus typed builders in Rust, TypeScript, and Python, all reducing to the same document that `salvor graph validate` checks. | `salvor graph validate examples/graphs/research-review-publish.json`; no key |

Every `[[mcp_servers]]` entry above spawns a local child process over stdio; an
entry can instead reach a server hosted elsewhere with `url` (plus
`bearer_token_env` for auth), shown commented-out in
[`web-research/agent.toml`](web-research/agent.toml) and explained in
[`web-research/README.md`](web-research/README.md#remote-mcp-servers).

`todo-agent`, `approval-loop`, and `compliance` are ordinary Rust files
(`main.rs`) wired into `crates/salvor-runtime/Cargo.toml` as out-of-package
`[[example]]` targets, so `cargo build`/`cargo test --workspace` still
compile-gate them without running anything.

The thin Python and TypeScript client SDKs under [`../sdks/`](../sdks/) each also
ship a runnable model-only example against a `salvor serve` control plane;
[`polyglot-service/`](polyglot-service/) is the fuller version that adds a
human-in-the-loop resume over HTTP.

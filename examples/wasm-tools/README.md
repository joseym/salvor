# wasm-tools: an untrusted WebAssembly component as a Salvor tool

This example runs an agent whose only tool is a WebAssembly component
executed inside a wasmtime sandbox. The component ([`guest/`](guest/)) is
treated as untrusted code: it gets **no capability the operator did not
grant** (here: none at all), it runs under hard per-call caps on wall time
and memory, and everything the model is told about it (name,
description, schema, side-effect class) comes from [`agent.toml`](agent.toml),
never from the binary itself.

The guest contract is one WIT world, `salvor:tool@0.1.0`
([`guest/wit/tool.wit`](guest/wit/tool.wit)):

```wit
world tool {
    export call: func(input: string) -> result<string, string>;
}
```

JSON goes in, JSON comes out, and a guest-level failure is the `err` side of
the result. That is the entire surface a tool author writes against, in any
language with a component toolchain: this directory builds the Rust guest,
and the recipes at the bottom build the same world from Python and
JavaScript. All three run under the identical host.

## What the operator declares (and the guest cannot)

Look at the `[[wasm_tools]]` entry in [`agent.toml`](agent.toml):

- `effect = "read"` is **required, with no default**. This is stricter than
  MCP on purpose. An MCP server legitimately self-describes, so an
  unannotated MCP tool falls back to the safe `write` reading; a sandboxed
  binary has no channel to describe itself, so a missing `effect` means the
  operator has not decided, and the config parser refuses the file.
- `name`, `description`, and `input_schema` are operator-authored. A hostile
  binary's self-description would be a prompt-injection surface aimed at
  your model, so the binary's self-description is never read, and rendering
  configuration requires instantiating no untrusted code.
- `[wasm_tools.limits]` caps one call: `wall_time_ms` (an epoch deadline
  that kills a runaway loop) and `memory_bytes` (a denied allocation past
  the cap fails the call, with an error naming the cap). These are per-call
  rails, separate from the run-level `[budgets]`: a call that hits a limit
  becomes an error tool result the model can react to; it never parks the
  run.
- There is no `[wasm_tools.grants]` section here, and that absence is the
  default posture: no filesystem, no environment, no arguments, no network,
  no stdio except stderr captured into tracing. A grant, when you do want
  one, is a directory preopen:

  ```toml
  [wasm_tools.grants]
  preopen = [{ host = "./data", guest = "/data", perms = "read" }]
  ```

  `perms` is `"read"` or `"read_write"`. Directory preopens are the whole
  v0.2 capability surface; network access is deliberately not offered (a
  tool that needs the network belongs on an MCP server, where the operator
  is choosing to run a networked process).

## Prerequisites

1. **Rust with the wasm32-wasip2 target** (stock rustup; no extra tooling):

   ```sh
   rustup target add wasm32-wasip2
   ```

2. **An Anthropic API key**, exported as `DEMO_ANTHROPIC_API_KEY`; it is
   read at run time and never written to any file. A subscription OAuth
   token (`sk-ant-oat...`) works too: add `api_key_kind = "oauth"` to the
   `[llm]` section first.

## Build the guest

From the repository root:

```sh
cargo build --manifest-path examples/wasm-tools/guest/Cargo.toml \
  --target wasm32-wasip2 --release --target-dir target/wasm-guests
```

A plain `cargo build` against the wasm32-wasip2 target emits the component
directly (~160 KB); no cargo-component, no adapter step. The guest source
([`guest/src/lib.rs`](guest/src/lib.rs)) is `wit_bindgen::generate!` plus
one impl block. The explicit `--target-dir` keeps build output in the
workspace's `target/` instead of nesting one inside the example;
`agent.toml`'s `path` points there.

Optionally pin the binary: compute its hash and uncomment `sha256 = "..."`
in `agent.toml`. With a pin set, the runtime refuses to load any other bytes
at that path, before compiling them, let alone running them:

```sh
shasum -a 256 target/wasm-guests/wasm32-wasip2/release/wordcount_guest.wasm
```

## Run

From the repository root (the wasm paths in `agent.toml` resolve relative to
the agent file, so any working directory works; the store path below is just
tidy):

```sh
salvor --store /tmp/salvor-wasm.db run \
  --agent examples/wasm-tools/agent.toml \
  --input @examples/wasm-tools/input.json
```

The model calls `wordcount` once per snippet and replies with a comparison.
A run is small: two tool calls and three short model turns, held under the
$0.25 cost rail by the `[budgets]` section. A measured live run came to
about $0.02 (1709 input tokens, 314 output tokens at list price).

## The same tool from Python and JavaScript

The WIT world is the contract, not the language. Both recipes below were
built and run against this repository's host (the `salvor-wasm` crate) with
the versions shown; each produces a component you can wire into an agent
file exactly like the Rust one, or drive through the proof harness at the
bottom. Both guests implement three input modes so the harness can prove the
sandbox, not just the happy path: `wordcount` (the JSON round trip), `fail`
(a guest-level error), and `spin` (a runaway loop for the wall-time trap).
The JSON shape is otherwise yours to define.

### Python (componentize-py 0.25.0, Python 3.14.6)

`app.py`:

```python
import json

import wit_world
from componentize_py_types import Err


class WitWorld(wit_world.WitWorld):
    def call(self, input: str) -> str:
        try:
            request = json.loads(input)
        except json.JSONDecodeError as e:
            raise Err(f"input is not JSON: {e}")
        mode = request.get("mode")
        if mode == "wordcount":
            text = request.get("text", "")
            return json.dumps({"words": len(text.split()), "chars": len(text)})
        if mode == "fail":
            raise Err(f"guest failure: {request.get('message', 'requested')}")
        if mode == "spin":
            counter = 0
            while True:
                counter += 1
        raise Err(f"unknown mode: {mode}")
```

The bindings module is named after the world (`tool` world, `wit_world`
module with a `WitWorld` protocol class), and the `err` side of the WIT
result is raised as `componentize_py_types.Err`. Build it (with `wit/`
copied beside `app.py`):

```sh
python3 -m venv .venv && .venv/bin/pip install componentize-py==0.25.0
.venv/bin/componentize-py --wit-path wit --world tool componentize app -o pytool.wasm
```

The build takes about 1.3 s and produces an 18 MB component (it embeds
CPython). It runs under the identical host, well inside the default 128 MiB
memory cap.

### JavaScript (jco 1.25.2, ComponentizeJS 0.21.0, Node v26.4.0)

`tool.js`:

```js
export function call(input) {
  let request;
  try {
    request = JSON.parse(input);
  } catch (err) {
    throw `input is not JSON: ${err}`;
  }
  switch (request.mode) {
    case "wordcount": {
      const text = request.text ?? "";
      const words = text.split(/\s+/).filter((w) => w.length > 0).length;
      return JSON.stringify({ words, chars: [...text].length });
    }
    case "fail":
      throw `guest failure: ${request.message ?? "requested"}`;
    case "spin": {
      let counter = 0n;
      for (;;) {
        counter += 1n;
      }
    }
    default:
      throw `unknown mode: ${request.mode}`;
  }
}
```

The world's export becomes a plain exported function; returning fulfills the
`ok` side, and **throwing a string** is the `err` side of
`result<string, string>`. Build it (with `wit/` copied beside `tool.js`):

```sh
npm install @bytecodealliance/jco @bytecodealliance/componentize-js
npx jco componentize tool.js --wit wit --world-name tool \
  --disable http fetch-event --out jstool.wasm
```

The `--disable http fetch-event` flags matter: jco's StarlingMonkey engine
wires in `wasi:http` (its `fetch` support) by default, and this host
deliberately does not link `wasi:http`, because network access is outside
the v0.2 sandbox fence. Without the flags, instantiation fails with
"component imports instance `wasi:http/types@0.2.10`, but a matching
implementation was not found in the linker", which is the fence working as
designed. The build takes about 2.7 s and produces a 12 MB component (it
embeds the StarlingMonkey JS engine).

### Proving a guest under the host

`salvor-wasm` ships an ignored integration test that drives any component
implementing the three modes through the standard proof set: the unicode
JSON round trip, a guest error crossing as an error (not a crash), and the
wall-time cap killing the `spin` loop:

```sh
SALVOR_WASM_COMPONENT=/path/to/your-guest.wasm \
  cargo test -p salvor-wasm --test wasm_tool -- --ignored external_component_proof
```

Both recipe guests above pass it. That demonstrates the polyglot claim in executable
form: one WIT world, one host, three languages.

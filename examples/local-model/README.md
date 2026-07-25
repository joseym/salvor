# Example: a Salvor agent running entirely against a local model

This is the zero-cost path: an agent with no tools and no MCP servers,
answering one question, using a model running on your own machine. There is
no API key anywhere in this directory and no network call ever reaches
Anthropic. Durability, replay, and budgets all work exactly the same as
every other example; the only thing that changes is where the model call
goes.

That works because Salvor's model client speaks one wire protocol, the
Anthropic Messages API (`POST {base_url}/v1/messages`), and both Ollama
(0.14+) and LM Studio (0.4.1+) answer to that same protocol. Pointing an
agent at a local model is a `base_url` change in `agent.toml`, not a
different code path. See `crates/salvor-llm/src/lib.rs` and
`crates/salvor-cli/src/agent_config.rs` for where that is implemented.

## What is here

- `agent.toml`: a plain assistant, no tools, no MCP servers. The `[llm]`
  section sets `base_url` to Ollama's default local endpoint and
  `api_key_env` to a variable name nothing sets, so the run sends no
  `x-api-key` header at all.
- `input.json`: one question.

## Prerequisites

Pick one local model server. You only need one of these, not both.

### Option A: Ollama (the default in `agent.toml`)

1. Install Ollama: <https://ollama.com/download>.
2. Pull a small model and start the server (`ollama serve` runs automatically
   after install on macOS; if it is not already running, run it yourself):
   ```sh
   ollama pull llama3.2
   ```
3. Ollama listens on `http://localhost:11434` by default, which is what
   `agent.toml` already points at. The `model` field, `llama3.2`, is the tag
   you pulled.

### Option B: LM Studio

1. Install LM Studio: <https://lmstudio.ai>.
2. In LM Studio, download a model and load it, then start the local server
   (the "Local Server" tab, or the `lms server start` CLI command).
3. LM Studio listens on `http://localhost:1234` by default. To switch
   `agent.toml` to it, change one line in the `[llm]` section:
   ```toml
   base_url = "http://localhost:1234"
   ```
   and change `model` to the identifier LM Studio shows for the loaded
   model (visible in its UI and in `GET /v1/models`), for example
   `llama-3.2-3b-instruct`. Everything else in `agent.toml` is unchanged.

Either way, no account, no API key, and no per-token cost.

## Running it

From the repository root, with the model server from either option above
running:

```sh
cargo build

salvor --store /tmp/salvor-local-model.db \
    run --agent examples/local-model/agent.toml \
        --input @examples/local-model/input.json
```

The run prints its id, then the model's answer to the question in
`input.json`. Inspect what actually happened with the same commands every
other example uses:

```sh
salvor --store /tmp/salvor-local-model.db list
salvor --store /tmp/salvor-local-model.db history <run-id>
salvor --store /tmp/salvor-local-model.db replay <run-id>
```

The event log, the durable store, and replay are identical whether the
model call went to Ollama, LM Studio, or the public Anthropic endpoint;
none of that machinery depends on where the model lives.

## What a small local model can and cannot do here

This example asks one plain question with no tools, which is well within
reach of a small model like `llama3.2` (the 3B tag). It will answer
noticeably slower than a hosted frontier model and the answer will be
shorter and less polished. Do not expect a small local model to reliably
drive multi-step tool use the way `examples/web-research` or
`examples/python-tools` do; tool-calling reliability varies a great deal by
model and quantization, and a 3B-class model will miss or malform tool
calls that a larger model handles without trouble. If you want to try tools
against a local model, start by pointing `examples/python-tools/agent.toml`
at a larger local model (an 8B+ instruct model with explicit tool-use
training) rather than expecting it from the smallest model you can pull.

## If nothing is listening on the port

If you run the command above without a local model server running, the
model call fails with a plain connection error (Ollama and LM Studio are not
started), not a config or parse error: `agent.toml` still loads and the run
still starts, gets as far as the model call, and stops there. If the server
is running but the model in `agent.toml` was never pulled or loaded, Ollama
and LM Studio both answer with an HTTP 404 saying the model was not found,
which Salvor surfaces as a plain "model call" error rather than a run that
appears to hang.

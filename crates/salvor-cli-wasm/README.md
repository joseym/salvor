# salvor-cli-wasm

A thin `wasm-bindgen` wrapper over the pure
[`salvor-cli-core`](../salvor-cli-core) crate. It compiles the CLI's own clap
parse tree, its own value-to-text renderer, and its own agent-file parse to
WebAssembly, so a terminal drawn in a browser can parse a typed command line,
answer `--help`, draw a run listing, print a run's history, and say whether an
`agent.toml` is valid with the REAL parsers and renderers rather than
hand-written copies that drift the moment either side changes.

`salvor-cli-core` stays pure and IO-free; all the wasm plumbing lives here. It
is the same split, and the same payoff, as
[`salvor-replay-wasm`](../salvor-replay-wasm) has for the state fold.

## API surface

```ts
// Parse a full argv (program name at index 0) into what it means, as JSON.
// Returns {"ok":true,"command":{...}} on a parse, or
// {"ok":false,"message":{kind,is_error,exit_code,plain,ansi}} when clap
// refuses the line or is displaying help or a version. Throws only if argvJson
// is not a JSON array of strings: a refused command line is a message, not a
// throw.
function parseArgv(argvJson: string): string;

// The `--help` page for the root ("") or a named subcommand ("list",
// "graph validate"). Throws on a path that names no subcommand.
function helpText(path: string): string;      // unstyled
function helpTextAnsi(path: string): string;  // with ANSI escape codes

// The `salvor list` table, from a JSON array of RunSummary objects each
// carrying an added `status` key (the label folded from that run's log).
function renderList(rowsJson: string): string;       // as `salvor list` writes it
function renderListPlain(rowsJson: string): string;  // as `salvor list | cat` reads

// The `salvor history` listing, from a run's event log as the wire JSON the
// store writes (the same input salvor-replay-wasm's `deriveState` takes): one
// line per event, in log order, each newline terminated.
function renderHistory(logJson: string): string;       // as `salvor history` writes it
function renderHistoryPlain(logJson: string): string;  // as `salvor history | cat` reads

// An agent definition, parsed and validated with the CLI's own parser.
// Returns {"ok":true,"config":{...}} for a file it accepts, or
// {"ok":false,"error":"..."} carrying the message `salvor agent validate`
// prints. A refused file is a message, not a throw.
function parseAgentToml(text: string): string;
```

The JSON shapes these carry are documented in
[`types/index.d.ts`](types/index.d.ts), which is the one place a TypeScript
consumer reads.

## Plain and styled

Every text surface comes in both forms, because a terminal emulator wants the
escape codes and a plain `<pre>` does not.

- **Plain** is what `.to_string()` yields on a `clap::Error` or a
  `clap::builder::StyledStr`. Both strip styling regardless of the command's
  `ColorChoice`, so there is no colour setting that makes them emit escapes.
- **Styled** requires calling `.ansi()` explicitly. That is the only way real
  escape codes come out of clap.

The list table runs the other way: `render::list_table` styles its STATUS
column unconditionally (stripping is the writer's job in the real CLI, which
prints through an `anstream` stream), so `renderList` is the styled form and
`renderListPlain` strips it with `anstream::adapter::strip_str`, the same pass
`anstream` makes when the CLI's stdout is a pipe.

The history listing takes the same pair for a different reason:
`render::history_line` emits no escape codes today, so the two forms are equal.
The pair is kept, and the equality asserted rather than assumed, because
"give me the plain one" has to stay true if the listing ever grows a styled
column.

## No standard output

There is no standard output in wasm, so nothing here writes to a stream: every
surface returns a `String` and the caller decides where it goes. clap's help
comes from `Command::render_long_help` (the long form, which is what `--help`
prints; `-h` prints the short one) and its refusals from `Error::to_string` and
`Error::render`. None of clap's stream-writing helpers are reachable from this
crate, and a grep for the printing macros and the standard streams over `src/`
finds nothing.

## The same-render proof (this crate's reason to exist)

Every committed case is rendered natively **and** through the wasm module, and
compared byte for byte. The chain has two links:

1. `tests/same_render.rs` (native, runs under `cargo test`) checks two things
   per case. First, the wasm-facing function's output equals what calling
   `salvor-cli-core` directly produces: the divergence guard, so the browser
   cannot show a table, a help page, or a refusal the real CLI would not.
   Second, that output still equals the committed fixture: the drift guard,
   which is the half that notices a change to `salvor-cli-core` itself, since
   a renderer change moves both sides together.
2. `js/same-render.mjs` (Node, runs the wasm build) feeds the same committed
   inputs through `renderList`, `helpText`, `parseArgv`, `renderHistory`, and
   `parseAgentToml` and asserts the results equal that same committed expected.

Together: **native == committed == wasm**, all three checked live. The
comparison count is the committed corpus: two per row set, two per help page,
one per argv, two per event log, one per agent file.

The corpus is deliberately wide. The list tables cover every status label the
STATUS column can print (each takes a different colour branch), an unrecognised
label, an empty table, and a wide row. The help pages cover the root, flat
verbs, both nested groups, and a nested verb under each. The parse cases cover
every refusal shape the CLI has, including the two custom `did you mean` tips
that a plain `value_parser` would have replaced with clap's string-similarity
guess (which for `--group awaiting-model` names the WRONG group).

The event logs cover the hero fixture's own ten events (the run behind the
terminal on salvor.run, so the page's listing is measured against the listing
that run really produces) plus the ways a run stops short: a budget crossing,
a suspension and its resume, a random observation, and a failure. Each takes a
different arm of the event renderer.

The agent files are the real thing. `fixtures/agents/` holds copies of files
this repository ships (`examples/hero/agent.toml` and three others), checked
against their originals on every run so a fixture cannot outlive the file it
was about, and a separate test parses **every** agent file in the repository
live. Alongside them sit definitions written to be refused, one per rule:
an unknown field, both prompt settings, an MCP server with no transport and one
with two, a wasm tool with no `effect`, a malformed idempotency path, and text
that is not TOML. Their committed expectation is the CLI's own message, so a
reworded refusal is a visible diff rather than a silent change to what a page
shows somebody who mistyped their file.

## Building

```sh
# The npm package a web page consumes (--target web), output to pkg/:
wasm-pack build --target web --out-dir pkg --out-name salvor_cli_wasm

# The Node build the proof drives (--target nodejs), pkg-node/:
wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_cli_wasm
```

`pkg/` and `pkg-node/` are wasm-pack outputs (each self-gitignored) and are not
committed. Latest `.wasm` size: **1.19 MB** optimized, **412 KB** gzipped.

It grew from 471 KB when the agent-file parse landed, and the growth is the
parse: a TOML reader, and the contract layer of `salvor-tools` for the one type
that decides what an idempotency path may say. Reaching for the real
`IdempotencyPath::parse` rather than restating the rule here is the whole point
of the export, so the bytes are the price of the guarantee. The MCP client and
its Tokio runtime are NOT in there: `salvor-cli-core` takes `salvor-tools` with
`default-features = false`, which is what leaves them out, and the
wasm32-unknown-unknown build in CI is what proves it stayed that way.

## Running the proof

```sh
# Native side (fast, no wasm toolchain):
cargo test -p salvor-cli-wasm

# Wasm side, after building the nodejs package:
node js/same-render.mjs
```

## Regenerating fixtures

The reference inputs and their native outputs are committed under `fixtures/`.
To rewrite them after changing the reference corpus:

```sh
REGEN_FIXTURES=1 cargo test -p salvor-cli-wasm --test same_render -- --ignored regenerate
```

The `--store` help line prints the value of `SALVOR_STORE`, and its default
feeds every parsed command, so the fixtures are generated and checked with that
variable unset. The tests refuse to run rather than produce a mysterious diff
if it is set.

## Purity

The wasm build reaches `salvor-replay` with `default-features = false` on both
paths into it, so the `rng` feature (the one randomness-drawing constructor) is
off and the module draws no randomness. The parse and render cores are ordinary
Rust that also builds natively, so `cargo build/test --workspace` needs no wasm
toolchain.

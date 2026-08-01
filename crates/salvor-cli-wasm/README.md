# salvor-cli-wasm

A thin `wasm-bindgen` wrapper over the pure
[`salvor-cli-core`](../salvor-cli-core) crate. It compiles the CLI's own clap
parse tree and its own value-to-text renderer to WebAssembly, so a terminal
drawn in a browser can parse a typed command line, answer `--help`, and draw a
run listing with the REAL parser and renderer rather than a hand-written copy
that drifts the moment either side changes.

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
```

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
   inputs through `renderList`, `helpText`, and `parseArgv` and asserts the
   results equal that same committed expected.

Together: **native == committed == wasm**, all three checked live. The
comparison count is the committed corpus: two per row set, two per help page,
one per argv, **60** as it stands.

The corpus is deliberately wide. The list tables cover every status label the
STATUS column can print (each takes a different colour branch), an unrecognised
label, an empty table, and a wide row. The help pages cover the root, flat
verbs, both nested groups, and a nested verb under each. The parse cases cover
every refusal shape the CLI has, including the two custom `did you mean` tips
that a plain `value_parser` would have replaced with clap's string-similarity
guess (which for `--group awaiting-model` names the WRONG group).

## Building

```sh
# The npm package a web page consumes (--target web), output to pkg/:
wasm-pack build --target web --out-dir pkg --out-name salvor_cli_wasm

# The Node build the proof drives (--target nodejs), pkg-node/:
wasm-pack build --target nodejs --out-dir pkg-node --out-name salvor_cli_wasm
```

`pkg/` and `pkg-node/` are wasm-pack outputs (each self-gitignored) and are not
committed. Latest `.wasm` size: **471 KB** optimized, **185 KB** gzipped.

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

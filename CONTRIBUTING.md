# Contributing

Salvor is a durable execution runtime, so the bar for a change is not "it works on my machine" but "a crash at any point still leaves an honest log." That shapes most of what follows.

## Before you start

For anything larger than a bug fix, open an issue first. The design has a few load-carrying invariants (below) and a change that violates one is usually rejected on principle rather than on style, which is a frustrating way to spend a weekend.

## Getting set up

```sh
cargo build                 # produces target/debug/salvor
cargo test --workspace      # the Rust suite
cog install-hook --all      # commit-message linting, needs cocogitto
```

The web UI lives in `bridge/` and needs Node 24 or newer:

```sh
cd bridge && npm ci && npm test
```

`salvor serve --dev` runs the API and the Angular dev server together with hot reload.

## The invariants

These are the rules a review will hold you to.

**Write before you act.** An event is appended before the runtime does the thing the event describes. A resume replays what is recorded; anything not recorded did not happen.

**Replay executes nothing.** Replaying a log must make zero live calls. If a change makes replay reach for the network, the change is wrong.

**Absent is not zero.** When a value is unknown, omit it. Do not substitute a default that reads as a measurement, because a zero that means "we never looked" is a lie a dashboard will repeat.

**Writes are never replayed blind.** A tool's effect (read, write, idempotent) decides what a resume may re-run. A write left dangling by a crash blocks the resume until a human reconciles it, and that decision is recorded as an event.

**Schema changes are additive.** New fields carry `#[serde(default, skip_serializing_if = ...)]` so an old log still folds and a new log written without the field is byte-identical to what the previous version produced. There are pinned-JSON tests for this; keep them passing.

## Tests

New behavior needs a test that would fail without it. For anything touching durability, the test that matters is a crash test: kill the run at the boundary your change introduces and assert the resumed log is byte-identical with no duplicate writes. `crates/salvor-runtime/tests/release_gate.rs` does this across every boundary of a reference run and is the suite most likely to catch a mistake.

## Commits

Conventional Commits, enforced by `cog verify` in the commit-msg hook. Write the message so it explains the change to someone who has no other context: no internal plan or milestone references, and no attribution trailers.

Comments should state a constraint the code cannot show on its own. A comment explaining what the next line does, or where an idea came from, is noise by the time the PR merges.

## AI assistance

Use whatever tools you like, and say so in the pull request. A disclosed AI-assisted patch with tests gets a normal review. An undisclosed one that turns out to be unreviewed generated code wastes a maintainer's afternoon, which is the actual thing being asked about here.

You are responsible for every line you submit: that you understand it, that you can defend the design, and that the tests you added would genuinely fail without the change.

## Compatibility

Salvor is pre-1.0. A 0.x release may change the CLI or the HTTP API without a deprecation window, so pin the exact version you depend on rather than a range. The one promise that holds regardless is the schema-additivity invariant above ("Schema changes are additive"): an old log still folds under a new version, and a new log written without a field is byte-identical to what the previous version produced. [CHANGELOG.md](CHANGELOG.md) records what moved release to release.

## Licensing

Contributions are dual-licensed under MIT or Apache-2.0, matching the project. By opening a pull request you agree to that, with no additional terms.

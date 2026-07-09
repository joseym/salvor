# Salvor

A durable execution runtime for AI agents, in Rust.

Event-sourced runs, typed tool contracts with side-effect classification, crash-exact resume, and hard budgets — deployed as a single static binary with an embedded store.

**Status:** early development, pre-0.1. Nothing here is usable yet.

## Workspace

| Crate | Purpose |
|---|---|
| `salvor-core` | Event model, replay engine, budget enforcement, deterministic context |
| `salvor-store` | `EventStore` trait + SQLite (WAL) implementation |
| `salvor-llm` | Messages API client (Anthropic hosted and local endpoints) |
| `salvor-tools` | `ToolHandler` trait, effect classification, MCP client |
| `salvor-cli` | The `salvor` binary: `run`, `resume`, `list`, `history`, `replay` |

## Development

Run this once after cloning (install cocogitto first with `brew install cocogitto` if you do not have it):

```
cog install-hook --all
```

Commit messages follow Conventional Commits, enforced by `cog verify` in the commit-msg hook. Releases are cut with `cog bump`.

## License

MIT OR Apache-2.0

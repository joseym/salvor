# salvor

A durable execution runtime for AI agents, in Rust. Event-sourced runs, typed
tool contracts with side-effect classification, crash-exact resume, and hard
budgets, deployed as a single static binary with an embedded store.

This is the umbrella crate. The runtime currently ships as the `salvor-*`
crates and the `salvor` binary; a future release re-exports the public surface
from here.

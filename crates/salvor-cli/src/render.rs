//! Turning runtime and store values into the text the CLI prints.
//!
//! Almost all of it lives in [`salvor_cli_core::render`] and is re-exported
//! here unchanged, so every `render::` path in this crate resolves as before.
//! That module is pure: a function from a value to a `String`, with no IO, no
//! store access, and no clock, which is what lets a browser terminal format
//! salvor's output with the same code this binary does.
//!
//! What stays here is the one table that cannot be pure, because its input
//! describes live processes on this machine.

pub use salvor_cli_core::render::*;

use crate::serve_kill::RunningServer;

/// The `salvor serve --kill` table: one numbered row per discovered `salvor
/// serve` process, so an operator picking one at the prompt can name it by
/// number, pid, or port.
#[must_use]
pub fn server_table(servers: &[RunningServer]) -> String {
    let mut out = format!("{:>3}  {:<8}  {:<21}  {}\n", "#", "PID", "BIND", "STORE");
    for (index, server) in servers.iter().enumerate() {
        out.push_str(&format!(
            "{:>3}  {:<8}  {:<21}  {}\n",
            index + 1,
            server.pid,
            server.bind,
            server.store,
        ));
    }
    out
}

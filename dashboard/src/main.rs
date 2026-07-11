//! The Salvor dashboard: a client-side-rendered Leptos app, compiled to wasm.
//!
//! This is the entry point Trunk builds. It mounts the [`app::App`] root into
//! the document body; everything else lives in the modules below.
//!
//! # Module map
//!
//! - [`config`]: which control-plane server to talk to.
//! - [`replay`]: the reuse layer over `salvor-replay`; the real state fold.
//! - [`sse`]: the server-sent-events client and connection state.
//! - [`status`]: the run-status model and the `StatusBadge` / `RunRef` atoms.
//! - [`pricing`]: the dollar-cost estimate layer over exact token counts.
//! - [`inspector`]: the run inspector view, its rows, and the scrubber.
//! - [`app`]: the shell and routing skeleton.
//! - [`views`]: the four views; the inspector is now real, the rest placeholders.

// This is the foundation. It defines the shared atoms (StatusBadge,
// RunRef), the run-status model, and the SSE client that the four views
// will consume; those consumers do not exist yet, so several items are defined
// ahead of their first use. Allowed crate-wide, with a note, rather than
// peppering the foundation with per-item allows. Each such item carries a unit
// test or a documented role, and the allow comes off as the views land.
#![allow(dead_code)]

mod app;
mod config;
mod inspector;
mod pricing;
mod replay;
mod sse;
mod status;
mod views;

use leptos::mount::mount_to_body;

use crate::app::App;

/// Mounts the app into `<body>`. Trunk calls this on load.
fn main() {
    mount_to_body(App);
}

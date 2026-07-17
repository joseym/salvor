//! Guarantees the dashboard asset folder exists before the `ui` feature's
//! embed macro runs.
//!
//! rust-embed's derive requires its `folder` to exist at compile time. A fresh
//! checkout that has not run `salvor build` yet has no `bridge/dist` tree, and
//! a build with the `ui` feature would then fail to compile at all. Creating
//! the folder here keeps that build honest: an empty folder embeds no files, so
//! the server answers `/` with the plain-text "built without the dashboard"
//! note instead of failing to build.
//!
//! This runs only when the `ui` feature is active (Cargo sets
//! `CARGO_FEATURE_UI`); a headless build touches nothing.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_UI").is_none() {
        return;
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is always set for a build script");
    let assets = Path::new(&manifest).join("../../bridge/dist/bridge/browser");
    // A best-effort create: if it already exists (the common case), this is a
    // no-op; a genuine failure surfaces when the embed macro cannot find it.
    let _ = std::fs::create_dir_all(&assets);
}

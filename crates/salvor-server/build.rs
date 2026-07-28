//! Two independent, unconditional jobs: guarantee the dashboard asset folder
//! exists before the `ui` feature's embed macro runs, and stamp the build
//! with the source commit `GET /v1/capabilities` reports.
//!
//! ## The dashboard asset shim
//!
//! rust-embed's derive requires its `folder` to exist at compile time. A fresh
//! checkout that has not run `salvor build` yet has no `bridge/dist` tree, and
//! a build with the `ui` feature would then fail to compile at all. Creating
//! the folder here keeps that build honest: an empty folder embeds no files, so
//! the server answers `/` with the plain-text "built without the dashboard"
//! note instead of failing to build.
//!
//! This half runs only when the `ui` feature is active (Cargo sets
//! `CARGO_FEATURE_UI`); a headless build touches nothing.
//!
//! ## The commit stamp
//!
//! `lib.rs`'s capabilities handler wants a short commit hash alongside
//! `env!("CARGO_PKG_VERSION")`, but a version number is compile-time-constant
//! for a crate while a commit hash is a fact about the CHECKOUT, not the
//! crate, so it has to be shelled out for and threaded through as an
//! environment variable (`SALVOR_SERVER_GIT_COMMIT`) the handler reads with
//! `option_env!`. This runs unconditionally (not gated on the `ui` feature):
//! the version/commit stamp is a headless-server fact too.
//!
//! Degrades honestly: a source tarball with no `.git`, or a checkout with no
//! `git` on PATH, sets no environment variable at all, so
//! `option_env!("SALVOR_SERVER_GIT_COMMIT")` is `None` and the handler omits
//! the `commit` key entirely rather than printing a placeholder like
//! `"unknown"`. A non-clean working tree at build time (`git status
//! --porcelain` prints anything at all: modified tracked files, staged
//! changes, OR untracked files sitting in the tree) appends `-dirty` to the
//! hash; this is a snapshot taken when the build script last ran, not a live
//! fact, because Cargo only reruns a build script when its declared
//! `rerun-if-changed` inputs change — here, `.git/HEAD` and `.git/index`
//! (checkouts, commits, and staged/unstaged
//! changes to already-tracked files all touch one of the two). Editing a
//! tracked file without staging it will not by itself trigger a rerun; this
//! is the same staleness every git-describe-based version stamp accepts.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_ui_asset_shim();
    emit_git_commit();
}

fn emit_ui_asset_shim() {
    if std::env::var_os("CARGO_FEATURE_UI").is_none() {
        return;
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is always set for a build script");
    let assets = Path::new(&manifest).join("../../bridge/dist/bridge/browser");
    // If it already exists (the common case in a checkout) this is a no-op. When it cannot be
    // created the build is doomed either way, so say why here rather than letting the embed macro
    // report a missing folder: the usual cause is building from a published crate, where the path
    // points outside the package and no registry build can produce it.
    if let Err(err) = std::fs::create_dir_all(&assets) {
        println!(
            "cargo::warning=the `ui` feature needs {} , which could not be created ({err}). \
             The dashboard assets live outside this package, so `ui` only works from a checkout \
             that has run the Bridge build; a crates.io build should leave the feature off.",
            assets.display()
        );
    }
}

fn emit_git_commit() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is always set for a build script");
    let repo_root = Path::new(&manifest).join("../..");

    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join(".git/HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join(".git/index").display()
    );

    let Ok(rev) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&repo_root)
        .output()
    else {
        // No `git` on PATH: build from a source tarball or a minimal
        // container. No commit key, never a placeholder.
        return;
    };
    if !rev.status.success() {
        // Not a git checkout at all (no `.git`, e.g. a source tarball
        // extracted without history). Same honest omission.
        return;
    }
    let mut commit = String::from_utf8_lossy(&rev.stdout).trim().to_string();
    if commit.is_empty() {
        return;
    }

    // Best-effort dirty detection: if `git status --porcelain` cannot run for
    // any reason, this simply skips the `-dirty` suffix rather than failing
    // the build over a cosmetic detail.
    if let Ok(status) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .output()
        && status.status.success()
        && !status.stdout.is_empty()
    {
        commit.push_str("-dirty");
    }

    println!("cargo:rustc-env=SALVOR_SERVER_GIT_COMMIT={commit}");
}

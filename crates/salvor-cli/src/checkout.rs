//! Checkout discovery and the login-shell subprocess helper shared by
//! `salvor build` and `salvor serve --dev`: the two verbs that need a real
//! salvor checkout with `bridge/` next to it, and that run node/npm to work
//! on it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Walks up from the current directory for the workspace root: a directory
/// with a `Cargo.toml` that declares the salvor workspace and a sibling
/// `bridge/` tree. Names what it looked for when the search runs out.
pub fn find_repo_root() -> Result<PathBuf> {
    let start = std::env::current_dir().context("reading the current directory")?;
    find_repo_root_from(&start)
}

/// The walk-up search behind [`find_repo_root`], parameterized on the start
/// directory so a test can drive it without mutating this process's actual
/// current directory (global, shared state that a parallel test run cannot
/// safely touch).
fn find_repo_root_from(start: &Path) -> Result<PathBuf> {
    let mut dir = start;
    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.is_file()
            && dir.join("bridge").is_dir()
            && std::fs::read_to_string(&cargo)
                .map(|text| text.contains("[workspace]") && text.contains("crates/salvor-cli"))
                .unwrap_or(false)
        {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => bail!(
                "not inside a salvor checkout: walked up from {} and found no workspace \
                 Cargo.toml declaring the salvor members alongside a bridge/ directory",
                start.display()
            ),
        }
    }
}

/// Installs the Bridge's node dependencies (`npm ci`) if `node_modules` is
/// missing, printing a progress line first. A silent no-op once they are
/// present, which is the common case after the first run.
pub async fn ensure_node_modules(bridge: &Path) -> Result<()> {
    if !bridge.join("node_modules").is_dir() {
        println!("installing dashboard dependencies (npm ci)");
        run_shell(bridge, "npm ci").await?;
    }
    Ok(())
}

/// Runs one shell line in `dir` to completion, through a login shell (`bash
/// -lc`), inheriting this process's streams. Bails, naming the step, on a
/// non-zero exit.
///
/// A login shell so a node toolchain managed by nvm, and cargo under
/// `~/.cargo/bin`, resolve the same way they do at an interactive prompt
/// (plain `bash -c`, or spawning `node`/`npm` directly, does not source the
/// profile that sets that up, and an interactive-shell nvm hook under zsh in
/// particular is not reliable when driven non-interactively).
pub async fn run_shell(dir: &Path, line: &str) -> Result<()> {
    let status = tokio::process::Command::new("bash")
        .arg("-lc")
        .arg(line)
        .current_dir(dir)
        .status()
        .await
        .with_context(|| format!("spawning `{line}`"))?;
    if !status.success() {
        bail!("`{line}` failed ({status})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The walk-up search finds the real workspace root from a nested
    /// directory below it: the same detection `salvor build` relies on and
    /// that `salvor serve --dev` reuses unchanged (see `commands::serve`).
    /// Driven through `find_repo_root_from` rather than `find_repo_root`, so
    /// this never touches the test process's actual current directory (a
    /// parallel test run cannot safely race on that shared, global state).
    #[test]
    fn finds_the_root_from_a_nested_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/salvor-cli\"]\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("bridge")).unwrap();
        let nested = root.join("crates").join("salvor-cli").join("src");
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_repo_root_from(&nested);

        // No canonicalization here: the walk-up returns the ancestor path as
        // given, not a symlink-resolved one (macOS's temp directory is
        // itself a symlink, `/var` -> `/private/var`, which would otherwise
        // make this comparison flaky on exactly that platform).
        assert_eq!(found.unwrap(), root.to_path_buf());
    }

    /// Without a `bridge/` directory alongside the workspace `Cargo.toml`,
    /// the search fails honestly rather than reporting a checkout that
    /// cannot actually serve the dashboard or run `ng serve`.
    #[test]
    fn refuses_a_workspace_with_no_bridge_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/salvor-cli\"]\n",
        )
        .unwrap();

        let found = find_repo_root_from(root);

        assert!(found.is_err());
    }

    /// A directory with neither a workspace `Cargo.toml` nor a `bridge/`
    /// tree anywhere above it (a fresh, empty temp directory stands in for
    /// "outside any salvor checkout") fails rather than looping forever or
    /// finding some unrelated ancestor.
    #[test]
    fn refuses_a_directory_outside_any_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let found = find_repo_root_from(dir.path());
        assert!(found.is_err());
    }
}

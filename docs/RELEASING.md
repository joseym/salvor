# Releasing Salvor

This document describes how Salvor is distributed to end users and how a release
is cut. The distribution pipeline is built on [dist](https://opensource.axo.dev/cargo-dist/)
(the tool formerly known as cargo-dist). It is set up but dormant: the release
workflow exists in the repository and fires only when a version tag is pushed.
Nothing is published, tagged, or built for release until that first tag lands
with the prerequisites below in place.

## What end users get

The pipeline ships a single prebuilt `salvor` binary per platform, plus
installers that download the right one. None of these require a Rust toolchain
on the user's machine. The Rust compiler is only ever needed inside CI.

Once the first release is published, the install commands are:

- Homebrew (macOS and Linux):

  ```sh
  brew install joseym/tap/salvor
  ```

- Shell one-liner (macOS and Linux):

  ```sh
  curl --proto '=https' --tlsv1.2 -LsSf https://github.com/joseym/salvor/releases/latest/download/salvor-cli-installer.sh | sh
  ```

- PowerShell one-liner (Windows):

  ```powershell
  powershell -ExecutionPolicy Bypass -c "irm https://github.com/joseym/salvor/releases/latest/download/salvor-cli-installer.ps1 | iex"
  ```

- npm (global install of the binary):

  ```sh
  npm install -g @salvor-run/cli
  ```

Rust users who want to build from source can still use cargo. This is the one
path that needs a toolchain:

```sh
cargo install salvor-cli --no-default-features --features ui
```

`--no-default-features` matters: it drops the `fixture` feature and its
test-only binaries, so cargo builds only the real `salvor` binary. Naming
`ui` back in matters just as much: without it the binary serves the API and
answers `/` with a note saying it has no dashboard. A cargo install cannot
run the npm build, so it embeds whatever `bridge/dist` holds, which is empty
unless you built the Bridge in that checkout first. See the
binary note at the end.

## The binary that ships

The released artifact is exactly one binary, `salvor`, built from the
`salvor-cli` crate with default features off.

`salvor-cli` has `default = ["fixture"]`, and the `fixture` feature pulls in
three test-and-demo-only binaries through `required-features`
(`salvor-mcp-count-fixture`, `salvor-demo-research`, `salvor-demo-model`). The
release build turns default features off, so cargo never compiles those three
and only `salvor` is produced. The binary stays fully functional with the
feature off: MCP-client support and the WebAssembly sandbox arrive through the
unconditional library dependencies (`salvor-tools` and `salvor-wasm`), not
through the cli crate's own features.

The binary is self-contained and has no system-library dependencies:

- SQLite is compiled in (rusqlite `bundled`), so there is no libsqlite to find.
- TLS is rustls (reqwest `rustls-tls`), so there is no system OpenSSL to link.
- The wasmtime runtime and the rmcp SDK are statically included.

Because wasmtime and rmcp are linked in statically, the binary is large (tens of
megabytes). That size buys the no-dependency property: a user downloads one file
and runs it, with nothing else to install.

## How a release is cut

Versioning uses [cocogitto](https://docs.cocogitto.io). `cog` owns the version
and the tag; `dist` owns the build and publish. The two meet at the tag: `cog
bump` writes a `v`-prefixed tag (`cog.toml` sets `tag_prefix = "v"`), and the
release workflow triggers on that tag pattern.

To cut a release:

1. Bump and tag:

   ```sh
   cog bump <version>     # e.g. cog bump 0.1.0
   ```

   This runs the pre-bump hooks from `cog.toml`: `scripts/set-version.sh` stamps
   the version into the root `Cargo.toml` `[workspace.package]` (every crate
   inherits it) and refreshes `Cargo.lock`, then `cargo build` proves the bumped
   workspace still compiles. `cog` then writes the version commit and the tag
   `v<version>`. It does not push; that is the next step.

2. Push the tag:

   ```sh
   git push --follow-tags
   ```

3. The push of a `v*` tag triggers `.github/workflows/release.yml`. The workflow:
   - builds the `salvor` binary for every target,
   - assembles the archives, the shell/PowerShell/npm/Homebrew installers, and
     the SHA-256 checksums,
   - creates the GitHub Release and attaches all of it,
   - pushes the Homebrew formula to the tap repo,
   - publishes the npm installer package to the registry.

Rust is required only in CI; end users never need it installed.

The release workflow is separate from `ci.yml`. `ci.yml` runs the test, clippy,
and format gates on pushes and pull requests to `main`; it is untouched by this
setup. The release workflow does not run on pushes or pull requests at all. It
runs only on a pushed version tag.

## Prerequisites for the first real release

The pipeline is dormant now. The following must exist before the first tag is
pushed. They are grouped by which installer each one unlocks, so you can set up a
subset and ship a subset. The GitHub repository is required by every
installer; each installer above it is independent.

**GitHub repository (required for every installer):**

- The repository `joseym/salvor` exists and the code is pushed to it.
- GitHub Actions is enabled. The built-in `GITHUB_TOKEN` (the workflow requests
  `contents: write`) is enough to create the Release and upload the artifacts.

With only this in place, the GitHub Release, its per-platform archives and
checksums, the shell installer (`curl | sh`), and the PowerShell installer all
work. These need no extra secrets.

**Homebrew tap (required for `brew install`):**

- A repository `joseym/homebrew-tap` exists (public).
- A secret named `HOMEBREW_TAP_TOKEN` is set on the `joseym/salvor` repository.
  It is a GitHub personal access token with write access to
  `joseym/homebrew-tap`, because the release job pushes the generated formula
  across repositories and the default `GITHUB_TOKEN` cannot write to another
  repo. This is the token the Homebrew publish job reads.

With the tap repo and this token, `brew install joseym/tap/salvor` works. The
formula file is `salvor.rb` and is pushed to the tap by the workflow.

**npm (required for `npm install -g @salvor-run/cli`):**

- The npm package name is `@salvor-run/cli`, scoped to match `@salvor-run/client`
  rather than a personal account. The unscoped name `salvor` was published
  briefly while this was still being decided (it's real, v0.5.1, and will keep
  working for anyone who already installed it) but is not the intended
  long-term package; new installs and all docs point at the scoped name.
- A secret named `NPM_TOKEN` is set on the `joseym/salvor` repository. It is an
  npm automation token with publish rights for the `@salvor-run` org. The
  publish job reads it as `NODE_AUTH_TOKEN` and runs `npm publish --access public`,
  which is required for a scoped package to publish as public rather than
  defaulting to private.

With the token in place, `npm install -g @salvor-run/cli` works.

**crates.io** publishing runs from `.github/workflows/crates.yml` on the same
tag, using trusted publishing (no stored token). It calls
`scripts/publish-crates.sh`, which walks the family bottom-up and skips any
version already on the index, so an interrupted run resumes safely and the same
script works by hand: `scripts/publish-crates.sh --dry-run` packages everything
without uploading. Each crate needs a trusted publisher registered at
crates.io/crates/<name>/settings before CI can publish it.

## Current state, and what the first tag activates

What exists today:

- `dist-workspace.toml` holds the dist configuration.
- `.github/workflows/release.yml` is generated and committed, and triggers only
  on a `v*` tag.
- Per-package dist metadata in `crates/salvor-cli/Cargo.toml` (default features
  off) and `crates/salvor-tools/Cargo.toml` (excluded from distribution).

What does not exist yet, and comes into being only when the first tag is pushed
with the prerequisites in place:

- No GitHub Release, archives, or checksums exist until a tag build runs.
- No Homebrew formula lives in a tap until the tap repo and token exist and a
  tag build pushes it.
- No npm package is published until the npm token exists and a tag build
  publishes it.

Until then the machinery stays inert: editing this config or the workflow
changes nothing user-visible until a `v*` tag is pushed.

To regenerate the workflow after editing `dist-workspace.toml`, run `dist
generate`. To preview what a release would produce without building or
publishing, run `dist plan`.

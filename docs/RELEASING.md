# Releasing Salvor

This document describes how Salvor is distributed to end users and how a release
is cut. The distribution pipeline is built on [dist](https://opensource.axo.dev/cargo-dist/)
(the tool formerly known as cargo-dist), paired with hand-written GitHub
workflows that fire on the same tag. Four releases have shipped through it so
far: v0.5.0, v0.5.1, v0.5.2, and the current v0.5.3. Pushing a `v*` tag builds
the `salvor` binary for every target, publishes the crate family to crates.io,
the Python SDK to PyPI, the TypeScript SDK and the CLI installer to npm, and
creates the GitHub Release with the binaries attached. Two channels are not
live yet, the Homebrew tap and the container image; see "Prerequisites for
each release channel" below for what each is missing.

## What end users get

The pipeline ships a single prebuilt `salvor` binary per platform, plus
installers that download the right one. None of these require a Rust toolchain
on the user's machine. The Rust compiler is only ever needed inside CI.

The install commands are:

- Homebrew (macOS and Linux). Not live yet; see "Homebrew tap" under
  Prerequisites for what's missing:

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

However it was installed, the binary carries its own shell completion and needs
no extra artifact from the pipeline: `salvor completions <shell>` prints a
static script, and `eval "$(COMPLETE=zsh salvor)"` in an rc file adds dynamic
completion of run ids and agent identities from the user's own store. Neither
is packaged or shipped separately — the binary generates both at runtime — so a
release has nothing to do for either. The README's "Shell completion" section is
the user-facing instructions.

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
   cog bump <version>     # e.g. cog bump 0.5.4
   ```

   This runs the pre-bump hooks from `cog.toml`. `scripts/set-version.sh` stamps
   the version everywhere it has to live: the root `Cargo.toml`
   `[workspace.package]` (every crate inherits it via `version.workspace =
   true`), the caret requirements on the internal `salvor-*` entries in
   `[workspace.dependencies]`, the `=`-pinned sibling versions in the `salvor`
   facade crate's `Cargo.toml`, the same sibling requirements in the crates that
   depend on a sibling directly instead of through the workspace (`salvor-cli`,
   `salvor-wasm`, `salvor-replay-wasm`), the version fields in
   `sdks/typescript/package.json` and `package-lock.json`, the version in
   `sdks/python/pyproject.toml`, and the `salvor_version` field in
   `docs/cli-manifest.json`. It then runs `cargo update --workspace` to refresh
   `Cargo.lock`. `cargo build`, the second pre-bump hook, proves the bumped
   workspace still compiles. `cog` then writes the version commit and the tag
   `v<version>`. It does not push; that is the next step.

2. Push the tag:

   ```sh
   git push --follow-tags
   ```

3. The push of a `v*` tag triggers four workflows, all matching the same tag
   pattern:
   - `.github/workflows/release.yml`, generated by `dist generate`, builds the
     `salvor` binary for every target, assembles the archives, the
     shell/PowerShell/npm installers, and the SHA-256 checksums, creates the
     GitHub Release and attaches all of it, publishes `@salvor-run/cli` to the
     npm registry, and builds the Homebrew formula and attaches it to the
     release. It does not push the formula to the tap; `"homebrew"` is left out
     of `publish-jobs` in `dist-workspace.toml` until `HOMEBREW_TAP_TOKEN`
     exists (see "Homebrew tap" under Prerequisites).
   - `.github/workflows/crates.yml` publishes the `salvor-*` crate family to
     crates.io through `scripts/publish-crates.sh`.
   - `.github/workflows/pypi.yml` publishes the Python SDK to PyPI.
   - `.github/workflows/npm-client.yml` publishes the TypeScript SDK,
     `@salvor-run/client`, to the npm registry. This is a different package
     from the CLI installer that `release.yml` publishes.

Rust is required only in CI; end users never need it installed.

A container image is a separate, parallel pipeline: `.github/workflows/docker.yml`
builds `ghcr.io/joseym/salvor` on pushes to `main` that touch the workspace or
the Dockerfile (proof it still builds, skipped for commits that cannot affect
the image) and publishes it for `linux/amd64` and `linux/arm64` on the same
`v*` tag this section describes, whatever that tag touched. A stable tag
publishes the version and moves `latest`; a
prerelease tag publishes only its own version, so `latest` never points at a
release candidate. It shares nothing with `dist` or the workflow above —
different job, different artifact, different registry. It has not published
an image yet; see "Container image" under Prerequisites for what the next tag
does. See [CONTAINER.md](CONTAINER.md) for how to run the image.

The release workflow is separate from `ci.yml`. `ci.yml` runs the test, clippy,
and format gates on pushes and pull requests to `main`; it is untouched by this
setup. The release workflow does not run on pushes or pull requests at all. It
runs only on a pushed version tag.

## Prerequisites for each release channel

Every channel below has run at least once except two: the Homebrew tap and the
container image. They are grouped by which installer or registry each one
serves. The GitHub repository underlies all of them; the others are
independent of each other.

**GitHub repository (satisfied):**

- The repository `joseym/salvor` exists, is public, and the code is pushed to
  it.
- GitHub Actions is enabled. The built-in `GITHUB_TOKEN` (the workflow requests
  `contents: write`) is enough to create the Release and upload the artifacts.

The GitHub Release, its per-platform archives and checksums, the shell
installer (`curl | sh`), and the PowerShell installer all work today. These
need no extra secrets.

**Homebrew tap (not live yet):**

- The repository `joseym/homebrew-tap` exists.
- A secret named `HOMEBREW_TAP_TOKEN` is not set on the `joseym/salvor`
  repository. It would need to be a GitHub personal access token with write
  access to `joseym/homebrew-tap`, because the release job would push the
  generated formula across repositories and the default `GITHUB_TOKEN` cannot
  write to another repo.
- `"homebrew"` is not in `publish-jobs` in `dist-workspace.toml`, so even with
  the token set, the formula is only built and attached to the GitHub Release;
  nothing pushes it to the tap.

Until both the token and the `publish-jobs` entry exist, `brew install
joseym/tap/salvor` does not work. Add `"homebrew"` back to `publish-jobs`
alongside the secret, then run `dist generate` to regenerate `release.yml`.

**npm (satisfied, both packages):**

- The npm package name for the CLI installer is `@salvor-run/cli`, scoped to
  match `@salvor-run/client` rather than a personal account. The unscoped name
  `salvor` was published briefly while this was still being decided (it's
  real, v0.5.1, and keeps working for anyone who already installed it) but is
  not the intended long-term package; new installs and all docs point at the
  scoped name.
- The npm package name for the TypeScript SDK is `@salvor-run/client`,
  published separately by `.github/workflows/npm-client.yml`.
- A secret named `NPM_TOKEN` is set on the `joseym/salvor` repository. It is an
  npm automation token with publish rights for the `@salvor-run` org. Both the
  `release.yml` npm job and `npm-client.yml` read it into `NODE_AUTH_TOKEN`
  and run `npm publish --access public`, which is required for a scoped
  package to publish as public rather than defaulting to private.

Both `npm install -g @salvor-run/cli` and depending on `@salvor-run/client` as
a library work today.

**crates.io (satisfied).** Publishing runs from `.github/workflows/crates.yml`
on the same tag, using trusted publishing (no stored token). It calls
`scripts/publish-crates.sh`, which walks the family bottom-up and skips any
version already on the index, so an interrupted run resumes safely and the same
script works by hand: `scripts/publish-crates.sh --dry-run` packages everything
without uploading. Each crate needs a trusted publisher registered at
crates.io/crates/<name>/settings before CI can publish it. The family is live
today at 0.5.3, including `salvor`, `salvor-cli`, `salvor-core`, and
`salvor-runtime`.

**PyPI (satisfied).** Publishing runs from `.github/workflows/pypi.yml` on the
same tag, also using trusted publishing (no stored token). It tests the Python
SDK, builds the sdist and wheel from `sdks/python`, checks the built version
against the tag, and uploads with `skip-existing: true` so a re-run of the
same tag is safe. `salvor` on PyPI is live today at 0.5.3.

**Container image (not live yet):**

- `.github/workflows/docker.yml` was added after v0.5.3 shipped and has not run
  yet. On a push to `main` it proves the image still builds, without publishing
  anything; `ghcr.io/joseym/salvor` gets its first published image on whichever
  tag ships next.
- It uses the repository's built-in `GITHUB_TOKEN` (granted `packages:
  write`); no separate registry credential is needed, so nothing else has to
  be set up before that first tag.

See [CONTAINER.md](CONTAINER.md) for how to run the image once it's
published.

## Current state, as of v0.5.3

Live today:

- `dist-workspace.toml` holds the dist configuration; per-package dist
  metadata lives in `crates/salvor-cli/Cargo.toml` (default features off) and
  `crates/salvor-tools/Cargo.toml` (excluded from distribution).
- `v0.5.0` through `v0.5.3` have each shipped a GitHub Release with archives,
  checksums, and the shell and PowerShell installers.
- `@salvor-run/cli` (the CLI installer, `npm install -g @salvor-run/cli`) and
  the legacy unscoped `salvor` package (v0.5.1 only) are both on the npm
  registry.
- `@salvor-run/client`, the TypeScript SDK, is on the npm registry.
- The `salvor-*` crate family, including `salvor`, `salvor-cli`,
  `salvor-core`, and `salvor-runtime`, is on crates.io.
- `salvor`, the Python SDK, is on PyPI.
- `dist-workspace.toml` now builds seven targets: `aarch64-apple-darwin`,
  `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`,
  `x86_64-apple-darwin`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`,
  and `x86_64-unknown-linux-musl`. The two musl targets were added after
  v0.5.3 shipped; the next tag is their first release.

Not live yet:

- No Homebrew formula lives in the tap. `publish-jobs` in
  `dist-workspace.toml` omits `"homebrew"`, and `HOMEBREW_TAP_TOKEN` is not
  set, so `brew install joseym/tap/salvor` does not work. See "Homebrew tap"
  above.
- No container image has been published. `.github/workflows/docker.yml` was
  added after v0.5.3 and has never run: its first branch build proves the
  Dockerfile compiles, and the next `v*` tag is its first publish to
  `ghcr.io/joseym/salvor`. See "Container image" above and
  [CONTAINER.md](CONTAINER.md).

To regenerate `release.yml` after editing `dist-workspace.toml`, run `dist
generate`. To preview what a release would produce without building or
publishing, run `dist plan`.

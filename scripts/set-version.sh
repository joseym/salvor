#!/usr/bin/env bash
#
# set-version.sh <version>: stamp a new version across the workspace.
#
# Salvor declares its version exactly once: the [workspace.package] version in
# the root Cargo.toml, which every crate inherits via `version.workspace = true`.
# The in-workspace path dependencies DO carry a version requirement (a caret
# range, e.g. `version = "0.9.0"`), because crates.io refuses to publish a path
# dependency without one: once a crate is published, its `path = "../..."` is
# stripped and only that version requirement resolves the sibling. So this
# script rewrites the single top-level `version = "..."` line (the sole one at
# column zero), then makes a second pass over every crates/*/Cargo.toml
# rewriting the version on any line that also carries `path = "../salvor` -
# an internal sibling dependency, never a third-party one (serde, tokio, ...),
# which are left untouched. Finally it runs `cargo update --workspace` so
# Cargo.lock matches.
#
# Idempotent: stamping the current version rewrites the same bytes and leaves no
# diff. Called by cocogitto's pre_bump_hooks (see cog.toml).

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: set-version.sh <version>

Set the workspace version (e.g. 0.2.1 or 0.3.0-rc.1) in the root Cargo.toml's
[workspace.package], then refresh Cargo.lock.
EOF
}

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
esac

if [ "$#" -ne 1 ]; then
  usage >&2
  exit 2
fi

VERSION=$1

# major.minor.patch with an optional -prerelease and/or +build suffix.
if ! printf '%s' "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
  echo "set-version.sh: not a semver version: '$VERSION'" >&2
  exit 2
fi

ROOT=$(cd "$(dirname "$0")/.." && pwd)

# The floor a caret requirement actually needs is major.minor.0: everything
# from there up to the next minor is already compatible, so a patch release
# (0.5.2 -> 0.5.3) must leave the floor alone, and only a minor or major
# release moves it. Stamping the exact patch instead would ratchet the floor
# on every release and make the script non-idempotent against the committed
# tree between minor bumps.
FLOOR=$(printf '%s' "$VERSION" | sed -E 's/^([0-9]+\.[0-9]+)\..*/\1.0/')

MANIFEST="$ROOT/Cargo.toml"
tmp="$MANIFEST.tmp.$$"

# Clean up the scratch file if the rewrite fails partway through.
trap 'rm -f "$tmp"' EXIT

# The [workspace.package] version is the only `version = ` that starts at
# column zero, so `^version` never matches a dependency's.
#
# The internal salvor-* entries in [workspace.dependencies] also carry a caret
# version requirement (its floor, per the FLOOR comment above), which is what
# gets published to crates.io as each crate's dependency requirement on its
# siblings. Left at the old floor, a minor bump (0.5.x -> 0.6.0) falls outside
# the caret range and `cargo update` fails to resolve the workspace; a stale
# published crate would also refuse a same-family sibling it was actually
# built against. The pattern is anchored on `^salvor-` so it only ever touches
# those lines, never a third-party dependency's `version = "..."`.
sed -E \
  -e "s/^version = \"[0-9][^\"]*\"/version = \"$VERSION\"/" \
  -e "s/^(salvor-[A-Za-z0-9_-]+ = \{ path = \"[^\"]*\", version = \")[0-9][^\"]*(\")/\1$FLOOR\2/" \
  "$MANIFEST" >"$tmp"
mv "$tmp" "$MANIFEST"

# The umbrella facade pins each sibling to `="<this version>"`, so the family it fronts can never
# be mixed with a version it was not tested against. Those pins are literal strings, so they have to
# be stamped here too or the next bump leaves them pointing at a version that no longer exists.
FACADE="$ROOT/crates/salvor/Cargo.toml"
if [ -f "$FACADE" ]; then
  ftmp="$FACADE.tmp.$$"
  trap 'rm -f "$tmp" "$ftmp"' EXIT
  sed -E "s/version = \"=[0-9][^\"]*\"/version = \"=$VERSION\"/g" "$FACADE" >"$ftmp"
  mv "$ftmp" "$FACADE"
  echo "stamped the facade's sibling pins"
fi

# Several crates depend on an in-workspace sibling directly (not via
# `workspace = true`), most often so they can turn off its default features,
# which Cargo forbids overriding on a workspace-inherited dependency. Each of
# those direct dependencies still carries the same caret version requirement
# and the same publish-time consequences as the workspace.dependencies table
# above, so every crates/*/Cargo.toml gets the same rewrite here. The pattern
# is anchored on the `path = "../salvor` substring, which never appears in a
# third-party dependency's line, so a serde/tokio version is never touched.
# Stamped to the exact new version (not the floor): unlike the shared
# workspace.dependencies table, these are hand-listed per crate, so leaving
# them at a stale floor across patch releases is how the eight of them went
# stale in the first place.
for member_manifest in "$ROOT"/crates/*/Cargo.toml; do
  [ -f "$member_manifest" ] || continue
  mtmp="$member_manifest.tmp.$$"
  trap 'rm -f "$tmp" "$ftmp" "$mtmp"' EXIT
  sed -E "s#(path = \"\.\./salvor[^\"]*\", version = \")[0-9][^\"]*(\")#\1$VERSION\2#" "$member_manifest" >"$mtmp"
  mv "$mtmp" "$member_manifest"
done
echo "stamped the direct sibling dependency requirements"

# The SDKs ship the same protocol as the crates and are versioned with them, but they are not Cargo
# packages, so nothing else here would move them. Left unstamped they silently drift, which is how
# they ended up two minor versions behind the family.
for sdk_manifest in \
  "$ROOT/sdks/typescript/package.json" \
  "$ROOT/sdks/typescript/package-lock.json"; do
  [ -f "$sdk_manifest" ] || continue
  stmp="$sdk_manifest.tmp.$$"
  # Only the top-level "version" key, which sits at two spaces of indent; a dependency's version is
  # nested deeper and must not move.
  sed -E "s/^  \"version\": \"[0-9][^\"]*\"/  \"version\": \"$VERSION\"/" "$sdk_manifest" >"$stmp"
  mv "$stmp" "$sdk_manifest"
done

PYPROJECT="$ROOT/sdks/python/pyproject.toml"
if [ -f "$PYPROJECT" ]; then
  ptmp="$PYPROJECT.tmp.$$"
  sed -E "s/^version = \"[0-9][^\"]*\"/version = \"$VERSION\"/" "$PYPROJECT" >"$ptmp"
  mv "$ptmp" "$PYPROJECT"
fi
echo "stamped the SDK manifests"

# The CLI manifest is a checked-in description of the CLI surface whose salvor_version field is
# generated from the Rust env!("CARGO_PKG_VERSION") at build time; it must stay in sync or the
# cli_manifest test will fail. Only the top-level "salvor_version" key (indented two spaces) is
# rewritten; version strings in other parts of the JSON (e.g. in help text) are not touched.
MANIFEST_JSON="$ROOT/docs/cli-manifest.json"
if [ -f "$MANIFEST_JSON" ]; then
  jtmp="$MANIFEST_JSON.tmp.$$"
  trap 'rm -f "$tmp" "$ftmp" "$jtmp"' EXIT
  sed -E "s/^  \"salvor_version\": \"[0-9][^\"]*\"/  \"salvor_version\": \"$VERSION\"/" "$MANIFEST_JSON" >"$jtmp"
  mv "$jtmp" "$MANIFEST_JSON"
fi

# Refresh the lockfile's workspace-member entries to the new version.
( cd "$ROOT" && cargo update --workspace )

echo "set workspace version to $VERSION"

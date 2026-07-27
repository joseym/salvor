#!/usr/bin/env bash
#
# set-version.sh <version>: stamp a new version across the workspace.
#
# Salvor declares its version exactly once: the [workspace.package] version in
# the root Cargo.toml, which every crate inherits via `version.workspace = true`.
# The in-workspace path dependencies carry no version requirement, so this
# script rewrites only that single `version = "..."` line (the sole one at
# column zero; every dependency version sits inside an inline table or after a
# `name = ` key). Third-party dependency versions (serde, tokio, ...) are left
# untouched. Finally it runs `cargo update --workspace` so Cargo.lock matches.
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

MANIFEST="$ROOT/Cargo.toml"
tmp="$MANIFEST.tmp.$$"

# Clean up the scratch file if the rewrite fails partway through.
trap 'rm -f "$tmp"' EXIT

# The [workspace.package] version is the only `version = ` that starts at
# column zero, so `^version` never matches a dependency's.
sed -E \
  -e "s/^version = \"[0-9][^\"]*\"/version = \"$VERSION\"/" \
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

# Refresh the lockfile's workspace-member entries to the new version.
( cd "$ROOT" && cargo update --workspace )

echo "set workspace version to $VERSION"

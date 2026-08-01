#!/usr/bin/env python3
"""release-facts.py: generate the derivable half of docs/RELEASING.md.

Everything this script emits is a fact the repository already states somewhere
else: the shipped versions are git tags, the tag-triggered workflows are the
`on:` blocks in .github/workflows, the binary targets and the publish channels
are keys in dist-workspace.toml, and the crate publish order is the ORDER array
in scripts/publish-crates.sh. Restating any of that by hand in prose is how the
document went stale, so the prose no longer states it and this does.

It writes into docs/RELEASING.md between two markers and touches nothing else in
the file. The reasoning around the facts (why the pipeline is shaped this way,
the trusted-publisher bootstrap, what an operator should do) stays outside the
markers, hand-written.

Every input is a file in this checkout or a git tag. Nothing is fetched from
crates.io, npm, PyPI or a container registry: a gate that needs the network
flakes, and a flaky gate gets turned off. That also means facts about what a
registry currently holds are not derivable here and stay prose.

Usage:
  scripts/release-facts.py            # check: fail if the checked-in block drifted
  scripts/release-facts.py --write    # regenerate the block in place

Both modes render through the same `render_block`, so the checked-in copy and
the assertion can never disagree about the format.

Python rather than shell because the inputs are TOML, YAML and git plumbing;
parsing those with sed would be the same hand-maintenance problem one level
down. Only the standard library is used, so CI needs nothing installed.
"""

from __future__ import annotations

import re
import subprocess
import sys
import textwrap
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RELEASING_MD = REPO_ROOT / "docs" / "RELEASING.md"
DIST_WORKSPACE = REPO_ROOT / "dist-workspace.toml"
PUBLISH_CRATES = REPO_ROOT / "scripts" / "publish-crates.sh"
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"

BEGIN_SENTINEL = "BEGIN GENERATED RELEASE FACTS"
END_SENTINEL = "END GENERATED RELEASE FACTS"

REGENERATE_COMMAND = "scripts/release-facts.py --write"

BEGIN_MARKER = f"""<!-- {BEGIN_SENTINEL}. Everything down to the closing marker is
     generated from this repository by scripts/release-facts.py. Do not edit it
     by hand: change the tag, workflow, dist-workspace.toml or publish-crates.sh
     it is derived from, then run `{REGENERATE_COMMAND}` and commit
     the result. CI fails when this block and the repository disagree. -->"""

END_MARKER = f"<!-- {END_SENTINEL} -->"

# dist can push these two installers to a registry of their own, so for them
# "is it in publish-jobs" is a real question. The shell and PowerShell
# installers have no registry: they are files attached to the GitHub Release,
# and the Release itself is the publish.
REGISTRY_INSTALLERS = ("homebrew", "npm")

# A tag has to look like this to count as a shipped version. Anything else
# under `v*` is refused rather than skipped, so a release can never go missing
# from the table because its tag was spelled unusually.
VERSION_TAG = re.compile(r"^v(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$")


class DerivationError(Exception):
    """An input could not be read as the shape this script expects."""


# --------------------------------------------------------------------------
# git
# --------------------------------------------------------------------------


def git(*args: str) -> str:
    """Run a read-only git command in the repo and return its stdout."""
    result = subprocess.run(
        ["git", "-C", str(REPO_ROOT), *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise DerivationError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def version_sort_key(tag: str) -> tuple:
    """Order tags by semver, with a prerelease sorting below its own release.

    Not `git tag --sort=v:refname`: that sorts v1.0.0-rc.1 above v1.0.0, which
    would name a release candidate as the current version.
    """
    match = VERSION_TAG.match(tag)
    if match is None:
        raise DerivationError(
            f"tag {tag!r} matches `v*` but is not a version: expected v<major>.<minor>.<patch>"
        )
    major, minor, patch, prerelease = match.groups()
    if prerelease is None:
        # A release outranks every prerelease of the same numbers.
        return (int(major), int(minor), int(patch), 1, ())
    # Numeric prerelease identifiers compare numerically and rank below
    # alphanumeric ones, per semver.
    parts = tuple(
        (0, int(part), "") if part.isdigit() else (1, 0, part) for part in prerelease.split(".")
    )
    return (int(major), int(minor), int(patch), 0, parts)


def shipped_versions() -> list[str]:
    """Every `v*` tag in this checkout, oldest first."""
    tags = [line.strip() for line in git("tag", "--list", "v*").splitlines() if line.strip()]
    if not tags:
        raise DerivationError(
            "no `v*` tags in this checkout, so no release history can be derived. "
            "A shallow or tagless clone causes this: run `git fetch --tags`, and in CI "
            "check out with `fetch-depth: 0`."
        )
    return sorted(tags, key=version_sort_key)


def workspace_version() -> str:
    """The version from [workspace.package] in the root Cargo.toml.

    The bump commit stamps this via scripts/set-version.sh before the tag exists,
    so check mode uses this to validate one-ahead state.
    """
    root_cargo = REPO_ROOT / "Cargo.toml"
    config = tomllib.loads(root_cargo.read_text())
    workspace = config.get("workspace", {})
    package = workspace.get("package", {})
    version = package.get("version")
    if not version:
        raise DerivationError("root Cargo.toml has no [workspace.package] version field")
    # Ensure it has the v prefix for comparison
    if not version.startswith("v"):
        version = f"v{version}"
    return version


def first_version_containing(path: Path, versions: list[str]) -> str | None:
    """The earliest shipped version whose history holds `path`'s first commit.

    Answers "which release first ran this workflow" without asking any registry
    what it currently holds. Returns None for a workflow added since the last
    tag.
    """
    relative = path.relative_to(REPO_ROOT).as_posix()
    commits = git("log", "--diff-filter=A", "--format=%H", "--", relative).split()
    if not commits:
        raise DerivationError(
            f"no commit adds {relative}; a shallow clone causes this, "
            "so check out with `fetch-depth: 0`."
        )
    # `git log` is newest first, and a path can be added more than once if it
    # was ever deleted and restored. The oldest such commit is the one every
    # later tag containing the file descends from.
    added = commits[-1]
    for version in versions:
        result = subprocess.run(
            ["git", "-C", str(REPO_ROOT), "merge-base", "--is-ancestor", added, version],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0:
            return version
    return None


# --------------------------------------------------------------------------
# The `on:` block of a workflow
# --------------------------------------------------------------------------


def _unquote(text: str) -> str:
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "\"'":
        return text[1:-1]
    return text


def _scalar_or_flow(text: str):
    """A quoted or bare scalar, or an inline `[a, b]` sequence."""
    if text.startswith("[") and text.endswith("]"):
        inner = text[1:-1].strip()
        if not inner:
            return []
        return [_unquote(part.strip()) for part in inner.split(",")]
    return _unquote(text)


class _Lines:
    """Significant lines of a YAML region, as (indent, text) with a cursor."""

    def __init__(self, raw_lines: list[str]) -> None:
        self.items: list[tuple[int, str]] = []
        for raw in raw_lines:
            stripped = raw.strip()
            if not stripped or stripped.startswith("#"):
                continue
            self.items.append((len(raw) - len(raw.lstrip(" ")), stripped))
        self.pos = 0

    def peek(self) -> tuple[int, str] | None:
        return self.items[self.pos] if self.pos < len(self.items) else None


def _parse_node(lines: _Lines, indent: int):
    head = lines.peek()
    if head is not None and head[1].startswith("-"):
        return _parse_sequence(lines, indent)
    return _parse_mapping(lines, indent)


def _parse_sequence(lines: _Lines, indent: int) -> list:
    result = []
    while True:
        head = lines.peek()
        if head is None or head[0] != indent or not head[1].startswith("-"):
            return result
        text = head[1][1:].strip()
        lines.pos += 1
        if not text:
            raise DerivationError("a bare `-` with a nested block is more YAML than this parses")
        if ": " in text or text.endswith(":"):
            raise DerivationError(f"a mapping inside a sequence is more YAML than this parses: {text!r}")
        result.append(_scalar_or_flow(text))


def _parse_mapping(lines: _Lines, indent: int) -> dict:
    result: dict = {}
    while True:
        head = lines.peek()
        if head is None or head[0] != indent or head[1].startswith("-"):
            return result
        key, separator, rest = head[1].partition(":")
        if not separator:
            raise DerivationError(f"expected `key: value`, got {head[1]!r}")
        key = _unquote(key.strip())
        rest = rest.strip()
        lines.pos += 1
        if rest:
            result[key] = _scalar_or_flow(rest)
            continue
        nested = lines.peek()
        result[key] = _parse_node(lines, nested[0]) if nested and nested[0] > indent else None


def workflow_triggers(path: Path) -> dict:
    """The `on:` mapping of one workflow file.

    A purpose-built reader for the small subset of YAML these `on:` blocks use,
    so the gate depends on nothing beyond the standard library. It refuses
    anything it does not understand rather than guessing, which would let a
    workflow silently drop out of the table.
    """
    raw_lines = path.read_text().splitlines()
    start = None
    for index, line in enumerate(raw_lines):
        # YAML reads a bare `on` as a boolean, so a workflow may quote the key.
        if line.rstrip() in ("on:", '"on":', "'on':"):
            start = index
            break
    if start is None:
        raise DerivationError(f"{path.name} has no `on:` block")
    end = len(raw_lines)
    for index in range(start + 1, len(raw_lines)):
        line = raw_lines[index]
        if line and not line[0].isspace() and not line.lstrip().startswith("#"):
            end = index
            break
    try:
        parsed = _parse_mapping(_Lines(raw_lines[start:end]), 0)
    except DerivationError as error:
        raise DerivationError(f"{path.name}: {error}") from error
    return parsed.get("on") or {}


def pattern_to_regex(pattern: str) -> re.Pattern:
    """Compile one GitHub ref filter pattern.

    GitHub's filter patterns are not globs and not regexes: `*` matches any run
    of characters other than `/`, `**` matches any run at all, and `+` and `?`
    repeat the item just before them rather than standing on their own.
    """
    tokens: list[str] = []
    index = 0
    while index < len(pattern):
        char = pattern[index]
        if pattern.startswith("**", index):
            tokens.append(".*")
            index += 2
        elif char == "*":
            tokens.append("[^/]*")
            index += 1
        elif char in "+?":
            if not tokens:
                raise DerivationError(f"pattern {pattern!r} starts with {char!r}, which repeats "
                                      "the item before it")
            tokens[-1] = f"(?:{tokens[-1]}){char}"
            index += 1
        elif char == "[":
            close = pattern.find("]", index + 1)
            if close == -1:
                raise DerivationError(f"pattern {pattern!r} has an unclosed `[`")
            tokens.append(pattern[index : close + 1])
            index = close + 1
        else:
            tokens.append(re.escape(char))
            index += 1
    return re.compile("^" + "".join(tokens) + "$")


def tag_triggered_workflows(current_version: str, versions: list[str]) -> list[dict]:
    """Workflows whose push-tag patterns match the current version tag."""
    matched = []
    for path in sorted(WORKFLOW_DIR.glob("*.yml")):
        triggers = workflow_triggers(path)
        push = triggers.get("push")
        if not isinstance(push, dict):
            continue
        patterns = push.get("tags")
        if patterns is None:
            continue
        if isinstance(patterns, str):
            patterns = [patterns]
        if not any(pattern_to_regex(p).match(current_version) for p in patterns):
            continue
        matched.append(
            {
                "name": path.name,
                "patterns": list(patterns),
                "first_version": first_version_containing(path, versions),
            }
        )
    return matched


# --------------------------------------------------------------------------
# dist-workspace.toml and publish-crates.sh
# --------------------------------------------------------------------------


def dist_config() -> dict:
    config = tomllib.loads(DIST_WORKSPACE.read_text())
    dist = config.get("dist")
    if not isinstance(dist, dict):
        raise DerivationError("dist-workspace.toml has no [dist] table")
    return dist


def publish_order() -> list[str]:
    """The crate names in the ORDER array of scripts/publish-crates.sh."""
    text = PUBLISH_CRATES.read_text()
    match = re.search(r"^ORDER=\(\n(.*?)^\)$", text, re.MULTILINE | re.DOTALL)
    if match is None:
        raise DerivationError("scripts/publish-crates.sh has no `ORDER=(` array")
    crates = []
    for line in match.group(1).splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            crates.extend(line.split())
    if not crates:
        raise DerivationError("the ORDER array in scripts/publish-crates.sh is empty")
    return crates


# --------------------------------------------------------------------------
# Rendering
# --------------------------------------------------------------------------


def _code_list(values) -> str:
    return ", ".join(f"`{value}`" for value in values)


def _paragraph(text: str) -> str:
    """Wrap a sentence to the column the hand-written prose around it uses.

    Table rows are left alone: markdown gives a row nowhere to break.
    """
    return textwrap.fill(text, width=79, break_long_words=False, break_on_hyphens=False)


def render_block(override_current: str | None = None) -> str:
    versions = shipped_versions()
    current = override_current if override_current else versions[-1]
    workflows = tag_triggered_workflows(current, versions)
    dist = dist_config()
    targets = sorted(dist.get("targets") or [])
    installers = sorted(dist.get("installers") or [])
    publish_jobs = list(dist.get("publish-jobs") or [])
    crates = publish_order()
    workflow_total = len(sorted(WORKFLOW_DIR.glob("*.yml")))

    lines: list[str] = [BEGIN_MARKER, ""]

    lines += [
        "### Versions",
        "",
        _paragraph(f"{len(versions)} versions have shipped, and `{current}` is current."),
        "",
        _paragraph(_code_list(versions) + "."),
        "",
    ]

    lines += [
        "### Workflows a version tag starts",
        "",
        _paragraph(
            f"{len(workflows)} of the {workflow_total} workflows in `.github/workflows` "
            f"match the tag `{current}`."
        ),
        "",
        "| Workflow | Tag patterns | First version that ran it |",
        "| --- | --- | --- |",
    ]
    for workflow in workflows:
        first = f"`{workflow['first_version']}`" if workflow["first_version"] else "not yet tagged"
        lines.append(
            f"| `.github/workflows/{workflow['name']}` "
            f"| {_code_list(workflow['patterns'])} | {first} |"
        )
    lines.append("")

    lines += [
        "### Binary targets",
        "",
        _paragraph(
            f"`dist-workspace.toml` builds the `salvor` binary for {len(targets)} targets."
        ),
        "",
    ]
    lines += [f"- `{target}`" for target in targets]
    lines.append("")

    lines += [
        "### Installer channels",
        "",
        _paragraph(
            "`installers` in `dist-workspace.toml` decides what is built; `publish-jobs` "
            "decides what is pushed anywhere beyond the GitHub Release."
        ),
        "",
        "| Channel | What a tag does with it |",
        "| --- | --- |",
    ]
    for installer in installers:
        if installer in publish_jobs:
            state = "built, attached to the release, and published to its registry"
        elif installer in REGISTRY_INSTALLERS:
            state = (
                "built and attached to the release only; absent from `publish-jobs`, "
                "so nothing pushes it to its registry"
            )
        else:
            state = "built and attached to the release, which is its only distribution point"
        lines.append(f"| `{installer}` | {state} |")
    lines.append("")

    lines += [
        "### Crate publish order",
        "",
        _paragraph(
            f"`scripts/publish-crates.sh` publishes {len(crates)} crates in this order, "
            "bottom-up, so every crate is on the index before the crate that depends on it."
        ),
        "",
    ]
    lines += [f"{position}. `{crate}`" for position, crate in enumerate(crates, start=1)]
    lines += ["", END_MARKER]

    return "\n".join(lines)


def splice_block(document: str, block: str) -> str:
    """Replace the marked region of `document` with `block`."""
    lines = document.splitlines(keepends=True)
    begin = end = None
    for index, line in enumerate(lines):
        if BEGIN_SENTINEL in line and begin is None:
            begin = index
        elif END_SENTINEL in line:
            end = index
    if begin is None or end is None:
        raise DerivationError(
            f"{RELEASING_MD.relative_to(REPO_ROOT)} is missing the generated-region markers "
            f"({BEGIN_SENTINEL} / {END_SENTINEL})"
        )
    if end < begin:
        raise DerivationError("the generated-region markers are in the wrong order")
    return "".join(lines[:begin]) + block + "\n" + "".join(lines[end + 1 :])


def main(argv: list[str]) -> int:
    write = False
    override_current = None
    i = 0
    while i < len(argv):
        argument = argv[i]
        if argument in ("-h", "--help"):
            print(__doc__.strip())
            return 0
        if argument == "--write":
            write = True
        elif argument == "--current":
            i += 1
            if i >= len(argv):
                print(f"error: --current requires a value", file=sys.stderr)
                return 2
            override_current = argv[i]
        else:
            print(f"usage: {Path(__file__).name} [--write] [--current <version>]", file=sys.stderr)
            return 2
        i += 1

    try:
        document = RELEASING_MD.read_text()

        # In check mode (no --write, no --current), determine expected current from
        # max(latest_tag, workspace_version). This lets check pass when the bump commit
        # has stamped the workspace version but the tag doesn't exist yet.
        check_current = override_current
        if not write and not override_current:
            versions = shipped_versions()
            workspace_ver = workspace_version()
            latest_tag = versions[-1] if versions else None
            # Compute max of latest_tag and workspace_version using version ordering
            if latest_tag and version_sort_key(workspace_ver) > version_sort_key(latest_tag):
                check_current = workspace_ver
            elif latest_tag:
                check_current = latest_tag
            else:
                check_current = workspace_ver

        updated = splice_block(document, render_block(check_current or override_current))
    except DerivationError as error:
        print(f"release-facts.py: {error}", file=sys.stderr)
        return 2

    if write:
        if updated != document:
            RELEASING_MD.write_text(updated)
            print(f"release-facts.py: rewrote the generated block in {RELEASING_MD.relative_to(REPO_ROOT)}")
        else:
            print("release-facts.py: the generated block was already current")
        return 0

    if updated != document:
        print(
            "release-facts.py: the generated block in docs/RELEASING.md no longer matches the "
            "repository. Regenerate it with `" + REGENERATE_COMMAND + "` and commit the result.",
            file=sys.stderr,
        )
        return 1

    print("release-facts.py: docs/RELEASING.md matches the repository")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

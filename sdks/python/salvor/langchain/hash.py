"""Canonical JSON, content hashes, and the thread-id to run-id rule.

Everything here has to be reproducible across processes and across weeks,
because a resumed invoke re-derives the same values and compares them against
what the log recorded. So the canonical form here mirrors the one the Rust
runtime uses for ``agent_def_hash`` and ``request_hash``
(``crates/salvor-runtime/src/hash.rs``): compact JSON with object keys sorted,
hashed with SHA-256, prefixed with the algorithm that produced it.

It also mirrors, byte for byte, the TypeScript SDK's
``src/langchain/hash.ts``. That is the harder constraint and the reason this
module writes its own JSON rather than calling :func:`json.dumps` with
``sort_keys``: Python renders the float ``1.0`` as ``"1.0"`` and JavaScript
renders the same double as ``"1"``, so a run recorded by a Node process and
resumed by a Python one would derive two different request hashes and replay
nothing. :func:`json_stringify` therefore formats every number the way
ECMAScript's ``Number::toString`` does, which is what ``JSON.stringify``
uses.

Two documented seams remain, both inherited from the TypeScript file and both
harmless for the values a model request carries:

* keys are sorted by Unicode code point rather than by UTF-16 code unit, which
  differs from JavaScript only for keys holding characters above U+FFFF;
* a Python ``int`` is written in full, while JavaScript would lose precision
  past 2**53.

Nothing compares a hash computed here against one computed in Rust: the
client-performed model call is the client's own claim, and salvor stores the
string without recomputing it. What matters is that this file agrees with
itself, and with its TypeScript twin, forever.
"""

from __future__ import annotations

import hashlib
import re
from decimal import Decimal
from typing import Any, List, Tuple

__all__ = [
    "canonical_json",
    "hash_value",
    "is_uuid",
    "json_stringify",
    "run_id_for_thread",
    "sha256_hex",
]


def canonical_json(value: Any) -> str:
    """Render ``value`` as compact JSON with object keys recursively sorted.

    The sorted twin of :func:`json_stringify`, and the form every hash in this
    package is taken over. Two values that differ only in the order their keys
    were built in canonicalize to the same string, which is what lets a
    re-invoke rebuild a request from different code and still land on the
    recorded hash.
    """
    return _write(value, sort=True, seen=set())


def json_stringify(value: Any) -> str:
    """Render ``value`` as compact JSON, keys in their own order.

    Python's stand-in for JavaScript's ``JSON.stringify``: the same output for
    the same value, including the way numbers are rendered. Used where the
    TypeScript middleware uses ``JSON.stringify`` and key order is the value's
    own (a tool result being checked for an exact round trip, a system message
    whose content is a list of blocks).
    """
    return _write(value, sort=False, seen=set())


def sha256_hex(text: str) -> str:
    """Lowercase hex of the SHA-256 digest of ``text``, over its UTF-8 bytes."""
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def hash_value(value: Any) -> str:
    """The content hash of a value: ``sha256:`` plus the hex SHA-256 of its
    canonical JSON.

    The same string shape the runtime records, so a log holds one kind of hash
    however the call was performed.
    """
    return "sha256:" + sha256_hex(canonical_json(value))


_UUID = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
    re.IGNORECASE,
)


def is_uuid(text: str) -> bool:
    """Whether ``text`` already is a UUID, in which case it is used as the run id."""
    return bool(_UUID.match(text))


def run_id_for_thread(thread_id: str) -> str:
    """The salvor run id for a LangGraph ``thread_id``.

    A thread id that is already a UUID is the run id, unchanged, so an
    application that mints UUID thread ids can look a run up by the id it
    already holds. Anything else is hashed: SHA-256 of the thread id, the first
    16 bytes taken, with the version nibble set to 8 (RFC 9562's custom
    version, which is what a hash-derived id honestly is) and the variant bits
    set to the RFC's ``10``. The mapping is total, stable forever, and one-way:
    two different thread ids give two different runs, and the same thread id
    gives the same run on every machine that ever drives it, in either SDK.
    """
    if is_uuid(thread_id):
        return thread_id.lower()
    digest = bytearray(hashlib.sha256(thread_id.encode("utf-8")).digest()[:16])
    digest[6] = (digest[6] & 0x0F) | 0x80  # version 8: custom, hash-derived
    digest[8] = (digest[8] & 0x3F) | 0x80  # variant 10x: RFC 4122/9562
    hexed = digest.hex()
    return "-".join(
        (hexed[0:8], hexed[8:12], hexed[12:16], hexed[16:20], hexed[20:32])
    )


# -- the writer ---------------------------------------------------------------


def _write(node: Any, sort: bool, seen: set) -> str:
    """One value, as JSON text.

    ``seen`` holds the ids of the containers on the path from the root, so a
    value that points back at itself is refused by name rather than recursing
    until the interpreter gives up.
    """
    if node is None:
        return "null"
    if node is True:
        return "true"
    if node is False:
        return "false"
    if isinstance(node, str):
        return _string(node)
    if isinstance(node, int):
        return str(node)
    if isinstance(node, float):
        return _number(node)
    if isinstance(node, (list, tuple)):
        if id(node) in seen:
            raise ValueError("canonical_json: the value has a cycle")
        seen.add(id(node))
        try:
            return "[" + ",".join(_write(item, sort, seen) for item in node) + "]"
        finally:
            seen.discard(id(node))
    if isinstance(node, dict):
        if id(node) in seen:
            raise ValueError("canonical_json: the value has a cycle")
        seen.add(id(node))
        try:
            entries = _entries(node, sort)
            return (
                "{"
                + ",".join(
                    _string(key) + ":" + _write(item, sort, seen) for key, item in entries
                )
                + "}"
            )
        finally:
            seen.discard(id(node))
    # A value with an ISO form (a date, a datetime) canonicalizes the way
    # JavaScript's `Date.toJSON` does, so the two SDKs agree about a timestamp
    # that reached a request body. Anything else is `null`, which is what
    # `JSON.stringify` does with a function or an `undefined`.
    isoformat = getattr(node, "isoformat", None)
    if callable(isoformat):
        try:
            return _string(str(isoformat()))
        except Exception:  # pragma: no cover - an object lying about its shape
            return "null"
    return "null"


def _entries(node: dict, sort: bool) -> List[Tuple[str, Any]]:
    """A dict's writable entries: string keys, no callables, optionally sorted.

    Keys are coerced with ``str`` because that is what ``JSON.stringify`` does
    with a non-string key, and callables are dropped because that is what it
    does with a function-valued field.
    """
    pairs = [
        (str(key), item) for key, item in node.items() if not callable(item)
    ]  # type: List[Tuple[str, Any]]
    if sort:
        pairs.sort(key=lambda pair: pair[0])
    return pairs


def _string(text: str) -> str:
    """One JSON string literal, escaped the way ``JSON.stringify`` escapes.

    Built by hand rather than by :func:`json.dumps` so the escape set is
    visible and cannot drift: the two named escapes JSON has, the five
    shorthands, and ``\\uXXXX`` for everything else below a space. Characters
    above ASCII are written through, as both SDKs write them.
    """
    out = ['"']
    for char in text:
        if char == '"':
            out.append('\\"')
        elif char == "\\":
            out.append("\\\\")
        elif char == "\n":
            out.append("\\n")
        elif char == "\r":
            out.append("\\r")
        elif char == "\t":
            out.append("\\t")
        elif char == "\b":
            out.append("\\b")
        elif char == "\f":
            out.append("\\f")
        elif ord(char) < 0x20:
            out.append("\\u%04x" % ord(char))
        else:
            out.append(char)
    out.append('"')
    return "".join(out)


def _number(value: float) -> str:
    """One double, rendered the way ECMAScript's ``Number::toString`` renders it.

    Python and JavaScript both print the shortest decimal that reads back as
    the same double, so the digits agree; what does not agree is how the two
    lay those digits out. Python writes ``1.0`` where JavaScript writes ``1``,
    ``1e-07`` where JavaScript writes ``1e-7``, and ``1e+16`` where JavaScript
    writes ``10000000000000000``. This applies the ECMAScript rule to Python's
    digits so the two SDKs hash a number to the same bytes.
    """
    if value != value or value in (float("inf"), float("-inf")):
        return "null"
    if value == 0:
        return "0"  # JavaScript renders both zeros as `0`
    sign = "-" if value < 0 else ""
    digits, exponent = _shortest_digits(abs(value))
    count = len(digits)
    # `point` is where the decimal point falls: the value is `0.digits * 10**point`.
    point = exponent + count
    if count <= point <= 21:
        return sign + digits + "0" * (point - count)
    if 0 < point <= 21:
        return sign + digits[:point] + "." + digits[point:]
    if -6 < point <= 0:
        return sign + "0." + "0" * (-point) + digits
    mantissa = digits if count == 1 else digits[0] + "." + digits[1:]
    marker = "+" if point - 1 >= 0 else "-"
    return sign + mantissa + "e" + marker + str(abs(point - 1))


def _shortest_digits(value: float) -> Tuple[str, int]:
    """The shortest round-tripping digits of a positive double, and their
    exponent: ``value == int(digits) * 10 ** exponent``, with no trailing zero
    left in ``digits``."""
    _, raw, exponent = Decimal(repr(value)).as_tuple()
    digits = list(raw)  # type: List[Any]
    exponent = int(exponent)
    while len(digits) > 1 and digits[-1] == 0:
        digits.pop()
        exponent += 1
    return "".join(str(digit) for digit in digits), exponent

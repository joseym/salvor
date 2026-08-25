/**
 * Canonical JSON, content hashes, and the thread-id to run-id rule.
 *
 * Everything here has to be reproducible across processes and across weeks,
 * because a resumed invoke re-derives the same values and compares them against
 * what the log recorded. So the canonical form here mirrors the one the Rust
 * runtime uses for `agent_def_hash` and `request_hash`
 * (`crates/salvor-runtime/src/hash.rs`): compact JSON with object keys sorted,
 * hashed with SHA-256, prefixed with the algorithm that produced it.
 *
 * The one difference worth naming: keys are sorted in UTF-16 code-unit order
 * (JavaScript's own string comparison) rather than UTF-8 byte order. The two
 * agree for every key below U+10000, which is every tool name, field name and
 * message role a model request carries. Nothing compares a hash computed here
 * against one computed in Rust anyway: the client-performed model call is the
 * client's own claim, and salvor stores the string without recomputing it, so
 * what matters is that this file agrees with itself forever.
 *
 * Hashing is async because the digest is `crypto.subtle`, the one SHA-256 that
 * exists unchanged in Node 18 and in a browser tab. The SDK's browser-safety
 * rule (no Node builtin anywhere under `src/`) holds here too.
 */

/**
 * Render a value as compact JSON with object keys recursively sorted.
 *
 * `undefined` follows `JSON.stringify`: dropped from an object, rendered as
 * `null` in an array. A value with a `toJSON` method is asked for its JSON form
 * first, so `Date` and LangChain's serializable classes canonicalize the same
 * way they would serialize.
 */
export function canonicalJson(value: unknown): string {
  const seen = new Set<object>();

  function write(node: unknown): string {
    if (node === null) return "null";
    if (typeof node === "boolean") return node ? "true" : "false";
    if (typeof node === "number") {
      return Number.isFinite(node) ? JSON.stringify(node) : "null";
    }
    if (typeof node === "string") return JSON.stringify(node);
    if (typeof node === "bigint") return JSON.stringify(node.toString());
    if (typeof node !== "object") return "null";

    const object = node as { toJSON?: () => unknown };
    if (typeof object.toJSON === "function") return write(object.toJSON());

    if (seen.has(node)) {
      throw new TypeError("canonicalJson: the value has a cycle");
    }
    seen.add(node);
    try {
      if (Array.isArray(node)) {
        return `[${node.map((item) => write(item === undefined ? null : item)).join(",")}]`;
      }
      const entries = Object.entries(node as Record<string, unknown>)
        .filter(([, item]) => item !== undefined && typeof item !== "function")
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
      return `{${entries
        .map(([key, item]) => `${JSON.stringify(key)}:${write(item)}`)
        .join(",")}}`;
    } finally {
      seen.delete(node);
    }
  }

  return write(value);
}

/** Lowercase hex of the SHA-256 digest of `text`, over its UTF-8 bytes. */
export async function sha256Hex(text: string): Promise<string> {
  const bytes = await sha256Bytes(text);
  let out = "";
  for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
  return out;
}

async function sha256Bytes(text: string): Promise<Uint8Array> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(text),
  );
  return new Uint8Array(digest);
}

/**
 * The content hash of a value: `sha256:` plus the hex SHA-256 of its canonical
 * JSON. The same string shape the runtime records, so a log holds one kind of
 * hash however the call was performed.
 */
export async function hashValue(value: unknown): Promise<string> {
  return `sha256:${await sha256Hex(canonicalJson(value))}`;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Whether `text` already is a UUID, in which case it is used as the run id. */
export function isUuid(text: string): boolean {
  return UUID.test(text);
}

/**
 * The salvor run id for a LangGraph `thread_id`.
 *
 * A thread id that is already a UUID is the run id, unchanged, so an
 * application that mints UUID thread ids can look a run up by the id it already
 * holds. Anything else is hashed: SHA-256 of the thread id, the first 16 bytes
 * taken, with the version nibble set to 8 (RFC 9562's custom version, which is
 * what a hash-derived id honestly is) and the variant bits set to the RFC's
 * `10`. The mapping is total, stable forever, and one-way: two different thread
 * ids give two different runs, and the same thread id gives the same run on
 * every machine that ever drives it.
 */
export async function runIdForThread(threadId: string): Promise<string> {
  if (isUuid(threadId)) return threadId.toLowerCase();
  const bytes = (await sha256Bytes(threadId)).slice(0, 16);
  bytes[6] = (bytes[6] & 0x0f) | 0x80; // version 8: custom, hash-derived
  bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10x: RFC 4122/9562
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20, 32),
  ].join("-");
}

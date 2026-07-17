/**
 * THE ONE JSON HIGHLIGHTER for this feature (hand-rolled, no library). Every pretty-printed JSON
 * site inside the Inbox (the reconciliation card's evidence `<dd>`, the evidence panel's `<pre>`
 * blocks) routes through this, so highlighting is everywhere or nowhere within the feature.
 *
 * The Inspector feature ports the SAME algorithm independently in its own feature directory,
 * because this build keeps `features/inbox/` and `features/inspector/` from reaching into each
 * other. Two copies of one small pure function, not a shared abstraction — a candidate for later
 * consolidation into `core/` or a shared `ui/` module, not a defect in either feature.
 *
 * SAFETY:
 *   1. It never weakens escaping. Every token's text is escaped before it is wrapped in a `<span>`,
 *      so a string value containing markup renders inert. The only unwrapped text is JSON's own
 *      structural punctuation (`{}[],:` and whitespace), none of which is HTML-special.
 *   2. It never changes text content. Escaped tokens decode back to the original bytes on read, so
 *      the rendered block's `textContent` stays byte-identical to `JSON.stringify(...)` — select-
 *      and-copy yields the same JSON highlighting never touched.
 * Angular's own `[innerHTML]` sanitizer is a second, independent layer on top of this (it allows
 * `<span class="...">` — both element and attribute are on its safe allowlist — so a bug in `esc`
 * here would still be caught before it reached the DOM).
 */

const ESC_MAP: Readonly<Record<string, string>> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
};

function esc(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ESC_MAP[c] ?? c);
}

/** Matches a JSON string (a key when immediately followed by `:`), a number, or a `true`/`false`/
 * `null` literal — everything else (structural punctuation, whitespace) passes through untouched. */
const JSON_TOKEN_RE =
  /"(?:\\.|[^"\\])*"(\s*:)?|\b(?:true|false|null)\b|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/g;

/**
 * Pretty-print `value` at `indent` spaces (0/undefined for compact single-line, the mode the
 * evidence-`<dd>` call sites use) with each token wrapped in a `span.j-key` / `.j-str` /
 * `.j-num` / `.j-bool` / `.j-null`. Returns an HTML string — bind it with `[innerHTML]`, never
 * concatenate it into user-facing prose outside a dedicated pre/dd.
 */
export function jsonHi(value: unknown, indent?: number): string {
  const json = JSON.stringify(value, null, indent);
  if (json === undefined) return '';
  return json.replace(JSON_TOKEN_RE, (m: string, keyColon: string | undefined) => {
    let cls: string;
    if (m[0] === '"') cls = keyColon !== undefined ? 'j-key' : 'j-str';
    else if (m === 'true' || m === 'false') cls = 'j-bool';
    else if (m === 'null') cls = 'j-null';
    else cls = 'j-num';
    return `<span class="${cls}">${esc(m)}</span>`;
  });
}

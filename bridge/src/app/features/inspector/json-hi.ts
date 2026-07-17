/**
 * THE ONE JSON HIGHLIGHTER for the Inspector.
 *
 * Every pretty-printed JSON site in the Inspector routes through this, so highlighting is
 * everywhere or nowhere. It is a string→string function: it takes a value, stringifies it at a
 * given indent, and wraps each JSON token in a `<span class="j-*">`. Two invariants, both pinned
 * by the unit tests beside this file:
 *
 *   1. It NEVER weakens escaping. Every token's text is run through {@link esc} before it is
 *      wrapped, so a string value containing markup renders inert. The only text left unwrapped is
 *      JSON's structural punctuation (`{}[],:` and whitespace), none of which is HTML-special.
 *   2. It NEVER changes text content. Tokens are esc'd (which the browser decodes back on read),
 *      so the rendered block's `textContent` is byte-identical to `JSON.stringify(value, null,
 *      indent)` — select-and-copy yields the same JSON as before highlighting existed.
 *
 * The regex matches strings (a key is a string immediately followed by `:`), numbers, and the
 * three literals; everything it does not match is safe structural text passed straight through.
 */

/** HTML-escape the four characters that matter inside an element's text or an attribute value. */
export function esc(s: unknown): string {
  return String(s).replace(
    /[&<>"]/g,
    (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c] as string,
  );
}

/**
 * Syntax-highlight a JSON value as an HTML string of `<span class="j-*">` tokens over bare
 * structural punctuation. `indent` is passed straight to `JSON.stringify`. Returns `''` when the
 * value stringifies to `undefined` (nothing to show).
 */
export function jsonHi(value: unknown, indent: number): string {
  const json = JSON.stringify(value, null, indent);
  if (json === undefined) return ''; // value was undefined — nothing to show
  return json.replace(
    /"(?:\\.|[^"\\])*"(\s*:)?|\b(?:true|false|null)\b|-?\d+(?:\.\d+)?(?:[eE][+\-]?\d+)?/g,
    (m: string, keyColon: string | undefined) => {
      let cls: string;
      if (m[0] === '"') cls = keyColon !== undefined ? 'j-key' : 'j-str';
      else if (m === 'true' || m === 'false') cls = 'j-bool';
      else if (m === 'null') cls = 'j-null';
      else cls = 'j-num';
      return `<span class="${cls}">${esc(m)}</span>`;
    },
  );
}

/** Shorthand: highlight at indent 2, the timeline payload's indent. */
export function pretty(value: unknown): string {
  return jsonHi(value, 2);
}

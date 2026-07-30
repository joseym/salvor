import { esc, jsonHi, pretty } from './json-hi';
// This file and json-highlight.spec.ts both exercise the single shared highlighter; the two sets
// of assertions are kept as independent contracts over the one module.

/** Render highlighted HTML into a detached element and report what actually landed in the DOM. */
function render(html: string): { text: string; jSpans: number; foreign: string[] } {
  const el = document.createElement('pre');
  el.innerHTML = html;
  const foreign: string[] = [];
  let jSpans = 0;
  for (const child of Array.from(el.querySelectorAll('*'))) {
    if (
      child.tagName === 'SPAN' &&
      Array.from(child.classList).some((c) => /^j-(key|str|num|bool|null)$/.test(c))
    ) {
      jSpans++;
    } else {
      foreign.push(child.tagName + (child.className ? '.' + child.className : ''));
    }
  }
  return { text: el.textContent ?? '', jSpans, foreign };
}

describe('esc', () => {
  it('escapes the four HTML-significant characters and nothing else', () => {
    expect(esc('a & b < c > d " e')).toBe('a &amp; b &lt; c &gt; d &quot; e');
    expect(esc("it's fine")).toBe("it's fine");
    expect(esc(42)).toBe('42');
  });
});

describe('jsonHi tokenizer', () => {
  it('classifies a key by the colon lookahead, and a bare string as a value', () => {
    const el = document.createElement('pre');
    el.innerHTML = jsonHi({ vendor: 'Northwind' }, 0);
    const key = el.querySelector('.j-key');
    const str = el.querySelector('.j-str');
    // the key token is the whole match INCLUDING its colon, with quotes HTML-escaped in source but
    // decoded on read; the value is a bare string token
    expect(key?.textContent).toBe('"vendor":');
    expect(str?.textContent).toBe('"Northwind"');
    // and the raw HTML did escape the quotes (never weakens escaping)
    expect(jsonHi({ vendor: 'x' }, 0)).toContain('<span class="j-key">&quot;vendor&quot;:</span>');
  });

  it('classifies numbers, booleans and null distinctly', () => {
    const html = jsonHi({ n: -12.5, e: 6.02e23, b: true, f: false, z: null }, 0);
    expect(html).toContain('<span class="j-num">-12.5</span>');
    expect(html).toContain('<span class="j-num">6.02e+23</span>');
    expect(html).toContain('<span class="j-bool">true</span>');
    expect(html).toContain('<span class="j-bool">false</span>');
    expect(html).toContain('<span class="j-null">null</span>');
  });

  it('leaves structural punctuation bare: only j-* spans exist in the render', () => {
    const audit = render(pretty({ a: [1, 2], b: { c: 'x' } }));
    expect(audit.foreign).toHaveLength(0);
    expect(audit.jSpans).toBeGreaterThan(0);
  });

  it('never changes text content: textContent round-trips through JSON at the same indent', () => {
    const value = {
      ticket_id: 'T-4821',
      amount_usd: 128.4,
      ok: true,
      note: null,
      items: [1, 2, 3],
      nested: { a: 'b' },
    };
    for (const indent of [0, 1, 2]) {
      const audit = render(jsonHi(value, indent));
      expect(JSON.stringify(JSON.parse(audit.text), null, indent)).toBe(audit.text);
      expect(audit.text).toBe(JSON.stringify(value, null, indent));
    }
  });

  it('is escape-safe: a string value carrying markup renders inert (no foreign elements, no img)', () => {
    const value = { evil: '<img src=x onerror=alert(1)>', close: '</span><script>bad()</script>' };
    const audit = render(jsonHi(value, 2));
    expect(audit.foreign, `foreign nodes: ${audit.foreign.join(', ')}`).toHaveLength(0);
    // and the dangerous text survives byte-for-byte as data, not as parsed markup
    expect(JSON.parse(audit.text)).toEqual(value);
  });

  it('returns the empty string for an undefined value (nothing to show)', () => {
    expect(jsonHi(undefined, 2)).toBe('');
  });
});

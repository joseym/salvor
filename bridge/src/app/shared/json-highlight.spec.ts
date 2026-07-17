import { jsonHi } from './json-hi';

describe('jsonHi', () => {
  it('wraps keys, strings, numbers, booleans, and null in their own span class', () => {
    const html = jsonHi({ a: 'hi', n: 3.5, ok: true, gone: null }, 2);
    expect(html).toContain('<span class="j-key">&quot;a&quot;:</span>');
    expect(html).toContain('<span class="j-str">&quot;hi&quot;</span>');
    expect(html).toContain('<span class="j-num">3.5</span>');
    expect(html).toContain('<span class="j-bool">true</span>');
    expect(html).toContain('<span class="j-null">null</span>');
  });

  it('escapes HTML-special characters inside a string value, staying inert', () => {
    const html = jsonHi({ payload: '<img src=x onerror=alert(1)>' });
    expect(html).not.toContain('<img');
    expect(html).toContain('&lt;img src=x onerror=alert(1)&gt;');
  });

  it('is copy-faithful: textContent-equivalent (tags stripped) equals JSON.stringify at the same indent', () => {
    const value = { tool: 'charge', input: { amount: 10, note: 'a "quoted" & <b> value' } };
    const html = jsonHi(value, 2);
    const stripped = html
      .replace(/<span class="j-[a-z]+">/g, '')
      .replace(/<\/span>/g, '')
      .replace(/&amp;/g, '&')
      .replace(/&lt;/g, '<')
      .replace(/&gt;/g, '>')
      .replace(/&quot;/g, '"');
    expect(stripped).toBe(JSON.stringify(value, null, 2));
  });

  it('renders compact (no indent) when indent is omitted, matching the evidence-dd call sites', () => {
    const html = jsonHi({ a: 1 });
    expect(html).not.toContain('\n');
  });

  it('leaves structural punctuation and whitespace outside any span', () => {
    const html = jsonHi({ a: 1, b: 2 }, 2);
    expect(html).toContain('{\n');
    expect(html).toContain(',\n');
  });

  it('returns empty string for an undefined value (JSON.stringify itself returns undefined)', () => {
    expect(jsonHi(undefined)).toBe('');
  });
});

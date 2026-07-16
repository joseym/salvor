import { Component, afterNextRender, signal } from '@angular/core';

/** A single token to render as a swatch/spec row. */
interface TokenSpec {
  name: string; // CSS custom property, without the leading --
  prop: string; // the CSS property used to resolve the token's computed value
  kind: 'color' | 'text';
}

interface TokenGroup {
  label: string;
  tokens: TokenSpec[];
}

/** A resolved row: the token plus its computed value in each theme. */
interface ResolvedToken extends TokenSpec {
  light: string;
  dark: string;
}

interface ResolvedGroup {
  label: string;
  tokens: ResolvedToken[];
}

interface ContrastPair {
  label: string;
  fg: string;
  bg: string;
  threshold: number; // WCAG AA: 4.5 for normal text, 3 for large text / non-text UI
}

interface ResolvedContrast extends ContrastPair {
  lightRatio: number;
  darkRatio: number;
  lightPass: boolean;
  darkPass: boolean;
}

const GROUPS: TokenGroup[] = [
  {
    label: 'The six anchors (single definition site)',
    tokens: [
      { name: 'anchor-paper', prop: 'background-color', kind: 'color' },
      { name: 'anchor-ink', prop: 'background-color', kind: 'color' },
      { name: 'anchor-blue', prop: 'background-color', kind: 'color' },
      { name: 'anchor-success', prop: 'background-color', kind: 'color' },
      { name: 'anchor-warn', prop: 'background-color', kind: 'color' },
      { name: 'anchor-danger', prop: 'background-color', kind: 'color' },
    ],
  },
  {
    label: 'Semantic colors (remap with theme)',
    tokens: [
      'bg',
      'surface',
      'surface-warm',
      'fg',
      'fg-2',
      'muted',
      'meta',
      'faint',
      'border',
      'border-soft',
      'edge-strong',
      'wash',
      'accent',
      'accent-hover',
      'accent-active',
      'accent-on',
      'success',
      'warn',
      'danger',
      'amber',
      'amber-ink',
      'amber-wash',
      'amber-row',
      'amber-row-hover',
      'red',
      'red-wash',
    ].map((name) => ({ name, prop: 'background-color', kind: 'color' as const })),
  },
  {
    label: 'Radius',
    tokens: ['radius-sm', 'radius-md', 'radius-lg', 'radius-pill'].map((name) => ({
      name,
      prop: 'border-radius',
      kind: 'text' as const,
    })),
  },
  {
    label: 'Elevation (box-shadow, mixed from ink — never black)',
    tokens: ['elev-ring', 'elev-raised', 'focus-ring'].map((name) => ({
      name,
      prop: 'box-shadow',
      kind: 'text' as const,
    })),
  },
  {
    label: 'Type family',
    tokens: ['font-display', 'font-body', 'font-mono'].map((name) => ({
      name,
      prop: 'font-family',
      kind: 'text' as const,
    })),
  },
  {
    label: 'Type scale',
    tokens: ['text-xs', 'text-sm', 'text-base', 'text-lg', 'text-xl', 'text-2xl', 'text-3xl', 'text-4xl'].map(
      (name) => ({ name, prop: 'font-size', kind: 'text' as const }),
    ),
  },
  {
    label: 'Line height / tracking',
    tokens: [
      { name: 'leading-body', prop: 'line-height', kind: 'text' as const },
      { name: 'leading-tight', prop: 'line-height', kind: 'text' as const },
      { name: 'tracking-display', prop: 'letter-spacing', kind: 'text' as const },
    ],
  },
  {
    label: 'Motion',
    tokens: [
      { name: 'motion-fast', prop: 'transition-duration', kind: 'text' as const },
      { name: 'motion-base', prop: 'transition-duration', kind: 'text' as const },
      { name: 'ease-standard', prop: 'transition-timing-function', kind: 'text' as const },
    ],
  },
  {
    label: 'Spacing',
    tokens: [
      'space-1',
      'space-2',
      'space-3',
      'space-4',
      'space-5',
      'space-6',
      'space-8',
      'space-12',
    ].map((name) => ({ name, prop: 'width', kind: 'text' as const })),
  },
  {
    label: 'Layout',
    tokens: [
      'nav-w',
      'well',
      'well-form',
      'container-max',
      'container-gutter-desktop',
      'container-gutter-tablet',
      'container-gutter-phone',
      'section-y-desktop',
      'section-y-tablet',
      'section-y-phone',
    ].map((name) => ({ name, prop: 'width', kind: 'text' as const })),
  },
];

const CONTRAST_PAIRS: ContrastPair[] = [
  { label: 'fg on bg (body text)', fg: 'fg', bg: 'bg', threshold: 4.5 },
  { label: 'fg-2 on bg (secondary text)', fg: 'fg-2', bg: 'bg', threshold: 4.5 },
  { label: 'muted on bg', fg: 'muted', bg: 'bg', threshold: 4.5 },
  { label: 'faint on bg (11px floor)', fg: 'faint', bg: 'bg', threshold: 4.5 },
  { label: 'accent on bg (link)', fg: 'accent', bg: 'bg', threshold: 4.5 },
  { label: 'accent-on on accent (button label)', fg: 'accent-on', bg: 'accent', threshold: 4.5 },
  { label: 'success on bg', fg: 'success', bg: 'bg', threshold: 4.5 },
  { label: 'warn on bg', fg: 'warn', bg: 'bg', threshold: 4.5 },
  { label: 'danger on bg', fg: 'danger', bg: 'bg', threshold: 4.5 },
  { label: 'border on bg (hairline, non-text — WCAG 1.4.11)', fg: 'border', bg: 'bg', threshold: 3 },
  { label: 'edge-strong on bg (input boundary — WCAG 1.4.11)', fg: 'edge-strong', bg: 'bg', threshold: 3 },
];

function parseRgb(value: string): [number, number, number] | null {
  const m = value.match(/rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)/i);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

function relativeLuminance([r, g, b]: [number, number, number]): number {
  const chan = (c: number) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  const [rl, gl, bl] = [chan(r), chan(g), chan(b)];
  return 0.2126 * rl + 0.7152 * gl + 0.0722 * bl;
}

function contrastRatio(a: string, b: string): number {
  const rgbA = parseRgb(a);
  const rgbB = parseRgb(b);
  if (!rgbA || !rgbB) return 0;
  const lA = relativeLuminance(rgbA);
  const lB = relativeLuminance(rgbB);
  const [lighter, darker] = lA > lB ? [lA, lB] : [lB, lA];
  return (lighter + 0.05) / (darker + 0.05);
}

/**
 * Dev-only token-parity instrument: renders every
 * token defined in styles/tokens.css next to its resolved value in both
 * themes, plus an AA contrast sweep over the load-bearing fg/bg pairs.
 *
 * Measurement approach: custom properties inherit down the tree, and the
 * theme remap selectors in tokens.css key off `:root[data-theme]` (the
 * <html> element). So to read a token's DARK value without disturbing the
 * page the user is looking at, this flips `document.documentElement`'s
 * `data-theme` to each value in turn, reads every token's computed value
 * off a detached probe element, and restores the original attribute — all
 * synchronously in one task, so there is no visible flash.
 */
@Component({
  selector: 'bridge-token-parity',
  templateUrl: './token-parity.html',
})
export class TokenParity {
  readonly groups = signal<ResolvedGroup[]>([]);
  readonly contrasts = signal<ResolvedContrast[]>([]);

  constructor() {
    afterNextRender(() => this.measure());
  }

  private measure(): void {
    const root = document.documentElement;
    const originalTheme = root.dataset['theme'];
    const probe = document.createElement('div');
    probe.style.position = 'absolute';
    probe.style.visibility = 'hidden';
    probe.style.pointerEvents = 'none';
    document.body.appendChild(probe);

    const readAll = (): Map<string, string> => {
      const computed = getComputedStyle(probe);
      const values = new Map<string, string>();
      for (const group of GROUPS) {
        for (const token of group.tokens) {
          probe.style.setProperty(token.prop, `var(--${token.name})`);
          values.set(token.name, computed.getPropertyValue(token.prop).trim());
          probe.style.removeProperty(token.prop);
        }
      }
      for (const pair of CONTRAST_PAIRS) {
        for (const name of [pair.fg, pair.bg]) {
          if (!values.has(name)) {
            probe.style.setProperty('background-color', `var(--${name})`);
            values.set(name, computed.backgroundColor.trim());
            probe.style.removeProperty('background-color');
          }
        }
      }
      return values;
    };

    root.dataset['theme'] = 'light';
    const light = readAll();
    root.dataset['theme'] = 'dark';
    const dark = readAll();

    if (originalTheme === undefined) {
      delete root.dataset['theme'];
    } else {
      root.dataset['theme'] = originalTheme;
    }
    probe.remove();

    this.groups.set(
      GROUPS.map((group) => ({
        label: group.label,
        tokens: group.tokens.map((token) => ({
          ...token,
          light: light.get(token.name) ?? '',
          dark: dark.get(token.name) ?? '',
        })),
      })),
    );

    this.contrasts.set(
      CONTRAST_PAIRS.map((pair) => {
        const lightRatio = contrastRatio(light.get(pair.fg) ?? '', light.get(pair.bg) ?? '');
        const darkRatio = contrastRatio(dark.get(pair.fg) ?? '', dark.get(pair.bg) ?? '');
        return {
          ...pair,
          lightRatio,
          darkRatio,
          lightPass: lightRatio >= pair.threshold,
          darkPass: darkRatio >= pair.threshold,
        };
      }),
    );
  }
}

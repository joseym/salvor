import { Injectable, signal } from '@angular/core';

export type Theme = 'light' | 'dark';

const STORAGE_KEY = 'salvor.theme';
const MEDIA_QUERY = '(prefers-color-scheme: dark)';

/**
 * Theme service. Ported from the design spec's boot/setTheme semantics:
 *   - A saved choice in localStorage always wins.
 *   - Absent a saved choice, the OS preference decides, and the app keeps
 *     following OS changes live (an explicit toggle stops that following).
 *   - The chosen theme is applied as `data-theme` on <html>, matching the
 *     CSS remap selectors in styles/tokens.css.
 *
 * Signal-based, no NgZone dependency — safe under zoneless change detection.
 */
@Injectable({ providedIn: 'root' })
export class ThemeService {
  readonly theme = signal<Theme>(this.initialTheme());

  constructor() {
    this.apply(this.theme());

    if (typeof window !== 'undefined' && window.matchMedia) {
      window.matchMedia(MEDIA_QUERY).addEventListener('change', (event) => {
        if (!this.storedTheme()) {
          this.set(event.matches ? 'dark' : 'light', { persist: false });
        }
      });
    }
  }

  toggle(): void {
    this.set(this.theme() === 'dark' ? 'light' : 'dark');
  }

  set(mode: Theme, opts: { persist?: boolean } = { persist: true }): void {
    this.theme.set(mode);
    this.apply(mode);
    if (opts.persist !== false) {
      this.persist(mode);
    }
  }

  private initialTheme(): Theme {
    const saved = this.storedTheme();
    if (saved) return saved;
    if (typeof window !== 'undefined' && window.matchMedia) {
      return window.matchMedia(MEDIA_QUERY).matches ? 'dark' : 'light';
    }
    return 'light';
  }

  private storedTheme(): Theme | null {
    if (typeof localStorage === 'undefined') return null;
    try {
      const value = localStorage.getItem(STORAGE_KEY);
      return value === 'light' || value === 'dark' ? value : null;
    } catch {
      return null;
    }
  }

  private persist(mode: Theme): void {
    if (typeof localStorage === 'undefined') return;
    try {
      localStorage.setItem(STORAGE_KEY, mode);
    } catch {
      /* storage blocked (private browsing, sandboxed preview) — no fallback theater */
    }
  }

  private apply(mode: Theme): void {
    if (typeof document === 'undefined') return;
    document.documentElement.dataset['theme'] = mode;
  }
}

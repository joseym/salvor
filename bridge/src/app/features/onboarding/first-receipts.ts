import {
  Component,
  DestroyRef,
  ElementRef,
  ViewChild,
  computed,
  effect,
  inject,
  signal,
} from '@angular/core';

import { FirstReceiptsService, type StepId } from '../../core/first-receipts';
import { ForkIntentService } from '../../core/fork-intent';
import { ViewService } from '../../core/view';

const LEDE =
  'Salvor keeps a running record. Each time something happens in a run — one job an agent ' +
  'carried out — Salvor writes a line for it. That line is a receipt: a fact added to the run’s ' +
  'log, never edited later, only added to. That is the whole idea, so the five steps below are ' +
  'not a rehearsal — each one ticks the moment you do the real thing, logged exactly like ' +
  'everything else.';

/**
 * The First Receipts dock and its coach-mark layer. Mounted once in the shell, so it rides on
 * every view. The collapsed tab is always reachable; the expanded panel is a small log of the
 * operator's OWN firsts, each ticked step rendered as a receipt line in the app's receipt voice.
 *
 * COACH-MARK. The front-most unticked step earns a ring on its REAL target element plus a
 * `popover=auto` callout beside it — NO scrim (the fork hazard review is the app's one modal),
 * native light-dismiss, Escape closes, no focus trap, and it never gates the page. It shows only
 * while the panel is expanded, so a collapsed dock adds nothing over the rest of the app. When a
 * fork is requested the fork step ticks and the callout light-dismisses with it, giving the fork
 * dialog precedence.
 */
@Component({
  selector: 'bridge-first-receipts',
  templateUrl: './first-receipts.html',
})
export class FirstReceipts {
  readonly fr = inject(FirstReceiptsService);
  private readonly view = inject(ViewService);
  private readonly forkIntent = inject(ForkIntentService);
  private readonly destroyRef = inject(DestroyRef);

  readonly lede = LEDE;

  @ViewChild('callout') private callout?: ElementRef<HTMLElement>;

  /** The active step's target rect, in viewport (fixed) coordinates — null when there is no active
   * step, the panel is collapsed, or the target is not currently painted. */
  private readonly rect = signal<{ top: number; left: number; width: number; height: number } | null>(null);

  /** The step the operator explicitly light-dismissed the callout for. Cleared when the active step
   * changes, so a dismissal is honoured for that step but the next step still gets its coach-mark. */
  private readonly calloutDismissedFor = signal<StepId | undefined>(undefined);

  readonly activeStep = computed(() => {
    const id = this.fr.activeStepId();
    return id ? this.fr.steps.find((s) => s.id === id) : undefined;
  });

  /** The ring shows whenever the panel is open, a step is active, and its target is on screen. */
  readonly coachVisible = computed(() => this.fr.open() && this.activeStep() !== undefined && this.rect() !== null);

  private rafId = 0;

  constructor() {
    // Track the active target while the coach-mark could be visible. A rAF loop runs ONLY while the
    // panel is open and a step is active; it stops the instant either goes false, so nothing polls
    // in the background.
    effect(() => {
      const active = this.fr.open() ? this.activeStep() : undefined;
      this.stopTracking();
      if (active) this.track(active.target);
      else this.rect.set(null);
    });

    // When the active step changes, a stale dismissal no longer applies.
    effect(() => {
      const id = this.fr.activeStepId();
      if (this.calloutDismissedFor() !== id) this.calloutDismissedFor.set(undefined);
    });

    // Show or hide the callout to match the coach state, honouring a per-step dismissal. Guarded so
    // a double show/hide (the popover is already in that state) never throws into the console.
    effect(() => {
      const step = this.activeStep();
      const show =
        this.coachVisible() && step !== undefined && this.calloutDismissedFor() !== step.id;
      this.setCallout(show);
    });

    // FORK PRECEDENCE: a fork request light-dismisses the callout before the hazard review opens.
    // (The fork step also ticks here, advancing the coach-mark; hiding is the belt under that.)
    effect(() => {
      if (this.forkIntent.intent()) this.setCallout(false);
    });

    this.destroyRef.onDestroy(() => this.stopTracking());
  }

  private track(selector: string): void {
    const step = () => {
      const el = document.querySelector<HTMLElement>(selector);
      // getClientRects().length is 0 for a display:none element (a target in a [hidden] view), so
      // the ring never chases something the operator cannot see.
      const next =
        el && el.getClientRects().length > 0
          ? (() => {
              const r = el.getBoundingClientRect();
              return { top: r.top, left: r.left, width: r.width, height: r.height };
            })()
          : null;
      // Only write the signal when the geometry actually changed, so a steady layout does not
      // re-fire the coach effects (and change detection) every animation frame.
      const cur = this.rect();
      if (
        (next === null) !== (cur === null) ||
        (next && cur && (next.top !== cur.top || next.left !== cur.left || next.width !== cur.width || next.height !== cur.height))
      ) {
        this.rect.set(next);
      }
      this.rafId = requestAnimationFrame(step);
    };
    this.rafId = requestAnimationFrame(step);
  }
  private stopTracking(): void {
    if (this.rafId) cancelAnimationFrame(this.rafId);
    this.rafId = 0;
  }

  // ── ring geometry (a 2px accent ring drawn OVER the real target, pointer-events: none) ──
  ringStyle(): Record<string, string> {
    const r = this.rect();
    if (!r) return { display: 'none' };
    return {
      top: `${r.top - 3}px`,
      left: `${r.left - 3}px`,
      width: `${r.width + 6}px`,
      height: `${r.height + 6}px`,
    };
  }
  calloutStyle(): Record<string, string> {
    const r = this.rect();
    if (!r) return {};
    const w = 300;
    // Prefer sitting to the RIGHT of the target, so a ring on a vertically stacked control (the nav
    // links) never covers its neighbours. Fall back below-left only when there is no room to the
    // right. Always clamped inside the viewport.
    const rightLeft = r.left + r.width + 10;
    const roomRight = rightLeft + w + 8 <= window.innerWidth;
    const left = roomRight ? rightLeft : Math.min(Math.max(8, r.left), window.innerWidth - w - 8);
    const top = roomRight
      ? Math.min(Math.max(8, r.top), window.innerHeight - 120)
      : Math.min(r.top + r.height + 8, window.innerHeight - 120);
    return { top: `${top}px`, left: `${left}px` };
  }

  private setCallout(show: boolean): void {
    const el = this.callout?.nativeElement as (HTMLElement & { showPopover?: () => void; hidePopover?: () => void }) | undefined;
    if (!el) return;
    const open = el.matches(':popover-open');
    try {
      if (show && !open) el.showPopover?.();
      else if (!show && open) el.hidePopover?.();
    } catch {
      /* the popover was mid-transition to the state we asked for — nothing to do */
    }
  }

  /** The callout's own toggle: a user light-dismiss (Escape, click-away) records the dismissal for
   * this step, so the effect does not immediately reopen it. */
  onCalloutToggle(e: Event): void {
    if ((e as ToggleEvent).newState === 'closed') {
      const id = this.fr.activeStepId();
      if (id) this.calloutDismissedFor.set(id);
    }
  }

  short(id: string | undefined): string {
    return id ? id.slice(0, 8) : '';
  }
}

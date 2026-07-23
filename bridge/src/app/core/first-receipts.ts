import { Injectable, type Signal, computed, effect, inject, signal } from '@angular/core';

import { RunsService, SALVOR_CLIENT } from './api';
import { ForkIntentService } from './fork-intent';
import { ViewService } from './view';

/**
 * FIRST RECEIPTS — the onboarding checklist.
 *
 * A persistent dock that walks a brand-new operator through reading and steering the record for
 * the first time. It follows the minute-one strip's exact persistence idiom (see
 * `features/runs/runs.ts`: an open/dismiss pair, the choice persisted, a read failure defaulting to
 * SHOWN so the teaching is the safe fallback, never the suppression) — with ONE deliberate change,
 * stated here: the strip DEFAULTS OPEN, this dock defaults COLLAPSED to its tab. The design calls
 * for "a persistent collapsed dock", and the dock rides on every view rather than above one table,
 * so auto-expanding it on every fresh visit would sit a panel over whatever the operator came to
 * do. So the three-way rule is: no stored preference yet → collapsed (the persistent collapsed
 * dock); a stored choice → honoured; a read that THREW (private browsing, a locked-down embed) →
 * expanded, the read-failure-defaults-to-SHOWN/teaching fallback the strip established.
 *
 * THE MECHANISM IS THE LESSON. Every step's predicate reads a REAL signal the app already
 * maintains — the active view and run id ({@link ViewService}), the scrubber's playhead position
 * (the Inspector's own scrub, forwarded through {@link noteScrub}), a committed Inbox action (the
 * card's own announce, forwarded through {@link noteInboxAction}), a fork request
 * ({@link ForkIntentService}). None of them is a tutorial-only event: a step ticks because the
 * operator did the real thing, logged the same way everything else is. A step whose predicate is
 * already true before the dock is ever opened ticks silently — {@link tick} is first-write-wins and
 * nothing here announces, so a thing you already did simply shows as already done.
 */

export type StepId = 'inspect' | 'scrub' | 'inbox' | 'fork' | 'filter';

export interface ReceiptStep {
  readonly id: StepId;
  /** The instruction, standalone: it must make sense to someone who skipped every other surface. */
  readonly label: string;
  /** Teaches WHY the product works this way, from zero — every term defined at point of use. */
  readonly why: string;
  /** The receipt-voice coda appended after the tick's own clause. */
  readonly coda: string;
  /** A CSS selector for the REAL element the coach-mark rings — never a tutorial-only affordance. */
  readonly target: string;
}

/** One recorded first: the UTC clock time it happened and the receipt-voice clause for it. */
export interface Tick {
  readonly at: string;
  readonly detail: string;
}

/**
 * The five steps, with copy written to the owner's one binding constraint: assume ZERO prior
 * understanding. "receipt", "run", "log", "scrub", "parked" and "fork" are each defined in plain
 * language at the point they are first used. The why-copy teaches the reason the product works this
 * way — an append-only record — to someone who has never heard the phrase "event sourcing", in the
 * house mechanism voice: no cheerfulness, no praise.
 */
export const STEPS: readonly ReceiptStep[] = [
  {
    id: 'inspect',
    label: 'Open a run and read its log.',
    why:
      'A run is one job an agent carried out. Its log is the ordered list of receipts for that ' +
      'job — every step it took, oldest first. A receipt is one line the app wrote down when ' +
      'something happened; nothing here is ever edited, only added. The Inspector is where you ' +
      'read that list, and reading can never change it, so you can open anything and look freely.',
    coda: 'reading a log never changes it.',
    target: '.nav-link[data-view="inspector"]',
  },
  {
    id: 'scrub',
    label: 'Scrub back to an earlier point in a run.',
    why:
      'To scrub is to drag the playhead — the marker for how far through the log you are reading ' +
      '— back to an earlier receipt, the way you would drag a video back. The log is not ' +
      'shortened or rewound; you are only choosing where to stop reading. Because receipts are ' +
      'only ever added and never removed, every earlier moment is still there to return to, exactly.',
    coda: 'the log stayed whole; you only moved the playhead.',
    target: '#prefix',
  },
  {
    id: 'inbox',
    label: 'Clear a run that is waiting on you.',
    why:
      'Some runs stop and wait for a person — they are parked. A parked run needs someone to ' +
      'approve it, raise its spending limit, or record how a half-finished step turned out. The ' +
      'Inbox lists every parked run with the one action that frees it. Taking that action adds a ' +
      'new receipt saying what you decided; it never rewrites what already happened.',
    coda: 'logged as a new receipt on the run’s log, not an edit.',
    target: '.nav-link[data-view="inbox"]',
  },
  {
    id: 'fork',
    label: 'Open a fork, read the warning, and back out.',
    why:
      'To fork is to start a brand-new run from a chosen point inside an existing one — a ' +
      '"what if we had gone differently here" that leaves the original run untouched. A fork can ' +
      'repeat real actions the first run took, so the app shows a warning and creates nothing ' +
      'until you confirm. Open it to see what it would repeat, then close it: opening and backing ' +
      'out writes nothing.',
    coda: 'nothing was forked; no new run was written.',
    target: '#fork-here-btn',
  },
  {
    id: 'filter',
    label: 'Filter the run list by state.',
    why:
      'Every run is in some state: running, completed, failed, or waiting on you. On the Runs ' +
      'list you can filter — hide every row except the state you pick — so the runs that need you ' +
      'are easy to find. Filtering only changes what you see; it touches no run and writes no receipt.',
    coda: 'the list narrowed; no run changed.',
    target: '#q',
  },
];

export const TOTAL_STEPS = STEPS.length;
const STEP_IDS = new Set<string>(STEPS.map((s) => s.id));

const KEY = 'salvor.firstReceipts';

interface Persisted {
  /** Whether the dock is EXPANDED. */
  readonly open: boolean;
  readonly steps: Readonly<Record<string, Tick>>;
}

/**
 * Read the persisted dock state. Three-way, as the class comment sets out:
 *   - no key yet → the persistent collapsed dock (open: false), no ticks;
 *   - a stored object → the operator's own choice, with any ticks recorded so far;
 *   - a read that THREW (storage unavailable) → open: true, the SHOWN/teaching fallback.
 * A malformed-but-readable value never throws JSON.parse past the catch, so it also degrades to the
 * teaching fallback rather than crashing the shell.
 */
export function readState(): Persisted {
  try {
    const raw = localStorage.getItem(KEY);
    if (raw === null) return { open: false, steps: {} };
    const parsed = JSON.parse(raw) as Partial<Persisted> | null;
    const steps: Record<string, Tick> = {};
    if (parsed && typeof parsed.steps === 'object' && parsed.steps !== null) {
      for (const [id, t] of Object.entries(parsed.steps)) {
        if (STEP_IDS.has(id) && t && typeof t.at === 'string' && typeof t.detail === 'string') {
          steps[id] = { at: t.at, detail: t.detail };
        }
      }
    }
    return { open: parsed?.open === true, steps };
  } catch {
    return { open: true, steps: {} };
  }
}

function writeState(state: Persisted): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(state));
  } catch {
    /* persistence is a nicety; the in-memory signals still govern this session */
  }
}

// ── pure predicates (unit-tested) ──────────────────────────────────────────────────────────────

/** Step 1: the Inspector is the active view over a real run. */
export function inspectorVisited(view: string, runId: string | undefined): boolean {
  return view === 'inspector' && runId !== undefined;
}

/** Step 2: the playhead sits before the end of the log — an earlier moment is being read. */
export function scrubbedEarlier(prefixN: number, total: number): boolean {
  return total > 0 && prefixN < total;
}

/** Step 5: the committed filter names a run state (a `status:` term or its `group:` classifier —
 * the vocabulary the health chips and status pills write). */
export function filteredByState(query: string): boolean {
  return /(^|\s)(status|group):/.test(query.trim());
}

/** The receipt-voice clause for a committed Inbox action, taken from the card's own announce
 * message ("Resumed run 88f2abcd. …", "Recorded the outcome for run …", "Abandoned run …") — the
 * leading sentence, lower-cased, so it reads as "you resumed run 88f2abcd". */
export function inboxReceiptDetail(message: string): string {
  const first = message.split(/\.(?:\s|$)/)[0]?.trim() ?? '';
  const clause = first ? first.charAt(0).toLowerCase() + first.slice(1) : 'cleared a run that was waiting on you';
  return `you ${clause}`;
}

function short(runId: string | undefined): string {
  return runId ? runId.slice(0, 8) : '';
}

function nowHHMM(): string {
  // UTC HH:MM, the same register the connection pill and the Inspector clock already use.
  return new Date().toISOString().slice(11, 16);
}

const PRACTICE_LABEL = { practice: 'first-receipts' } as const;
const WAITING_STATES = new Set(['suspended', 'budget_exceeded', 'needs_reconciliation']);

@Injectable({ providedIn: 'root' })
export class FirstReceiptsService {
  private readonly view = inject(ViewService);
  private readonly forkIntent = inject(ForkIntentService);
  private readonly runs = inject(RunsService);
  private readonly client = inject(SALVOR_CLIENT);

  readonly steps = STEPS;
  readonly total = TOTAL_STEPS;

  private readonly initial = readState();
  private readonly _ticks = signal<ReadonlyMap<StepId, Tick>>(
    new Map(Object.entries(this.initial.steps) as [StepId, Tick][]),
  );
  private readonly _open = signal<boolean>(this.initial.open);

  readonly ticks: Signal<ReadonlyMap<StepId, Tick>> = this._ticks.asReadonly();
  /** Whether the expanded panel is shown. The collapsed tab is the complement — one or the other. */
  readonly open: Signal<boolean> = this._open.asReadonly();
  readonly count = computed(() => this._ticks().size);
  readonly complete = computed(() => this.count() === this.total);

  /** The front-most step still to do — the one that earns the coach-mark. Undefined once every
   * step is ticked. Order matters (front-most), but a step ticks the instant its predicate goes
   * true in ANY order, so this is presentation, not a gate. */
  readonly activeStepId = computed<StepId | undefined>(() => {
    const ticked = this._ticks();
    return this.steps.find((s) => !ticked.has(s.id))?.id;
  });

  // ── practice seed (parked material) ────────────────────────────────────────────────────────
  /** True when at least one run is parked — the Inbox step has real material to act on. */
  readonly hasParkedRun = computed(() => this.runs.runs().some((r) => WAITING_STATES.has(r.status.state)));
  /** The id of the practice run this dock seeded, if any — so its abandon control can name it. */
  readonly practiceRunId = signal<string | undefined>(undefined);
  readonly seeding = signal(false);
  readonly seedError = signal<string | undefined>(undefined);

  constructor() {
    // Step 1 — Inspector visit. A root signal, so this fires from boot: a visit made before the
    // dock was ever opened ticks silently (first-write-wins in `tick`).
    effect(() => {
      if (inspectorVisited(this.view.view(), this.view.runId())) {
        this.tick('inspect', `you opened run ${short(this.view.runId())}’s log`);
      }
    });

    // Step 5 — a state filter committed into ?q= (the pills and the query field write the same term).
    effect(() => {
      if (filteredByState(this.view.query())) this.tick('filter', 'you filtered the run list by state');
    });

    // Step 4 — a fork request through the one fork door (the Inspector scrubber or a Runs row).
    effect(() => {
      const intent = this.forkIntent.intent();
      if (intent) this.tick('fork', `you opened a fork of run ${short(intent.runId)}`);
    });
  }

  /** Forwarded from the Inspector's real scrub (see `inspector.ts#setPrefix`): the playhead moved
   * to `prefixN` of `total`. Reads the genuine scrubber position, never a tutorial event. */
  noteScrub(prefixN: number, total: number): void {
    if (scrubbedEarlier(prefixN, total)) this.tick('scrub', 'you scrubbed back to an earlier point');
  }

  /** Forwarded from a committed Inbox action (see `inbox.ts#onCommitted`): the card's own announce
   * message, which names the verb and the run. */
  noteInboxAction(message: string): void {
    this.tick('inbox', inboxReceiptDetail(message));
  }

  /** Record a step's first occurrence. First-write-wins: a later trigger for a step already ticked
   * is a no-op, so the recorded time stays the moment it FIRST happened and a re-run of the same
   * real action never rewrites history. */
  tick(id: StepId, detail: string): void {
    if (this._ticks().has(id)) return;
    const next = new Map(this._ticks());
    next.set(id, { at: nowHHMM(), detail });
    this._ticks.set(next);
    this.persist();
  }

  receiptOf(id: StepId): Tick | undefined {
    return this._ticks().get(id);
  }
  isTicked(id: StepId): boolean {
    return this._ticks().has(id);
  }

  /** Expand the panel — the reopen path from the tab, the "?" popover and About. Never called by a
   * tick or by completion: the dock never auto-expands past its collapsed tab. */
  reopen(): void {
    this._open.set(true);
    this.persist();
  }
  /** Collapse the panel back to its tab. Reopenable forever. */
  dismiss(): void {
    this._open.set(false);
    this.persist();
  }

  private persist(): void {
    writeState({ open: this._open(), steps: Object.fromEntries(this._ticks()) });
  }

  /**
   * The honest fallback for the Inbox step when nothing is parked: create a REAL run through the
   * real API, tagged `practice` on its labels so it is never mistaken for genuine work, and record
   * its id so the operator can abandon it. Uses the first registered agent — a practice run is only
   * meaningful against an agent the control plane actually knows. Silent-failing into `seedError`
   * so a control plane without a registered agent says so rather than throwing.
   */
  async seedPractice(): Promise<void> {
    if (this.seeding()) return;
    this.seeding.set(true);
    this.seedError.set(undefined);
    try {
      const agents = await this.client.listAgents();
      const agent = agents[0];
      if (!agent) {
        this.seedError.set('No agent is registered on this control plane, so there is nothing to run yet.');
        return;
      }
      const runId = await this.client.startRun(agent, null, { labels: { ...PRACTICE_LABEL } });
      this.practiceRunId.set(runId);
      await this.runs.refresh().catch(() => undefined);
    } catch (err) {
      this.seedError.set(err instanceof Error ? err.message : String(err));
    } finally {
      this.seeding.set(false);
    }
  }

  /** Retire the practice run through the real abandon endpoint — the deletable half of the seed. */
  async abandonPractice(): Promise<void> {
    const id = this.practiceRunId();
    if (!id) return;
    try {
      await this.client.abandon(id, 'first-receipts practice run, retired');
      this.practiceRunId.set(undefined);
      await this.runs.refresh().catch(() => undefined);
    } catch (err) {
      this.seedError.set(err instanceof Error ? err.message : String(err));
    }
  }
}

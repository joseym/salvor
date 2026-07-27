import { Injectable, type Signal, inject, signal } from '@angular/core';
import type { ClientRunDriver, SalvorEvent } from '@salvor-run/client';

import { SALVOR_CLIENT } from './client';
import { type ConnectionState, createConnectionStateMachine } from './connection-state';
import { errorMessage } from './errors';

const TERMINAL_EVENT_KINDS: ReadonlySet<string> = new Set(['RunCompleted', 'RunFailed']);
const DEFAULT_POLL_INTERVAL_MS = 2000;

/**
 * The open-by-id fallback path for a client-driven run: a lookup that misses `GET /v1/runs`
 * falls back to trying the run as client-driven. A client-driven run has no server-side driver
 * task, so it has no SSE
 * surface — `GET /v1/runs/{id}/events` is server-driven-run only. The only way to read it
 * is `ClientRunDriver.log(fromSeq)`, a plain GET with no push, so this channel polls it.
 * That is honestly `Polling`, never `Live` — the fourth connection-pill state exists
 * specifically so this path never borrows the SSE surface's label.
 */
export interface ClientRunChannel {
  readonly runId: string;
  /** The pill's sole authority for this channel. Read-only by construction. */
  readonly state: Signal<ConnectionState>;
  readonly events: Signal<SalvorEvent[]>;
  readonly error: Signal<string | undefined>;
  /** Begin polling `log(fromSeq)` every `intervalMs` (default 2000ms) until a terminal
   * event (`RunCompleted`/`RunFailed`) is observed in the polled log. Idempotent while
   * already polling. */
  start(intervalMs?: number): void;
  /** Stop polling. `state` returns to `Snapshot`. */
  stop(): void;
}

class ClientRunChannelImpl implements ClientRunChannel {
  readonly runId: string;
  readonly state: Signal<ConnectionState>;
  readonly events: Signal<SalvorEvent[]>;
  readonly error: Signal<string | undefined>;

  private readonly machine = createConnectionStateMachine();
  private readonly _events = signal<SalvorEvent[]>([]);
  private readonly _error = signal<string | undefined>(undefined);

  private nextSeq = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private generation = 0;

  constructor(
    private readonly driver: ClientRunDriver,
    initialLog: SalvorEvent[],
  ) {
    this.runId = driver.runId;
    this.state = this.machine.state;
    this.events = this._events.asReadonly();
    this.error = this._error.asReadonly();
    if (initialLog.length > 0) {
      this._events.set(initialLog);
      this.nextSeq = initialLog[initialLog.length - 1]!.seq + 1;
      if (initialLog.some((event) => TERMINAL_EVENT_KINDS.has(event.kind))) {
        this.machine.driver.toEnded(false);
      }
    }
  }

  start(intervalMs = DEFAULT_POLL_INTERVAL_MS): void {
    if (this.timer !== undefined) return;
    if (this.machine.state().kind === 'ended') return; // already resting, nothing to poll for
    const generation = ++this.generation;
    this._error.set(undefined);
    const tick = async (): Promise<void> => {
      if (this.generation !== generation) return;
      try {
        const fresh = await this.driver.log(this.nextSeq);
        if (this.generation !== generation) return;
        if (fresh.length > 0) {
          this.nextSeq = fresh[fresh.length - 1]!.seq + 1;
          this._events.update((events) => [...events, ...fresh]);
        }
        if (fresh.some((event) => TERMINAL_EVENT_KINDS.has(event.kind))) {
          this.machine.driver.toEnded(false);
          this.timer = undefined;
          return;
        }
        this.machine.driver.toPolling();
        this.timer = setTimeout(() => void tick(), intervalMs);
      } catch (err) {
        if (this.generation !== generation) return;
        this._error.set(errorMessage(err));
        this.machine.driver.toSnapshot();
        this.timer = undefined;
      }
    };
    void tick();
  }

  stop(): void {
    this.generation += 1;
    if (this.timer !== undefined) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
    if (this.machine.state().kind === 'polling') this.machine.driver.toSnapshot();
  }
}

@Injectable({ providedIn: 'root' })
export class ClientRunService {
  private readonly client = inject(SALVOR_CLIENT);

  /**
   * Attach to a client-driven run by id, the fallback path for a lookup that finds
   * nothing under `GET /v1/runs`. Every attach goes through `POST /v1/client-runs`
   * (`SalvorClient.openClientRun`), which mints a FRESH drive token and supersedes
   * whatever lease previously existed — this is a reattach (the intended case: your own
   * run, re-opened after a refresh or a crashed tab), never a passive spectator API. The
   * returned channel only ever calls the token-free `log()` read, but the act of
   * attaching itself is already a takeover; callers must not use this to "peek" at a
   * run someone else is actively driving.
   */
  async openById(runId: string): Promise<ClientRunChannel> {
    const driver = await this.client.openClientRun({ runId });
    return new ClientRunChannelImpl(driver, driver.logEnvelopes);
  }
}

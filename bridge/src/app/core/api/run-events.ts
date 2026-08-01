import { Injectable, type Signal, inject, signal } from '@angular/core';
import type { EndFrame, EventStream, SalvorClient, SalvorEvent } from '@salvor-run/client';

import { SALVOR_CLIENT } from './client';
import { type ConnectionState, createConnectionStateMachine } from './connection-state';
import { errorMessage } from './errors';

/**
 * One run's live event surface: `GET /v1/runs/{id}/events` (server-sent events), wrapped
 * around `SalvorClient.streamEvents`. This channel does not reimplement reconnect: the
 * SDK's `streamEvents` already resumes a dropped connection with `?from_seq` and merges
 * the result into one gap-free, duplicate-free sequence (see `sdks/typescript/src/client.ts`,
 * `generate()`); this channel's job is to surface that merged sequence as a signal and to
 * drive the connection pill's state machine from the only evidence the SDK's public surface
 * exposes: an event was actually yielded, or the stream actually reached its terminal frame,
 * or the stream actually gave up after its retry budget.
 */
export interface RunEventsChannel {
  readonly runId: string;
  /** The pill's sole authority for this channel. Read-only by construction: see
   * `connection-state.ts`; nothing on this interface can set it. */
  readonly state: Signal<ConnectionState>;
  /** Every event received so far, in sequence order, gap-free and duplicate-free across
   * any reconnect the SDK performed underneath. */
  readonly events: Signal<SalvorEvent[]>;
  /** The terminal frame, once the stream reaches its resting point. */
  readonly end: Signal<EndFrame | undefined>;
  readonly error: Signal<string | undefined>;
  /** Begin (or resume, after a `disconnect()`) consuming the stream from the next
   * sequence this channel has not yet seen. Idempotent while already connected. */
  connect(): void;
  /** Stop consuming. `events` and `end` are left as they were; `state` returns to
   * `Snapshot` since a disconnected channel makes no freshness claim. */
  disconnect(): void;
}

class RunEventsChannelImpl implements RunEventsChannel {
  readonly runId: string;
  readonly state: Signal<ConnectionState>;
  readonly events: Signal<SalvorEvent[]>;
  readonly end: Signal<EndFrame | undefined>;
  readonly error: Signal<string | undefined>;

  private readonly machine = createConnectionStateMachine();
  private readonly _events = signal<SalvorEvent[]>([]);
  private readonly _end = signal<EndFrame | undefined>(undefined);
  private readonly _error = signal<string | undefined>(undefined);

  private nextSeq: number;
  private activeIterator: AsyncIterator<SalvorEvent> | undefined;

  constructor(
    private readonly client: SalvorClient,
    runId: string,
    fromSeq: number,
  ) {
    this.runId = runId;
    this.nextSeq = fromSeq;
    this.state = this.machine.state;
    this.events = this._events.asReadonly();
    this.end = this._end.asReadonly();
    this.error = this._error.asReadonly();
  }

  connect(): void {
    if (this.activeIterator) return;
    this._error.set(undefined);
    const stream = this.client.streamEvents(this.runId, { fromSeq: this.nextSeq });
    const iterator = stream[Symbol.asyncIterator]();
    this.activeIterator = iterator;
    void this.consume(stream, iterator);
  }

  disconnect(): void {
    const iterator = this.activeIterator;
    this.activeIterator = undefined;
    this.machine.driver.toSnapshot();
    // Best-effort: the SDK exposes no abort handle, but returning the async iterator
    // unwinds its generator body (releasing the underlying stream reader) per the
    // language's own async-iterator protocol.
    if (iterator?.return) {
      void iterator.return().catch(() => {
        /* best-effort unwind; the channel is already reporting Snapshot regardless */
      });
    }
  }

  private async consume(stream: EventStream, iterator: AsyncIterator<SalvorEvent>): Promise<void> {
    const isCurrent = () => this.activeIterator === iterator;
    try {
      for (;;) {
        const result = await iterator.next();
        if (!isCurrent()) return; // a disconnect() won the race
        if (result.done) break;
        const event = result.value;
        this.nextSeq = event.seq + 1;
        this._events.update((events) => [...events, event]);
        if (this.machine.state().kind !== 'connected') this.machine.driver.toConnected();
      }
      if (!isCurrent()) return;
      this._end.set(stream.end);
      this.machine.driver.toEnded(stream.end?.detached ?? false);
    } catch (err) {
      if (!isCurrent()) return;
      this._error.set(errorMessage(err));
      this.machine.driver.toSnapshot();
    } finally {
      if (isCurrent()) this.activeIterator = undefined;
    }
  }
}

@Injectable({ providedIn: 'root' })
export class RunEventsService {
  private readonly client = inject(SALVOR_CLIENT);

  /**
   * Open a channel over one run's event stream, starting at `fromSeq` (default 0: a full
   * replay-then-tail, matching the server's own default). The channel is inert until
   * `connect()` is called, so a caller wires up its signals first.
   */
  open(runId: string, options: { fromSeq?: number } = {}): RunEventsChannel {
    return new RunEventsChannelImpl(this.client, runId, options.fromSeq ?? 0);
  }
}

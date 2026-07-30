import { type Signal, signal } from '@angular/core';

/**
 * The connection pill's state, and its ONLY authority.
 *
 * Ported invariant (design-audit-2026-07-16.md, "Ended/Live/Snapshot pill driven by real
 * transport state": mislabeling was already a fixed regression once). Two transport
 * surfaces exist in the API layer, each honest about what it actually is:
 *
 *   - The SSE event stream (`run-events.ts`) is either not yet subscribed (`Snapshot`,
 *     a static REST read), actively receiving frames over an open connection (`Live`),
 *     or has reached its terminal `end` frame (`Ended`).
 *   - The client-driven open-by-id fallback (`client-run.ts`) has NO server push at all:
 *     it can only poll `log(fromSeq)` on an interval, so it is never honestly `Live`;
 *     while it is actively polling that is labeled `Polling`, not faked as `Live`.
 *
 * `Polling` is therefore reachable only from the client-run channel, and `Live` only from
 * the events channel; both channels share this one union and this one machine shape so a
 * consumer (a future pill component) renders one type, not two near-duplicates.
 */
export type ConnectionState =
  | { readonly kind: 'idle'; readonly label: 'Snapshot'; readonly asOf: string }
  | { readonly kind: 'connected'; readonly label: 'Live'; readonly since: string }
  | {
      readonly kind: 'ended';
      readonly label: 'Ended';
      readonly at: string;
      readonly detached: boolean;
    }
  | { readonly kind: 'polling'; readonly label: 'Polling'; readonly lastPolledAt: string };

export type ConnectionKind = ConnectionState['kind'];

function nowIso(): string {
  return new Date().toISOString();
}

function snapshotState(asOf?: string): ConnectionState {
  return { kind: 'idle', label: 'Snapshot', asOf: asOf ?? nowIso() };
}

/**
 * The transition surface for one connection state machine instance. This interface is
 * deliberately never exposed on a public class field: see {@link createConnectionStateMachine}.
 * Every method name is a specific, evidenced transition (not a generic setter), and every
 * call site of a `ConnectionDriver` in this codebase is a line that just observed real
 * transport activity (a frame arrived, the terminal frame closed the stream, a poll
 * request resolved). UI code never sees this interface at all.
 */
export interface ConnectionDriver {
  /** No active subscription (yet, or no longer): a static snapshot as of `asOf` (default: now). */
  toSnapshot(asOf?: string): void;
  /** An SSE connection is open and has just delivered a frame. */
  toConnected(): void;
  /** The stream (SSE or polled log) reached a real terminal point. */
  toEnded(detached: boolean): void;
  /** A poll of the client-run log just completed successfully. */
  toPolling(): void;
}

/**
 * Construct one connection state machine: a readonly {@link Signal} for every reader, and a
 * {@link ConnectionDriver} held ONLY by the one channel that owns this instance
 * (`RunEventsChannel` or `ClientRunChannel`). There is no public setter anywhere in this
 * module: `state` is typed `Signal<ConnectionState>`, which has no `.set`/`.update` at
 * the type level, and `driver` is never re-exported by the owning channel. The two are
 * returned together, once, at construction, specifically so nothing outside the owning
 * channel can ever hold both halves.
 */
export function createConnectionStateMachine(): {
  state: Signal<ConnectionState>;
  driver: ConnectionDriver;
} {
  const state = signal<ConnectionState>(snapshotState());
  const driver: ConnectionDriver = {
    toSnapshot(asOf) {
      state.set(snapshotState(asOf));
    },
    toConnected() {
      state.set({ kind: 'connected', label: 'Live', since: nowIso() });
    },
    toEnded(detached) {
      state.set({ kind: 'ended', label: 'Ended', at: nowIso(), detached });
    },
    toPolling() {
      state.set({ kind: 'polling', label: 'Polling', lastPolledAt: nowIso() });
    },
  };
  return { state: state.asReadonly(), driver };
}

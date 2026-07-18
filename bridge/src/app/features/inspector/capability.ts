import { InjectionToken } from '@angular/core';

/**
 * What the control plane advertises it can do. So the Inspector's "Fork this run…" offer is
 * CAPABILITY-GATED: the code path exists and is exercised, but it renders only when
 * {@link ServerCapabilities.fork} is true.
 *
 * The v0.4 server now genuinely answers `GET /v1/capabilities`, and a REAL probe of it exists —
 * `core/api/capabilities.ts`'s `CapabilityProbeService`. This token stays pinned to
 * {@link NO_FORK_CAPABILITIES} regardless: the offer needs a fork-POINT to fork FROM, and that
 * picker lives on the graph canvas, which has not shipped yet. Wiring a true probe result to this
 * token before the canvas exists would light a "Fork this run…" button with nowhere to send the
 * operator — a dead end, not a feature. The canvas is what flips this token over to the real
 * probe, with no redesign here.
 */
export interface ServerCapabilities {
  /** The server exposes a fork API (`POST /v1/runs/{id}/fork`). False today. */
  readonly fork: boolean;
}

/** The honest default for this build: no fork runtime behind the offer. */
export const NO_FORK_CAPABILITIES: ServerCapabilities = { fork: false };

/**
 * The gate itself, as a pure function so it can be unit-tested both ways independently of Angular.
 * The offer is shown only when fork is genuinely advertised — never inferred, never defaulted on.
 */
export function forkOffered(caps: ServerCapabilities): boolean {
  return caps.fork === true;
}

/**
 * Injected capabilities, so a future surface (or a test) can supply a fork-advertising server without
 * touching the Inspector. Defaults to {@link NO_FORK_CAPABILITIES}.
 */
export const SERVER_CAPABILITIES = new InjectionToken<ServerCapabilities>('SERVER_CAPABILITIES', {
  providedIn: 'root',
  factory: () => NO_FORK_CAPABILITIES,
});

import { InjectionToken } from '@angular/core';

/**
 * What the control plane advertises it can do. Today this build talks to a real Salvor server that
 * has NO fork API — forking is a v0.4 plan (see the M16.A decomposition, "the server has no graph
 * engine, no fork API"). So the Inspector's "Fork this run…" offer is CAPABILITY-GATED: the code
 * path exists and is exercised, but it renders only when {@link ServerCapabilities.fork} is true,
 * which in production means never — until v0.4 flips this one flag and lights the path up with no
 * redesign.
 */
export interface ServerCapabilities {
  /** The server exposes a fork API (`POST /v1/runs/{id}/forks` or equivalent). False today. */
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

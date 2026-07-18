import { InjectionToken, type Signal, inject } from '@angular/core';

import { CapabilityProbeService } from '../../core/api';

/**
 * What the control plane advertises it can do. The Inspector's "Fork this run…" offer is
 * CAPABILITY-GATED: it renders only when {@link ServerCapabilities.fork} is true.
 *
 * Since the canvas landed, this token is wired to the REAL probe of `GET /v1/capabilities`
 * (`core/api/capabilities.ts`'s {@link CapabilityProbeService}): the graph canvas now exists, so
 * a fork offer has somewhere to send the operator. The probe degrades honestly — an unreachable
 * or pre-v0.4 control plane resolves to `{ fork: false }` with nothing thrown and nothing logged,
 * and the offer simply stays hidden.
 */
export interface ServerCapabilities {
  /** The server exposes a fork API (`POST /v1/runs/{id}/fork`). */
  readonly fork: boolean;
}

/** The honest default before a probe resolves, and after a failed one: no fork runtime assumed. */
export const NO_FORK_CAPABILITIES: ServerCapabilities = { fork: false };

/**
 * The gate itself, as a pure function so it can be unit-tested both ways independently of Angular.
 * The offer is shown only when fork is genuinely advertised — never inferred, never defaulted on.
 */
export function forkOffered(caps: ServerCapabilities): boolean {
  return caps.fork === true;
}

/**
 * Injected capabilities, as a SIGNAL: the probe answers asynchronously, and a consumer reading a
 * one-shot snapshot would freeze the pre-probe default forever. The factory also fires the probe,
 * so injecting the token is enough — no consumer has to remember to ask.
 */
export const SERVER_CAPABILITIES = new InjectionToken<Signal<ServerCapabilities>>('SERVER_CAPABILITIES', {
  providedIn: 'root',
  factory: () => {
    const probe = inject(CapabilityProbeService);
    void probe.probe();
    return probe.capabilities;
  },
});

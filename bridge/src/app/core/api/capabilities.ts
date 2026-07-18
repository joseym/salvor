import { Injectable, type Signal, inject, signal } from '@angular/core';

import { SALVOR_API_CONFIG, SALVOR_CLIENT, type SalvorApiConfig } from './client';
import { graphRequest } from './graph-http';
import { type CapabilityProbe, parseCapabilityProbe } from './graph-types';

/** The honest default before a probe resolves, and the honest fallback when one fails: no
 * capability is ever assumed. */
export const UNPROBED_CAPABILITIES: CapabilityProbe = { fork: false, raw: {} };

/**
 * The REAL probe of `GET /v1/capabilities` (`API.md`): "what this build of the control plane can
 * do, for a dashboard to probe before offering a capability-gated action." This is a genuine
 * network call against a real endpoint, not a stand-in — v0.4's server always answers it, and a
 * pre-v0.4 or unreachable server degrades honestly (see below), never with a fabricated `true`.
 *
 * CONSTRAINT: this service's result is intentionally NOT wired to the Inspector's fork offer yet.
 * `features/inspector/capability.ts`'s `SERVER_CAPABILITIES` token stays pinned to
 * `NO_FORK_CAPABILITIES` until the canvas's fork-point picker exists — lighting the offer
 * before there is a node to fork FROM would be a dead-end button. See that file's own comment.
 *
 * Degrades honestly: any failure (no server, network error, malformed body, timeout) resolves to
 * `{ fork: false }` with nothing thrown and nothing logged to the console — an unreachable
 * control plane is not a console error here, it is simply a build with no advertised
 * capabilities, the same posture {@link AgentRegistryService} takes toward an unregistered hash.
 */
@Injectable({ providedIn: 'root' })
export class CapabilityProbeService {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly config: SalvorApiConfig | undefined = inject(SALVOR_API_CONFIG, { optional: true }) ?? undefined;

  private readonly _capabilities = signal<CapabilityProbe>(UNPROBED_CAPABILITIES);
  private readonly _probed = signal(false);

  readonly capabilities: Signal<CapabilityProbe> = this._capabilities.asReadonly();
  /** Whether {@link probe} has resolved at least once — a real answer, or the honest degraded
   * default — distinct from the default value itself, so a caller can tell "not yet asked" from
   * "asked, and the server has no fork". */
  readonly probed: Signal<boolean> = this._probed.asReadonly();

  /** Probe the server once. Never throws and never logs — see the degradation note above. */
  async probe(): Promise<CapabilityProbe> {
    try {
      const obj = await graphRequest(this.client, this.config, 'GET', '/v1/capabilities');
      const caps = parseCapabilityProbe(obj);
      this._capabilities.set(caps);
      return caps;
    } catch {
      this._capabilities.set(UNPROBED_CAPABILITIES);
      return UNPROBED_CAPABILITIES;
    } finally {
      this._probed.set(true);
    }
  }
}

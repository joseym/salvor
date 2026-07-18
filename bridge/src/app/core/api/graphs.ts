import { Injectable, type Signal, inject, signal } from '@angular/core';
import type { Graph } from '@salvor/client';

import { SALVOR_API_CONFIG, SALVOR_CLIENT, type SalvorApiConfig } from './client';
import { errorMessage } from './errors';
import { graphRequest } from './graph-http';
import {
  type GraphDocumentRecord,
  type GraphSummary,
  type ValidateResult,
  parseGraphDocumentRecord,
  parseGraphSummary,
  parseValidateResult,
} from './graph-types';

/**
 * The graph catalog: `GET /v1/graphs` (list), `GET /v1/graphs/{hash}` (read one document back),
 * `POST /v1/graphs/validate` (submit's dry run, node/edge-precise — see `API.md`, "Graphs and
 * graph runs"). Graph SUBMISSION (`POST /v1/graphs`) is deliberately not part of this surface:
 * authoring a graph is the canvas's job; this service only reads what a graph author has
 * already stored, the same read-only posture `AgentRegistryService` takes toward the agent
 * registry.
 */
@Injectable({ providedIn: 'root' })
export class GraphsService {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly config: SalvorApiConfig | undefined = inject(SALVOR_API_CONFIG, { optional: true }) ?? undefined;

  private readonly _graphs = signal<GraphSummary[]>([]);
  private readonly _loading = signal(false);
  private readonly _error = signal<string | undefined>(undefined);

  readonly graphs: Signal<GraphSummary[]> = this._graphs.asReadonly();
  readonly loading: Signal<boolean> = this._loading.asReadonly();
  readonly error: Signal<string | undefined> = this._error.asReadonly();

  /** Re-fetch the full graph catalog. Throws on failure after recording `error`. */
  async refresh(): Promise<GraphSummary[]> {
    this._loading.set(true);
    this._error.set(undefined);
    try {
      const obj = await graphRequest(this.client, this.config, 'GET', '/v1/graphs');
      const graphs = ((obj['graphs'] as Record<string, unknown>[]) ?? []).map(parseGraphSummary);
      this._graphs.set(graphs);
      return graphs;
    } catch (err) {
      this._error.set(errorMessage(err));
      throw err;
    } finally {
      this._loading.set(false);
    }
  }

  /** Read one stored graph document back by hash. Throws `SalvorApiError` (`unknown_graph`) if
   * nothing is stored under it — the caller's problem to route, same as every other lookup here. */
  async get(hash: string): Promise<GraphDocumentRecord> {
    const obj = await graphRequest(this.client, this.config, 'GET', `/v1/graphs/${hash}`);
    return parseGraphDocumentRecord(obj);
  }

  /** Validate a document without storing it — the graph counterpart of `/replay`'s dry run.
   * Always resolves (`valid: true`, or `valid: false` with the full node/edge-precise error
   * list); it never throws for an invalid document, only for a genuine transport failure. */
  async validate(document: Graph): Promise<ValidateResult> {
    const obj = await graphRequest(this.client, this.config, 'POST', '/v1/graphs/validate', document);
    return parseValidateResult(obj);
  }
}

import { Injectable, type Signal, inject, signal } from '@angular/core';
import { SalvorApiError, type Labels } from '@salvor-run/client';

import { SALVOR_API_CONFIG, SALVOR_CLIENT, type SalvorApiConfig } from './client';
import { errorMessage } from './errors';
import { graphRequest } from './graph-http';
import {
  type ForkOutcome,
  type ForksIndex,
  type GraphProjection,
  type GraphRunStart,
  parseForkCreated,
  parseForkHazard,
  parseForkPreview,
  parseForksIndex,
  parseGraphProjection,
  parseGraphRunStart,
} from './graph-types';

/** The fields a fork request may carry. `acknowledgeWrites` defaults to `[]` server-side when
 * omitted; `dryRun` defaults to `false` server-side when omitted. */
export interface ForkRequest {
  readonly fromNode: string;
  readonly acknowledgeWrites?: readonly number[];
  readonly dryRun?: boolean;
}

/**
 * A graph run's server-side surface, past `POST /v1/graph-runs`: the per-node projection the
 * canvas will render (`GET /v1/runs/{id}/graph`), and the fork verbs (`POST /v1/runs/{id}/fork`,
 * `GET /v1/runs/{id}/forks`). Every OTHER run surface (status, events, resume) is already
 * served by the ordinary run services (`RunDetailService`, `RunEventsService`) unchanged, because
 * a graph run is an ordinary run with a richer log (`API.md`, "Graphs and graph runs": "all work
 * on it through their existing code"). This service exists only for what those cannot answer: the
 * graph-shaped facts.
 *
 * {@link fork} surfaces the `409 write_replay_hazard` refusal as a TYPED {@link ForkOutcome}
 * (`kind: 'hazard'`), never a thrown string: a hazard-review dialog (not yet built) needs
 * `details.writes` verbatim, not a caught exception's message. Every OTHER fork refusal
 * (`invalid_fork_node`, `origin_needs_reconciliation`, `not_a_graph_run`, `unknown_graph`) is not
 * given that treatment; it throws `SalvorApiError` uncaught, exactly this API layer's existing
 * convention for a refusal with nothing structured to render (see `RunDetailService.resume`'s
 * `NeedsReconciliationError`, which propagates the same way).
 */
@Injectable({ providedIn: 'root' })
export class GraphRunService {
  private readonly client = inject(SALVOR_CLIENT);
  private readonly config: SalvorApiConfig | undefined = inject(SALVOR_API_CONFIG, { optional: true }) ?? undefined;

  private readonly _projection = signal<GraphProjection | undefined>(undefined);
  private readonly _loading = signal(false);
  private readonly _error = signal<string | undefined>(undefined);

  readonly projection: Signal<GraphProjection | undefined> = this._projection.asReadonly();
  readonly loading: Signal<boolean> = this._loading.asReadonly();
  readonly error: Signal<string | undefined> = this._error.asReadonly();

  /** Start a run of a stored graph; returns its id at once (fire-and-return, the same shape
   * `POST /v1/runs` uses for an ordinary agent run). */
  async start(graphHash: string, input: unknown = null, options: { labels?: Labels } = {}): Promise<GraphRunStart> {
    const body: Record<string, unknown> = { graph_hash: graphHash, input };
    if (options.labels !== undefined) body['labels'] = options.labels;
    const obj = await graphRequest(this.client, this.config, 'POST', '/v1/graph-runs', body);
    return parseGraphRunStart(obj);
  }

  /** Fetch one graph run's per-node projection and make it the current {@link projection}. */
  async loadProjection(runId: string): Promise<GraphProjection> {
    this._loading.set(true);
    this._error.set(undefined);
    try {
      const obj = await graphRequest(this.client, this.config, 'GET', `/v1/runs/${runId}/graph`);
      const projection = parseGraphProjection(obj);
      this._projection.set(projection);
      return projection;
    } catch (err) {
      this._error.set(errorMessage(err));
      throw err;
    } finally {
      this._loading.set(false);
    }
  }

  /**
   * Fork a graph run from a node boundary. On a `dry_run` request the write-hazard question is
   * answered as part of the ordinary `200` preview (`kind: 'preview'`, per `API.md`: "the
   * structural refusals above still apply under dry_run"); on a REAL fork attempt, an
   * unacknowledged write refuses with `409 write_replay_hazard`, decoded here into
   * `kind: 'hazard'` instead of being thrown.
   */
  async fork(runId: string, request: ForkRequest): Promise<ForkOutcome> {
    const body: Record<string, unknown> = { from_node: request.fromNode };
    if (request.acknowledgeWrites !== undefined) body['acknowledge_writes'] = request.acknowledgeWrites;
    if (request.dryRun !== undefined) body['dry_run'] = request.dryRun;
    try {
      const obj = await graphRequest(this.client, this.config, 'POST', `/v1/runs/${runId}/fork`, body);
      return request.dryRun ? parseForkPreview(obj) : parseForkCreated(obj);
    } catch (err) {
      if (err instanceof SalvorApiError && err.code === 'write_replay_hazard') {
        return parseForkHazard(err.message, err.details);
      }
      throw err;
    }
  }

  /** The forks of a run, as the server's own derived index (`API.md`: "not a fact the origin
   * recorded"). */
  async listForks(runId: string): Promise<ForksIndex> {
    const obj = await graphRequest(this.client, this.config, 'GET', `/v1/runs/${runId}/forks`);
    return parseForksIndex(obj);
  }
}

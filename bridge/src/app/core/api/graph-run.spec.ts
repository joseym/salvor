import { TestBed } from '@angular/core/testing';
import { SalvorApiError, SalvorClient } from '@salvor-run/client';

import { SALVOR_CLIENT } from './client';
import { GraphRunService } from './graph-run';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('GraphRunService', () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
    TestBed.configureTestingModule({
      providers: [{ provide: SALVOR_CLIENT, useValue: new SalvorClient('http://test.local') }],
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  describe('start', () => {
    it('starts a graph run and returns its id at once', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ run: '6f...', status: 'running' }, 201));
      const service = TestBed.inject(GraphRunService);

      const result = await service.start('sha256:abc', { question: 'hi' });

      expect(result).toEqual({ run: '6f...', status: 'running', raw: { run: '6f...', status: 'running' } });
      const [, init] = fetchMock.mock.calls[0]!;
      expect(JSON.parse((init as RequestInit).body as string)).toEqual({
        graph_hash: 'sha256:abc',
        input: { question: 'hi' },
      });
    });
  });

  describe('loadProjection — GET /v1/runs/{id}/graph', () => {
    it('decodes a full projection, absent-vs-present per node', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          graph_hash: 'sha256:g1',
          current_node: 'approve',
          nodes: [
            { node: 'research', state: 'exited' },
            { node: 'approve', state: 'entered' },
            {
              node: 'reject',
              state: 'skipped',
              reason: 'no live inbound edge: an upstream branch routed to another case',
            },
          ],
        }),
      );
      const service = TestBed.inject(GraphRunService);

      const projection = await service.loadProjection('run-1');

      expect(projection).toBe(service.projection());
      expect(projection.graphHash).toBe('sha256:g1');
      expect(projection.currentNode).toBe('approve');
      expect(projection.forkedFrom).toBeUndefined();

      const [research, approve, reject] = projection.nodes;
      expect(research).toMatchObject({ node: 'research', state: 'exited' });
      expect(research!.reason).toBeUndefined();
      expect(research!.branchCase).toBeUndefined();
      expect(approve).toMatchObject({ node: 'approve', state: 'entered' });
      expect(reject!.reason).toContain('no live inbound edge');
    });

    it('ABSENT-KEY HANDLING: current_node and forked_from are undefined, never null or a fabricated value, when the server omits them', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({ graph_hash: 'sha256:g1', nodes: [{ node: 'start', state: 'exited' }] }),
      );
      const service = TestBed.inject(GraphRunService);

      const projection = await service.loadProjection('run-1');

      expect(projection.currentNode).toBeUndefined();
      expect(projection.forkedFrom).toBeUndefined();
    });

    it('decodes forked_from and a branch_case/map when the server records them', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          graph_hash: 'sha256:g1',
          nodes: [{ node: 'approve', state: 'exited', branch_case: 'yes', map: { fanned_out: 3 } }],
          forked_from: {
            run_id: 'origin-1',
            through_seq: 3,
            from_node: 'publish',
            graph_hash: 'sha256:g1',
            acknowledged_writes: [4],
          },
        }),
      );
      const service = TestBed.inject(GraphRunService);

      const projection = await service.loadProjection('run-2');

      expect(projection.nodes[0]!.branchCase).toBe('yes');
      expect(projection.nodes[0]!.map).toEqual({ fanned_out: 3 });
      // A fold marker decodes verbatim, alongside the map one, when recorded.
      const foldResponse = jsonResponse({
        graph_hash: 'sha256:g1',
        nodes: [
          {
            node: 'refine',
            state: 'exited',
            fold: {
              iterations: [
                { index: 0, joined: true },
                { index: 1, joined: true },
              ],
              converged: { winner_index: 1, reason: 'score >= threshold' },
            },
          },
        ],
      });
      fetchMock.mockResolvedValueOnce(foldResponse);
      const foldProjection = await service.loadProjection('run-3');
      expect(foldProjection.nodes[0]!.fold).toEqual({
        iterations: [
          { index: 0, joined: true },
          { index: 1, joined: true },
        ],
        converged: { winner_index: 1, reason: 'score >= threshold' },
      });
      expect(projection.forkedFrom).toEqual({
        runId: 'origin-1',
        throughSeq: 3,
        fromNode: 'publish',
        graphHash: 'sha256:g1',
        acknowledgedWrites: [4],
        raw: {
          run_id: 'origin-1',
          through_seq: 3,
          from_node: 'publish',
          graph_hash: 'sha256:g1',
          acknowledged_writes: [4],
        },
      });
    });

    it('records a typed error on 409 not_a_graph_run and rethrows', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({ error: { code: 'not_a_graph_run', message: 'this run has no graph' } }, 409),
      );
      const service = TestBed.inject(GraphRunService);

      await expect(service.loadProjection('run-3')).rejects.toThrow('this run has no graph');
      expect(service.error()).toContain('this run has no graph');
    });
  });

  describe('fork — the write_replay_hazard typed surface', () => {
    it('a REAL fork (not dry_run) that hits a 409 write_replay_hazard resolves to a typed hazard outcome, never a thrown string', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse(
          {
            error: {
              code: 'write_replay_hazard',
              message: 'forking run origin-1 would re-execute 1 recorded write(s) at seq 4',
              details: {
                writes: [
                  {
                    seq: 4,
                    tool: 'publish',
                    input: { doc: 'x' },
                    idempotency_key: null,
                    recorded_at: '2026-07-15T00:00:00Z',
                  },
                ],
              },
            },
          },
          409,
        ),
      );
      const service = TestBed.inject(GraphRunService);

      const outcome = await service.fork('origin-1', { fromNode: 'publish' });

      expect(outcome.kind).toBe('hazard');
      if (outcome.kind !== 'hazard') throw new Error('expected a hazard outcome');
      expect(outcome.code).toBe('write_replay_hazard');
      expect(outcome.message).toContain('would re-execute 1 recorded write');
      expect(outcome.writes).toEqual([
        {
          seq: 4,
          tool: 'publish',
          input: { doc: 'x' },
          idempotencyKey: null,
          recordedAt: '2026-07-15T00:00:00Z',
          raw: {
            seq: 4,
            tool: 'publish',
            input: { doc: 'x' },
            idempotency_key: null,
            recorded_at: '2026-07-15T00:00:00Z',
          },
        },
      ]);
    });

    it('a successful fork (writes acknowledged) resolves to a typed forked outcome', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse(
          {
            run: 'child-1',
            status: 'running',
            forked_from: {
              run_id: 'origin-1',
              through_seq: 3,
              from_node: 'publish',
              graph_hash: 'sha256:g1',
              acknowledged_writes: [4],
            },
          },
          201,
        ),
      );
      const service = TestBed.inject(GraphRunService);

      const outcome = await service.fork('origin-1', { fromNode: 'publish', acknowledgeWrites: [4] });

      expect(outcome).toMatchObject({ kind: 'forked', run: 'child-1', status: 'running' });
      const [, init] = fetchMock.mock.calls[0]!;
      expect(JSON.parse((init as RequestInit).body as string)).toEqual({
        from_node: 'publish',
        acknowledge_writes: [4],
      });
    });

    it('dry_run: true resolves to a typed preview, even when it reports it would not proceed — never thrown', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          dry_run: true,
          origin: 'origin-1',
          from_node: 'publish',
          through_seq: 3,
          graph_hash: 'sha256:g1',
          prefix_event_count: 4,
          writes: [
            { seq: 4, tool: 'publish', input: {}, idempotency_key: null, recorded_at: '2026-07-15T00:00:00Z' },
          ],
          unacknowledged_writes: [4],
          would_proceed: false,
        }),
      );
      const service = TestBed.inject(GraphRunService);

      const outcome = await service.fork('origin-1', { fromNode: 'publish', dryRun: true });

      expect(outcome).toMatchObject({
        kind: 'preview',
        origin: 'origin-1',
        fromNode: 'publish',
        throughSeq: 3,
        wouldProceed: false,
        unacknowledgedWrites: [4],
      });
      const [, init] = fetchMock.mock.calls[0]!;
      expect(JSON.parse((init as RequestInit).body as string)).toEqual({ from_node: 'publish', dry_run: true });
    });

    it('every OTHER fork refusal (e.g. invalid_fork_node) still throws uncaught, unlike write_replay_hazard', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({ error: { code: 'invalid_fork_node', message: 'origin never entered node ghost' } }, 409),
      );
      const service = TestBed.inject(GraphRunService);

      await expect(service.fork('origin-1', { fromNode: 'ghost' })).rejects.toMatchObject({
        code: 'invalid_fork_node',
      });
    });

    it('origin_needs_reconciliation also throws uncaught, carrying the intent in details', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse(
          {
            error: {
              code: 'origin_needs_reconciliation',
              message: 'origin is parked at a dangling write',
              details: { intent: { tool: 'charge', input: {} } },
            },
          },
          409,
        ),
      );
      const service = TestBed.inject(GraphRunService);

      let caught: unknown;
      try {
        await service.fork('origin-1', { fromNode: 'publish' });
      } catch (err) {
        caught = err;
      }
      expect(caught).toBeInstanceOf(SalvorApiError);
      expect((caught as SalvorApiError).code).toBe('origin_needs_reconciliation');
      expect((caught as SalvorApiError).details['intent']).toEqual({ tool: 'charge', input: {} });
    });
  });

  describe('listForks', () => {
    it('decodes the derived forks index', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          run: 'origin-1',
          derived: true,
          forks: [{ run: 'child-1', from_node: 'publish', through_seq: 3, acknowledged_writes: [4] }],
        }),
      );
      const service = TestBed.inject(GraphRunService);

      const index = await service.listForks('origin-1');

      expect(index.derived).toBe(true);
      expect(index.forks).toEqual([
        {
          run: 'child-1',
          fromNode: 'publish',
          throughSeq: 3,
          acknowledgedWrites: [4],
          raw: { run: 'child-1', from_node: 'publish', through_seq: 3, acknowledged_writes: [4] },
        },
      ]);
    });
  });
});

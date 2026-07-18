import { TestBed } from '@angular/core/testing';
import { GraphBuilder, SalvorClient } from '@salvor/client';

import { SALVOR_CLIENT } from './client';
import { GraphsService } from './graphs';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('GraphsService', () => {
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

  describe('refresh — GET /v1/graphs', () => {
    it('decodes the catalog and exposes it as a signal', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          graphs: [
            { graph: 'sha256:g1', node_count: 3, edge_count: 2, entry_nodes: ['research'], terminal_nodes: ['publish'] },
          ],
        }),
      );
      const service = TestBed.inject(GraphsService);
      expect(service.graphs()).toEqual([]);

      const result = await service.refresh();

      expect(result).toBe(service.graphs());
      expect(service.graphs()).toEqual([
        {
          graph: 'sha256:g1',
          nodeCount: 3,
          edgeCount: 2,
          entryNodes: ['research'],
          terminalNodes: ['publish'],
          raw: { graph: 'sha256:g1', node_count: 3, edge_count: 2, entry_nodes: ['research'], terminal_nodes: ['publish'] },
        },
      ]);
      expect(service.error()).toBeUndefined();
      expect(service.loading()).toBe(false);
    });

    it('records a typed error and rethrows on failure', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ error: { code: 'internal', message: 'boom' } }, 500));
      const service = TestBed.inject(GraphsService);

      await expect(service.refresh()).rejects.toThrow('boom');
      expect(service.error()).toContain('boom');
      expect(service.loading()).toBe(false);
    });
  });

  describe('get — GET /v1/graphs/{hash}', () => {
    it('reads one stored document back', async () => {
      const document = { schema_version: 1, nodes: [], edges: [] };
      fetchMock.mockResolvedValueOnce(jsonResponse({ graph: 'sha256:g1', document }));
      const service = TestBed.inject(GraphsService);

      const record = await service.get('sha256:g1');

      expect(record.graph).toBe('sha256:g1');
      expect(record.document).toEqual(document);
      const [url] = fetchMock.mock.calls[0]!;
      expect(url).toBe('http://test.local/v1/graphs/sha256:g1');
    });

    it('throws unknown_graph uncaught (404)', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({ error: { code: 'unknown_graph', message: 'no graph stored under that hash' } }, 404),
      );
      const service = TestBed.inject(GraphsService);

      await expect(service.get('sha256:missing')).rejects.toMatchObject({ code: 'unknown_graph' });
    });
  });

  describe('validate — POST /v1/graphs/validate', () => {
    it('a valid document resolves valid:true with the summary counts', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          valid: true,
          graph: 'sha256:g1',
          summary: { node_count: 3, edge_count: 2, entry_nodes: ['research'], terminal_nodes: ['publish'] },
        }),
      );
      const service = TestBed.inject(GraphsService);
      const document = new GraphBuilder()
        .tool('research', 'search')
        .tool('publish', 'post')
        .edge('research', 'publish')
        .build();

      const result = await service.validate(document);

      expect(result.valid).toBe(true);
      if (!result.valid) throw new Error('expected a valid result');
      expect(result.summary).toEqual({ nodeCount: 3, edgeCount: 2, entryNodes: ['research'], terminalNodes: ['publish'] });
      const [, init] = fetchMock.mock.calls[0]!;
      expect(JSON.parse((init as RequestInit).body as string)).toEqual(document);
    });

    it('an invalid document resolves valid:false with the full node/edge-precise error list — never throws', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          valid: false,
          errors: [
            {
              code: 'dangling_edge',
              message: 'edge `approve` -> `ghost` references unknown node id `ghost`',
              edge: { from: 'approve', to: 'ghost' },
              missing: 'ghost',
              suggestion: null,
            },
          ],
        }),
      );
      const service = TestBed.inject(GraphsService);

      const result = await service.validate({ schema_version: 1, nodes: [], edges: [] });

      expect(result.valid).toBe(false);
      if (result.valid) throw new Error('expected an invalid result');
      expect(result.errors).toHaveLength(1);
      expect(result.errors[0]).toMatchObject({
        code: 'dangling_edge',
        edge: { from: 'approve', to: 'ghost' },
        missing: 'ghost',
      });
      // an explicit null suggestion decodes to null, not undefined — absent-vs-null matters here
      expect(result.errors[0]!.suggestion).toBeNull();
    });

    it('a malformed-document refusal (one error, code malformed_document) still resolves, not throws', async () => {
      fetchMock.mockResolvedValueOnce(
        jsonResponse({
          valid: false,
          errors: [{ code: 'malformed_document', message: 'unknown field `bogus`' }],
        }),
      );
      const service = TestBed.inject(GraphsService);

      const result = await service.validate({ schema_version: 1, nodes: [], edges: [] });

      expect(result.valid).toBe(false);
      if (result.valid) throw new Error('expected an invalid result');
      expect(result.errors[0]!.code).toBe('malformed_document');
      expect(result.errors[0]!.edge).toBeUndefined();
      expect(result.errors[0]!.suggestion).toBeUndefined();
    });
  });
});

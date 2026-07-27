import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor-run/client';

import { SALVOR_CLIENT } from './client';
import { RunsService } from './runs';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('RunsService', () => {
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

  it('surfaces the e3182c5 enriched fields, honestly absent (not zero) when a log did not fold', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        runs: [
          {
            run: 'run-1',
            status: { state: 'completed', output: 42 },
            event_count: 10,
            first_recorded_at: '2026-01-01T00:00:00Z',
            last_recorded_at: '2026-01-01T00:00:05Z',
            usage: { input_tokens: 250, output_tokens: 50 },
            step_count: 2,
            agent_def_hash: 'sha256:deadbeef',
          },
          {
            // A run whose log could not fold: status AND the enriched fields are absent.
            run: 'run-2',
            event_count: 3,
            first_recorded_at: '2026-01-01T00:00:00Z',
            last_recorded_at: '2026-01-01T00:00:01Z',
          },
        ],
      }),
    );

    const service = TestBed.inject(RunsService);
    expect(service.loading()).toBe(false);
    expect(service.runs()).toEqual([]);

    const result = await service.refresh();

    expect(result).toBe(service.runs());
    expect(service.runs()).toHaveLength(2);
    const [first, second] = service.runs();
    expect(first!.usage).toEqual({ inputTokens: 250, outputTokens: 50 });
    expect(first!.stepCount).toBe(2);
    expect(first!.agentDefHash).toBe('sha256:deadbeef');
    expect(second!.usage).toBeUndefined();
    expect(second!.stepCount).toBeUndefined();
    expect(second!.agentDefHash).toBeUndefined();
    expect(service.error()).toBeUndefined();
    expect(service.lastLoadedAt()).toBeTruthy();
    expect(service.loading()).toBe(false);
  });

  it('records a typed error and rethrows on failure', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({ error: { code: 'internal', message: 'boom' } }, 500),
    );
    const service = TestBed.inject(RunsService);

    await expect(service.refresh()).rejects.toThrow('boom');
    expect(service.error()).toContain('boom');
    expect(service.loading()).toBe(false);
  });
});

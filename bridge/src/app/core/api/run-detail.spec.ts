import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor-run/client';

import { SALVOR_CLIENT } from './client';
import { RunDetailService } from './run-detail';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('RunDetailService', () => {
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

  it('loads one run and exposes it as a signal', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        run: 'run-1',
        status: { state: 'suspended', reason: 'needs input', input_schema: { type: 'object' } },
        event_count: 4,
      }),
    );
    const service = TestBed.inject(RunDetailService);

    const state = await service.load('run-1');

    expect(state).toBe(service.run());
    expect(service.run()?.status.state).toBe('suspended');
    expect(service.loading()).toBe(false);
    expect(service.error()).toBeUndefined();
  });

  it('resume re-derives the run so consequence re-rendering has fresh data', async () => {
    fetchMock
      // resume
      .mockResolvedValueOnce(jsonResponse({ run: 'run-1', outcome: 'driving', status: 'running' }))
      // the re-fetch resume triggers
      .mockResolvedValueOnce(
        jsonResponse({
          run: 'run-1',
          status: { state: 'running' },
          event_count: 5,
        }),
      );
    const service = TestBed.inject(RunDetailService);

    const result = await service.resume('run-1', { answer: 42 });

    expect(result.outcome).toBe('driving');
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(service.run()?.eventCount).toBe(5);
    expect(service.run()?.status.state).toBe('running');
  });

  it('resolve records the receipt state directly, no extra round trip', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({ run: 'run-1', status: { state: 'running' }, event_count: 6 }),
    );
    const service = TestBed.inject(RunDetailService);

    const state = await service.resolve('run-1', { charged: true });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(state.eventCount).toBe(6);
    expect(service.run()).toBe(state);
  });
});

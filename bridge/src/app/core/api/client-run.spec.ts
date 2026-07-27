import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor-run/client';

import { SALVOR_CLIENT } from './client';
import { ClientRunService } from './client-run';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function envelope(seq: number, kind: string): Record<string, unknown> {
  return {
    run_id: 'cr-1',
    seq,
    schema_version: 1,
    recorded_at: '2026-01-01T00:00:00Z',
    event: { kind, payload: {} },
  };
}

async function waitFor(assertion: () => void, timeoutMs = 4000): Promise<void> {
  const start = Date.now();
  for (;;) {
    try {
      assertion();
      return;
    } catch (err) {
      if (Date.now() - start > timeoutMs) throw err;
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }
}

describe('ClientRunService (open-by-id fallback)', () => {
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

  it('polls (never claims Live, since a client-driven run has no SSE) and settles Ended on a terminal event', async () => {
    fetchMock
      .mockResolvedValueOnce(
        jsonResponse({ run: 'cr-1', drive_token: 'tok-1', log: [envelope(0, 'RunStarted')] }),
      )
      .mockResolvedValueOnce(jsonResponse({ log: [] }))
      .mockResolvedValueOnce(jsonResponse({ log: [envelope(1, 'RunCompleted')] }));

    const service = TestBed.inject(ClientRunService);
    const channel = await service.openById('cr-1');
    expect(channel.runId).toBe('cr-1');
    expect(channel.events()).toHaveLength(1);
    expect(channel.state().kind).toBe('idle');

    const observedKinds = new Set<string>([channel.state().kind]);
    channel.start(5);

    await waitFor(() => expect(channel.state().kind).toBe('polling'));
    observedKinds.add(channel.state().kind);
    expect(channel.state().label).toBe('Polling');

    await waitFor(() => expect(channel.state().kind).toBe('ended'));
    observedKinds.add(channel.state().kind);

    expect(channel.events().map((e) => e.seq)).toEqual([0, 1]);
    expect(channel.state()).toEqual(expect.objectContaining({ kind: 'ended', label: 'Ended' }));
    // Architectural guarantee, not just this run's luck: client-run.ts never calls
    // `driver.toConnected()` anywhere, so 'connected'/Live is unreachable from this path.
    expect(observedKinds.has('connected')).toBe(false);
  });

  it('stop() returns the pill to Snapshot and halts further polling', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse({ run: 'cr-1', drive_token: 'tok-1', log: [] }))
      .mockResolvedValue(jsonResponse({ log: [] }));

    const service = TestBed.inject(ClientRunService);
    const channel = await service.openById('cr-1');
    channel.start(5);
    await waitFor(() => expect(channel.state().kind).toBe('polling'));

    const callsAtStop = fetchMock.mock.calls.length;
    channel.stop();
    expect(channel.state().kind).toBe('idle');

    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(fetchMock.mock.calls.length).toBe(callsAtStop); // no further polling after stop()
  });
});

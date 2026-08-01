import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor-run/client';

import { SALVOR_CLIENT } from './client';
import { RunEventsService } from './run-events';

function envelope(seq: number, kind = 'ToolCallCompleted'): Record<string, unknown> {
  return {
    run_id: 'run-1',
    seq,
    schema_version: 1,
    recorded_at: '2026-01-01T00:00:00Z',
    event: { kind, payload: {} },
  };
}

function dataFrame(seq: number, kind?: string): string {
  return `data: ${JSON.stringify(envelope(seq, kind))}\n\n`;
}

function endFrame(status: unknown, detached = false): string {
  return `event: end\ndata: ${JSON.stringify({ status, detached })}\n\n`;
}

/** A one-shot SSE body: enqueues every frame then closes the stream, simulating either a
 * clean server close (when the last frame is an `end` frame) or a dropped connection
 * (when it just stops, which is exactly what a mid-tail network drop looks like to a
 * `fetch` reader: `done: true` with no `event: end` ever seen). */
function sseBody(...frames: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  const bytes = encoder.encode(frames.join(''));
  return new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
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

describe('RunEventsService (reconnect)', () => {
  let fetchMock: ReturnType<typeof vi.fn>;
  let requestedUrls: string[];

  beforeEach(() => {
    requestedUrls = [];
    fetchMock = vi.fn(async (input: string | URL) => {
      const url = input.toString();
      requestedUrls.push(url);
      if (requestedUrls.length === 1) {
        // First connect (?from_seq=0): two events, then the raw byte stream just stops:
        // no `event: end` frame. That is what a dropped connection looks like to the
        // reader, and it is the ONLY way this test induces a drop: no internal SDK hook
        // is touched, the SDK's own `streamEvents` retry loop is exercised unmodified.
        return new Response(sseBody(dataFrame(0), dataFrame(1)), { status: 200 });
      }
      // The SDK's reconnect: must ask for exactly the next unseen sequence.
      expect(url).toContain('from_seq=2');
      return new Response(sseBody(dataFrame(2), endFrame({ state: 'completed', output: null })), {
        status: 200,
      });
    });
    vi.stubGlobal('fetch', fetchMock);
    TestBed.configureTestingModule({
      providers: [{ provide: SALVOR_CLIENT, useValue: new SalvorClient('http://test.local') }],
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('reconnects gap-free and duplicate-free after a mid-stream drop, and the pill tracks real transport only', async () => {
    const service = TestBed.inject(RunEventsService);
    const channel = service.open('run-1');

    expect(channel.state().kind).toBe('idle');
    expect(channel.state().label).toBe('Snapshot');

    channel.connect();

    // Evidence-driven: Live only after a real frame arrived.
    await waitFor(() => expect(channel.state().kind).toBe('connected'));
    expect(channel.state().label).toBe('Live');

    // The drop is invisible to this channel: the SDK's own reconnect absorbs it, so the
    // pill does not flicker back to Snapshot on a transient drop it successfully recovered
    // from. It settles Ended only once the real terminal frame arrives.
    await waitFor(() => expect(channel.state().kind).toBe('ended'));

    expect(channel.events().map((e) => e.seq)).toEqual([0, 1, 2]);
    expect(new Set(channel.events().map((e) => e.seq)).size).toBe(3); // no duplicates
    expect(requestedUrls).toHaveLength(2); // one drop, one reconnect
    expect(requestedUrls[0]).toContain('from_seq=0');
    expect(requestedUrls[1]).toContain('from_seq=2');

    expect(channel.state()).toEqual(
      expect.objectContaining({ kind: 'ended', label: 'Ended', detached: false }),
    );
    expect(channel.end()?.status?.state).toBe('completed');
  });

  it('disconnect() returns the pill to Snapshot and a late resolution cannot resurrect Live (no drift after disconnect)', async () => {
    const service = TestBed.inject(RunEventsService);
    const channel = service.open('run-1');
    channel.connect();

    await waitFor(() => expect(channel.state().kind).toBe('connected'));
    channel.disconnect();

    expect(channel.state().kind).toBe('idle');
    expect(channel.state().label).toBe('Snapshot');

    // Give the in-flight reconnect (already scheduled before disconnect()) time to settle;
    // it must never overwrite the state a deliberate disconnect already set.
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(channel.state().kind).toBe('idle');
  });
});

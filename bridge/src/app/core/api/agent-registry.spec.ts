import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor/client';

import { SALVOR_CLIENT } from './client';
import { AgentRegistryService } from './agent-registry';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('AgentRegistryService', () => {
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

  async function flush(): Promise<void> {
    for (let i = 0; i < 5; i++) await Promise.resolve();
  }

  it('resolves a JSON-format definition carrying a top-level name', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        agent: 'sha256:abc123',
        format: 'json',
        definition: JSON.stringify({ model: 'claude-sonnet-4-5', name: 'support-triage' }),
      }),
    );
    const service = TestBed.inject(AgentRegistryService);

    service.resolve(['sha256:abc123']);
    await flush();

    expect(service.names().get('sha256:abc123')).toBe('support-triage');
  });

  it('never scrapes a TOML definition for a name (deny_unknown_fields means the real schema has none, and pattern-matching would risk pulling a nested [[wasm_tools]] name)', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        agent: 'sha256:def456',
        format: 'toml',
        definition: 'model = "claude-sonnet-4-5"\n\n[[wasm_tools]]\nname = "wordcount"\n',
      }),
    );
    const service = TestBed.inject(AgentRegistryService);

    service.resolve(['sha256:def456']);
    await flush();

    expect(service.names().has('sha256:def456')).toBe(false);
  });

  it('a registered hash with no name field stays unresolved, honestly (registration alone is not a name)', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        agent: 'sha256:aaa111',
        format: 'json',
        definition: JSON.stringify({ model: 'claude-sonnet-4-5' }),
      }),
    );
    const service = TestBed.inject(AgentRegistryService);

    service.resolve(['sha256:aaa111']);
    await flush();

    expect(service.names().has('sha256:aaa111')).toBe(false);
  });

  it('an unregistered hash (404) fails silently — no name, no thrown error', async () => {
    fetchMock.mockResolvedValueOnce(
      new Response(JSON.stringify({ error: { code: 'unknown_agent', message: 'no agent registered' } }), {
        status: 404,
        headers: { 'Content-Type': 'application/json' },
      }),
    );
    const service = TestBed.inject(AgentRegistryService);

    expect(() => service.resolve(['sha256:zzz999'])).not.toThrow();
    await flush();

    expect(service.names().has('sha256:zzz999')).toBe(false);
  });

  it('caches per hash: a second resolve() for the same hash issues no second fetch', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({
        agent: 'sha256:cached1',
        format: 'json',
        definition: JSON.stringify({ name: 'daily-digest' }),
      }),
    );
    const service = TestBed.inject(AgentRegistryService);

    service.resolve(['sha256:cached1']);
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(1);

    service.resolve(['sha256:cached1']);
    await flush();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(service.names().get('sha256:cached1')).toBe('daily-digest');
  });

  it('batches distinct hashes into one fetch each, concurrently', async () => {
    fetchMock.mockImplementation((input: unknown) => {
      const url = String(input);
      const hash = url.split('/').pop();
      return Promise.resolve(
        jsonResponse({ agent: hash, format: 'json', definition: JSON.stringify({ name: `agent-${hash}` }) }),
      );
    });
    const service = TestBed.inject(AgentRegistryService);

    service.resolve(['sha256:one', 'sha256:two', 'sha256:three']);
    await flush();

    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(service.names().get('sha256:one')).toBe('agent-sha256:one');
    expect(service.names().get('sha256:two')).toBe('agent-sha256:two');
    expect(service.names().get('sha256:three')).toBe('agent-sha256:three');
  });
});

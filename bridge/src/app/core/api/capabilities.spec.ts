import { TestBed } from '@angular/core/testing';
import { SalvorClient } from '@salvor/client';

import { SALVOR_CLIENT } from './client';
import { CapabilityProbeService, UNPROBED_CAPABILITIES } from './capabilities';

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('CapabilityProbeService — the real GET /v1/capabilities probe', () => {
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

  it('starts unprobed, with the honest fork:false default', () => {
    const service = TestBed.inject(CapabilityProbeService);
    expect(service.probed()).toBe(false);
    expect(service.capabilities()).toEqual(UNPROBED_CAPABILITIES);
  });

  it('is a real network call: it hits GET /v1/capabilities and surfaces a genuine fork:true', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ capabilities: { fork: true } }));
    const service = TestBed.inject(CapabilityProbeService);

    const result = await service.probe();

    expect(fetchMock).toHaveBeenCalledWith('http://test.local/v1/capabilities', expect.anything());
    expect(result.fork).toBe(true);
    expect(service.capabilities().fork).toBe(true);
    expect(service.probed()).toBe(true);
  });

  it('DEGRADATION: a network failure (no server, e.g. the fixture/no-server target) resolves to fork:false with no throw and no console error', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
    fetchMock.mockRejectedValueOnce(new TypeError('fetch failed'));
    const service = TestBed.inject(CapabilityProbeService);

    await expect(service.probe()).resolves.toEqual(UNPROBED_CAPABILITIES);

    expect(service.capabilities()).toEqual(UNPROBED_CAPABILITIES);
    expect(service.probed()).toBe(true);
    expect(consoleError).not.toHaveBeenCalled();
    consoleError.mockRestore();
  });

  it('DEGRADATION: a non-2xx (e.g. a pre-v0.4 server with no capabilities route) resolves to fork:false silently', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
    fetchMock.mockResolvedValueOnce(new Response('not found', { status: 404 }));
    const service = TestBed.inject(CapabilityProbeService);

    const result = await service.probe();

    expect(result).toEqual(UNPROBED_CAPABILITIES);
    expect(consoleError).not.toHaveBeenCalled();
    consoleError.mockRestore();
  });

  it('DEGRADATION: a malformed body resolves to fork:false silently', async () => {
    fetchMock.mockResolvedValueOnce(new Response('not json{{', { status: 200 }));
    const service = TestBed.inject(CapabilityProbeService);

    await expect(service.probe()).resolves.toEqual(UNPROBED_CAPABILITIES);
  });

  it('never fabricates fork:true — a capabilities object missing fork decodes to false', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ capabilities: {} }));
    const service = TestBed.inject(CapabilityProbeService);

    const result = await service.probe();

    expect(result.fork).toBe(false);
  });

  it('parses the server build block, commit included, when the server sends it', async () => {
    fetchMock.mockResolvedValueOnce(
      jsonResponse({ capabilities: { fork: true }, server: { version: '0.1.0', commit: 'a1b2c3d' } }),
    );
    const service = TestBed.inject(CapabilityProbeService);

    const result = await service.probe();

    expect(result.server).toEqual({ version: '0.1.0', commit: 'a1b2c3d' });
  });

  it('parses the server build block with commit absent (a tarball or no-git build)', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ capabilities: { fork: true }, server: { version: '0.1.0' } }));
    const service = TestBed.inject(CapabilityProbeService);

    const result = await service.probe();

    expect(result.server).toEqual({ version: '0.1.0' });
    expect(result.server?.commit).toBeUndefined();
  });

  it('ABSENT-TOLERANT: a pre-server-block server (no `server` key at all) still decodes fork cleanly, with server left undefined', async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse({ capabilities: { fork: true } }));
    const service = TestBed.inject(CapabilityProbeService);

    const result = await service.probe();

    expect(result.fork).toBe(true);
    expect(result.server).toBeUndefined();
  });
});

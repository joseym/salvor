import { SalvorApiError, type SalvorClient } from '@salvor-run/client';

import type { SalvorApiConfig } from './client';

/**
 * A minimal raw-fetch primitive for the graph control-plane endpoints (`API.md`, "Graphs and
 * graph runs"). `@salvor-run/client` does not wrap these yet (the Bridge's typed graph API layer
 * ships ahead of the SDK gaining this surface), so `graphs.ts`, `graph-run.ts`, and
 * `capabilities.ts` talk to them directly over `fetch`, mirroring `SalvorClient`'s own private
 * `request()` byte-for-byte (same header/timeout/error-envelope handling) so a future SDK release
 * can absorb this without changing any call site's behavior.
 */
export async function graphRequest(
  client: SalvorClient,
  config: SalvorApiConfig | undefined,
  method: string,
  path: string,
  body?: unknown,
): Promise<Record<string, unknown>> {
  const controller = new AbortController();
  const timeoutMs = config?.timeoutMs ?? 30_000;
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const headers: Record<string, string> = {};
    if (config?.token) headers['Authorization'] = `Bearer ${config.token}`;
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const resp = await fetch(`${client.baseUrl}${path}`, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: controller.signal,
    });
    const text = await resp.text();
    const parsed = text ? (JSON.parse(text) as Record<string, unknown>) : {};
    if (!resp.ok) throw errorFromEnvelope(resp.status, parsed);
    return parsed;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Decode the control plane's one error-envelope shape (`{ error: { code, message, details } }`)
 * into the SDK's own `SalvorApiError`, so a caller here matches on `.code` exactly like every
 * other surface in this app does against `@salvor-run/client`'s own errors.
 */
function errorFromEnvelope(status: number, parsed: Record<string, unknown>): SalvorApiError {
  const envelope = (parsed['error'] as Record<string, unknown>) ?? {};
  const code = (envelope['code'] as string) ?? 'unknown';
  const message = (envelope['message'] as string) ?? `request failed with status ${status}`;
  const details = envelope['details'] as Record<string, unknown> | undefined;
  return new SalvorApiError(code, message, status, details);
}

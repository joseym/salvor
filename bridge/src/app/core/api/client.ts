import { InjectionToken, type Provider, inject } from '@angular/core';
import { SalvorClient, type SalvorClientOptions } from '@salvor-run/client';

/** Where the control plane lives, and how to authenticate to it. */
export interface SalvorApiConfig {
  readonly baseUrl: string;
  readonly token?: string;
  readonly timeoutMs?: number;
  readonly maxStreamRetries?: number;
}

/**
 * The API layer's one configuration point. A real deployment provides this via
 * {@link provideSalvorApi} in `app.config.ts`, once there is a shell to read a runtime base
 * URL from. Absent that, {@link SALVOR_CLIENT}'s
 * factory falls back to the SDK README's own local-dev default
 * (`http://127.0.0.1:8080`, the address the SDK's own example uses),
 * a placeholder for this service layer, not a production default.
 */
export const SALVOR_API_CONFIG = new InjectionToken<SalvorApiConfig>('SALVOR_API_CONFIG');

const LOCAL_DEV_DEFAULT: SalvorApiConfig = { baseUrl: 'http://127.0.0.1:8080' };

/** Register the control plane's location and auth for the whole app. */
export function provideSalvorApi(config: SalvorApiConfig): Provider {
  return { provide: SALVOR_API_CONFIG, useValue: config };
}

/**
 * The one {@link SalvorClient} instance the API layer shares. Every service in this
 * directory injects this token rather than constructing its own client, so a test can
 * substitute a fake by overriding the provider: none of the SDK's zero-dependency
 * `fetch` usage needs mocking at the module level.
 */
export const SALVOR_CLIENT = new InjectionToken<SalvorClient>('SALVOR_CLIENT', {
  providedIn: 'root',
  factory: (): SalvorClient => {
    const config = inject(SALVOR_API_CONFIG, { optional: true }) ?? LOCAL_DEV_DEFAULT;
    const options: SalvorClientOptions = {};
    if (config.token !== undefined) options.token = config.token;
    if (config.timeoutMs !== undefined) options.timeoutMs = config.timeoutMs;
    if (config.maxStreamRetries !== undefined) options.maxStreamRetries = config.maxStreamRetries;
    return new SalvorClient(config.baseUrl, options);
  },
});

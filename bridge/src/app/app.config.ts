import { ApplicationConfig, provideBrowserGlobalErrorListeners } from '@angular/core';
import { provideRouter, withInMemoryScrolling } from '@angular/router';

import { routes } from './app.routes';
import { provideSalvorApi } from './core/api';

/**
 * The control plane's base URL. Default is SAME-ORIGIN (''), so a deployment that serves the
 * built app and proxies `/v1` to `salvor serve` on one origin needs no CORS (the server ships
 * none — see e2e-serve.sh, which sets exactly this shape up for the Playwright suite). A page can
 * override it at runtime by setting `window.__SALVOR_API_BASE__` before the app boots.
 */
declare global {
  interface Window {
    __SALVOR_API_BASE__?: string;
  }
}
const apiBase = (typeof window !== 'undefined' && window.__SALVOR_API_BASE__) || '';

export const appConfig: ApplicationConfig = {
  providers: [
    provideBrowserGlobalErrorListeners(),
    provideRouter(routes, withInMemoryScrolling({ anchorScrolling: 'disabled' })),
    provideSalvorApi({ baseUrl: apiBase }),
  ],
};

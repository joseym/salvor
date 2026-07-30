import { Component, isDevMode } from '@angular/core';
import { Routes } from '@angular/router';

/**
 * The router drives URL state only; the shell (App) renders all five view sections at once and
 * toggles `.is-active` from {@link ViewService}. So every path resolves to this deliberately
 * empty sink: there is no `<router-outlet>` content to swap. The `data.view` on each route is
 * what ViewService reads to decide which section is active.
 *
 * Routes use PATH urls, not hash routing. Legacy hash deep links are redirected; see
 * ViewService.
 */
@Component({ selector: 'bridge-route-sink', template: '' })
export class RouteSink {}

export const routes: Routes = [
  { path: '', pathMatch: 'full', redirectTo: 'runs' },
  { path: 'runs', component: RouteSink, data: { view: 'runs' } },
  { path: 'inspector', component: RouteSink, data: { view: 'inspector' } },
  { path: 'inspector/:runId', component: RouteSink, data: { view: 'inspector' } },
  { path: 'inbox', component: RouteSink, data: { view: 'inbox' } },
  { path: 'workflows', component: RouteSink, data: { view: 'workflows' } },
  { path: 'workflows/:hashPrefix', component: RouteSink, data: { view: 'workflows' } },
  { path: 'spend', component: RouteSink, data: { view: 'spend' } },
  // Dev-only token-parity verification instrument. Absent from a
  // production `ng build` bundle: isDevMode() is defined away.
  ...(isDevMode()
    ? [
        {
          path: 'tokens',
          loadComponent: () => import('./features/token-parity/token-parity').then((m) => m.TokenParity),
        },
      ]
    : []),
  { path: '**', redirectTo: 'runs' },
];

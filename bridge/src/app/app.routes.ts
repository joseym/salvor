import { Routes } from '@angular/router';
import { isDevMode } from '@angular/core';

export const routes: Routes = [
  {
    path: '',
    loadComponent: () => import('./features/home/home').then((m) => m.Home),
  },
  // Dev-only token-parity verification instrument.
  // isDevMode() reflects the build configuration — Angular's production
  // build defines it away, so this route does not exist in a `ng build`
  // (production) bundle at all, not even lazily.
  ...(isDevMode()
    ? [
        {
          path: 'tokens',
          loadComponent: () => import('./features/token-parity/token-parity').then((m) => m.TokenParity),
        },
      ]
    : []),
];

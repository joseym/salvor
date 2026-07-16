import { Component } from '@angular/core';

/**
 * Placeholder shell content. S0 ships no app features — this is a stand-in
 * so the route table and shell chrome (nav, theme toggle) have somewhere
 * to render. Later work replaces this with the real nav + Runs default.
 */
@Component({
  selector: 'bridge-home',
  template: `
    <section class="mx-auto max-w-2xl px-6 py-16">
      <h1 class="text-2xl">Salvor Bridge</h1>
      <p class="mt-3 text-fg-2">
        Scaffold only: workspace, token system, and theme toggle.
        No app features ship yet.
      </p>
    </section>
  `,
})
export class Home {}

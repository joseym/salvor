import { Component, computed, input } from '@angular/core';

import { shortId } from './inbox-model';

/**
 * A tiny component for a truncated, copyable run id. Reuses `.runref`/`.runid`/
 * `.copy` — already global, shared with the Runs ledger (`app.css`) — so this
 * needs no styles of its own.
 */
@Component({
  selector: 'bridge-run-ref',
  imports: [],
  templateUrl: './run-ref.html',
})
export class RunRef {
  readonly id = input.required<string>();
  readonly short = computed(() => shortId(this.id()));

  async copy(ev: Event): Promise<void> {
    ev.stopPropagation();
    try {
      await navigator.clipboard.writeText(this.id());
    } catch {
      /* clipboard blocked — no fallback theater, matching the Runs ledger's own copy() */
    }
  }
}

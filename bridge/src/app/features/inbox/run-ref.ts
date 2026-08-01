import { Component, computed, input } from '@angular/core';

import { shortId } from './inbox-model';

/** How the value is shaped, so it truncates (or doesn't) the right way:
 *  - `id`    a run id: truncated to 8 chars.
 *  - `hash`  a content hash (`agent_def_hash`): the `sha256:` prefix kept, then 8 hex.
 *  - `label` a human label (a client-driven run's agent): shown in full, never truncated. */
export type RunRefKind = 'id' | 'hash' | 'label';

/**
 * A tiny component for a truncated, copyable reference: a run id by default, or (via `kind`) an
 * agent hash or label. Reuses `.runref`/`.runid`/`.agent-label`/`.copy`, already global, shared
 * with the Runs ledger (`app.css`), so this needs no styles of its own.
 */
@Component({
  selector: 'bridge-run-ref',
  imports: [],
  templateUrl: './run-ref.html',
})
export class RunRef {
  readonly id = input.required<string>();
  readonly kind = input<RunRefKind>('id');

  readonly short = computed(() => {
    const value = this.id();
    switch (this.kind()) {
      case 'hash':
        return 'sha256:' + value.replace(/^sha256:/, '').slice(0, 8);
      case 'label':
        return value;
      default:
        return shortId(value);
    }
  });
  readonly valueClass = computed(() => (this.kind() === 'label' ? 'agent-label' : 'runid'));

  async copy(ev: Event): Promise<void> {
    ev.stopPropagation();
    try {
      await navigator.clipboard.writeText(this.id());
    } catch {
      /* clipboard blocked: no fallback theater, matching the Runs ledger's own copy() */
    }
  }
}

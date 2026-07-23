import type { WfGraph } from './wf-model';

/**
 * THE DRAFT STORE. A draft is a graph the operator is authoring that has not been published, so it
 * has no hash and no server identity yet (the hash IS the version — see {@link WfGraph}). The
 * prototype modelled its one draft as an in-memory fixture; here drafts live in `localStorage`, so
 * an author's unpublished work survives a reload, and "Save draft" is a real local write rather
 * than a no-op. Published graphs never live here — those are the server's, read from `GET /v1/graphs`.
 *
 * The seeded `refund-sweep` draft is ported verbatim from the prototype fixture, defects and all
 * (a duplicate id, a dangling edge, a cycle): it is the draft the fork specs pick to prove the
 * "draft with zero runs" fork-menu state, and a real validator has something genuinely broken to
 * name. Seeding is idempotent: it fills an EMPTY store once and never overwrites an author's edits.
 */
const DRAFTS_KEY = 'salvor.wf.drafts';

/** The ported prototype draft — deliberately broken, one defect per validator error class: a
 * malformed agent hash, a duplicate node id, a zero-worker fan-out, a dangling edge one keystroke
 * from a real node, a cycle, and a case label on a non-branch edge. Six errors, exactly. */
export const REFUND_SWEEP_DRAFT: WfGraph = {
  key: 'draft:refund-sweep',
  hash: null,
  name: 'refund-sweep',
  state: 'draft',
  nodes: [
    { id: 'n_start', kind: 'agent', name: 'Pick the invoices', agentHash: 'sha256:not-a-real-hash' } /* malformed hash */,
    { id: 'n_fetch', kind: 'tool', name: 'Fetch the invoice', tool: 'lookup_invoice', effect: 'read', input: {} },
    { id: 'n_charge', kind: 'tool', name: 'Charge the card', tool: 'charge_card', effect: 'write', idempotencyKey: null, input: {} },
    { id: 'n_charge', kind: 'tool', name: 'Charge the card again', tool: 'charge_card', effect: 'write', idempotencyKey: null, input: {} } /* duplicate id */,
    { id: 'n_pick', kind: 'branch', name: 'Retry or give up', cases: ['retry', 'give_up'] },
    { id: 'n_fan', kind: 'map', name: 'Email each watcher', over: '${n_fetch.watchers}', concurrency: 0, body: { tool: 'send_email', effect: 'idempotent' } } /* fans out to nobody */,
    /* This hash used to pass the client's old 16-hex tolerance untouched — the padding is
       mechanical (sync-ledger item 4's own pattern), the same fix `complete_hash` offers, applied
       up front so the seeded defect count stays six, not seven, once the client requires the
       server's real 64-hex form. */
    { id: 'n_notify', kind: 'agent', name: 'Draft the closing note', agentHash: `sha256:${'9f2c41b7c0e5a8d3'.padEnd(64, '0')}` },
  ],
  edges: [
    { from: 'n_start', to: 'n_fetch' },
    { from: 'n_fetch', to: 'n_charge' },
    { from: 'n_charge', to: 'n_pick' },
    { from: 'n_pick', to: 'n_fan', label: 'retry' },
    { from: 'n_pick', to: 'n_notifyy', label: 'give_up' } /* dangling — one key off */,
    { from: 'n_fan', to: 'n_fetch' } /* closes a cycle */,
    { from: 'n_charge', to: 'n_notify', label: 'final' } /* case label, but not a branch */,
  ],
};

function readStore(): Record<string, WfGraph> {
  try {
    const raw = localStorage.getItem(DRAFTS_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Record<string, WfGraph>;
    return parsed && typeof parsed === 'object' ? parsed : {};
  } catch {
    return {};
  }
}

function writeStore(store: Record<string, WfGraph>): void {
  try {
    localStorage.setItem(DRAFTS_KEY, JSON.stringify(store));
  } catch {
    /* a locked-down embed has no storage — a failed persist is never a reason to crash the canvas */
  }
}

/** Every draft, seeding the ported `refund-sweep` once into an empty store (never overwriting). */
export function loadDrafts(): WfGraph[] {
  const store = readStore();
  if (Object.keys(store).length === 0) {
    store[REFUND_SWEEP_DRAFT.key] = REFUND_SWEEP_DRAFT;
    writeStore(store);
  }
  return Object.values(store);
}

/** Persist one draft under its key, returning the full draft list after the write. */
export function saveDraft(graph: WfGraph): WfGraph[] {
  const store = readStore();
  store[graph.key] = graph;
  writeStore(store);
  return Object.values(store);
}

/** Drop a draft (used after a publish promotes it to a server graph). */
export function removeDraft(key: string): WfGraph[] {
  const store = readStore();
  delete store[key];
  writeStore(store);
  return Object.values(store);
}

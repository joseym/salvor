import { describe, expect, it } from 'vitest';

import {
  STALL_GRACE_MS,
  agentIdentity,
  derivedStatus,
  groupOf,
  isHash,
  isWaiting,
  labelOf,
  overdueOf,
  overdueSince,
  toRunRow,
  waitingOnOf,
  type RunRow,
} from './run-model';
import type { RunStatus, RunSummary } from '@salvor-run/client';

function row(agentDefHash: string | undefined): RunRow {
  return { id: 'r1', status: 'completed', eventCount: 1, agentDefHash };
}
/** A run whose log the server folded (usage/step_count present): the "graph run" signal when it
 * also carries no agent_def_hash. */
function foldedRow(agentDefHash: string | undefined): RunRow {
  return { id: 'r1', status: 'completed', eventCount: 1, stepCount: 3, agentDefHash };
}

describe('isHash', () => {
  it('a sha256: value is hash-shaped', () => {
    expect(isHash('sha256:abc123')).toBe(true);
  });
  it('a caller-supplied readable label is not hash-shaped', () => {
    expect(isHash('aarg_jd_parser_v1')).toBe(false);
  });
});

describe('agentIdentity: the honest renderings', () => {
  it('no agent_def_hash AND an unfolded log: kind "none", a hyphen (nothing recorded to show)', () => {
    const id = agentIdentity(row(undefined));
    expect(id).toEqual({ text: '-', kind: 'none' });
  });

  it('no agent_def_hash but the log DID fold: kind "graph", "graph run" (a GraphRunStarted head)', () => {
    const id = agentIdentity(foldedRow(undefined));
    expect(id).toEqual({ text: 'graph run', kind: 'graph' });
  });

  it('the graph rendering carries no hash: GET /v1/runs carries nothing graph-shaped on the row', () => {
    const id = agentIdentity(foldedRow(undefined));
    expect(id.hash).toBeUndefined();
  });

  it('a folded run WITH an agent_def_hash is still an agent run, never "graph"', () => {
    const id = agentIdentity(foldedRow('sha256:e8a1d362'));
    expect(id.kind).toBe('hash');
  });

  it('a caller-supplied readable label (not hash-shaped): shown as-is, kind "label"', () => {
    const id = agentIdentity(row('aarg_jd_parser_v1'));
    expect(id).toEqual({ text: 'aarg_jd_parser_v1', kind: 'label', hash: 'aarg_jd_parser_v1' });
  });

  it('a hash-shaped agent_def_hash the registry resolved a name for: kind "name", hash rides along', () => {
    const names = new Map([['sha256:e8a1d362', 'support-triage']]);
    const id = agentIdentity(row('sha256:e8a1d362'), names);
    expect(id).toEqual({ text: 'support-triage', kind: 'name', hash: 'sha256:e8a1d362' });
  });

  it('a hash-shaped agent_def_hash with no resolved name (unregistered, or not yet looked up): kind "hash", the hash itself', () => {
    const id = agentIdentity(row('sha256:e8a1d362'));
    expect(id).toEqual({ text: 'sha256:e8a1d362', kind: 'hash', hash: 'sha256:e8a1d362' });
  });

  it('an unrelated resolved name in the map does not leak onto a different hash', () => {
    const names = new Map([['sha256:other', 'billing-reconciler']]);
    const id = agentIdentity(row('sha256:e8a1d362'), names);
    expect(id.kind).toBe('hash');
    expect(id.text).toBe('sha256:e8a1d362');
  });
});

describe('derivedStatus: in-progress + driverless + stale ⟹ stalled (the one derivation rule)', () => {
  const NOW = 1_000_000_000_000;
  const stale = new Date(NOW - STALL_GRACE_MS - 1).toISOString();
  const fresh = new Date(NOW - 1_000).toISOString();

  it('fires only when ALL THREE hold: in-progress fold, driver none, and last event stale', () => {
    expect(derivedStatus('running', 'none', stale, NOW)).toBe('stalled');
  });

  it('an ATTACHED driver is never stalled, however old the last event', () => {
    expect(derivedStatus('running', 'attached', stale, NOW)).toBe('running');
  });

  it('within the grace period a driverless running run is NOT yet stalled (the just-opened case)', () => {
    expect(derivedStatus('running', 'none', fresh, NOW)).toBe('running');
  });

  it('an ABSENT driver field is not evidence of a stall; never derived as one (honesty over a guess)', () => {
    expect(derivedStatus('running', undefined, stale, NOW)).toBe('running');
  });

  // THE REAL-WORLD SHAPE (defect A): the owner's abandoned client-runs died mid model-call, so
  // they fold to `awaiting_model`, never literal `running`. The whole IN-PROGRESS family (every
  // non-terminal resting state where a driver SHOULD be attached) must stall the same way.
  it('the entire in-progress family stalls the same way: running, awaiting_model, awaiting_tool, not_started', () => {
    for (const s of ['running', 'awaiting_model', 'awaiting_tool', 'not_started']) {
      expect(derivedStatus(s, 'none', stale, NOW)).toBe('stalled');
    }
  });

  // This is the failing-first proof: against the OLD rule (`state !== 'running'`), every one of
  // these, except plain `running`, passed straight through unchanged, so the owner's real
  // `awaiting_model` stalls never fired.
  it('a human-waiting state or a terminal state never stalls, however driverless and stale: it is a normal, honest wait', () => {
    for (const s of ['suspended', 'budget_exceeded', 'needs_reconciliation', 'completed', 'failed']) {
      expect(derivedStatus(s, 'none', stale, NOW)).toBe(s);
    }
  });

  it('a running run with no last event at all reads as infinitely stale, so driverless ⟹ stalled', () => {
    expect(derivedStatus('running', 'none', undefined, NOW)).toBe('stalled');
  });

  it('stalled is in the WAITING group and labelled "stalled": it sorts with waiting-on-you', () => {
    expect(groupOf('stalled')).toBe('waiting');
    expect(isWaiting('stalled')).toBe(true);
    expect(labelOf('stalled')).toBe('stalled');
  });

  // SLEEPING (durable timer): the CLI's exact call (`status_group` in
  // salvor-cli-core/src/render.rs) is progress, not waiting, because `waiting` means a PERSON is
  // the only thing that moves the run, and a sleeping run moves itself once its wake_at instant
  // arrives. It must therefore never land in the Inbox, and it must never be misread as stalled:
  // a sleeping run holds no driver BY DESIGN, for as long as its nap lasts, so a driverless-and-
  // stale sleeping run is a normal rest, not a stall.
  it('sleeping groups with PROGRESS, not waiting: it never queues in the Inbox', () => {
    expect(groupOf('sleeping')).toBe('progress');
    expect(isWaiting('sleeping')).toBe(false);
    expect(labelOf('sleeping')).toBe('sleeping');
  });

  it('a sleeping run driverless-and-stale is NOT stalled: no driver while asleep is by design', () => {
    expect(derivedStatus('sleeping', 'none', stale, NOW)).toBe('sleeping');
  });

  it('a sleeping run with no last event at all (infinitely stale) is still not stalled', () => {
    expect(derivedStatus('sleeping', 'none', undefined, NOW)).toBe('sleeping');
  });

  // SIGNAL WAIT (durable timers item): a `suspended` run parked on an external signal rather than
  // a person makes the identical call `sleeping` makes, via the SAME functions, extended to see the
  // discriminator through one optional trailing parameter (the least invasive extension: every
  // existing single-argument call keeps its old behavior unchanged). A GATE (no discriminator) must
  // keep grouping and deriving exactly as before.
  it('a signal wait groups with PROGRESS, not waiting: it never queues in the Inbox', () => {
    expect(groupOf('suspended', 'signal')).toBe('progress');
    expect(isWaiting('suspended', 'signal')).toBe(false);
  });

  it('a human gate (no discriminator) still groups as waiting, unchanged', () => {
    expect(groupOf('suspended')).toBe('waiting');
    expect(isWaiting('suspended')).toBe(true);
  });

  it('a signal wait driverless and stale is NOT stalled: nobody holds a driver for a webhook wait either', () => {
    expect(derivedStatus('suspended', 'none', stale, NOW, 'signal')).toBe('suspended');
  });

  it('a signal wait with no last event at all (infinitely stale) is still not stalled', () => {
    expect(derivedStatus('suspended', 'none', undefined, NOW, 'signal')).toBe('suspended');
  });

  it('a human gate, driverless and stale, was never stalled either, before or after this extension', () => {
    expect(derivedStatus('suspended', 'none', stale, NOW)).toBe('suspended');
  });
});

describe('waitingOnOf: the signal discriminator read off a RunStatus.raw', () => {
  function status(over: Partial<RunStatus> & { raw?: Record<string, unknown> }): RunStatus {
    return { state: 'suspended', raw: {}, ...over };
  }

  it('a suspended status carrying kind: "signal" on its raw JSON reads as a signal wait', () => {
    expect(waitingOnOf(status({ raw: { state: 'suspended', kind: 'signal' } }))).toBe('signal');
  });

  it('a suspended status with no kind on raw JSON reads as a human gate (undefined)', () => {
    expect(waitingOnOf(status({ raw: { state: 'suspended' } }))).toBeUndefined();
  });

  it('a non-suspended status never reads as a signal wait, even if raw carried the key somehow', () => {
    expect(waitingOnOf(status({ state: 'running', raw: { kind: 'signal' } }))).toBeUndefined();
  });
});

describe('overdueSince: the clock-only check, the Inspector\'s wasm fold has no other way to tell', () => {
  const NOW = Date.parse('2026-08-20T12:00:00Z');

  it('a wake_at before now is overdue, by the whole seconds since it passed', () => {
    expect(overdueSince('2026-08-20T10:00:00Z', NOW)).toEqual({ overdue: true, overdueSeconds: 7200 });
  });

  it('a wake_at still ahead of now is not overdue', () => {
    expect(overdueSince('2026-08-20T13:00:00Z', NOW)).toEqual({ overdue: false });
  });

  it('a wake_at equal to now is not overdue yet: passed means strictly after, not at, the deadline', () => {
    expect(overdueSince('2026-08-20T12:00:00Z', NOW)).toEqual({ overdue: false });
  });

  it('no wake_at at all is not overdue (nothing to be overdue against)', () => {
    expect(overdueSince(undefined, NOW)).toEqual({ overdue: false });
  });
});

describe('overdueOf: the server field wins when present, wake_at otherwise', () => {
  const NOW = Date.parse('2026-08-20T12:00:00Z');
  function sleeping(opts: { wakeAt?: string; overdue?: boolean; overdueSeconds?: number }): RunStatus {
    return { state: 'sleeping', wakeAt: opts.wakeAt, overdue: opts.overdue, overdueSeconds: opts.overdueSeconds, raw: {} };
  }

  it('a non-sleeping status is never overdue, whatever the typed fields carry', () => {
    const status: RunStatus = { state: 'running', overdue: true, overdueSeconds: 99, raw: {} };
    expect(overdueOf(status, NOW)).toEqual({ overdue: false });
  });

  it('before the server has an opinion, a wake_at in the past derives overdue from the clock', () => {
    expect(overdueOf(sleeping({ wakeAt: '2026-08-20T10:00:00Z' }), NOW)).toEqual({
      overdue: true,
      overdueSeconds: 7200,
    });
  });

  it('before the deadline, nothing renders as overdue: derived from the clock, same as the server would say', () => {
    expect(overdueOf(sleeping({ wakeAt: '2026-08-20T13:00:00Z' }), NOW)).toEqual({ overdue: false });
  });

  it('the server\'s own overdue/overdueSeconds win outright once present, over what the clock alone would derive', () => {
    // wakeAt here is still in the FUTURE by the browser's own math, but the server's clock says
    // otherwise (e.g. clock skew): the server's word is final, never second-guessed.
    expect(
      overdueOf(sleeping({ wakeAt: '2026-08-20T13:00:00Z', overdue: true, overdueSeconds: 45 }), NOW),
    ).toEqual({ overdue: true, overdueSeconds: 45 });
  });

  it('the server can also say not-overdue explicitly, and that wins too, even past a naive wake_at read', () => {
    expect(overdueOf(sleeping({ wakeAt: '2026-08-20T10:00:00Z', overdue: false }), NOW)).toEqual({
      overdue: false,
    });
  });
});

describe('toRunRow: bakes the derived status and carries driver evidence', () => {
  const NOW = 1_000_000_000_000;
  const stale = new Date(NOW - STALL_GRACE_MS - 1).toISOString();

  it('a running run the server reports driverless-and-stale becomes stalled on the row', () => {
    const r = toRunRow(
      { run: 'r1', status: { state: 'running', raw: {} }, eventCount: 4, lastRecordedAt: stale, driver: 'none', raw: {} },
      NOW,
    );
    expect(r.status).toBe('stalled');
    expect(r.driver).toBe('none');
  });

  it('the same run WITH an attached driver stays running', () => {
    const r = toRunRow(
      { run: 'r1', status: { state: 'running', raw: {} }, eventCount: 4, lastRecordedAt: stale, driver: 'attached', raw: {} },
      NOW,
    );
    expect(r.status).toBe('running');
    expect(r.driver).toBe('attached');
  });

  // THE REAL-WORLD SHAPE (defect A): the owner's 6 abandoned client-runs died mid model-call: a
  // RunStarted then a ModelCallRequested with no completion, so they fold to `awaiting_model`,
  // never literal `running`. This must derive to `stalled` exactly like the `running` case above.
  it('an awaiting_model run the server reports driverless-and-stale becomes stalled too: a run that died mid model-call', () => {
    const r = toRunRow(
      {
        run: 'r1',
        status: { state: 'awaiting_model', raw: {} },
        eventCount: 2,
        lastRecordedAt: stale,
        driver: 'none',
        raw: {},
      },
      NOW,
    );
    expect(r.status).toBe('stalled');
    expect(r.driver).toBe('none');
  });

  // THE STATED ACCEPTANCE CASE: a sleeping run reporting driver: "none" (its normal resting
  // state, parked on a durable timer) and an event past STALL_GRACE_MS must NOT become stalled.
  it('a sleeping run driverless and stale stays sleeping on the row (no driver while asleep is by design)', () => {
    const r = toRunRow(
      {
        run: 'r1',
        status: { state: 'sleeping', raw: { wake_at: '2026-08-20T09:00:00Z' } },
        eventCount: 2,
        lastRecordedAt: stale,
        driver: 'none',
        raw: {},
      },
      NOW,
    );
    expect(r.status).toBe('sleeping');
    expect(r.driver).toBe('none');
  });

  it('a sleeping run before its wake_at is explicitly not overdue on the row: it still reads exactly as before', () => {
    const r = toRunRow(
      {
        run: 'r1',
        status: { state: 'sleeping', wakeAt: new Date(NOW + 3_600_000).toISOString(), raw: {} },
        eventCount: 2,
        raw: {},
      },
      NOW,
    );
    // false, not absent: a reader must never treat "not overdue" and "unknown" as the same thing.
    expect(r.overdue).toBe(false);
    expect(r.overdueSeconds).toBeUndefined();
  });

  it('a sleeping run past its wake_at is overdue on the row, derived from the clock, and stays sleeping (no new state)', () => {
    const r = toRunRow(
      {
        run: 'r1',
        status: { state: 'sleeping', wakeAt: new Date(NOW - 7_200_000).toISOString(), raw: {} },
        eventCount: 2,
        raw: {},
      },
      NOW,
    );
    expect(r.status).toBe('sleeping');
    expect(r.overdue).toBe(true);
    expect(r.overdueSeconds).toBe(7200);
  });

  it('the server-computed overdue/overdueSeconds win over the row\'s own clock derivation', () => {
    const r = toRunRow(
      {
        run: 'r1',
        status: {
          state: 'sleeping',
          wakeAt: new Date(NOW - 7_200_000).toISOString(),
          overdue: true,
          overdueSeconds: 30,
          raw: {},
        },
        eventCount: 2,
        raw: {},
      },
      NOW,
    );
    expect(r.overdue).toBe(true);
    expect(r.overdueSeconds).toBe(30);
  });

  it('a non-sleeping row never carries overdue, even one long finished', () => {
    const r = toRunRow(
      { run: 'r1', status: { state: 'completed', raw: {} }, eventCount: 2, raw: {} },
      NOW,
    );
    expect(r.overdue).toBeUndefined();
    expect(r.overdueSeconds).toBeUndefined();
  });

  // THE STATED ACCEPTANCE CASE for signal waits: a suspended run reporting driver: "none" (its
  // normal resting state, parked on an external system, not a person) and an event past
  // STALL_GRACE_MS must NOT become stalled, and the row must carry `waitingOn` forward so every
  // downstream reader of the flat row (the ledger's group counts, sort, filter) can make the same
  // call without re-reading `raw` by hand.
  it('a suspended run waiting on a signal, driverless and stale, stays suspended on the row and carries waitingOn', () => {
    const r = toRunRow(
      {
        run: 'r1',
        status: { state: 'suspended', raw: { state: 'suspended', kind: 'signal' } },
        eventCount: 2,
        lastRecordedAt: stale,
        driver: 'none',
        raw: {},
      },
      NOW,
    );
    expect(r.status).toBe('suspended');
    expect(r.waitingOn).toBe('signal');
  });

  it('a suspended run with no discriminator (a human gate) carries no waitingOn', () => {
    const r = toRunRow(
      {
        run: 'r1',
        status: { state: 'suspended', raw: { state: 'suspended' } },
        eventCount: 2,
        lastRecordedAt: stale,
        driver: 'none',
        raw: {},
      },
      NOW,
    );
    expect(r.status).toBe('suspended');
    expect(r.waitingOn).toBeUndefined();
  });
});

describe('toRunRow: labels pass through honestly (absent, never a fabricated {})', () => {
  function summary(over: Partial<RunSummary>): RunSummary {
    return {
      run: 'r1',
      status: { state: 'completed', raw: {} },
      eventCount: 3,
      raw: {},
      ...over,
    };
  }

  it('a run with no labels recorded carries none through', () => {
    const r = toRunRow(summary({}));
    expect(r.labels).toBeUndefined();
  });

  it('a run with labels carries them through flat', () => {
    const r = toRunRow(summary({ labels: { build_id: 'bld_7f3a2c', jd_id: 'jd_2291' } }));
    expect(r.labels).toEqual({ build_id: 'bld_7f3a2c', jd_id: 'jd_2291' });
  });

  it('agentDefHash carries through unchanged, hash or label alike', () => {
    expect(toRunRow(summary({ agentDefHash: 'sha256:abc' })).agentDefHash).toBe('sha256:abc');
    expect(toRunRow(summary({ agentDefHash: 'aarg_cover_writer_v1' })).agentDefHash).toBe(
      'aarg_cover_writer_v1',
    );
  });
});

describe('abandoned: the operator-retired terminal (state-not-status vocabulary)', () => {
  it('groups with the TERMINAL family, never waiting: the health strip counts it terminal, never attention', () => {
    expect(groupOf('abandoned')).toBe('terminal');
  });

  it('is not a waiting state, so it never floats to the top of the attention sort', () => {
    expect(isWaiting('abandoned')).toBe(false);
  });

  it('has its own human label, distinct from failed', () => {
    expect(labelOf('abandoned')).toBe('abandoned');
    expect(labelOf('abandoned')).not.toBe(labelOf('failed'));
  });

  it('carries its server state through toRunRow unchanged (never re-derived to stalled or failed)', () => {
    const r = toRunRow({
      run: 'r1',
      status: { state: 'abandoned', raw: { state: 'abandoned' } },
      eventCount: 3,
    } as unknown as RunSummary);
    expect(r.status).toBe('abandoned');
  });
});

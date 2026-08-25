import {
  groupOfStatus,
  isSignalWait,
  overdueOfStatus,
  sleepingBandHtml,
  statusStateOf,
  waitingOnOfStatus,
} from './state-model';

describe('statusStateOf: wasm status kind -> the server snake_case state slug', () => {
  it('maps Sleeping to sleeping, carrying no other field onto the slug', () => {
    expect(statusStateOf({ kind: 'Sleeping', wake_at: '2026-08-14T09:00:00Z' })).toBe('sleeping');
  });

  it('maps the rest of the family unchanged, for the same fixed vocabulary run-model.ts keys on', () => {
    expect(statusStateOf({ kind: 'NotStarted' })).toBe('not_started');
    expect(statusStateOf({ kind: 'Running' })).toBe('running');
    expect(statusStateOf({ kind: 'AwaitingModel' })).toBe('awaiting_model');
    expect(statusStateOf({ kind: 'AwaitingTool' })).toBe('awaiting_tool');
    expect(statusStateOf({ kind: 'Suspended', reason: 'x', input_schema: {} })).toBe('suspended');
    expect(statusStateOf({ kind: 'BudgetExceeded', budget: { kind: 'steps', limit: 1 }, observed: 1 })).toBe(
      'budget_exceeded',
    );
    expect(statusStateOf({ kind: 'NeedsReconciliation' })).toBe('needs_reconciliation');
    expect(statusStateOf({ kind: 'Completed', output: null })).toBe('completed');
    expect(statusStateOf({ kind: 'Failed', error: 'boom' })).toBe('failed');
    expect(statusStateOf({ kind: 'Abandoned' })).toBe('abandoned');
  });

  it('maps Suspended to suspended whether or not it carries the waiting_on discriminator', () => {
    expect(statusStateOf({ kind: 'Suspended', reason: 'x', input_schema: {}, waiting_on: 'signal' })).toBe(
      'suspended',
    );
  });
});

describe('waitingOnOfStatus / isSignalWait: the signal discriminator on a wasm status', () => {
  it('a Suspended status carrying waiting_on: "signal" reads as a signal wait', () => {
    const status = { kind: 'Suspended', reason: 'x', input_schema: {}, waiting_on: 'signal' } as const;
    expect(waitingOnOfStatus(status)).toBe('signal');
    expect(isSignalWait(status)).toBe(true);
  });

  it('a Suspended status with no waiting_on is a human gate (undefined), not a signal wait', () => {
    const status = { kind: 'Suspended', reason: 'x', input_schema: {} } as const;
    expect(waitingOnOfStatus(status)).toBeUndefined();
    expect(isSignalWait(status)).toBe(false);
  });

  it('a non-Suspended status never reads as a signal wait', () => {
    const status = { kind: 'Sleeping', wake_at: '2026-08-14T09:00:00Z' } as const;
    expect(waitingOnOfStatus(status)).toBeUndefined();
    expect(isSignalWait(status)).toBe(false);
  });
});

describe('groupOfStatus: a wasm status folds to the same group the Runs ledger would', () => {
  it('a signal wait groups as progress: it never queues in the Inbox, mirroring sleeping', () => {
    expect(groupOfStatus({ kind: 'Suspended', reason: 'x', input_schema: {}, waiting_on: 'signal' })).toBe(
      'progress',
    );
  });

  it('a human gate still groups as waiting, unchanged', () => {
    expect(groupOfStatus({ kind: 'Suspended', reason: 'x', input_schema: {} })).toBe('waiting');
  });

  it('sleeping groups as progress here too, the same call run-model.ts makes', () => {
    expect(groupOfStatus({ kind: 'Sleeping', wake_at: '2026-08-14T09:00:00Z' })).toBe('progress');
  });
});

describe('overdueOfStatus: the Inspector\'s only overdue check, purely clock-derived (the wasm fold has no clock)', () => {
  const NOW = Date.parse('2026-08-20T12:00:00Z');

  it('a Sleeping status whose wake_at has passed is overdue, by the whole seconds since', () => {
    expect(overdueOfStatus({ kind: 'Sleeping', wake_at: '2026-08-20T10:00:00Z' }, NOW)).toEqual({
      overdue: true,
      overdueSeconds: 7200,
    });
  });

  it('a Sleeping status whose wake_at is still ahead is not overdue', () => {
    expect(overdueOfStatus({ kind: 'Sleeping', wake_at: '2026-08-20T13:00:00Z' }, NOW)).toEqual({
      overdue: false,
    });
  });

  it('a non-Sleeping status has no overdue verdict at all: the question does not apply', () => {
    expect(overdueOfStatus({ kind: 'Running' }, NOW)).toBeUndefined();
  });
});

describe('sleepingBandHtml: the sleeping band reads calm before the deadline, attention after', () => {
  it('before the deadline, it states the wake time and offers no action: nothing to do yet', () => {
    const html = sleepingBandHtml('09:00:00Z', { overdue: false });
    expect(html).toContain('Sleeping.');
    expect(html).toContain('Wakes at <span class="mono">09:00:00Z</span>');
    expect(html).not.toContain('Overdue');
    expect(html).not.toContain('salvor wake');
  });

  it('once overdue, it states the deadline, how long ago, that nothing has woken it, and how to wake it by hand', () => {
    const html = sleepingBandHtml('09:00:00Z', { overdue: true, overdueSeconds: 7200 });
    expect(html).toContain('Overdue.');
    expect(html).toContain('Was due at <span class="mono">09:00:00Z</span>');
    expect(html).toContain('2h ago');
    expect(html).toContain('nothing has woken it');
    expect(html).toContain('salvor wake');
  });

  it('overdue with no known duration still reads honestly, never a fabricated "0s ago"', () => {
    const html = sleepingBandHtml('09:00:00Z', { overdue: true });
    expect(html).not.toContain('0s ago');
    expect(html).toContain('nothing has woken it');
  });
});

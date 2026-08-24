import { groupOfStatus, isSignalWait, statusStateOf, waitingOnOfStatus } from './state-model';

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

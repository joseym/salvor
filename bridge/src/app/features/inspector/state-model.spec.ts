import { statusStateOf } from './state-model';

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
});

import type { RunStateJson } from '../inspector/wasm-fold';
import { ceilingFor } from './cost-fold';

function budgetExceededState(limit: number): RunStateJson {
  return {
    status: { kind: 'BudgetExceeded', budget: { kind: 'cost_usd', limit }, observed: limit + 0.01 },
    next_seq: 4,
    usage: { input_tokens: 100, output_tokens: 20 },
  };
}

function runningState(): RunStateJson {
  return { status: { kind: 'Running' }, next_seq: 2, usage: { input_tokens: 10, output_tokens: 1 } };
}

describe('ceilingFor — no agent registry in this build, so only a crossed ceiling is known', () => {
  it('reads the ceiling from the run’s own BudgetExceeded event', () => {
    const c = ceilingFor(budgetExceededState(0.5));
    expect(c).toEqual({ usd: 0.5, src: 'crossed' });
  });

  it('is unknown for a run that never crossed a ceiling — never guessed from a registry', () => {
    expect(ceilingFor(runningState())).toEqual({ usd: null, src: 'unknown' });
  });

  it('is unknown when the fold itself never produced a state (an unreadable log)', () => {
    expect(ceilingFor(undefined)).toEqual({ usd: null, src: 'unknown' });
  });
});

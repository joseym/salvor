import type { CostTotal } from '../inspector/pricing';
import type { RunStateJson } from '../inspector/wasm-fold';
import { allUnpriced, ceilingFor, type FoldedRun } from './cost-fold';

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

describe('ceilingFor: no agent registry in this build, so only a crossed ceiling is known', () => {
  it('reads the ceiling from the run’s own BudgetExceeded event', () => {
    const c = ceilingFor(budgetExceededState(0.5));
    expect(c).toEqual({ usd: 0.5, src: 'crossed' });
  });

  it('is unknown for a run that never crossed a ceiling: never guessed from a registry', () => {
    expect(ceilingFor(runningState())).toEqual({ usd: null, src: 'unknown' });
  });

  it('is unknown when the fold itself never produced a state (an unreadable log)', () => {
    expect(ceilingFor(undefined)).toEqual({ usd: null, src: 'unknown' });
  });
});

const PRICED: CostTotal = { complete: true, usd: 0.03, unpriced: [] };
const UNPRICED: CostTotal = { complete: false, usd: null, unpriced: ['claude-opus-4-8'] };
/** A zero-call run's cost reads `complete: true` VACUOUSLY: there is no unpriced model to name
 *  when there were no calls at all. This is the exact shape that trips defect B. */
const NO_CALLS: CostTotal = { complete: true, usd: 0, unpriced: [] };

function foldedRun(over: Partial<FoldedRun> & { id: string }): FoldedRun {
  return {
    events: [],
    readable: true,
    agent: null,
    usage: { inputTokens: 0, outputTokens: 0 },
    cost: NO_CALLS,
    calls: 0,
    state: undefined,
    ...over,
  };
}

describe('allUnpriced, the page-wide lede condition (defect B: the owner’s 29 all-unpriced runs)', () => {
  it('THE REAL SHAPE: a mix of zero-usage non-terminal runs and completed unpriced runs still shows the banner', () => {
    const runs = [
      // 6 driverless non-terminal runs, zero usage, zero model calls (the owner's stalled runs).
      foldedRun({ id: 'stalled-1' }),
      foldedRun({ id: 'stalled-2' }),
      // 29 completed runs whose model is genuinely absent from the price table.
      foldedRun({ id: 'run-1', calls: 3, cost: UNPRICED }),
      foldedRun({ id: 'run-2', calls: 5, cost: UNPRICED }),
    ];
    expect(allUnpriced(runs)).toBe(true);
  });

  it('any run with a genuinely priced call suppresses the banner (zero-vs-absent: a real $ is a figure, not an absence)', () => {
    const runs = [
      foldedRun({ id: 'stalled-1' }),
      foldedRun({ id: 'run-1', calls: 3, cost: UNPRICED }),
      foldedRun({ id: 'run-2', calls: 2, cost: PRICED }),
    ];
    expect(allUnpriced(runs)).toBe(false);
  });

  it('zero-call runs alone (no run anywhere ever called a model) state no unpriced fact: no banner', () => {
    const runs = [foldedRun({ id: 'stalled-1' }), foldedRun({ id: 'stalled-2' })];
    expect(allUnpriced(runs)).toBe(false);
  });

  it('an unreadable log is excluded from the check entirely, whatever its cost figure claims', () => {
    const runs = [foldedRun({ id: 'broken', readable: false, calls: 4, cost: PRICED })];
    expect(allUnpriced(runs)).toBe(false);
  });

  it('no runs at all: no banner (nothing to state)', () => {
    expect(allUnpriced([])).toBe(false);
  });
});

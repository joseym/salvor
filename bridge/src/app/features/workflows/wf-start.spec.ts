import { SalvorApiError } from '@salvor-run/client';
import { describe, expect, it } from 'vitest';

import { startRefusal } from './wf-start';

describe('startRefusal, what a refused graph run says', () => {
  it('names the empty tool registry behind an unknown_tool refusal, and how to get one', () => {
    const err = new SalvorApiError(
      'unknown_tool',
      'node `n_refund` names tool `issue_refund`, which is not registered',
      404,
    );
    const said = startRefusal(err);
    expect(said).toContain('issue_refund');
    expect(said).toContain('empty tool registry');
    expect(said).toContain('--demo-tools');
  });

  it('names the in-memory graph store behind an unknown_graph refusal, and that re-publishing is safe', () => {
    const err = new SalvorApiError('unknown_graph', 'no graph stored under sha256:aa', 404);
    const said = startRefusal(err);
    expect(said).toContain("server's memory");
    expect(said).toContain('identical hash');
  });

  it('reports every other refusal verbatim and invents no explanation for it', () => {
    const err = new SalvorApiError('unknown_agent', 'node `n_draft` references an unregistered agent', 404);
    expect(startRefusal(err)).toBe(
      'start refused: unknown_agent: node `n_draft` references an unregistered agent',
    );
  });

  it('survives a non-API failure (an unreachable server throws a plain Error)', () => {
    expect(startRefusal(new Error('Failed to fetch'))).toBe('start refused: Failed to fetch');
    expect(startRefusal('boom')).toBe('start refused: boom');
  });
});

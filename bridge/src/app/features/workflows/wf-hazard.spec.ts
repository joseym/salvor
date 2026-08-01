import { describe, expect, it } from 'vitest';

import type { ForkOutcome } from '../../core/api';
import { forkReady, reviewOf } from './wf-hazard';

function write(seq: number, key: string | null): Record<string, unknown> {
  return { seq, tool: 'issue_refund', input: { amount: 100 }, idempotency_key: key, recorded_at: '2026-07-12T10:31:05Z' };
}

const preview: ForkOutcome = {
  kind: 'preview',
  origin: 'r1',
  fromNode: 'n_refund',
  throughSeq: 17,
  graphHash: 'sha256:g',
  prefixEventCount: 17,
  writes: [
    { seq: 18, tool: 'issue_refund', input: { amount: 100 }, idempotencyKey: null, recordedAt: '2026-07-12T10:31:05Z', raw: write(18, null) },
    { seq: 24, tool: 'charge_card', input: {}, idempotencyKey: 'k', recordedAt: '2026-07-12T10:31:06Z', raw: write(24, 'k') },
  ],
  unacknowledgedWrites: [18, 24],
  wouldProceed: false,
  raw: {},
};

describe('reviewOf: a preview', () => {
  it('lists every write as a hazard row with seq/tool/input/key/recordedAt', () => {
    const review = reviewOf(preview);
    expect(review.hazards).toHaveLength(2);
    const first = review.hazards[0];
    expect(first).toEqual({ seq: 18, tool: 'issue_refund', input: { amount: 100 }, key: null, recordedAt: '2026-07-12T10:31:05Z' });
    expect(review.unacknowledged).toEqual([18, 24]);
    expect(review.refused).toBe(false);
  });

  it('preserves an explicit null idempotency key as null, distinct from a real key', () => {
    const review = reviewOf(preview);
    expect(review.hazards[0].key).toBeNull();
    expect(review.hazards[1].key).toBe('k');
  });
});

describe('reviewOf: a 409 hazard refusal folds identically', () => {
  it('treats every listed write as outstanding', () => {
    const hazard: ForkOutcome = {
      kind: 'hazard',
      code: 'write_replay_hazard',
      message: 'unacknowledged writes',
      writes: preview.kind === 'preview' ? preview.writes : [],
      raw: {},
    };
    const review = reviewOf(hazard);
    expect(review.unacknowledged).toEqual([18, 24]);
    expect(review.message).toBe('unacknowledged writes');
  });
});

describe('forkReady: the confirm gate', () => {
  it('stays blocked until every outstanding hazard is acknowledged', () => {
    const review = reviewOf(preview);
    expect(forkReady(review, new Set())).toBe(false);
    expect(forkReady(review, new Set([18]))).toBe(false);
    expect(forkReady(review, new Set([18, 24]))).toBe(true);
  });

  it('a clean fork (no hazards) is ready at once', () => {
    const clean: ForkOutcome = { ...preview, writes: [], unacknowledgedWrites: [] } as ForkOutcome;
    expect(forkReady(reviewOf(clean), new Set())).toBe(true);
  });

  it('a forked outcome is not a review and needs no acknowledgement', () => {
    const forked: ForkOutcome = {
      kind: 'forked',
      run: 'r2',
      status: 'suspended',
      forkedFrom: { runId: 'r1', throughSeq: 17, fromNode: 'n_refund', graphHash: 'sha256:g', acknowledgedWrites: [18, 24], raw: {} },
      raw: {},
    };
    const review = reviewOf(forked);
    expect(review.hazards).toHaveLength(0);
    expect(forkReady(review, new Set())).toBe(true);
  });
});

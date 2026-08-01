import type { ForkHazardWrite, ForkOutcome } from '../../core/api';

/**
 * THE HAZARD REVIEW MODEL. A fork replays the origin's recorded prefix and then re-executes from
 * the fork node, so every `Effect::Write` in the re-walked segment RUNS A SECOND TIME. The server
 * refuses a fork that would replay an unacknowledged write (`409 write_replay_hazard`) and answers
 * the same question up front on a `dry_run` preview. Both surface here as one flat list of rows the
 * dialog renders, with the exact facts the operator must read before acknowledging: the origin seq,
 * the tool, its recorded input, its idempotency key (an explicit `null` when the write recorded
 * none: that null is the whole reason it cannot be safely retried), and when it ran.
 */
export interface HazardRow {
  readonly seq: number;
  readonly tool: string;
  readonly input: unknown;
  /** `null` is a fact (no idempotency key was recorded), not a missing value, kept distinct. */
  readonly key: string | null;
  readonly recordedAt: string;
}

function rowOf(w: ForkHazardWrite): HazardRow {
  return { seq: w.seq, tool: w.tool, input: w.input, key: w.idempotencyKey, recordedAt: w.recordedAt };
}

export interface HazardReview {
  /** The hazards to acknowledge, one checkbox each. */
  readonly hazards: readonly HazardRow[];
  /** The seqs still needing acknowledgement (the confirm gate reads this). On a preview the server
   * says which remain; on a `409` refusal every listed write is unacknowledged by construction. */
  readonly unacknowledged: readonly number[];
  /** True when the fork is refused for a reason OTHER than an ack-able write (a structural refusal
   * the caller lets throw). A hazard/preview is never a refusal in this sense. */
  readonly refused: boolean;
  readonly message?: string;
}

/**
 * Fold a typed {@link ForkOutcome} into the review model. A `preview` (dry run) or a `hazard`
 * (`409`) both carry `writes`; a `preview` additionally says which are still unacknowledged (a
 * re-preview after ticking some), while a `hazard` treats them all as outstanding. A `forked`
 * outcome is not a review: the fork already happened, so it folds to an empty, unrefused review.
 */
export function reviewOf(outcome: ForkOutcome): HazardReview {
  if (outcome.kind === 'preview') {
    return {
      hazards: outcome.writes.map(rowOf),
      unacknowledged: outcome.unacknowledgedWrites,
      refused: false,
    };
  }
  if (outcome.kind === 'hazard') {
    return {
      hazards: outcome.writes.map(rowOf),
      unacknowledged: outcome.writes.map((w) => w.seq),
      refused: false,
      message: outcome.message,
    };
  }
  return { hazards: [], unacknowledged: [], refused: false };
}

/** The confirm gate: the fork may proceed only when every outstanding hazard has been ticked. A
 * clean fork (no hazards) is enabled at once. Mirrors the prototype's `forkSync` gate. */
export function forkReady(review: HazardReview, acknowledged: ReadonlySet<number>): boolean {
  if (review.refused) return false;
  return review.unacknowledged.every((seq) => acknowledged.has(seq));
}

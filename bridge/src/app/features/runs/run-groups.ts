import { agentIdentity, isWaiting, type RunRow } from './run-model';

/**
 * The Runs ledger's GROUPING mode: clustering the same rows the flat
 * table shows, keyed by what the run actually recorded, never a guess.
 *
 * Key priority: `labels.build_id` when the run was tagged with one, else the run's own agent
 * identity, else the literal `"Unlabelled"` bucket. Both `labels` and `agent_def_hash` are
 * recorded once on `RunStarted`. A build clusters the several agents a caller ran for one build
 * (AARG's "several agents … for one resume build"); a run with no build clusters by its own
 * agent (still real, recorded data); only a run missing BOTH lands in `"Unlabelled"`: it is never
 * guessed into a build or an agent it wasn't tagged with.
 *
 * This is a DIFFERENT axis from {@link groupOf}/{@link Group} in `run-model.ts`, which classifies
 * a single run's STATUS into the three-way progress/waiting/terminal split every status dot
 * reads. That split stays exactly as it was (the state-filter chips, now labelled "State" to stop
 * colliding with this new grouping control). This module's grouping is "what ran for what build",
 * a presentation MODE (grouped vs. flat) over the whole visible list, not a per-row classification.
 */
export type RunClusterKind = 'build' | 'agent' | 'unlabelled';

export interface RunClusterKey {
  readonly key: string;
  readonly kind: RunClusterKind;
  readonly name: string;
}

/** Which cluster one run belongs to, honestly: see the module doc for the priority order. */
export function clusterOf(r: RunRow, names: ReadonlyMap<string, string> = new Map()): RunClusterKey {
  const build = r.labels?.['build_id'];
  if (build) return { key: `build:${build}`, kind: 'build', name: build };
  const hash = r.agentDefHash;
  if (hash) return { key: `agent:${hash}`, kind: 'agent', name: agentIdentity(r, names).text };
  return { key: 'unlabelled', kind: 'unlabelled', name: 'Unlabelled' };
}

/**
 * A cluster's worst-first attention rank, from the SAME two facts the flat list's own sort key
 * reads (see `runs.ts#statusRank`): any waiting run in the group outranks any failed run, which
 * outranks a group with neither. Kept here (not re-derived ad hoc) so the group sort and the flat
 * sort can never quietly diverge.
 */
export function attentionRank(status: string): number {
  return isWaiting(status) ? 2 : status === 'failed' ? 1 : 0;
}

/** One header's worth of aggregate facts, plus the runs it holds (in the caller's given order:
 * pass already attention-sorted rows in and each group's own runs keep that order). */
export interface RunGroup extends RunClusterKey {
  readonly runs: readonly RunRow[];
  /** worst-first, like {@link attentionRank} over the group's own runs */
  readonly rank: number;
  /** count of runs in this group that are WAITING ON A HUMAN; never hidden, see runs.ts */
  readonly waiting: number;
  /** most recent `last` timestamp in the group, as epoch ms (tiebreaker after rank) */
  readonly latest: number;
  /** the distinct statuses present, for the header's plain-summary aggregate */
  readonly states: readonly string[];
}

/**
 * Cluster `visible` rows into groups and sort the groups worst-first (any group holding a waiting
 * run lands at the top exactly as a waiting run would in the flat list: ATTENTION SURVIVES
 * GROUPING). Ties break by most-recent activity.
 */
export function buildRunGroups(
  visible: readonly RunRow[],
  names: ReadonlyMap<string, string> = new Map(),
): RunGroup[] {
  const byKey = new Map<string, RunClusterKey & { runs: RunRow[] }>();
  for (const r of visible) {
    const k = clusterOf(r, names);
    let entry = byKey.get(k.key);
    if (!entry) {
      entry = { ...k, runs: [] };
      byKey.set(k.key, entry);
    }
    entry.runs.push(r);
  }
  const groups: RunGroup[] = [...byKey.values()].map((g) => ({
    key: g.key,
    kind: g.kind,
    name: g.name,
    runs: g.runs,
    rank: Math.max(...g.runs.map((r) => attentionRank(r.status))),
    waiting: g.runs.filter((r) => isWaiting(r.status)).length,
    latest: Math.max(...g.runs.map((r) => (r.last ? Date.parse(r.last) : 0))),
    states: [...new Set(g.runs.map((r) => r.status))],
  }));
  groups.sort((a, b) => b.rank - a.rank || b.latest - a.latest);
  return groups;
}

/**
 * WAITING RUNS MUST NOT HIDE: a group holding any waiting run may never collapse (its toggle is
 * disabled in the template, with a title saying why); this is the one predicate both the
 * template and the click handler consult, so the guarantee cannot drift between the two.
 */
export function canCollapse(g: RunGroup): boolean {
  return g.waiting === 0;
}

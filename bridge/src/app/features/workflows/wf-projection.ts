import type { GraphProjection, NodeProgress } from '../../core/api';
import type { WfGraph } from './wf-model';

/**
 * PROJECTION → PER-NODE STATE. In Run mode the canvas inks the route a run ACTUALLY took, folded
 * from the server's projection (`GET /v1/runs/{id}/graph`), never a designer's "happy path". The
 * projection lists only nodes the walk touched, each `entered | exited | skipped`, plus the
 * `current_node` (entered and not yet exited). A node the walk has not reached is simply absent —
 * and "absent" is NOT "skipped": the road not taken is information the brief refuses to drop.
 *
 * The canvas vocabulary the prototype used, derived here from the typed projection:
 *   current      · entered and not yet exited (the playhead)
 *   reached      · entered, and moved on from
 *   exited       · completed and left
 *   skipped      · the branch went the other way; this node never will run
 *   not-reached  · the walk has not got here yet; it still might
 */
export type WfNodeRunState = 'current' | 'reached' | 'exited' | 'skipped' | 'not-reached';

export interface WfNodeProjection {
  readonly state: WfNodeRunState;
  /** The branch case recorded at this node, when it is a branch that fired. */
  readonly branchCase?: string;
  /** The map fan-out marker the server recorded, verbatim (iteration progress), when present. */
  readonly map?: unknown;
}

function stateOf(p: NodeProgress, currentNode: string | undefined): WfNodeRunState {
  if (p.node === currentNode) return 'current';
  if (p.state === 'exited') return 'exited';
  if (p.state === 'skipped') return 'skipped';
  return 'reached'; // 'entered' but not the current node
}

/**
 * Map a run's projection onto every node of the graph it ran. Nodes present in the projection carry
 * their recorded state; every other node is `not-reached` (honest: the walk has not been there).
 * Returns a map keyed by node id. Guards graph mismatch by the caller (a projection whose
 * `graphHash` differs from the shown graph should not be inked) — see {@link projectionUsable}.
 */
export function projectNodeStates(
  g: WfGraph,
  projection: GraphProjection,
): Record<string, WfNodeProjection> {
  const byNode = new Map<string, NodeProgress>();
  for (const p of projection.nodes) byNode.set(p.node, p);
  const out: Record<string, WfNodeProjection> = {};
  for (const n of g.nodes) {
    const p = byNode.get(n.id);
    if (!p) {
      out[n.id] = { state: 'not-reached' };
      continue;
    }
    const entry: WfNodeProjection = { state: stateOf(p, projection.currentNode) };
    out[n.id] = {
      ...entry,
      ...(p.branchCase !== undefined ? { branchCase: p.branchCase } : {}),
      ...(p.map !== undefined ? { map: p.map } : {}),
    };
  }
  return out;
}

/**
 * An edge is inked as WALKED only when the run genuinely took it: for an ordinary edge, both ends
 * were reached; for a branch's outgoing edge, its label must equal the recorded case. An undecided
 * branch inks neither arm — nothing is decided, and drawing a guess is the lie this refuses.
 */
export function edgeWalked(
  from: string,
  to: string,
  label: string | undefined,
  isBranchSource: boolean,
  states: Record<string, WfNodeProjection>,
): boolean {
  const s = states[from];
  const t = states[to];
  if (!s || !t) return false;
  const reached = (x: WfNodeRunState) => x === 'reached' || x === 'exited' || x === 'current';
  if (!reached(s.state)) return false;
  if (isBranchSource) return label !== undefined && s.branchCase === label;
  return reached(t.state);
}

/** Whether a projection describes the graph currently shown — its `graphHash` must match. A fork's
 * projection names its own graph in the same field, so this works for forks unchanged. */
export function projectionUsable(g: WfGraph, projection: GraphProjection | undefined): boolean {
  return !!projection && !!g.hash && projection.graphHash === g.hash;
}

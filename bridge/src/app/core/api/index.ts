/**
 * The API layer: an Angular service wrapper around `@salvor-run/client`.
 *
 * Everything here is signal-based and zoneless-compatible (no `NgZone`, no RxJS on the
 * public surface: the SDK itself is promise/async-iterable native, so wrapping it in
 * signals needs no adapter layer). Five typed surfaces, one per endpoint group:
 *
 *   - {@link RunsService}: `GET /v1/runs` (the list, incl. the `e3182c5` enriched fields)
 *   - {@link AgentRegistryService}: `GET /v1/agents/{hash}`, batched/cached name resolution for
 *     the Runs ledger's agent column
 *   - {@link RunDetailService}: `GET /v1/runs/{id}`, `resume`, `resolve`
 *   - {@link RunEventsService}: `GET /v1/runs/{id}/events` (SSE), reconnect-aware
 *   - {@link ClientRunService}: the client-driven open-by-id fallback (`/v1/client-runs/*`)
 *
 * Plus the connection pill's one state machine ({@link ConnectionState}), shared by the
 * two streaming surfaces (`RunEventsService`'s channels reach `Snapshot`/`Live`/`Ended`;
 * `ClientRunService`'s channels reach `Snapshot`/`Polling`/`Ended`: never `Live`, since
 * a client-driven run has no server push to be live about).
 *
 * The v0.4 graph surface (`API.md`, "Graphs and graph runs") adds three more, all reading over
 * `graphRequest` (a raw-fetch primitive, `@salvor-run/client` does not wrap these endpoints yet):
 *
 *   - {@link GraphsService}: the graph catalog: list, read one back, validate-only
 *   - {@link GraphRunService}: `POST /v1/graph-runs`, the per-node projection, fork, forks
 *   - {@link CapabilityProbeService}: `GET /v1/capabilities`, a real probe NOT yet wired to the
 *     Inspector's fork offer (`features/inspector/capability.ts` stays pinned off, see its comment)
 */

export { SALVOR_API_CONFIG, SALVOR_CLIENT, type SalvorApiConfig, provideSalvorApi } from './client';
export {
  type ConnectionDriver,
  type ConnectionKind,
  type ConnectionState,
  createConnectionStateMachine,
} from './connection-state';
export { errorMessage } from './errors';
export { AgentRegistryService } from './agent-registry';
export { RunsService } from './runs';
export { RunDetailService } from './run-detail';
export { type RunEventsChannel, RunEventsService } from './run-events';
export { type ClientRunChannel, ClientRunService } from './client-run';
export { CapabilityProbeService, UNPROBED_CAPABILITIES } from './capabilities';
export { type ForkRequest, GraphRunService } from './graph-run';
export { GraphsService } from './graphs';
export {
  type CapabilityProbe,
  type ForkHazardWrite,
  type ForkListEntry,
  type ForkOrigin,
  type ForkOutcome,
  type ForksIndex,
  type GraphDocumentRecord,
  type GraphProjection,
  type GraphRunStart,
  type GraphSummary,
  type GraphSummaryCounts,
  type GraphValidationError,
  type NodeProgress,
  type NodeState,
  type ValidateResult,
} from './graph-types';

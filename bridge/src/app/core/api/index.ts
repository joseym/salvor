/**
 * The API layer: an Angular service wrapper around `@salvor/client`.
 *
 * Everything here is signal-based and zoneless-compatible (no `NgZone`, no RxJS on the
 * public surface — the SDK itself is promise/async-iterable native, so wrapping it in
 * signals needs no adapter layer). Four typed surfaces, one per endpoint group:
 *
 *   - {@link RunsService}      — `GET /v1/runs` (the list, incl. the `e3182c5` enriched fields)
 *   - {@link RunDetailService} — `GET /v1/runs/{id}`, `resume`, `resolve`
 *   - {@link RunEventsService} — `GET /v1/runs/{id}/events` (SSE), reconnect-aware
 *   - {@link ClientRunService} — the client-driven open-by-id fallback (`/v1/client-runs/*`)
 *
 * Plus the connection pill's one state machine ({@link ConnectionState}), shared by the
 * two streaming surfaces (`RunEventsService`'s channels reach `Snapshot`/`Live`/`Ended`;
 * `ClientRunService`'s channels reach `Snapshot`/`Polling`/`Ended` — never `Live`, since
 * a client-driven run has no server push to be live about).
 */

export { SALVOR_API_CONFIG, SALVOR_CLIENT, type SalvorApiConfig, provideSalvorApi } from './client';
export {
  type ConnectionDriver,
  type ConnectionKind,
  type ConnectionState,
  createConnectionStateMachine,
} from './connection-state';
export { errorMessage } from './errors';
export { RunsService } from './runs';
export { RunDetailService } from './run-detail';
export { type RunEventsChannel, RunEventsService } from './run-events';
export { type ClientRunChannel, ClientRunService } from './client-run';

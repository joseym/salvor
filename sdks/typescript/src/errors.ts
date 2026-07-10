/**
 * Errors the client raises.
 *
 * The control plane answers every failure with one JSON shape:
 *
 *     { "error": { "code": "unknown_run", "message": "...", "details": {...} } }
 *
 * `SalvorApiError` carries that `code` and `message` so callers match on a
 * stable token rather than parsing a sentence. The one refusal that carries
 * structured evidence, a resume blocked because a write was recorded but never
 * completed, gets its own subclass, `NeedsReconciliationError`, exposing the
 * recorded write intent.
 */

/** Base class for every error this client raises. */
export class SalvorError extends Error {
  constructor(message: string) {
    super(message);
    this.name = new.target.name;
  }
}

/** An error returned by the control plane, decoded from the error envelope. */
export class SalvorApiError extends SalvorError {
  /** The stable machine token, for example `"unknown_run"`. */
  readonly code: string;
  /** The HTTP status code. */
  readonly status: number;
  /** Optional structured evidence, present today only on a reconciliation refusal. */
  readonly details: Record<string, unknown>;

  constructor(
    code: string,
    message: string,
    status: number,
    details?: Record<string, unknown>,
  ) {
    super(`${code}: ${message}`);
    this.code = code;
    this.status = status;
    this.details = details ?? {};
  }
}

/**
 * Raised when a resume is refused because the run's log ends at a write intent
 * with no recorded completion. `intent` is the recorded write; verify what it
 * did externally, then call `resolve(runId, output)` to record its completion.
 */
export class NeedsReconciliationError extends SalvorApiError {
  /** The recorded write intent that must be reconciled. */
  get intent(): Record<string, unknown> {
    return (this.details.intent as Record<string, unknown>) ?? {};
  }
}

/** Raised when the event stream drops and cannot be resumed within the retry budget. */
export class SalvorStreamError extends SalvorError {}

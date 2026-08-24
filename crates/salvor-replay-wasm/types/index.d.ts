// Hand-written type surface for the salvor-replay-wasm package.
//
// wasm-pack generates a .d.ts for the two exported FUNCTIONS (their string/number
// signatures). It cannot describe the SHAPE of the JSON those functions carry,
// because the state crosses the boundary as a JSON string. This file pins that
// shape: `deriveState` returns a string that is `JSON.stringify(RunStateJson)`.
//
// This surface is a contract. The Rust `surface_pin` tests in src/lib.rs lock the
// exact serialized bytes for each variant; if a DTO there changes, those tests
// fail and this file must be updated in lockstep. The same-fold proof
// (js/same-fold.mjs) exercises every field of it against the native fold.
//
// Consuming (Angular / any TS), against the --target web build in pkg/:
//   import init, { deriveState, eventCount } from "salvor-replay-wasm";
//   import type { RunStateJson } from "salvor-replay-wasm/types";
//   await init();
//   const state: RunStateJson = JSON.parse(deriveState(logJson, n));

/** The three functions the wasm module exports. Mirrors the wasm-pack-generated
 *  signatures; kept here so `types/index.d.ts` is the one place a consumer reads. */
export function deriveState(logJson: string, prefixLen: number): string;
export function eventCount(logJson: string): number;
export function checkBudgets(logJson: string, budgetsJson: string): string;

/** The declaration `checkBudgets` takes as its second argument, stringified.
 *  Every dimension is optional and an absent one is never checked. These are
 *  the agent file's own key names, so the object `parseAgentToml` returns under
 *  `budgets` (with `pricing` folded in) can be passed straight through.
 *  An unknown key is refused rather than ignored. */
export interface BudgetsDeclaration {
  steps?: number | null;
  tokens?: number | null;
  cost_usd?: number | null;
  wall_time_seconds?: number | null;
  /** Required by the cost dimension, ignored by every other one. Without it a
   *  declared `cost_usd` is simply not checked, the same silence the runtime
   *  keeps (the agent builder is what refuses the combination). */
  pricing?: { input_per_mtok: number; output_per_mtok: number } | null;
}

/** What `checkBudgets` returns, parsed. `budget` and `observed` are present
 *  only when `crossed` is true, and are the pair the runtime would have
 *  recorded in its `BudgetExceeded` event. The two folded inputs come back
 *  alongside the verdict so a caller can show the arithmetic. */
export interface BudgetCheckJson {
  crossed: boolean;
  budget?: Budget;
  observed?: number;
  observations: BudgetObservations;
  extensions: BudgetExtensions;
}

/** The replay-derived quantities the check consumed: completed model calls,
 *  their recorded usage, and the span between the first and last recorded
 *  clock observation. */
export interface BudgetObservations {
  steps: number;
  input_tokens: number;
  output_tokens: number;
  elapsed_seconds: number;
}

/** What the resumes that answered earlier crossings granted, folded out of the
 *  log. A resume answering a suspension is not one of these. */
export interface BudgetExtensions {
  steps: number;
  tokens: number;
  cost_usd: number;
  wall_time_seconds: number;
}

/** Effect class of a tool call. Matches `salvor_replay::Effect`'s wire form. */
export type Effect = "read" | "idempotent" | "write";

/** Budget dimension. Matches `salvor_replay::BudgetKind`'s wire form. */
export type BudgetKind = "tokens" | "cost_usd" | "wall_time" | "steps";

/** A budget limit. `limit` is in the units implied by `kind`. */
export interface Budget {
  kind: BudgetKind;
  /** f64 on the wire; integral and exact for tokens/steps up to 2^53. */
  limit: number;
}

/** Token usage accumulated across every completed model call in the prefix. */
export interface TokenTotals {
  input_tokens: number;
  output_tokens: number;
}

/** Where the run stands, as a `kind`-tagged discriminated union. */
export type RunStatusJson =
  | { kind: "NotStarted" }
  | { kind: "Running" }
  | { kind: "AwaitingModel" }
  | { kind: "AwaitingTool" }
  /** Parked awaiting input the `input_schema` accepts. `waiting_on` names who
   *  owes that input when it is not a person: `"signal"` means an external
   *  system (a webhook, a callback) will resume the run, so it is nobody's
   *  task. Absent means a human gate, which is what every suspension recorded
   *  before the discriminator existed means. The key is `waiting_on` and not
   *  `kind` because `kind` is this union's own tag. */
  | {
      kind: "Suspended";
      reason: string;
      input_schema: unknown;
      waiting_on?: "signal";
    }
  /** Parked on a durable timer until `wake_at` (RFC 3339). Distinct from
   *  `Suspended`: nothing is waiting on a human. */
  | { kind: "Sleeping"; wake_at: string }
  | { kind: "BudgetExceeded"; budget: Budget; observed: number }
  | { kind: "NeedsReconciliation" }
  | { kind: "Completed"; output: unknown }
  | { kind: "Failed"; error: string }
  | {
      kind: "Abandoned";
      /** The operator's note, absent when none was given. */
      reason?: string;
      /** The write intent left unsettled, present only when a
       *  needs-reconciliation run was abandoned. */
      unresolved_write?: UnresolvedWrite;
    };

/** The write intent an abandonment left unsettled: a pointer (`seq`, `tool`)
 *  to the recorded intent whose effect stays unknown. */
export interface UnresolvedWrite {
  seq: number;
  tool: string;
}

/** The dangling call intent, when one exists, `kind`-tagged. */
export type PendingCallJson =
  | { kind: "Model"; seq: number; request_hash: string }
  | {
      kind: "Tool";
      seq: number;
      tool: string;
      input: unknown;
      effect: Effect;
      /** Absent when the tool declared no idempotency key. */
      idempotency_key?: string;
    };

/** Everything a folded log prefix implies about a run. The parsed result of
 *  `deriveState`. */
export interface RunStateJson {
  status: RunStatusJson;
  /** The position the next appended event will occupy. */
  next_seq: number;
  usage: TokenTotals;
  /** Present only when the status is AwaitingModel, AwaitingTool, or
   *  NeedsReconciliation (and carried through a terminal event that followed a
   *  dangling call). Absent otherwise. */
  pending_call?: PendingCallJson;
}

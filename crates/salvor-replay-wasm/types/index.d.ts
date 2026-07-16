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

/** The two functions the wasm module exports. Mirrors the wasm-pack-generated
 *  signatures; kept here so `types/index.d.ts` is the one place a consumer reads. */
export function deriveState(logJson: string, prefixLen: number): string;
export function eventCount(logJson: string): number;

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
  | { kind: "Suspended"; reason: string; input_schema: unknown }
  | { kind: "BudgetExceeded"; budget: Budget; observed: number }
  | { kind: "NeedsReconciliation" }
  | { kind: "Completed"; output: unknown }
  | { kind: "Failed"; error: string };

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

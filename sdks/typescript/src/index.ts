/**
 * @salvor/client: a thin TypeScript client over the Salvor control plane.
 *
 * Submit an agent definition, start a run, stream its events to completion, and
 * resume or reconcile it, all over HTTP. The client holds no durability logic:
 * the one Rust process on the other end owns exact replay, crash-safe resume,
 * and the write-ahead reconciliation rule.
 *
 * ```ts
 * import { SalvorClient } from "@salvor/client";
 *
 * const client = new SalvorClient("http://127.0.0.1:8080");
 * const agent = await client.registerAgent(agentToml);
 * const runId = await client.startRun(agent, { question: "..." });
 * const stream = client.streamEvents(runId);
 * for await (const event of stream) console.log(event.seq, event.kind);
 * console.log(stream.end?.status?.output);
 * ```
 */

export { SalvorClient } from "./client.js";
export type { SalvorClientOptions, EventStream } from "./client.js";
export {
  SalvorError,
  SalvorApiError,
  NeedsReconciliationError,
  SalvorStreamError,
} from "./errors.js";
export {
  eventUsage,
  type SalvorEvent,
  type EndFrame,
  type RunState,
  type RunStatus,
  type RunSummary,
  type ReplayState,
  type ResumeResult,
  type PendingCall,
  type Usage,
} from "./types.js";
export {
  GraphBuilder,
  SCHEMA_VERSION,
  type Graph,
  type GraphNode,
  type Edge,
  type JsonValue,
  type AgentPayload,
  type ToolPayload,
  type GatePayload,
  type BranchPayload,
  type BranchCase,
  type BranchCondition,
  type MapPayload,
  type MapBody,
  type AgentOptions,
  type ToolOptions,
  type GateOptions,
  type BranchOptions,
  type MapOptions,
} from "./graph.js";

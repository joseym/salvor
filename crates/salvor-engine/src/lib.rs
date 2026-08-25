//! The Salvor graph engine: drives a frozen graph document through its nodes,
//! recording the walk into one durable run log.
//!
//! # Where this crate sits, and why it is its own crate
//!
//! The engine is deliberately **not** part of `salvor-runtime` (that would drag
//! the graph document format into the built-in agent loop) and **not** part of
//! `salvor-graph` (that crate is a pure, IO-free leaf). It sits above both and
//! composes only their public surfaces: the graph document from `salvor-graph`,
//! and the durability substrate ([`RunCtx`](salvor_runtime::RunCtx),
//! [`drive_loop`](salvor_runtime::drive_loop)) from `salvor-runtime`. It reaches
//! into nothing private. That is a deliberate proof of the runtime's API
//! guardrail: everything the engine needs, an outside crate could also do.
//!
//! # What it drives
//!
//! [`run_graph`] opens a run's log with `GraphRunStarted`, walks the nodes in
//! deterministic topological order (see [`walk`]), and drives each one:
//!
//! - an **agent** node runs the built-in agent loop
//!   ([`drive_loop`](salvor_runtime::drive_loop)) inside the same log, framed by
//!   `NodeEntered` / `NodeExited`. A node that declares an `output_schema` runs
//!   the structured form of that loop
//!   ([`drive_loop_structured`](salvor_runtime::drive_loop_structured)) instead,
//!   so the node's output is the validated object the schema describes rather
//!   than the reply text, and downstream expressions can read its fields. The
//!   resolved agent may declare a schema of its own, in which case the node's
//!   declaration wins and the agent's is the fallback (see
//!   [`Agent::output_schema`](salvor_runtime::Agent::output_schema));
//! - a **tool** node records one tool call through the same write-ahead
//!   intent/completion machinery the built-in loop uses, honoring the tool's
//!   effect class. A tool that asks to park the run parks it: a suspension
//!   through `Suspended` / `Resumed`, a sleep through `SleepStarted` /
//!   `SleepCompleted`. Either way the node stays entered with no `NodeExited`,
//!   so a later drive re-enters it and continues from the recorded park. The
//!   sleep request rides inside the call's own completion, so the call settles
//!   (and releases any idempotency claim) before the timer starts;
//! - a **gate** node parks the run through the exact `Suspended` / `Resumed`
//!   machinery the built-in loop uses for a tool suspension: entering it records
//!   `NodeEntered`, then `suspend` records the gate's `approval_schema` as the
//!   suspension schema and the drive returns [`GraphOutcome::Parked`]. A later
//!   drive over the log (carrying the resume input the existing resume machinery
//!   appended) passes that input through the gate as its output and continues.
//!   A gate needs no event kind of its own. A resume input is ENFORCED against
//!   the gate's `approval_schema` at the accept edge, between the `suspend` and
//!   the `await_resume` that would record it, so a non-conforming approval is a
//!   typed refusal that appends nothing and leaves the run parked; a recorded
//!   `Resumed` is never re-judged on replay (see [`approval`]);
//! - a **branch** node routes on its input: an expression branch evaluates its
//!   cases in author order and the first true case wins; a model-decision branch
//!   drives the node's agent and maps the reply to a case name. Either way the
//!   chosen case is recorded as `BranchTaken`, the walk follows the like-named
//!   edge, and every node reachable only through a non-taken case is recorded
//!   `NodeSkipped`;
//! - a **map** node fans out over a list. Its `over` reference resolves against
//!   the routed value to a JSON array (a non-array is a typed
//!   [`EngineError::MapOverNotAList`] refused before `NodeEntered`); the engine
//!   records `NodeEntered`, then `MapFannedOut` with the resolved item list, then
//!   walks the list IN INDEX ORDER, and for each element records
//!   `MapIterationStarted` (carrying the derived child-run id), runs the body's
//!   work inline, and records `MapIterationJoined`. The joined output is the
//!   per-element outputs as a list in index order. Iterations run
//!   **inline and sequentially** in the parent's own log: the `concurrency` cap is
//!   accepted (the validator requires it be at least 1) but not honored: a
//!   deliberate v0.4 choice that costs only wall-clock and changes no event shape,
//!   so the whole fan-out is proven by the same single-log replay machinery
//!   already proven for linear and branching graphs. Concurrent child runs are
//!   not yet supported. A
//!   `subgraph` body, or a body node that is not an `agent` or `tool`, is a typed
//!   [`EngineError::UnsupportedMapBody`] refused before `NodeEntered`;
//! - a **fold** node runs its body up to `max_iterations` times, inline and
//!   sequentially in the same log. Pass 0's input is the fold's routed value;
//!   every later pass's input is the previous pass's output, which IS the
//!   accumulated value: there is no separate accumulator state and no merge rule,
//!   because the document has no vocabulary for one. A value that is an MCP
//!   result envelope (an object with a `content` ARRAY and a
//!   `structuredContent` key) contributes that PAYLOAD as the accumulated
//!   value, not the object around it: an MCP tool answers with a
//!   `{content, structuredContent}` envelope, and the payload is the value the
//!   loop is folding, so the next pass's input, the `stop_when` predicate, a
//!   `best_by` reference, and the join's own output all read the bare payload
//!   and a fold expression never reaches through a transport detail. This holds
//!   for the value ENTERING the fold as much as for one a pass produced, so a
//!   fold fed by a `tool` node over an edge folds the same shape at pass 0 that
//!   it folds at pass 3. Any other value (an agent body's structured object, a
//!   native tool's flat struct, an object that merely carries a field called
//!   `structuredContent` as data, a string, a list) is carried verbatim. The
//!   unwrap is DERIVATION, not
//!   recording: `ToolCallCompleted` still holds the whole envelope, and the
//!   payload is a pure function of it (see [`unwrap_pass_output`]). Nothing
//!   outside a fold is touched: an ordinary edge routes a node's recorded output
//!   verbatim, so a branch expression reading `structuredContent.` still reads
//!   what it always read. The engine
//!   records `NodeEntered`, then per pass `FoldIterationStarted`, the body's
//!   work inline,
//!   and `FoldIterationJoined`, and stops when the `stop_when` predicate holds
//!   over the pass just joined or when the bound is reached (there is no third
//!   cause: nothing stops a fold for "failing to improve"). Reaching the bound
//!   means what `on_bound` says it means: absent or `join` joins the passes
//!   anyway, and `fail` is a typed [`EngineError::FoldBoundExceeded`] returned
//!   from exactly where `FoldConverged` would have been recorded, so the passes
//!   and their joins stay in the log and no convergence and no `NodeExited`
//!   land. Otherwise the `join` rule
//!   picks the value the node produces: `last` takes the final pass, `all` takes
//!   every pass's value as a list in pass order, and `best_by` is an argmax over
//!   ALL passes of the reference's value, ordered by the expression language's
//!   own comparison ([`salvor_graph::expr::compare`]) so the argmax and the
//!   predicate beside it can never order values differently, keeping the earliest
//!   pass on a tie. A `best_by` with no comparable candidate in any pass is a
//!   typed [`EngineError::FoldNoComparableCandidate`] refused before
//!   `FoldConverged`. The chosen winner and the stop reason are recorded on
//!   `FoldConverged`, then `NodeExited`. A `subgraph` body, or a body node that
//!   is not an `agent` or `tool`, is a typed
//!   [`EngineError::UnsupportedFoldBody`] refused before `NodeEntered`;
//! - a **delay** node parks the run on a durable timer, the timer counterpart
//!   of the gate: `NodeEntered`, then `sleep_for` (a recorded clock reading
//!   followed by `SleepStarted { wake_at }`), then a wait. Before the deadline
//!   the drive returns [`GraphOutcome::Parked`] with
//!   [`ParkReason::Sleeping`](salvor_runtime::ParkReason::Sleeping) and no
//!   `NodeExited`, so a drive that arrives early records nothing and a later
//!   one re-enters the same node and continues from the recorded sleep. At or
//!   past the deadline `SleepCompleted` lands, `NodeExited` closes the node,
//!   and the walk continues with the node's input passed through UNCHANGED: a
//!   delay moves a run in time, never in value, so its output is its input
//!   verbatim. It needs no event kind of its own; the sleep pair is the whole
//!   vocabulary. The wait is a DURATION in the document and the instant is
//!   derived from the recorded reading, which is what keeps the same document
//!   runnable more than once (see [`salvor_graph::DelayNode`]).
//!
//! A node that is a map's or a fold's body is executed ONLY as that owner's
//! per-item or per-pass worker; it is never walked independently, so its own
//! events (a tool call, an agent loop) are recorded inline between the owner's
//! iteration markers and its node id is never framed with a `NodeEntered` of its
//! own. That keeps node ids unambiguous in the one log and is why forking INTO a
//! map iteration or a fold pass is refused: neither is a node boundary (see
//! [`plan_fork`]).
//!
//! After the last node the engine records the single terminal `RunCompleted`.
//! It records no terminal for a refusal: refuse-before-record is what keeps the
//! log free of events past one. Whether a refused run is DEAD or merely stuck is
//! the driver's call, and [`EngineError::is_permanent`] is how the engine tells
//! it apart; [`record_permanent_refusal`] is the append the drivers make when
//! the answer is dead. There is no ambient clock or randomness in any decision: everything the
//! engine feeds forward (the walk order, each node's input, the branch route, a
//! map's resolved item list and its per-iteration child ids, a fold's stop
//! decision, its winner and its recorded reason, an idempotent tool's
//! idempotency key) is a pure function of the document or of values the `RunCtx`
//! recorded, so a second drive over the recorded log replays with no live calls
//! and produces a byte-identical log. A map iteration's child-run id is
//! `sha256:` over the parent run id, the node id, and the index (see
//! [`map_child_run_id`]): pure recorded data, so replay reconstructs the
//! identical id without storing anything extra. The idempotency
//! key is derived from the call's position in the graph (graph hash, node id,
//! call index) rather than from drawn randomness, which is what lets a FORK of a
//! run re-walk a segment and present the same key its origin recorded. See
//! [`fork_safe_idempotency_key`] and the `salvor-server` fork endpoint.
//!
//! # Data flow
//!
//! Each node's output flows to its successors along the edges, and a node's
//! input is the recorded output of the live inbound edge that reaches it (the
//! graph input for an entry node with no inbound edge). A branch passes its
//! routed value through unchanged to the taken case's edge; the decision only
//! selects the route, never the data. A tool node's `input` references are still
//! not resolved yet; the upstream output is the downstream input
//! verbatim. When more than one live inbound edge reaches a node, the one whose
//! source id is smallest wins, so the merge is a pure function of the document.
//!
//! # Resolving agents and tools
//!
//! A node names its agent by hash and its tool by name; the engine turns those
//! into executables through the [`AgentResolver`] and [`ToolResolver`] traits
//! the caller supplies. Tests inject maps; the server wires its own
//! registries in separately. Keeping resolution behind a trait is what lets the engine stay
//! ignorant of where agents and tools actually come from.

#![warn(missing_docs)]

pub mod approval;
mod error;
pub mod fork;
mod walk;

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use salvor_core::{Effect, RunId};
use salvor_graph::expr::{Expr, Reference};
use salvor_graph::{
    AgentNode, BranchCondition, BranchNode, DelayNode, Edge, FoldBody, FoldJoin, FoldNode,
    GateNode, Graph, MapBody, MapNode, Node, OnBound,
};
use salvor_runtime::{
    Agent, LoopOutcome, ParkReason, Resumption, RunCtx, ToolCallResult, Waking, drive_loop,
    drive_loop_structured, hash_value, slept_output,
};
use salvor_tools::DynTool;
use serde_json::{Value, json};

pub use approval::{ApprovalViolation, approval_violations, parked_gate};
pub use error::EngineError;
pub use fork::{ForkError, ForkPlan, WriteHazard, plan_fork};

/// Resolves an `agent` node's declared hash to the [`Agent`] that executes it.
///
/// A small trait, not a fixed type, so a test can inject a map while the server
/// injects its agent registry. A [`HashMap<String, Agent>`](std::collections::HashMap)
/// implements it out of the box.
pub trait AgentResolver {
    /// The agent registered under `agent_hash`, or `None` if none is.
    fn resolve_agent(&self, agent_hash: &str) -> Option<&Agent>;
}

/// Resolves a `tool` node's declared name to the [`DynTool`] that executes it.
///
/// The tool counterpart of [`AgentResolver`]. A
/// [`HashMap<String, Box<dyn DynTool>>`](std::collections::HashMap) implements
/// it out of the box.
pub trait ToolResolver {
    /// The tool registered under `name`, or `None` if none is.
    fn resolve_tool(&self, name: &str) -> Option<&dyn DynTool>;
}

impl AgentResolver for HashMap<String, Agent> {
    fn resolve_agent(&self, agent_hash: &str) -> Option<&Agent> {
        self.get(agent_hash)
    }
}

impl ToolResolver for HashMap<String, Box<dyn DynTool>> {
    fn resolve_tool(&self, name: &str) -> Option<&dyn DynTool> {
        self.get(name).map(AsRef::as_ref)
    }
}

/// How a graph drive ended.
#[derive(Debug)]
pub enum GraphOutcome {
    /// The graph ran to completion; this is the final output the terminal
    /// `RunCompleted` recorded.
    Completed {
        /// The graph run's final output (the last node's output).
        output: Value,
    },
    /// A node parked the run durably (an agent's budget crossing or a tool
    /// suspension). The run survives restarts; resume it through the runtime's
    /// resume path, then drive the graph again to continue.
    Parked {
        /// The node that parked.
        node: String,
        /// Why it parked.
        reason: ParkReason,
    },
}

/// Computes a graph document's content hash: `sha256:` over its canonical JSON,
/// the exact string recorded in `GraphRunStarted`.
///
/// Reuses `salvor-runtime`'s canonical hashing (the same story behind
/// `agent_def_hash` and `request_hash`), so a graph run's `graph_hash` is
/// reproducible and matches whatever a control plane computes for the same
/// document.
///
/// # Errors
///
/// [`EngineError::GraphEncode`] if the document cannot be serialized (it always
/// can; the edge is kept honest rather than panicking).
pub fn graph_hash(graph: &Graph) -> Result<String, EngineError> {
    let value = serde_json::to_value(graph).map_err(EngineError::GraphEncode)?;
    Ok(hash_value(&value))
}

/// Drives `graph` to completion (or a park) over `ctx`, recording the walk into
/// the run's log.
///
/// The log opens with `GraphRunStarted { graph_hash }`, each node contributes
/// `NodeEntered` … its own events … `NodeExited`, and the run closes with one
/// `RunCompleted`. See the crate docs for the node handling and determinism
/// guarantees. Fresh, recovering, or replaying is entirely the `ctx`'s
/// business: the engine issues the same sequence of `RunCtx` calls either way,
/// which is what makes a second drive over the recorded log a byte-identical,
/// zero-live-call replay.
///
/// # Errors
///
/// [`EngineError::MapOverNotAList`] when a map node's `over` reference does not
/// resolve to a list, and [`EngineError::UnsupportedMapBody`] for a `subgraph` or
/// non-`agent`/`tool` body (both before the map's `NodeEntered` is recorded);
/// [`EngineError::UnsupportedFoldBody`] for a fold node's `subgraph` or
/// non-`agent`/`tool` body (before the fold's `NodeEntered`), and
/// [`EngineError::FoldNoComparableCandidate`] when a `best_by` join finds no
/// comparable value in any pass, and [`EngineError::FoldBoundExceeded`] when a
/// fold declaring `on_bound: fail` reaches its bound with `stop_when` still
/// unsatisfied (both after the passes ran, before `FoldConverged`);
/// [`EngineError::NoBranchCaseMatched`] when an expression branch
/// matches no case (also before its `NodeEntered`);
/// [`EngineError::BranchDecisionUnmatched`] when a model-decision branch's agent
/// names no case (after its `NodeEntered`, since the model had to run);
/// [`EngineError::UnknownAgent`] / [`EngineError::UnknownTool`] when a resolver
/// cannot supply a node's executable; [`EngineError::MalformedGraph`] when the
/// topology is not a DAG (or, unreachable in practice, a branch condition the
/// validator accepted fails to parse here, or a `delay` node's wait is zero or
/// out of range, refused before that node's `NodeEntered`); [`EngineError::ToolFailed`] when a
/// tool call fails; [`EngineError::Runtime`] for any replay divergence,
/// reconciliation refusal, provider, or store error.
pub async fn run_graph(
    ctx: &mut RunCtx,
    graph: &Graph,
    input: &Value,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
) -> Result<GraphOutcome, EngineError> {
    let hash = graph_hash(graph)?;
    // The recorded input always wins on replay; `begin_graph` returns it.
    let graph_input = ctx.begin_graph(&hash, input).await?;

    // Topology and routing state, all keyed on ids that borrow the document.
    let by_id: HashMap<&str, &Node> = graph.nodes.iter().map(|n| (n.id(), n)).collect();
    let mut inbound: HashMap<&str, Vec<&Edge>> = HashMap::new();
    for edge in &graph.edges {
        inbound.entry(edge.to.as_str()).or_default().push(edge);
    }
    // Branch conditions are parsed ONCE here (the validator already guarantees
    // they parse; a failure now is a MalformedGraph unreachable in practice).
    let branches = parse_branches(graph)?;
    // Every fold's `stop_when` (and its `best_by` reference) is parsed ONCE
    // here, on the same terms the branch conditions are.
    let folds = parse_folds(graph)?;
    // The ids of every node used as a `map` or `fold` body: they are the
    // per-item workers of their map and the per-pass workers of their fold, and
    // are executed ONLY inside that owner's loop, never walked independently, so
    // their events stay unambiguous inside the one log.
    let body_targets: HashSet<&str> = graph
        .nodes
        .iter()
        .filter_map(|node| match node {
            Node::Map(map) => match &map.body {
                MapBody::Node(target) => Some(target.as_str()),
                MapBody::Subgraph(_) => None,
            },
            Node::Fold(fold) => match &fold.body {
                FoldBody::Node(target) => Some(target.as_str()),
                FoldBody::Subgraph(_) => None,
            },
            _ => None,
        })
        .collect();

    // What each executed node produced, which nodes were skipped, and which case
    // each branch fired: the pure state the routing reads. `last_output` threads
    // the terminal output, seeded with the graph input so an empty graph still
    // completes with it (matching a linear graph with no nodes at all).
    let mut outputs: HashMap<&str, Value> = HashMap::new();
    let mut skipped: HashSet<&str> = HashSet::new();
    let mut branch_case: HashMap<&str, String> = HashMap::new();
    let mut last_output = graph_input.clone();

    for node in walk::walk_order(graph)? {
        let id = node.id();
        // A node used as a map or fold body is not walked independently: it runs
        // only as its owner's per-item or per-pass worker, inline in the loop
        // below. Nothing is recorded for it here: it is body-owned, not
        // "skipped".
        if body_targets.contains(id) {
            continue;
        }
        // A node with no live inbound edge was routed past: record the skip (its
        // sole marker) and move on. Predecessors are visited first in topological
        // order, so their skip/branch state is already known here.
        let Some(node_input) = select_input(
            id,
            &inbound,
            &by_id,
            &branch_case,
            &skipped,
            &outputs,
            &graph_input,
        ) else {
            ctx.node_skipped(id, SKIP_REASON).await?;
            skipped.insert(id);
            continue;
        };

        match node {
            Node::Agent(agent_node) => {
                let agent = agents
                    .resolve_agent(&agent_node.agent_hash)
                    .ok_or_else(|| EngineError::UnknownAgent {
                        node: agent_node.id.clone(),
                        agent_hash: agent_node.agent_hash.clone(),
                    })?;
                ctx.node_entered(id).await?;
                // The agent loop runs inside this same log via the runtime's
                // begin/drive_loop split: no second run head, and it returns the
                // output without recording a terminal (the engine owns that).
                match drive_agent_node(ctx, agent_node, agent, &node_input).await? {
                    LoopOutcome::Completed(output) => {
                        ctx.node_exited(id).await?;
                        last_output = output.clone();
                        outputs.insert(id, output);
                    }
                    LoopOutcome::Parked(reason) => {
                        return Ok(GraphOutcome::Parked {
                            node: agent_node.id.clone(),
                            reason,
                        });
                    }
                }
            }
            Node::Tool(tool_node) => {
                let tool = tools.resolve_tool(&tool_node.tool).ok_or_else(|| {
                    EngineError::UnknownTool {
                        node: tool_node.id.clone(),
                        tool: tool_node.tool.clone(),
                    }
                })?;
                ctx.node_entered(id).await?;
                // An idempotent tool's key is a PURE function of WHERE the call
                // sits in the graph (the graph hash, the node id, the call index
                // within the node), not of drawn randomness. That is what makes it
                // fork-safe: a fork re-walks the segment from its fork node and
                // re-executes the idempotent calls in it live, and this derivation
                // hands each of them the IDENTICAL key its origin recorded, so the
                // provider collapses the duplicate. It also leaves `Effect::Write`
                // as the sole class a fork must have acknowledged. Read and write
                // tools carry no key (see the built-in loop).
                let idempotency_key = match tool.effect() {
                    Effect::Idempotent => Some(fork_safe_idempotency_key(&hash, id, 0)),
                    Effect::Read | Effect::Write => None,
                };
                match ctx
                    .tool_call(tool, &node_input, idempotency_key.as_deref())
                    .await?
                {
                    ToolCallResult::Output(output) => {
                        ctx.node_exited(id).await?;
                        last_output = output.clone();
                        outputs.insert(id, output);
                    }
                    ToolCallResult::Failed(failure) => {
                        return Err(EngineError::ToolFailed {
                            node: tool_node.id.clone(),
                            message: failure.message,
                        });
                    }
                    ToolCallResult::Suspended(suspension) => {
                        // Whatever the tool said it waits on is recorded and
                        // reported. A node parked on a webhook is not a gate
                        // and must not read as one.
                        ctx.suspend_with_kind(
                            &suspension.reason,
                            &suspension.input_schema,
                            suspension.kind,
                        )
                        .await?;
                        match ctx.await_resume().await? {
                            Resumption::Parked => {
                                return Ok(GraphOutcome::Parked {
                                    node: tool_node.id.clone(),
                                    reason: ParkReason::Suspended {
                                        reason: suspension.reason,
                                        input_schema: suspension.input_schema,
                                        kind: suspension.kind,
                                    },
                                });
                            }
                            Resumption::Resumed(resume_input) => {
                                // The recorded resume input is the tool's answer.
                                ctx.node_exited(id).await?;
                                last_output = resume_input.clone();
                                outputs.insert(id, resume_input);
                            }
                        }
                    }
                    // The timer park, the same shape as the suspension above:
                    // the node stays entered with no `NodeExited`, so a later
                    // drive re-enters it and continues from the recorded sleep.
                    // The call settled before the sleep started (its completion
                    // carried the request), so a node asleep for a week holds
                    // no idempotency claim.
                    ToolCallResult::Sleeping(sleep) => {
                        ctx.sleep_until(sleep.wake_at).await?;
                        match ctx.await_wake().await? {
                            Waking::Asleep { wake_at } => {
                                return Ok(GraphOutcome::Parked {
                                    node: tool_node.id.clone(),
                                    reason: ParkReason::Sleeping { wake_at },
                                });
                            }
                            Waking::Woken => {
                                // The tool named a deadline instead of a value,
                                // so the node's output is derived from the wake
                                // instant its completion recorded: pure, and
                                // identical on every replay.
                                let output = slept_output(sleep.wake_at);
                                ctx.node_exited(id).await?;
                                last_output = output.clone();
                                outputs.insert(id, output);
                            }
                        }
                    }
                }
            }
            // A gate parks through the exact suspension machinery a tool uses:
            // NodeEntered, then `suspend` recording the gate's approval schema,
            // then a park. A later drive over the resumed log passes the resume
            // input through as the gate's output. No gate-specific event kind.
            Node::Gate(gate) => {
                ctx.node_entered(id).await?;
                let reason = gate_reason(gate);
                ctx.suspend(&reason, &gate.approval_schema).await?;
                // THE ACCEPT EDGE. Right here, and nowhere later, is where a
                // resume input may be judged: the gate's `Suspended` is on
                // disk, and the next line can append a `Resumed`. Refusing
                // before that append is what keeps the refusal free: nothing
                // lands in the log and the run stays parked at this gate,
                // waiting for an approval that conforms.
                //
                // The guard is `is_replaying()`. When history remains, the next
                // event is a RECORDED `Resumed`, and a recorded `Resumed` is
                // never re-judged: replay trusts what was written. That is
                // load-bearing rather than an optimization. If replay
                // re-validated, a stricter validator (or a new `jsonschema`
                // release) would turn logs that replayed yesterday into
                // refusals today, and a durable log that stops replaying is not
                // durable. So: check what has not been written, trust what has.
                if !ctx.is_replaying()
                    && let Some(input) = ctx.staged_resume_input()
                {
                    let violations = approval_violations(input, &gate.approval_schema);
                    if !violations.is_empty() {
                        return Err(EngineError::ApprovalSchemaViolation {
                            node: gate.id.clone(),
                            violations,
                        });
                    }
                }
                match ctx.await_resume().await? {
                    Resumption::Parked => {
                        return Ok(GraphOutcome::Parked {
                            node: gate.id.clone(),
                            reason: ParkReason::Suspended {
                                reason,
                                input_schema: gate.approval_schema.clone(),
                                // A gate is the human park by definition, so
                                // it names no discriminator.
                                kind: None,
                            },
                        });
                    }
                    Resumption::Resumed(resume_input) => {
                        ctx.node_exited(id).await?;
                        last_output = resume_input.clone();
                        outputs.insert(id, resume_input);
                    }
                }
            }
            // A delay parks on a timer the way a gate parks on a person: enter
            // the node, park, and leave no `NodeExited` behind, so a later
            // drive re-enters it and continues from the recorded sleep. No
            // delay-specific event kind: `SleepStarted` / `SleepCompleted` is
            // the whole vocabulary, exactly as `Suspended` / `Resumed` is the
            // gate's.
            Node::Delay(delay) => {
                // Refuse-before-record: the declared wait becomes a duration
                // here, ahead of `NodeEntered`, so a document the validator
                // would have refused leaves nothing in the log.
                let duration = delay_duration(delay)?;
                ctx.node_entered(id).await?;
                // `sleep_for` is `now()` then `sleep_until`: the clock reading
                // lands in the log as a `NowObserved` and the wake instant is
                // derived from it, so the instant is a pure function of
                // recorded data and every later drive derives the identical
                // one. A wake instant baked into the document could not make
                // that claim on a second run, which is why the field is a
                // duration.
                ctx.sleep_for(duration).await?;
                match ctx.await_wake().await? {
                    Waking::Asleep { wake_at } => {
                        return Ok(GraphOutcome::Parked {
                            node: delay.id.clone(),
                            reason: ParkReason::Sleeping { wake_at },
                        });
                    }
                    // A delay transforms nothing, so its output is its input
                    // verbatim, the same pass-through a branch performs: the
                    // node moves the run in time, never in value.
                    Waking::Woken => {
                        ctx.node_exited(id).await?;
                        last_output = node_input.clone();
                        outputs.insert(id, node_input);
                    }
                }
            }
            Node::Branch(branch) => {
                // A branch is a pure router: whichever case fires, the routed
                // value passes through unchanged to the taken edge.
                let cases = branches.get(id).expect("every branch node is parsed");
                let chosen: String = match &branch.agent_hash {
                    // Expression branch: choose purely, so a no-match refuses
                    // before NodeEntered and nothing lands past the refusal.
                    None => {
                        let case = choose_expression_case(id, cases, &node_input)?;
                        ctx.node_entered(id).await?;
                        case.to_owned()
                    }
                    // Model-decision branch: the agent must run first, so its
                    // NodeEntered and model events precede the mapping (and the
                    // BranchDecisionUnmatched refusal, if the reply names no case).
                    Some(agent_hash) => {
                        let agent = agents.resolve_agent(agent_hash).ok_or_else(|| {
                            EngineError::UnknownAgent {
                                node: branch.id.clone(),
                                agent_hash: agent_hash.clone(),
                            }
                        })?;
                        ctx.node_entered(id).await?;
                        let reply = match drive_loop(ctx, agent, &node_input).await? {
                            LoopOutcome::Completed(output) => output,
                            LoopOutcome::Parked(reason) => {
                                return Ok(GraphOutcome::Parked {
                                    node: branch.id.clone(),
                                    reason,
                                });
                            }
                        };
                        match_decision(branch, &reply)?.to_owned()
                    }
                };
                ctx.branch_taken(id, &chosen).await?;
                ctx.node_exited(id).await?;
                branch_case.insert(id, chosen);
                last_output = node_input.clone();
                outputs.insert(id, node_input);
            }
            Node::Map(map_node) => {
                match drive_map(ctx, map_node, &node_input, &by_id, agents, tools, &hash).await? {
                    MapOutcome::Joined(output) => {
                        last_output = output.clone();
                        outputs.insert(id, output);
                    }
                    MapOutcome::Parked { node, reason } => {
                        return Ok(GraphOutcome::Parked { node, reason });
                    }
                }
            }
            // A fold's parsed plan carries the node itself, so the drive takes
            // one argument for both.
            Node::Fold(_) => {
                let plan = folds.get(id).expect("every fold node is parsed");
                match drive_fold(ctx, plan, &node_input, &by_id, agents, tools, &hash).await? {
                    FoldOutcome::Converged(output) => {
                        last_output = output.clone();
                        outputs.insert(id, output);
                    }
                    FoldOutcome::Parked { node, reason } => {
                        return Ok(GraphOutcome::Parked { node, reason });
                    }
                }
            }
        }
    }

    ctx.complete_run(&last_output).await?;
    Ok(GraphOutcome::Completed {
        output: last_output,
    })
}

/// Records the terminal `RunFailed` a PERMANENT [`run_graph`] refusal deserves,
/// so a dead run stops masquerading as a running one.
///
/// [`run_graph`] itself never writes a terminal for a refusal, and that is
/// deliberate: refuse-before-record is what keeps the log free of events past a
/// refusal, and the engine cannot know whether its caller intends to re-drive.
/// The DRIVER does know, and it is the driver that owns the run's disposition.
/// So the drivers (the CLI's `graph run`, `resume`, and `graph fork` paths, and
/// the server's `drive_graph` task) call this with the error they just received
/// and THE SAME `ctx` that produced it. The same one matters: the append claims
/// the position that `ctx`'s cursor stands on, which is the position after
/// everything the refused drive replayed or wrote. A fresh `RunCtx` over the
/// same log stands at position zero and would diverge rather than append, which
/// is the machinery refusing to write a terminal onto a run it has not read.
///
/// Only a permanent error is recorded: one that
/// [`EngineError::is_permanent`] calls a pure function of the frozen document
/// and the recorded log, so re-driving reproduces it forever. A transient error
/// is left exactly as it was, and the run stays recoverable. Returns whether a
/// terminal was recorded, so a caller can say so in its own voice.
///
/// # Ordering, and the kill between the refusal and this append
///
/// The append goes through [`RunCtx::fail_run`](salvor_runtime::RunCtx::fail_run),
/// which is the same persist discipline every other event uses: the cursor
/// claims the next sequence and the envelope is durable before this returns.
/// That is also the "only when the log holds no terminal" guard, and it needs no
/// second read of the store. A log whose recorded next event is the identical
/// `RunFailed` (a driver that already did this) REPLAYS it and appends nothing;
/// a log holding a different terminal is a divergence the caller surfaces
/// rather than overwrites.
///
/// A `kill -9` landing between the refusal and this append therefore leaves a
/// log with no terminal at all, ending at whatever the refusal was recorded
/// past (for a fold, its last `FoldIterationJoined`). That log is not corrupt
/// and not stuck: the next drive replays it, re-derives the SAME permanent
/// refusal from the same recorded values, and this appends the `RunFailed`
/// then. Nothing re-executes, because everything before the refusal is history.
/// The window is a delay in the status an operator reads, never a divergence.
///
/// # Errors
///
/// [`RuntimeError`](salvor_runtime::RuntimeError) when the append does not
/// persist, or when the log already holds a different terminal. Callers report
/// the ORIGINAL engine refusal in that case: it is the real news, and losing
/// the terminal only means the run reads as recoverable when it is not.
pub async fn record_permanent_refusal(
    ctx: &mut RunCtx,
    error: &EngineError,
) -> Result<bool, salvor_runtime::RuntimeError> {
    if !error.is_permanent() {
        return Ok(false);
    }
    ctx.fail_run(&error.to_string()).await?;
    Ok(true)
}

/// The wait a `delay` node declares, as a duration the runtime can sleep for.
///
/// Two refusals, both pure functions of the frozen document and both raised
/// BEFORE the node's `NodeEntered`, so a document that trips either leaves
/// nothing in the log. A zero wait is the one
/// [`salvor_graph::validate`] already reports as `NonPositiveDelay`; this
/// re-checks it defensively for the same reason [`drive_fold`] re-checks a
/// fold's bound, because a validator is a submit-time gate and the engine can
/// be handed a document by some other route. A wait past `i64::MAX` seconds
/// cannot be a [`time::Duration`] at all; it is astronomically out of range
/// rather than merely long, and the alternative to naming it is a panic in a
/// conversion.
fn delay_duration(delay: &DelayNode) -> Result<time::Duration, EngineError> {
    if delay.seconds < 1 {
        return Err(EngineError::MalformedGraph {
            detail: format!(
                "delay node `{}`: seconds must be at least 1, found {}",
                delay.id, delay.seconds
            ),
        });
    }
    let seconds = i64::try_from(delay.seconds).map_err(|_| EngineError::MalformedGraph {
        detail: format!(
            "delay node `{}`: a wait of {} seconds is outside the representable range",
            delay.id, delay.seconds
        ),
    })?;
    Ok(time::Duration::seconds(seconds))
}

/// How driving one map node's fan-out ended.
enum MapOutcome {
    /// Every iteration joined; the map's output is the per-index outputs as a
    /// JSON array in index order.
    Joined(Value),
    /// An iteration parked the run (an `agent` body's budget crossing or a `tool`
    /// body's suspension), propagated up as a graph park at the map node.
    Parked {
        /// The map node that parked.
        node: String,
        /// Why it parked.
        reason: ParkReason,
    },
}

/// Drives one map node's fan-out inline and sequentially into the parent log.
///
/// Refuses an unsupported body form or a non-list `over` **before** recording the
/// map's `NodeEntered`, so nothing lands in the log past such a refusal. Then
/// records `NodeEntered`, `MapFannedOut` with the resolved item list, and for each
/// element in INDEX ORDER records `MapIterationStarted` (with the derived
/// child-run id), runs the body's work inline, and records `MapIterationJoined`.
/// The `concurrency` cap is accepted but not honored: iterations run
/// one after another, which is why the whole fan-out is a plain single-log replay.
async fn drive_map(
    ctx: &mut RunCtx,
    map_node: &MapNode,
    routed: &Value,
    by_id: &HashMap<&str, &Node>,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
    graph_hash: &str,
) -> Result<MapOutcome, EngineError> {
    let node_id = map_node.id.as_str();

    // Resolve the body up front so an unsupported body form refuses BEFORE the
    // map's NodeEntered is recorded.
    let body: &Node = match &map_node.body {
        MapBody::Node(target) => {
            let body_node =
                by_id
                    .get(target.as_str())
                    .copied()
                    .ok_or_else(|| EngineError::MalformedGraph {
                        detail: format!("map node `{node_id}`: body names unknown node `{target}`"),
                    })?;
            match body_node {
                Node::Agent(_) | Node::Tool(_) => body_node,
                other => {
                    return Err(EngineError::UnsupportedMapBody {
                        node: node_id.to_owned(),
                        detail: format!(
                            "a `{}` body node cannot be a per-item worker; only `agent` and `tool` bodies run",
                            other.kind_name()
                        ),
                    });
                }
            }
        }
        MapBody::Subgraph(_) => {
            return Err(EngineError::UnsupportedMapBody {
                node: node_id.to_owned(),
                detail: "an embedded `subgraph` body is not executed yet".to_owned(),
            });
        }
    };

    // Resolve `over` against the routed value; a non-array (including a missing
    // path) is a typed refusal BEFORE NodeEntered.
    let items = resolve_over(node_id, &map_node.over, routed)?;

    ctx.node_entered(node_id).await?;
    ctx.map_fanned_out(node_id, &Value::Array(items.clone()))
        .await?;

    let mut joined: Vec<Value> = Vec::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        let index = position as u64;
        let child_run = map_child_run_id(ctx.run_id(), node_id, index);
        ctx.map_iteration_started(node_id, index, &child_run)
            .await?;
        let call = BodyCall {
            owner: BodyOwner::Map,
            graph_hash,
            node_id,
            index,
        };
        match run_body(ctx, body, item, agents, tools, call).await? {
            IterationOutcome::Output(output) => joined.push(output),
            IterationOutcome::Parked(reason) => {
                return Ok(MapOutcome::Parked {
                    node: node_id.to_owned(),
                    reason,
                });
            }
        }
        // Joins are recorded strictly in index order, never completion order.
        ctx.map_iteration_joined(node_id, index).await?;
    }
    ctx.node_exited(node_id).await?;
    Ok(MapOutcome::Joined(Value::Array(joined)))
}

/// How one map iteration's or fold pass's body work ended.
enum IterationOutcome {
    /// The body produced this output for the iteration or pass.
    Output(Value),
    /// The body parked (an agent budget crossing or a tool suspension).
    Parked(ParkReason),
}

/// Which node kind owns an inline body run. Carried only so the unsupported-body
/// error the shared runner returns names the right kind; the two owners run the
/// body identically.
#[derive(Clone, Copy)]
enum BodyOwner {
    Map,
    Fold,
}

/// Where one map iteration's or fold pass's tool call sits, for deriving its
/// fork-safe idempotency key: the graph hash, the owning node's id, and the
/// iteration or pass index.
struct BodyCall<'a> {
    /// Which node kind owns this run.
    owner: BodyOwner,
    /// The graph document hash.
    graph_hash: &'a str,
    /// The owning map or fold node id (the "node" this call belongs to).
    node_id: &'a str,
    /// The zero-based iteration or pass index (the call index within the node).
    index: u64,
}

impl BodyCall<'_> {
    /// The refusal for a body node kind that cannot be a worker, named for the
    /// owner. Both callers validate the body kind before the loop, so this is
    /// only ever built on a path the caller already proved unreachable.
    fn unsupported_body(&self, detail: String) -> EngineError {
        match self.owner {
            BodyOwner::Map => EngineError::UnsupportedMapBody {
                node: self.node_id.to_owned(),
                detail,
            },
            BodyOwner::Fold => EngineError::UnsupportedFoldBody {
                node: self.node_id.to_owned(),
                detail,
            },
        }
    }
}

/// Drives one `agent` node's loop, wherever it sits: walked as a node of its
/// own, or run inline as a map's or fold's per-item worker.
///
/// A declared `output_schema` is the whole difference between the two loops.
/// With one, the runtime offers the model its answer tool and validates the
/// answer against the schema, so this node's output is that object and a
/// downstream expression can read a field of it; without one, the output is the
/// reply text as before.
///
/// Two places can declare it, and the rule is **node wins, else the agent's
/// own**. The node's `output_schema` is the graph author's statement about what
/// this position in this document needs, made with the whole document in view;
/// the agent's is the agent author's statement about what the agent always
/// produces, made without knowing which graph would call it. The more specific
/// declaration is the more informed one, so it takes the node's when there is
/// one and falls back to the agent's when there is not. A node that declares
/// nothing and an agent that declares nothing stay on the plain text loop, as
/// every graph did before either declaration existed.
async fn drive_agent_node(
    ctx: &mut RunCtx,
    node: &AgentNode,
    agent: &Agent,
    input: &Value,
) -> Result<LoopOutcome, EngineError> {
    let schema = node
        .output_schema
        .as_ref()
        .or_else(|| agent.output_schema());
    let outcome = match schema {
        Some(schema) => drive_loop_structured(ctx, agent, input, schema).await?,
        None => drive_loop(ctx, agent, input).await?,
    };
    Ok(outcome)
}

/// Runs one map iteration's or fold pass's body work inline: the referenced
/// `agent` or `tool` node's work with `item` as its input, recorded in the parent
/// log WITHOUT a `NodeEntered` frame of its own (the owner's markers bracket it
/// instead). The body kind was already validated as `agent` or `tool` by
/// [`drive_map`] or [`drive_fold`].
async fn run_body(
    ctx: &mut RunCtx,
    body: &Node,
    item: &Value,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
    call: BodyCall<'_>,
) -> Result<IterationOutcome, EngineError> {
    match body {
        Node::Agent(agent_node) => {
            let agent = agents
                .resolve_agent(&agent_node.agent_hash)
                .ok_or_else(|| EngineError::UnknownAgent {
                    node: agent_node.id.clone(),
                    agent_hash: agent_node.agent_hash.clone(),
                })?;
            match drive_agent_node(ctx, agent_node, agent, item).await? {
                LoopOutcome::Completed(output) => Ok(IterationOutcome::Output(output)),
                LoopOutcome::Parked(reason) => Ok(IterationOutcome::Parked(reason)),
            }
        }
        Node::Tool(tool_node) => {
            let tool =
                tools
                    .resolve_tool(&tool_node.tool)
                    .ok_or_else(|| EngineError::UnknownTool {
                        node: tool_node.id.clone(),
                        tool: tool_node.tool.clone(),
                    })?;
            // Each iteration or pass is a distinct call of the OWNING node, so
            // its idempotent key is derived from that node's id and the index:
            // the "several calls within one node" case
            // `fork_safe_idempotency_key`'s call-index parameter exists for. A
            // fork re-walking the loop presents each call's identical key;
            // Read/Write carry none.
            let idempotency_key = match tool.effect() {
                Effect::Idempotent => Some(fork_safe_idempotency_key(
                    call.graph_hash,
                    call.node_id,
                    call.index,
                )),
                Effect::Read | Effect::Write => None,
            };
            match ctx
                .tool_call(tool, item, idempotency_key.as_deref())
                .await?
            {
                ToolCallResult::Output(output) => Ok(IterationOutcome::Output(output)),
                ToolCallResult::Failed(failure) => Err(EngineError::ToolFailed {
                    node: tool_node.id.clone(),
                    message: failure.message,
                }),
                ToolCallResult::Suspended(suspension) => {
                    ctx.suspend_with_kind(
                        &suspension.reason,
                        &suspension.input_schema,
                        suspension.kind,
                    )
                    .await?;
                    match ctx.await_resume().await? {
                        Resumption::Parked => Ok(IterationOutcome::Parked(ParkReason::Suspended {
                            reason: suspension.reason,
                            input_schema: suspension.input_schema,
                            kind: suspension.kind,
                        })),
                        Resumption::Resumed(resume_input) => {
                            Ok(IterationOutcome::Output(resume_input))
                        }
                    }
                }
                // The timer park. No join is recorded for a parked iteration or
                // pass, so a later drive re-enters this same one and continues
                // from the recorded sleep, exactly as a suspension does.
                ToolCallResult::Sleeping(sleep) => {
                    ctx.sleep_until(sleep.wake_at).await?;
                    match ctx.await_wake().await? {
                        Waking::Asleep { wake_at } => {
                            Ok(IterationOutcome::Parked(ParkReason::Sleeping { wake_at }))
                        }
                        Waking::Woken => Ok(IterationOutcome::Output(slept_output(sleep.wake_at))),
                    }
                }
            }
        }
        // The caller validated the body kind as agent or tool before the loop.
        other => Err(call.unsupported_body(format!(
            "a `{}` body node cannot be a worker",
            other.kind_name()
        ))),
    }
}

/// How driving one fold node's loop ended.
enum FoldOutcome {
    /// The loop stopped and the `join` rule chose the value the node produces.
    Converged(Value),
    /// A pass parked the run (an `agent` body's budget crossing or a `tool`
    /// body's suspension), propagated up as a graph park at the fold node. No
    /// join is recorded for that pass, so a later drive re-drives the SAME pass.
    Parked {
        /// The fold node that parked.
        node: String,
        /// Why it parked.
        reason: ParkReason,
    },
}

/// Drives one fold node's bounded loop inline and sequentially into the parent
/// log.
///
/// Refuses an unsupported body form **before** recording the fold's
/// `NodeEntered`, so nothing lands in the log past such a refusal. Then records
/// `NodeEntered` and, for each pass in index order, `FoldIterationStarted`, the
/// body's work inline, and `FoldIterationJoined`. Pass 0's input is the fold's
/// routed value; every later pass's input is the previous pass's output, which
/// IS the accumulated value (the document has no vocabulary for a separate
/// accumulator or a merge rule). Both go through the same unwrap out of an MCP
/// result envelope (see [`unwrap_pass_output`]), so the body sees ONE shape
/// across the whole loop whether the fold was fed over an edge or by itself.
/// The loop stops
/// when `stop_when` holds over the
/// pass just joined, or when `max_iterations` passes have run. A bound reached
/// by a fold declaring `on_bound: fail` is a typed
/// [`EngineError::FoldBoundExceeded`] returned in place of the convergence;
/// otherwise the `join`
/// rule picks the winner, `FoldConverged` records it with the stop reason, and
/// `NodeExited` closes the node.
async fn drive_fold(
    ctx: &mut RunCtx,
    plan: &FoldPlan<'_>,
    routed: &Value,
    by_id: &HashMap<&str, &Node>,
    agents: &impl AgentResolver,
    tools: &impl ToolResolver,
    graph_hash: &str,
) -> Result<FoldOutcome, EngineError> {
    let fold = plan.node;
    let node_id = fold.id.as_str();

    // Resolve the body up front so an unsupported body form refuses BEFORE the
    // fold's NodeEntered is recorded.
    let body: &Node = match &fold.body {
        FoldBody::Node(target) => {
            let body_node =
                by_id
                    .get(target.as_str())
                    .copied()
                    .ok_or_else(|| EngineError::MalformedGraph {
                        detail: format!(
                            "fold node `{node_id}`: body names unknown node `{target}`"
                        ),
                    })?;
            match body_node {
                Node::Agent(_) | Node::Tool(_) => body_node,
                other => {
                    return Err(EngineError::UnsupportedFoldBody {
                        node: node_id.to_owned(),
                        detail: format!(
                            "a `{}` body node cannot be a per-pass worker; only `agent` and `tool` bodies run",
                            other.kind_name()
                        ),
                    });
                }
            }
        }
        FoldBody::Subgraph(_) => {
            return Err(EngineError::UnsupportedFoldBody {
                node: node_id.to_owned(),
                detail: "an embedded `subgraph` body is not executed yet".to_owned(),
            });
        }
    };

    // The validator requires a bound of at least 1; the engine re-checks it
    // defensively, before NodeEntered, because a zero bound would leave the
    // join with no pass to choose.
    if fold.max_iterations < 1 {
        return Err(EngineError::MalformedGraph {
            detail: format!(
                "fold node `{node_id}`: max_iterations must be at least 1, found {}",
                fold.max_iterations
            ),
        });
    }

    ctx.node_entered(node_id).await?;

    // Every pass's output, in pass order. The latest entry is the accumulated
    // value the next pass folds over and the predicate reads; the whole list is
    // what the join rule chooses from. Not pre-sized to the bound: the bound is
    // author-supplied and may be enormous, and each pass does real work, so
    // growing costs nothing next to reserving for passes that may never run.
    let mut passes: Vec<Value> = Vec::new();
    // THE ENTRY VALUE. The fold's routed value goes through the SAME unwrap its
    // pass outputs do. A fold reached over an inbound edge from a `tool` node is
    // handed that node's recorded output, which for a tool reached over MCP is
    // the whole result envelope; without this, pass 0 would fold the envelope
    // while every later pass folds a bare payload, so one body tool would see two
    // shapes in a single run and pass 0 would read every field at a path that is
    // not there. Derived once, here, because this is the single point pass 0's
    // input is chosen, and a fold that IS the entry node folds its graph input
    // exactly as before (a plain object is not an envelope and passes through
    // verbatim). Nothing outside a fold changes: an ordinary edge still routes a
    // node's recorded output verbatim, which is what branch expressions reading
    // `structuredContent.` depend on.
    let entry = unwrap_pass_output(routed.clone());
    let mut stopped_by_predicate = false;
    for position in 0..fold.max_iterations {
        let index = u64::from(position);
        ctx.fold_iteration_started(node_id, index).await?;
        let call = BodyCall {
            owner: BodyOwner::Fold,
            graph_hash,
            node_id,
            index,
        };
        // Pass 0 folds over the unwrapped entry value; every later pass folds
        // over the pass before it, which was unwrapped as it was pushed. Bound to
        // its own statement so the borrow of `passes` ends before the outcome is
        // pushed onto it.
        let outcome = {
            let input = passes.last().unwrap_or(&entry);
            run_body(ctx, body, input, agents, tools, call).await?
        };
        match outcome {
            // THE UNWRAP POINT for a pass output; the entry value above is the
            // only other one, and it calls the same function. The accumulated
            // value the fold carries is the pass output's `structuredContent`
            // payload when the output is an MCP result envelope, and the output
            // verbatim otherwise (see `unwrap_pass_output`). Applying it here,
            // where the output is pushed onto `passes`, is what makes every
            // consumer agree: the next pass's input, the `stop_when` predicate,
            // the `best_by` argmax, and the join's own output all read this
            // vector and therefore all read the bare payload. Nothing about the
            // RECORDING changes: `ToolCallCompleted` already holds the full
            // envelope and still does. This is derivation from recorded data,
            // pure and total, so a replay re-derives the identical value.
            IterationOutcome::Output(output) => passes.push(unwrap_pass_output(output)),
            // No join is recorded for a parked pass, so the next drive re-enters
            // this same pass and re-runs its body.
            IterationOutcome::Parked(reason) => {
                return Ok(FoldOutcome::Parked {
                    node: node_id.to_owned(),
                    reason,
                });
            }
        }
        ctx.fold_iteration_joined(node_id, index).await?;
        // The predicate reads the pass that just joined. It is a pure function
        // of recorded output, so replay re-decides identically.
        let joined = passes.last().expect("the pass that just joined");
        if plan.stop_when.eval(joined) {
            stopped_by_predicate = true;
            break;
        }
    }

    // THE BOUND VERDICT, before the join. A fold that declares `on_bound: fail`
    // treats `stop_when` as a REQUIREMENT, not an early exit, so reaching the
    // bound without it holding means the loop converged on nothing and there is
    // no value to join. The join is not consulted at all: a `best_by` argmax
    // over passes that all fell short would answer a question this fold did not
    // ask, so this check sits ahead of it and wins.
    //
    // THE RECORDING POINT, chosen deliberately. The passes and their joins are
    // already durably in the log and stay there: that work really happened, a
    // replay must reproduce it, and an operator reading the run needs to see
    // what the loop actually tried. What is refused is the CONVERGENCE. So this
    // returns from exactly the line `fold_converged` would have occupied: no
    // `FoldConverged`, no `NodeExited`, no terminal from the engine, which is
    // the same before-the-record discipline `FoldNoComparableCandidate` already
    // follows one arm below. An absent `on_bound`, or `OnBound::Join`, never
    // reaches this branch and behaves exactly as it did before the field
    // existed.
    if !stopped_by_predicate && fold.on_bound == Some(OnBound::Fail) {
        return Err(EngineError::FoldBoundExceeded {
            node: node_id.to_owned(),
            bound: fold.max_iterations,
        });
    }

    // The bound is at least 1 and every pass either pushed its output or
    // returned, so there is always a last pass here.
    let last = passes.len() - 1;
    let last_index = last as u64;
    let (winner_index, output) = match &fold.join {
        FoldJoin::Last => (last_index, passes[last].clone()),
        // Every pass contributes to an `all` join's output, so no single pass is
        // the winner; the recorded winner_index reads as the pass the loop
        // stopped at, which is what bounds the list.
        FoldJoin::All => (last_index, Value::Array(passes.clone())),
        FoldJoin::BestBy(reference) => {
            let parsed = plan
                .best_by
                .as_ref()
                .expect("a best_by join parses its reference at load");
            let index = best_by_index(&passes, parsed).ok_or_else(|| {
                EngineError::FoldNoComparableCandidate {
                    node: node_id.to_owned(),
                    reference: reference.clone(),
                }
            })?;
            (index as u64, passes[index].clone())
        }
    };

    ctx.fold_converged(
        node_id,
        winner_index,
        &stop_reason(fold, stopped_by_predicate, passes.len()),
    )
    .await?;
    ctx.node_exited(node_id).await?;
    Ok(FoldOutcome::Converged(output))
}

/// The value a fold actually folds, given a recorded one: the
/// `structuredContent` payload when the value is an MCP result envelope, and the
/// value verbatim otherwise. Applied to a pass's output and to the fold's own
/// entry value, so a fold folds bare payloads whether they entered over an edge
/// or were produced by a pass.
///
/// A fold pass whose body is a `tool` node produces the RECORDED TOOL RESULT.
/// For a tool reached over MCP that result is an envelope,
/// `{content, structuredContent?, isError?}`: the human-readable rendering
/// beside the machine-readable payload. The payload is the value the loop is
/// actually folding, so carrying the envelope forward would make every fold
/// expression reach through a transport detail (`structuredContent.score`
/// rather than `score`), and would make the same predicate wrong against a
/// native tool, which returns its flat struct with no envelope at all. Unwrap
/// once, here, and the document says the same thing whichever tool answers.
///
/// # What counts as an envelope
///
/// An object with BOTH a `content` array and a `structuredContent` key, which
/// is the recorded shape and the whole recorded shape. `salvor-tools` hands the
/// runtime `serde_json::to_value(&rmcp::model::CallToolResult)`, and that type
/// serializes `content: Vec<ContentBlock>` unconditionally (no
/// `skip_serializing_if`, so an empty result still writes `"content": []`)
/// while `structuredContent`, `isError`, and `_meta` are each written only when
/// present. So every MCP result carries a `content` ARRAY, and only a
/// structured one carries the payload key beside it.
///
/// Keying on the payload key ALONE would be wrong, and that is the point of the
/// pair: a bare tool or agent output is arbitrary author-shaped JSON, and an
/// object that happens to carry a field called `structuredContent` as data is a
/// legitimate accumulated value, not a transport wrapper. Such a value has no
/// `content` array beside it and passes through whole.
///
/// Everything else is returned untouched, which is what makes this safe for
/// every other body: an `agent` body with an `output_schema` produces a bare
/// object, a native tool produces a flat struct, a graph input is whatever the
/// operator submitted, and a non-object value (a string, a number, a list) has
/// no keys at all.
///
/// This is a pure, total function of a recorded value, which is what lets it
/// sit outside the recording entirely: `ToolCallCompleted` still holds the full
/// envelope, and this is re-derived from it identically on every replay. The
/// document validator's fold-reference check reads the same bare payload, so
/// what it checks at submit is what the engine folds at run time.
fn unwrap_pass_output(output: Value) -> Value {
    match output {
        // The guard is the envelope test; the `remove` below cannot then miss.
        Value::Object(mut fields)
            if fields.get("content").is_some_and(Value::is_array)
                && fields.contains_key("structuredContent") =>
        {
            fields
                .remove("structuredContent")
                .expect("the guard proved the key is present")
        }
        other => other,
    }
}

/// The index of the pass a `best_by` join wins with: the argmax over every
/// pass of the value `reference` names, ordered by the expression language's OWN
/// comparison ([`salvor_graph::expr::compare`]), so an argmax and the
/// `stop_when` predicate beside it can never order values differently.
///
/// A pass whose reference is missing, or names a value that does not order
/// (anything but a number or a string), cannot win: `compare` answers both
/// questions, so no type list is re-derived here. Ties keep the EARLIEST pass,
/// because only a strictly greater candidate displaces the incumbent. `None`
/// when no pass has a comparable value at all, which the caller refuses with
/// before recording a convergence.
fn best_by_index(passes: &[Value], reference: &Reference) -> Option<usize> {
    let mut best: Option<(usize, &Value)> = None;
    for (index, pass) in passes.iter().enumerate() {
        let Some(candidate) = reference
            .resolve(pass)
            .filter(|value| salvor_graph::expr::compare(value, value).is_some())
        else {
            continue;
        };
        let wins = match best {
            None => true,
            Some((_, incumbent)) => {
                salvor_graph::expr::compare(candidate, incumbent) == Some(Ordering::Greater)
            }
        };
        if wins {
            best = Some((index, candidate));
        }
    }
    best.map(|(index, _)| index)
}

/// The human-readable reason recorded on `FoldConverged`: which of the two stop
/// causes ended the loop, naming the predicate that fired or the bound that was
/// reached. A pure function of the document and the pass count, so it reproduces
/// byte for byte on replay (the cursor matches the recorded reason). There is no
/// third cause: no "failed to improve" rule stops a fold.
///
/// # Why both read verdict first, expression last
///
/// This string is rendered after a display prefix that is not this function's to
/// change (`fold <node> converged on [<i>]: `), and readers truncate. So the
/// VERDICT leads and the author's `stop_when` expression trails: truncation can
/// then only ever eat the expression, never the word that says which way the
/// loop went. The bound reason in particular has to survive standing beside the
/// word "converged", so it says plainly that the join was taken at the bound and
/// that the predicate never held, rather than opening with the bound and hiding
/// the negation past the cut.
///
/// The `on_bound: fail` case never reaches here at all: it returns
/// [`EngineError::FoldBoundExceeded`] from where `FoldConverged` would have been
/// recorded, and that error text is verdict-first already.
fn stop_reason(fold: &FoldNode, stopped_by_predicate: bool, passes: usize) -> String {
    if stopped_by_predicate {
        format!(
            "stop_when held after pass {}: `{}`",
            passes - 1,
            fold.stop_when
        )
    } else {
        format!(
            "joined at the max_iterations bound of {}; stop_when never held: `{}`",
            fold.max_iterations, fold.stop_when
        )
    }
}

/// The derived id of the child run executing one map iteration: `sha256:` over the
/// parent run id, the map node id, and the zero-based index.
///
/// A pure function of recorded data (the parent run id the `RunCtx` carries, the
/// document's node id, and the index), so a replay of the parent reconstructs the
/// identical id without storing anything extra, and it is stable across processes
/// and languages. Reuses `salvor-runtime`'s canonical hashing, the same story
/// behind [`graph_hash`] and [`fork_safe_idempotency_key`]. Iterations run
/// inline in the parent log today, so no separate log exists under this id
/// yet; it is recorded on `MapIterationStarted` as the durable, forward-compatible
/// identity a future concurrent-child-run would key each iteration's own log
/// on.
fn map_child_run_id(parent_run: RunId, node_id: &str, index: u64) -> String {
    hash_value(&json!({
        "parent_run": parent_run,
        "node": node_id,
        "index": index,
    }))
}

/// Resolves a map node's `over` reference against the routed value to the list of
/// items to fan out over.
///
/// The reference uses the same path grammar and missing-path semantics the branch
/// expressions use (see [`salvor_graph::expr::parse_reference`]). A reference that
/// fails to parse is a [`EngineError::MalformedGraph`] that does not arise for a
/// validated document; one that resolves to anything but a JSON array, including
/// a missing path, is a typed [`EngineError::MapOverNotAList`].
fn resolve_over(node_id: &str, over: &str, routed: &Value) -> Result<Vec<Value>, EngineError> {
    let reference =
        salvor_graph::expr::parse_reference(over).map_err(|error| EngineError::MalformedGraph {
            detail: format!(
                "map node `{node_id}`: `over` reference `{over}` is unparseable: {error}"
            ),
        })?;
    match reference.resolve(routed) {
        Some(Value::Array(items)) => Ok(items.clone()),
        _ => Err(EngineError::MapOverNotAList {
            node: node_id.to_owned(),
            over: over.to_owned(),
        }),
    }
}

/// The idempotency key a graph `tool` node's [`Effect::Idempotent`] call
/// presents: a pure function of the call's POSITION in the graph (the graph
/// hash, the node id, and the call index within the node), never of drawn
/// randomness.
///
/// This is what makes an idempotent tool fork-safe. A fork re-walks the segment
/// from its fork node, re-executing the idempotent calls in it live; deriving
/// the key from position means each re-executed call presents the IDENTICAL key
/// its origin recorded, so the provider collapses the duplicate. Drawing the key
/// from [`RunCtx::random`](salvor_runtime::RunCtx::random) instead would mint a
/// fresh key in the fork, and the provider would see a second, distinct call.
/// With this derivation, [`Effect::Write`] is the only effect class a fork must
/// have acknowledged, because a [`Effect::Read`] re-executes freely and a
/// [`Effect::Idempotent`] retry collapses.
///
/// `call_index` is the zero-based position of the call within the node. A `tool`
/// node makes exactly one call, so it is always `0` today; the parameter is
/// carried so a future node kind issuing several calls keeps their keys distinct.
///
/// Reuses `salvor-runtime`'s canonical hashing (the same behind `graph_hash`
/// itself), so the key is reproducible and stable across processes and
/// languages. Existing recorded logs are not disturbed: a key is plain data in
/// the log, and replay correlates on the RECORDED request, never on a
/// re-derivation, so a log whose idempotent call recorded a random-drawn key
/// still replays byte for byte under this build.
fn fork_safe_idempotency_key(graph_hash: &str, node_id: &str, call_index: u64) -> String {
    hash_value(&serde_json::json!({
        "graph_hash": graph_hash,
        "node": node_id,
        "call": call_index,
    }))
}

/// The reason recorded for every [`salvor_core::Event::NodeSkipped`]: a constant,
/// so it is trivially a pure function of the run and reproduces byte for byte on
/// replay (the cursor matches the recorded reason).
const SKIP_REASON: &str = "no live inbound edge: an upstream branch routed to another case";

/// Every branch node's cases, parsed once at load: the branch node id maps to
/// its cases as `(case name, optional parsed expression)` pairs, where the
/// expression is `None` for a `model_decision` case. All ids and names borrow
/// the graph document.
type ParsedBranches<'a> = HashMap<&'a str, Vec<(&'a str, Option<Expr>)>>;

/// Parses every branch node's case conditions once, up front. An `expression`
/// case parses to an [`Expr`]; a `model_decision` case has no expression, so it
/// stores `None`. The validator already guarantees each expression parses, so a
/// failure here is a [`EngineError::MalformedGraph`] that does not arise for a
/// validated document.
fn parse_branches(graph: &Graph) -> Result<ParsedBranches<'_>, EngineError> {
    let mut parsed = HashMap::new();
    for node in &graph.nodes {
        let Node::Branch(branch) = node else {
            continue;
        };
        let mut cases = Vec::with_capacity(branch.cases.len());
        for case in &branch.cases {
            let expr = match &case.when {
                BranchCondition::Expression(source) => {
                    Some(salvor_graph::expr::parse(source).map_err(|error| {
                        EngineError::MalformedGraph {
                            detail: format!(
                                "branch node `{}`: case `{}` has an unparseable condition: {error}",
                                branch.id, case.name
                            ),
                        }
                    })?)
                }
                BranchCondition::ModelDecision => None,
            };
            cases.push((case.name.as_str(), expr));
        }
        parsed.insert(branch.id.as_str(), cases);
    }
    Ok(parsed)
}

/// One fold node's parsed plan: the node itself plus its expression fields,
/// parsed once at load rather than once per pass.
struct FoldPlan<'a> {
    /// The document's fold node, borrowed for the drive.
    node: &'a FoldNode,
    /// The `stop_when` predicate, evaluated against each pass's output.
    stop_when: Expr,
    /// The `best_by` join's reference, `None` for the `last` and `all` joins,
    /// which carry no reference.
    best_by: Option<Reference>,
}

/// Every fold node's plan, keyed by node id (both borrow the graph document).
type ParsedFolds<'a> = HashMap<&'a str, FoldPlan<'a>>;

/// Parses every fold node's expression fields once, up front, exactly as
/// [`parse_branches`] does for the branch conditions: the `stop_when` predicate
/// and, for a `best_by` join, its reference. The validator already guarantees
/// both parse, so a failure here is a [`EngineError::MalformedGraph`] that does
/// not arise for a validated document.
fn parse_folds(graph: &Graph) -> Result<ParsedFolds<'_>, EngineError> {
    let mut parsed = HashMap::new();
    for node in &graph.nodes {
        let Node::Fold(fold) = node else {
            continue;
        };
        let stop_when = salvor_graph::expr::parse(&fold.stop_when).map_err(|error| {
            EngineError::MalformedGraph {
                detail: format!(
                    "fold node `{}`: `stop_when` is unparseable: {error}",
                    fold.id
                ),
            }
        })?;
        let best_by = match &fold.join {
            FoldJoin::BestBy(reference) => Some(
                salvor_graph::expr::parse_reference(reference).map_err(|error| {
                    EngineError::MalformedGraph {
                        detail: format!(
                            "fold node `{}`: the `best_by` reference `{reference}` is unparseable: {error}",
                            fold.id
                        ),
                    }
                })?,
            ),
            FoldJoin::Last | FoldJoin::All => None,
        };
        parsed.insert(
            fold.id.as_str(),
            FoldPlan {
                node: fold,
                stop_when,
                best_by,
            },
        );
    }
    Ok(parsed)
}

/// The input a node receives: the recorded output of its live inbound edge, or
/// the graph input for an entry node (no inbound edge). Returns `None` when no
/// inbound edge is live, which means the node was routed past and must be
/// skipped.
///
/// An inbound edge is live when its source ran (was not skipped) and, if the
/// source is a branch, the edge realizes the case that fired. Among several live
/// inbound edges the smallest source id wins, so a merge is a pure function of
/// the document.
fn select_input(
    id: &str,
    inbound: &HashMap<&str, Vec<&Edge>>,
    by_id: &HashMap<&str, &Node>,
    branch_case: &HashMap<&str, String>,
    skipped: &HashSet<&str>,
    outputs: &HashMap<&str, Value>,
    graph_input: &Value,
) -> Option<Value> {
    let edges = inbound.get(id).map(Vec::as_slice).unwrap_or_default();
    if edges.is_empty() {
        return Some(graph_input.clone());
    }
    let mut chosen: Option<&Edge> = None;
    for edge in edges {
        if !is_live_inbound(edge, by_id, branch_case, skipped) {
            continue;
        }
        chosen = match chosen {
            Some(best) if best.from <= edge.from => Some(best),
            _ => Some(edge),
        };
    }
    chosen.map(|edge| {
        outputs
            .get(edge.from.as_str())
            .cloned()
            .unwrap_or(Value::Null)
    })
}

/// Whether an inbound edge carries a live value into its destination: the source
/// ran, and if the source is a branch the edge's label names the fired case.
fn is_live_inbound(
    edge: &Edge,
    by_id: &HashMap<&str, &Node>,
    branch_case: &HashMap<&str, String>,
    skipped: &HashSet<&str>,
) -> bool {
    if skipped.contains(edge.from.as_str()) {
        return false;
    }
    match by_id.get(edge.from.as_str()) {
        // A branch only lets the edge realizing its fired case through.
        Some(Node::Branch(_)) => {
            branch_case.get(edge.from.as_str()).map(String::as_str) == edge.label.as_deref()
        }
        // Every non-branch source feeds all of its outbound edges.
        _ => true,
    }
}

/// The human-readable suspension reason a gate parks under: its prompt when it
/// has one, else a phrase derived from the node id. A pure function of the
/// document, so it reproduces on replay.
fn gate_reason(gate: &GateNode) -> String {
    gate.prompt
        .clone()
        .unwrap_or_else(|| format!("approval required at gate `{}`", gate.id))
}

/// Picks the first expression case whose condition is true, in author order.
/// Returns [`EngineError::NoBranchCaseMatched`] when none fires. A
/// `model_decision` case reaching here means an expression branch (no
/// `agent_hash`) carried one, which the validator rejects, so it is a
/// [`EngineError::MalformedGraph`] unreachable for a validated document.
fn choose_expression_case<'a>(
    node_id: &str,
    cases: &'a [(&'a str, Option<Expr>)],
    value: &Value,
) -> Result<&'a str, EngineError> {
    for (name, expr) in cases {
        match expr {
            Some(expr) if expr.eval(value) => return Ok(name),
            Some(_) => {}
            None => {
                return Err(EngineError::MalformedGraph {
                    detail: format!(
                        "branch node `{node_id}`: an expression branch must not carry a model-decision case"
                    ),
                });
            }
        }
    }
    Err(EngineError::NoBranchCaseMatched {
        node: node_id.to_owned(),
    })
}

/// Maps a decision agent's reply to a case name: the reply's final text,
/// trimmed, must exactly equal one of the branch's case names. Anything else is
/// [`EngineError::BranchDecisionUnmatched`], listing the case names.
fn match_decision<'a>(branch: &'a BranchNode, reply: &Value) -> Result<&'a str, EngineError> {
    let reply_text = reply
        .as_str()
        .map_or_else(|| reply.to_string(), |text| text.trim().to_owned());
    for case in &branch.cases {
        if case.name == reply_text {
            return Ok(case.name.as_str());
        }
    }
    Err(EngineError::BranchDecisionUnmatched {
        node: branch.id.clone(),
        reply: reply_text,
        cases: branch.cases.iter().map(|case| case.name.clone()).collect(),
    })
}

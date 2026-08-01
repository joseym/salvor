//! THE GRAPH RELEASE GATE: kill a reference GRAPH run at every possible event
//! boundary, resume it through the engine, and assert the continued run reaches
//! a byte-identical final log with zero duplicate `Write` executions.
//!
//! This is the graph sibling of `salvor-runtime/tests/release_gate.rs`, which
//! makes the identical claim for an agent run. It lives in `salvor-engine`
//! rather than beside that suite because the claim is about [`run_graph`], and
//! `salvor-runtime` cannot depend on `salvor-engine`: the dependency runs the
//! other way, so the runtime suite has no graph document to drive. The sweep's
//! shape, the re-execution table it asserts, and the whole-scenario write
//! accounting that closes it are lifted from that suite on purpose, so both
//! gates make the same claim in the same terms.
//!
//! # What the engine claims, and what this machine-checks
//!
//! The engine's crate docs claim that everything it feeds forward is a pure
//! function of the document or of recorded values: the walk order, each node's
//! input, the branch route, a map's resolved item list, its per-iteration child
//! ids, and an idempotent tool's key. That claim is what makes "a second drive
//! over the recorded log replays with no live calls and produces a
//! byte-identical log" true. Prose cannot check it. This sweep does, at every
//! prefix of every shape in the corpus.
//!
//! # The corpus
//!
//! Four shapes, each swept independently so each carries its own boundary
//! count rather than hiding behind an aggregate:
//!
//! - `gate`: `research` (agent) -> `approve` (gate) -> `publish` (Write tool).
//!   The gate is the most interesting boundary because it parks and resumes
//!   through the same `Suspended` / `Resumed` machinery a tool suspension uses,
//!   so a cut can land before the park, on it, on the resume, or after it.
//! - `branch`: `assess` (Read tool) -> `route` (expression branch) -> `publish`
//!   (Write tool) on the taken case, `reject` skipped on the other. The route
//!   must be a pure function of the recorded `assess` output, and the skip must
//!   reproduce; `reject`'s tool is registered with a counter, so a route that
//!   ever went the other way would show up as an execution rather than as an
//!   unresolved-tool error.
//! - `map`: `fanout` (map over three items, body a Write tool) -> `record`
//!   (Idempotent tool). The fan-out has the most events and the most chances to
//!   double-execute, and its body being a Write is what puts three
//!   reconciliation refusals inside one fan-out.
//! - `flagship`: all three at once, plus an agent node that crosses a
//!   `max_steps` budget mid-node and resumes with a recorded extension, plus an
//!   Idempotent tool whose key is derived from its position in the graph. The
//!   gate's recorded approval carries the list the map then fans out over, so
//!   the item list is a pure function of a recorded resume input rather than of
//!   the graph document alone.
//!
//! # `fold` is deliberately absent
//!
//! The engine refuses a `fold` node with `EngineError::UnsupportedNode` before
//! recording its `NodeEntered`, so a fold graph has no run to sweep: there is
//! no reference log, and every boundary would be the same one-event log. Its
//! absence from the corpus is therefore a property of the engine, not an
//! omission, and it is machine-checked rather than asserted in prose: see
//! [`a_fold_node_has_no_run_to_sweep`] below, and `fold_graph.rs` for the
//! refusal's own coverage.
//!
//! # What `map` running inline bounds the claim to
//!
//! A map's iterations run inline and sequentially in the parent's own log
//! today: the `concurrency` cap is accepted and not honored. So this gate
//! proves exactly what a single-log replay can prove, which for the shipped
//! engine is everything: iterations are recorded in index order, each body call
//! executes exactly once across a kill and its resume, and the fan-out replays
//! byte for byte. What it does not prove, because the engine does not do it, is
//! anything about concurrent child runs: no interleaving exists to be killed
//! between, and no per-iteration log exists to be truncated independently. When
//! iterations do get their own runs, this suite's claim will need extending to
//! cuts inside a child log; today there is no such cut.
//!
//! # How a kill is modeled, and why the document is re-supplied
//!
//! For each boundary, the control log's prefix is copied into a fresh
//! file-backed `SqliteStore` and the graph is driven again over it (same
//! injected constant clock and random source, same run id). That is exactly
//! what a `kill -9` leaves behind: some prefix of the log durable, everything
//! else gone with the process.
//!
//! A graph run's log records only the document's *hash*, on `GraphRunStarted`;
//! it never records the document. So every continuation must re-supply the same
//! `Graph` value, and `RunCtx::begin_graph` checks the re-supplied hash against
//! the recorded head and refuses a mismatch as a divergence at position 1. The
//! harness re-supplies `shape.graph` on every drive for exactly that reason.
//! The `input` argument is re-supplied alongside it but is *not* authoritative:
//! `begin_graph` returns the RECORDED input whenever history covers the head.
//! [`resuming_a_graph_run_must_re_supply_the_matching_document`] pins both
//! halves of that.
//!
//! There is no `Runtime` verb for a graph run, so the harness dispatches on
//! derived state itself, doing what `Runtime::start`/`resume`/`recover` do for
//! an agent run: a parked run resumes with the recorded input, validated
//! against the recorded suspension schema (or the extension shape) *before*
//! anything is set, exactly as `Runtime::resume` validates; a crashed run is
//! simply re-driven; and a dangling write intent refuses.
//!
//! # The expected re-execution table
//!
//! Identical to the agent gate's, because it is the same `RunCtx` machinery
//! underneath. For a cut at prefix length `c`, with a tool intent recorded at
//! sequence `i` (its completion at `i + 1`):
//!
//! - `c <= i`: neither intent nor completion survived; the call runs live
//!   exactly once during the continuation.
//! - `c == i + 1`, effect Read or Idempotent: the intent survived without its
//!   completion, so the call re-executes on recovery. That is the specified
//!   effect-table behavior, and the suite asserts the extra execution
//!   explicitly rather than ignoring it.
//! - `c == i + 1`, effect Write: the continuation REFUSES with the
//!   reconciliation error and executes nothing at all. One boundary per write
//!   call lands here, including the ones inside a map fan-out.
//! - `c >= i + 2`: the completed call replays from the log and never
//!   re-executes.
//!
//! Whole-scenario write accounting closes the loop per shape: write executions
//! represented by the prefix (one per recorded write completion) plus live
//! executions during the continuation must equal the control run's total
//! exactly.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use common::{
    ConstTool, EchoTool, ScriptedModel, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response, tool_use_response,
};
use salvor_core::{Effect, Event, EventEnvelope, ReplayError, RunId, RunStatus, derive_state};
use salvor_engine::{EngineError, GraphOutcome, run_graph};
use salvor_graph::{
    AgentSpec, BranchCondition, BranchSpec, FoldBody, FoldJoin, FoldSpec, GateSpec, Graph,
    GraphBuilder, MapBody, MapSpec, ToolSpec,
};
use salvor_runtime::{
    Agent, Budgets, RunCtx, RuntimeError, validate_against_schema, validate_extension_input,
};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::{Value, json};
use wiremock::MockServer;

/// The agent hash the corpus registers its one agent under. A graph run records
/// no agent definition hash of its own, so this is only a registry key.
const RESEARCH_HASH: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";

/// The operator's recorded budget extension, the same shape the agent gate uses.
fn extension_input() -> Value {
    json!({"extend": {"steps": 100}})
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// A comparable per-tool execution count, keyed by tool name, for the oracle
/// assertions. Every registered tool is present, so a tool that should not have
/// run is asserted at zero rather than merely unmentioned.
type ExecutionCounts = BTreeMap<String, usize>;

/// One boundary's fresh executables: the node registries plus the execution
/// counters their tools share. A boundary always builds its own set, so the
/// executions it performs are counted in isolation from every other boundary's.
struct Executors {
    agents: HashMap<String, Agent>,
    tools: HashMap<String, Box<dyn DynTool>>,
    counters: BTreeMap<String, Arc<AtomicUsize>>,
}

impl Executors {
    /// Assembles a set from the tools it should register, deriving the counter
    /// map from the same list so the two can never drift apart.
    fn new(
        agents: HashMap<String, Agent>,
        tools: Vec<(Box<dyn DynTool>, Arc<AtomicUsize>)>,
    ) -> Self {
        let mut registry: HashMap<String, Box<dyn DynTool>> = HashMap::new();
        let mut counters: BTreeMap<String, Arc<AtomicUsize>> = BTreeMap::new();
        for (tool, calls) in tools {
            counters.insert(tool.name().to_owned(), calls);
            registry.insert(tool.name().to_owned(), tool);
        }
        Self {
            agents,
            tools: registry,
            counters,
        }
    }

    fn snapshot(&self) -> ExecutionCounts {
        self.counters
            .iter()
            .map(|(name, calls)| (name.clone(), calls.load(Ordering::SeqCst)))
            .collect()
    }

    /// The all-zero counts, the base the oracle adds expected executions onto.
    fn zeroed(&self) -> ExecutionCounts {
        self.counters.keys().map(|name| (name.clone(), 0)).collect()
    }

    fn total(&self) -> usize {
        self.counters
            .values()
            .map(|calls| calls.load(Ordering::SeqCst))
            .sum()
    }
}

/// Builds a fresh set of executables. Boxed rather than a concrete type because
/// each shape captures different mock-server URIs and a different tool mix.
type BuildExecutors = Arc<dyn Fn() -> Executors + Send + Sync>;

/// The recorded input a parked run is resumed with, chosen from the derived
/// park state so the choice is a pure function of the log.
type ResumeInput = Arc<dyn Fn(&RunStatus) -> Value + Send + Sync>;

/// One shape of the corpus: a graph document, its input, and everything needed
/// to drive it repeatedly and identically.
struct Shape {
    /// Names the shape in every assertion message and in the sweep report.
    name: &'static str,
    graph: Graph,
    input: Value,
    run_id: RunId,
    /// The event-kind sequence the control run must record, pinned so a change
    /// in the engine's recorded shape is a visible diff here rather than a
    /// silent change in what the gate covers.
    expected_kinds: Vec<&'static str>,
    /// Write executions the uninterrupted control run performs.
    control_write_executions: usize,
    build: BuildExecutors,
    resume: ResumeInput,
}

/// Drives the graph once over `log`, optionally supplying a resume input.
///
/// The document is re-supplied on every drive because the log holds only its
/// hash; `begin_graph` checks that hash and hands back the RECORDED input, so
/// `shape.input` matters only when the log is empty.
async fn drive_once(
    shape: &Shape,
    executors: &Executors,
    store: &Arc<dyn EventStore>,
    log: Vec<EventEnvelope>,
    resume_input: Option<Value>,
) -> Result<GraphOutcome, EngineError> {
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        shape.run_id,
        log,
        fixed_clock(),
        fixed_random(),
    )?;
    if let Some(input) = resume_input {
        ctx.set_resume_input(input);
    }
    run_graph(
        &mut ctx,
        &shape.graph,
        &shape.input,
        &executors.agents,
        &executors.tools,
    )
    .await
}

/// Drives a graph run to completion the way an operator would: read the log,
/// derive state, dispatch. Parked states resume with the shape's recorded
/// input; crashed states are re-driven.
///
/// When `control` is given (a boundary continuation), the resume input is
/// cross-checked against the `Resumed` event recorded at the parked position in
/// the control log, proving continuations feed the run exactly what the control
/// run recorded.
async fn drive_to_completion(
    shape: &Shape,
    executors: &Executors,
    store: &Arc<dyn EventStore>,
    control: Option<&[EventEnvelope]>,
    label: &str,
) {
    // A completed drive needs at most one action per park plus one per leg; the
    // cap only bounds a runaway bug.
    for _ in 0..24 {
        let log = store.read_log(shape.run_id).await.expect("log reads");
        let state = derive_state(&log);
        let resume_input = match &state.status {
            RunStatus::Completed { .. } => return,
            RunStatus::Failed { error } => panic!("{label}: the run failed: {error}"),
            RunStatus::NeedsReconciliation => {
                panic!("{label}: reconciliation must be handled before dispatch")
            }
            RunStatus::Suspended { input_schema, .. } => {
                let input = (shape.resume)(&state.status);
                // The same pre-flight `Runtime::resume` performs for an agent
                // run: the input is checked against the recorded suspension
                // schema before anything is set or driven.
                validate_against_schema(&input, input_schema).unwrap_or_else(|error| {
                    panic!("{label}: the resume input is rejected: {error}")
                });
                if let Some(control) = control {
                    assert_recorded_resume(control, log.len(), &input, label);
                }
                Some(input)
            }
            RunStatus::BudgetExceeded { .. } => {
                let input = (shape.resume)(&state.status);
                validate_extension_input(&input).unwrap_or_else(|error| {
                    panic!("{label}: the extension input is rejected: {error}")
                });
                if let Some(control) = control {
                    assert_recorded_resume(control, log.len(), &input, label);
                }
                Some(input)
            }
            // NotStarted (a fresh run), or Running / AwaitingModel /
            // AwaitingTool: crashed mid-step, so re-drive and continue live
            // from the first unrecorded step.
            _ => None,
        };
        drive_once(shape, executors, store, log, resume_input)
            .await
            .unwrap_or_else(|error| panic!("{label}: the drive failed: {error}"));
    }
    panic!("{label}: the run did not complete within the action cap");
}

/// Asserts the control log recorded a `Resumed` event carrying exactly `input`
/// at `position`, the seam a continuation's resume writes into.
fn assert_recorded_resume(control: &[EventEnvelope], position: usize, input: &Value, label: &str) {
    match &control[position].event {
        Event::Resumed { input: recorded } => assert_eq!(
            recorded, input,
            "{label}: the resume input must be the recorded one (control position {position})"
        ),
        other => {
            panic!("{label}: expected Resumed at control position {position}, found {other:?}")
        }
    }
}

/// True when the prefix ends at a write intent with no completion, the one
/// shape a continuation must refuse.
fn ends_at_dangling_write_intent(prefix: &[EventEnvelope]) -> bool {
    matches!(
        prefix.last().map(|envelope| &envelope.event),
        Some(Event::ToolCallRequested {
            effect: Effect::Write,
            ..
        })
    )
}

/// The oracle: expected live executions per tool for a continuation from prefix
/// length `cut`, per the re-execution table in the module docs.
fn expected_executions(
    control: &[EventEnvelope],
    cut: usize,
    zeroed: &ExecutionCounts,
) -> ExecutionCounts {
    let mut counts = zeroed.clone();
    for (index, envelope) in control.iter().enumerate() {
        let Event::ToolCallRequested { tool, effect, .. } = &envelope.event else {
            continue;
        };
        let runs = if index >= cut {
            // Neither intent nor completion survived the cut: one live run.
            1
        } else if index + 1 == cut && !matches!(effect, Effect::Write) {
            // Dangling Read/Idempotent intent: re-executes on recovery.
            1
        } else {
            0
        };
        let slot = counts
            .get_mut(tool.as_str())
            .unwrap_or_else(|| panic!("unregistered tool in the log: {tool}"));
        *slot += runs;
    }
    counts
}

/// Write executions already represented by a prefix: one per recorded write
/// completion (the intent immediately precedes its completion in control).
fn prefix_write_executions(control: &[EventEnvelope], cut: usize) -> usize {
    control[..cut]
        .iter()
        .enumerate()
        .filter(|(index, envelope)| {
            matches!(envelope.event, Event::ToolCallCompleted { .. })
                && matches!(
                    control.get(index.wrapping_sub(1)).map(|e| &e.event),
                    Some(Event::ToolCallRequested {
                        effect: Effect::Write,
                        ..
                    })
                )
        })
        .count()
}

/// Live write executions a continuation performed, summed over the tools whose
/// intents the control log records with `Effect::Write`.
fn live_write_executions(control: &[EventEnvelope], executors: &Executors) -> usize {
    let counts = executors.snapshot();
    let mut write_tools: BTreeMap<&str, ()> = BTreeMap::new();
    for envelope in control {
        if let Event::ToolCallRequested {
            tool,
            effect: Effect::Write,
            ..
        } = &envelope.event
        {
            write_tools.insert(tool.as_str(), ());
        }
    }
    write_tools
        .keys()
        .map(|tool| counts.get(*tool).copied().unwrap_or_default())
        .sum()
}

/// A directory of per-boundary SQLite files under the OS temp dir, removed on
/// drop. (std-only so the crate needs no extra dev-dependency.)
struct SweepDir(PathBuf);

impl SweepDir {
    /// A directory of its own per caller. Tests in one binary run concurrently
    /// and this wipes the directory on the way in, so two callers sharing a path
    /// would delete each other's stores; `owner` keeps them apart.
    fn create(owner: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("salvor-graph-gate-{owner}-{}", std::process::id()));
        // A leftover from a crashed earlier run of this same pid slot would
        // poison the sweep; start clean.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("sweep dir creates");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SweepDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs one shape's uninterrupted control run on a file-backed store and
/// returns its full log: the oracle every continuation must reproduce exactly.
async fn record_control(shape: &Shape, dir: &Path) -> Vec<EventEnvelope> {
    let store: Arc<dyn EventStore> = Arc::new(
        SqliteStore::open(dir.join(format!("{}-control.db", shape.name)))
            .expect("control store opens"),
    );
    let executors = (shape.build)();
    drive_to_completion(shape, &executors, &store, None, "control").await;

    let log = store
        .read_log(shape.run_id)
        .await
        .expect("control log reads");
    assert_eq!(
        event_kinds(&log),
        shape.expected_kinds,
        "{}: the control run's recorded shape is pinned",
        shape.name
    );
    // The control run is the cut-0 case of the re-execution table: every call
    // in the log ran live exactly once.
    assert_eq!(
        executors.snapshot(),
        expected_executions(&log, 0, &executors.zeroed()),
        "{}: the control run executes every recorded call exactly once",
        shape.name
    );
    assert_eq!(
        live_write_executions(&log, &executors),
        shape.control_write_executions,
        "{}: the control run's write total is the declared one",
        shape.name
    );
    log
}

/// One boundary of one shape's sweep: copy the prefix into a fresh file-backed
/// store, continue, and assert the outcome. Returns whether this boundary was a
/// (correct) reconciliation refusal.
async fn continue_from_boundary(
    cut: usize,
    shape: Arc<Shape>,
    control: Arc<Vec<EventEnvelope>>,
    db_path: PathBuf,
) -> bool {
    let label = format!("{} cut {cut}", shape.name);
    let store: Arc<dyn EventStore> =
        Arc::new(SqliteStore::open(&db_path).expect("boundary store opens"));
    for envelope in &control[..cut] {
        store.append(envelope).await.expect("prefix event appends");
    }
    let executors = (shape.build)();

    // The refusal boundaries: a dangling write intent. The write may or may not
    // have landed before the kill; only a human may decide, so the continuation
    // must refuse with the reconciliation error and execute nothing.
    if ends_at_dangling_write_intent(&control[..cut]) {
        assert_eq!(
            derive_state(&control[..cut]).status,
            RunStatus::NeedsReconciliation,
            "{label}: a dangling write intent derives to reconciliation"
        );
        let prefix = control[..cut].to_vec();
        let error = drive_once(&shape, &executors, &store, prefix, None)
            .await
            .expect_err("a continuation over a dangling write intent must refuse");
        assert!(
            matches!(
                error,
                EngineError::Runtime(RuntimeError::Replay(
                    ReplayError::NeedsReconciliation { .. }
                ))
            ),
            "{label}: expected the reconciliation error, got {error:?}"
        );
        assert_eq!(
            executors.total(),
            0,
            "{label}: a refused continuation executes nothing"
        );
        assert_eq!(
            store.read_log(shape.run_id).await.expect("log reads").len(),
            cut,
            "{label}: a refused continuation appends nothing"
        );
        return true;
    }

    if cut == control.len() {
        // The full log: a re-drive is a pure end-to-end replay. It must reach
        // the completed outcome, execute nothing, and append nothing.
        let outcome = drive_once(&shape, &executors, &store, control[..].to_vec(), None)
            .await
            .expect("full-log replay is divergence free");
        assert!(
            matches!(outcome, GraphOutcome::Completed { .. }),
            "{label}: a full-log replay completes, got {outcome:?}"
        );
    } else {
        drive_to_completion(&shape, &executors, &store, Some(&control), &label).await;
    }

    let log = store.read_log(shape.run_id).await.expect("final log reads");
    assert_eq!(
        serde_json::to_string(&log).unwrap(),
        serde_json::to_string(&*control).unwrap(),
        "{label}: the continued log must be byte-identical to the control log"
    );

    let expected = expected_executions(&control, cut, &executors.zeroed());
    assert_eq!(
        executors.snapshot(),
        expected,
        "{label}: live executions must match the re-execution table"
    );

    // Whole-scenario write accounting: executions represented by the prefix
    // plus live executions equal the control total. Zero duplicates.
    assert_eq!(
        prefix_write_executions(&control, cut) + live_write_executions(&control, &executors),
        shape.control_write_executions,
        "{label}: write executions across the whole scenario must total the control count"
    );
    false
}

/// What one shape's sweep proved: how many boundaries it killed at, and how
/// many of them were reconciliation refusals.
struct SweepReport {
    name: &'static str,
    events: usize,
    boundaries: usize,
    refusals: usize,
}

/// Sweeps one shape: record its control run, then continue from every prefix
/// boundary `0..=len` on its own store file. Boundaries run concurrently; each
/// has its own store and its own execution counters.
async fn sweep(shape: Shape, dir: &Path) -> SweepReport {
    let name = shape.name;
    let control = Arc::new(record_control(&shape, dir).await);
    let shape = Arc::new(shape);

    let mut boundaries = tokio::task::JoinSet::new();
    for cut in 0..=control.len() {
        let shape = shape.clone();
        let control = control.clone();
        let db_path = dir.join(format!("{name}-boundary-{cut:03}.db"));
        boundaries.spawn(async move { continue_from_boundary(cut, shape, control, db_path).await });
    }

    let mut refusals = 0;
    let mut completed = 0;
    while let Some(result) = boundaries.join_next().await {
        match result {
            Ok(was_refusal) => {
                refusals += usize::from(was_refusal);
                completed += 1;
            }
            // Surface the boundary's own panic message instead of a bare
            // JoinError, so a failure names its shape and its cut.
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }
    assert_eq!(
        completed,
        control.len() + 1,
        "{name}: every boundary was swept"
    );
    assert_eq!(
        refusals, shape.control_write_executions,
        "{name}: exactly the write-intent prefixes refuse; every other boundary completes"
    );
    SweepReport {
        name,
        events: control.len(),
        boundaries: completed,
        refusals,
    }
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// The gate's approval schema: an object that must carry `approved`.
fn approval_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"approved": {"type": "boolean"}},
        "required": ["approved"]
    })
}

/// The flagship gate's approval schema: the approval also carries the list the
/// map downstream fans out over, so the item list is a function of a recorded
/// resume input rather than of the document alone.
fn targets_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"approved": {"type": "boolean"}, "targets": {"type": "array"}},
        "required": ["approved", "targets"]
    })
}

/// Shape one: an agent node, a gate, and a Write tool behind it.
fn gate_shape(research_uri: &str) -> Shape {
    let graph = GraphBuilder::new()
        .agent(AgentSpec::new("research", RESEARCH_HASH))
        .gate(
            GateSpec::new("approve", approval_schema())
                .prompt("Approve this draft for publication?"),
        )
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("research", "approve")
        .edge("approve", "publish")
        .build();
    let uri = research_uri.to_owned();
    Shape {
        name: "gate",
        graph,
        input: json!({"topic": "otters"}),
        run_id: fixed_run_id(50),
        expected_kinds: vec![
            "GraphRunStarted",
            "NodeEntered", // research
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NodeExited",  // research
            "NodeEntered", // approve (gate)
            "Suspended",
            "Resumed",
            "NodeExited",  // approve
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited", // publish
            "RunCompleted",
        ],
        control_write_executions: 1,
        build: Arc::new(move || {
            let mut agents: HashMap<String, Agent> = HashMap::new();
            agents.insert(
                RESEARCH_HASH.to_owned(),
                agent_builder(&uri).build().expect("the agent builds"),
            );
            let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
            Executors::new(agents, vec![(Box::new(publish), publish_calls)])
        }),
        resume: Arc::new(|status| match status {
            RunStatus::Suspended { .. } => json!({"approved": true}),
            other => panic!("the gate shape parks only at its gate, got {other:?}"),
        }),
    }
}

/// Shape two: a Read tool feeding an expression branch, a Write tool on the
/// taken case, and a skipped node on the other.
fn branch_shape() -> Shape {
    let graph = GraphBuilder::new()
        .tool(ToolSpec::new("assess", "assess"))
        .branch(
            BranchSpec::new("route")
                .on("score")
                .case("high", BranchCondition::Expression("score >= 0.8".into()))
                .case("low", BranchCondition::Expression("score < 0.8".into())),
        )
        .tool(ToolSpec::new("publish", "http_post"))
        .tool(ToolSpec::new("reject", "notify"))
        .edge("assess", "route")
        .labeled_edge("route", "publish", "high")
        .labeled_edge("route", "reject", "low")
        .build();
    Shape {
        name: "branch",
        graph,
        input: json!({"topic": "otters"}),
        run_id: fixed_run_id(51),
        expected_kinds: vec![
            "GraphRunStarted",
            "NodeEntered", // assess
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited",  // assess
            "NodeEntered", // route (branch)
            "BranchTaken",
            "NodeExited",  // route
            "NodeEntered", // publish
            "ToolCallRequested",
            "ToolCallCompleted",
            "NodeExited",  // publish
            "NodeSkipped", // reject (the low route)
            "RunCompleted",
        ],
        control_write_executions: 1,
        build: Arc::new(|| {
            // `assess` injects the structured value the branch routes on, and
            // it must be deterministic: a re-executed Read whose output changed
            // would change the route and the log with it.
            let (assess, assess_calls) =
                ConstTool::new("assess", Effect::Read, json!({"score": 0.9}));
            let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
            // Registered with a counter although the taken route never reaches
            // it: a route that ever went the other way must surface as an
            // execution here, not as an unresolved-tool error.
            let (notify, notify_calls) = EchoTool::new("notify", Effect::Write);
            Executors::new(
                HashMap::new(),
                vec![
                    (Box::new(assess), assess_calls),
                    (Box::new(publish), publish_calls),
                    (Box::new(notify), notify_calls),
                ],
            )
        }),
        resume: Arc::new(|status| panic!("the branch shape never parks, got {status:?}")),
    }
}

/// Shape three: a map fan-out over three items whose body is a Write tool,
/// followed by an Idempotent tool.
fn map_shape() -> Shape {
    let graph = GraphBuilder::new()
        .map(MapSpec::new(
            "fanout",
            "targets",
            2,
            MapBody::Node("worker".into()),
        ))
        .tool(ToolSpec::new("worker", "publish_worker"))
        .tool(ToolSpec::new("record", "record_tool"))
        .edge("fanout", "record")
        .build();
    let mut expected_kinds = vec!["GraphRunStarted", "NodeEntered", "MapFannedOut"];
    for _ in 0..3 {
        expected_kinds.extend([
            "MapIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "MapIterationJoined",
        ]);
    }
    expected_kinds.extend([
        "NodeExited",  // fanout
        "NodeEntered", // record
        "ToolCallRequested",
        "ToolCallCompleted",
        "NodeExited", // record
        "RunCompleted",
    ]);
    Shape {
        name: "map",
        graph,
        input: json!({"targets": ["alpha", "beta", "gamma"]}),
        run_id: fixed_run_id(52),
        expected_kinds,
        control_write_executions: 3,
        build: Arc::new(|| {
            // A Write body is what puts a reconciliation refusal inside the
            // fan-out, one per iteration.
            let (worker, worker_calls) = EchoTool::new("publish_worker", Effect::Write);
            let (record, record_calls) = EchoTool::new("record_tool", Effect::Idempotent);
            Executors::new(
                HashMap::new(),
                vec![
                    (Box::new(worker), worker_calls),
                    (Box::new(record), record_calls),
                ],
            )
        }),
        resume: Arc::new(|status| panic!("the map shape never parks, got {status:?}")),
    }
}

/// Shape four: everything at once. An agent node that crosses a `max_steps`
/// budget mid-node, a Read tool, an expression branch, a gate whose approval
/// carries the map's item list, a map fan-out over a Write body, an Idempotent
/// tool, and a skipped node.
fn flagship_shape(research_uri: &str) -> Shape {
    let graph = GraphBuilder::new()
        .agent(AgentSpec::new("research", RESEARCH_HASH))
        .tool(ToolSpec::new("assess", "assess"))
        .branch(
            BranchSpec::new("route")
                .on("score")
                .case("high", BranchCondition::Expression("score >= 0.8".into()))
                .case("low", BranchCondition::Expression("score < 0.8".into())),
        )
        .gate(
            GateSpec::new("approve", targets_schema())
                .prompt("Approve these targets for publication?"),
        )
        .map(MapSpec::new(
            "fanout",
            "targets",
            2,
            MapBody::Node("worker".into()),
        ))
        .tool(ToolSpec::new("worker", "publish_worker"))
        .tool(ToolSpec::new("record", "record_tool"))
        .tool(ToolSpec::new("reject", "notify"))
        .edge("research", "assess")
        .edge("assess", "route")
        .labeled_edge("route", "approve", "high")
        .labeled_edge("route", "reject", "low")
        .edge("approve", "fanout")
        .edge("fanout", "record")
        .build();
    let uri = research_uri.to_owned();
    let mut expected_kinds = vec![
        "GraphRunStarted",
        "NodeEntered", // research
        "NowObserved",
        "ModelCallRequested",
        "ModelCallCompleted",
        "ToolCallRequested", // lookup, the agent's own tool
        "ToolCallCompleted",
        "NowObserved",
        "BudgetExceeded", // max_steps = 1, crossed at the second iteration
        "Resumed",
        "ModelCallRequested",
        "ModelCallCompleted",
        "NodeExited",  // research
        "NodeEntered", // assess
        "ToolCallRequested",
        "ToolCallCompleted",
        "NodeExited",  // assess
        "NodeEntered", // route (branch)
        "BranchTaken",
        "NodeExited",  // route
        "NodeEntered", // approve (gate)
        "Suspended",
        "Resumed",
        "NodeExited",  // approve
        "NodeEntered", // fanout (map)
        "MapFannedOut",
    ];
    for _ in 0..2 {
        expected_kinds.extend([
            "MapIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "MapIterationJoined",
        ]);
    }
    expected_kinds.extend([
        "NodeExited",  // fanout
        "NodeEntered", // record
        "ToolCallRequested",
        "ToolCallCompleted",
        "NodeExited",  // record
        "NodeSkipped", // reject (the low route)
        "RunCompleted",
    ]);
    Shape {
        name: "flagship",
        graph,
        input: json!({"topic": "otters"}),
        run_id: fixed_run_id(53),
        expected_kinds,
        control_write_executions: 2,
        build: Arc::new(move || {
            let (lookup, lookup_calls) = EchoTool::new("lookup", Effect::Read);
            let mut agents: HashMap<String, Agent> = HashMap::new();
            agents.insert(
                RESEARCH_HASH.to_owned(),
                agent_builder(&uri)
                    .tool_dyn(Box::new(lookup))
                    // One completed model call is the whole budget, so the
                    // crossing fires inside the node, between its two turns.
                    .budgets(Budgets {
                        max_steps: Some(1),
                        ..Budgets::default()
                    })
                    .build()
                    .expect("the agent builds"),
            );
            let (assess, assess_calls) =
                ConstTool::new("assess", Effect::Read, json!({"score": 0.9}));
            let (worker, worker_calls) = EchoTool::new("publish_worker", Effect::Write);
            let (record, record_calls) = EchoTool::new("record_tool", Effect::Idempotent);
            let (notify, notify_calls) = EchoTool::new("notify", Effect::Write);
            let mut executors = Executors::new(
                agents,
                vec![
                    (Box::new(assess), assess_calls),
                    (Box::new(worker), worker_calls),
                    (Box::new(record), record_calls),
                    (Box::new(notify), notify_calls),
                ],
            );
            // The agent's own tool is not a graph node, so it is not in the
            // node registry; its counter still has to be, because its calls
            // appear in the log and the oracle counts every one of them.
            executors.counters.insert("lookup".to_owned(), lookup_calls);
            executors
        }),
        resume: Arc::new(|status| match status {
            RunStatus::Suspended { .. } => json!({"approved": true, "targets": ["alpha", "beta"]}),
            RunStatus::BudgetExceeded { .. } => extension_input(),
            other => panic!("the flagship shape parks only at its gate or budget, got {other:?}"),
        }),
    }
}

/// The one-turn model the `gate` shape's agent node drives against.
async fn gate_server() -> MockServer {
    ScriptedModel::mount(vec![(1, text_response("a draft about otters", 5, 3))]).await
}

/// The two-turn model the `flagship` shape's agent node drives against: a tool
/// call, then a final answer. Responses are keyed on the number of `messages`
/// in the request, so one stateless server serves the control run and every
/// concurrent continuation, however much of each replays.
async fn flagship_server() -> MockServer {
    ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_01", "lookup", json!({"q": "otters"}), 101, 11),
        ),
        (3, text_response("a draft about otters", 102, 12)),
    ])
    .await
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// THE GRAPH RELEASE GATE. For each shape in the corpus: record the
/// uninterrupted control run, then for every prefix boundary `0..=len` continue
/// from a fresh file-backed store and assert (a) the completed log is
/// byte-identical to the control log, (b) live executions match the
/// re-execution table exactly, (c) write executions across the whole scenario
/// total exactly the control count, and (d) every dangling-write-intent prefix
/// refuses with the reconciliation error, executing and appending nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn graph_release_gate_kill_at_every_event_boundary_resumes_identically() {
    let started = Instant::now();
    let dir = SweepDir::create("sweep");
    let gate_model = gate_server().await;
    let flagship_model = flagship_server().await;

    let shapes = vec![
        gate_shape(&gate_model.uri()),
        branch_shape(),
        map_shape(),
        flagship_shape(&flagship_model.uri()),
    ];

    let mut reports = Vec::with_capacity(shapes.len());
    for shape in shapes {
        reports.push(sweep(shape, dir.path()).await);
    }

    // The corpus is fixed, so its per-shape boundary counts are too: a shape
    // that quietly stopped covering the boundaries it claims to cover is a
    // failure here rather than an unnoticed weakening of the gate.
    let counts: Vec<(&str, usize, usize, usize)> = reports
        .iter()
        .map(|report| {
            (
                report.name,
                report.events,
                report.boundaries,
                report.refusals,
            )
        })
        .collect();
    assert_eq!(
        counts,
        vec![
            ("gate", 15, 16, 1),
            ("branch", 14, 15, 1),
            ("map", 21, 22, 3),
            ("flagship", 41, 42, 2),
        ],
        "each shape sweeps every boundary of its control log and refuses at each write intent"
    );

    let total: usize = reports.iter().map(|report| report.boundaries).sum();
    eprintln!(
        "graph release gate: {total} boundaries swept across {} shapes in {:.2?}",
        reports.len(),
        started.elapsed()
    );
}

/// A `fold` node has no run to sweep: the engine refuses it before recording
/// its `NodeEntered`, so a fold graph produces a one-event log with no
/// boundaries to kill at. This is why `fold` is absent from the corpus above,
/// checked here rather than asserted in the module docs.
#[tokio::test]
async fn a_fold_node_has_no_run_to_sweep() {
    let body = GraphBuilder::new()
        .tool(ToolSpec::new("inner", "inner_tool"))
        .build();
    let graph = GraphBuilder::new()
        .fold(FoldSpec::new(
            "refine",
            FoldBody::Subgraph(Box::new(body)),
            3,
            "score >= 0.85",
            FoldJoin::BestBy("score".into()),
        ))
        .build();

    let run_id = fixed_run_id(54);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let agents: HashMap<String, Agent> = HashMap::new();
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    let error = run_graph(&mut ctx, &graph, &json!({"score": 0.9}), &agents, &tools)
        .await
        .expect_err("the engine does not execute a fold node");
    assert!(
        matches!(error, EngineError::UnsupportedNode { kind: "fold", .. }),
        "expected the fold refusal, got {error:?}"
    );
    assert_eq!(
        event_kinds(&store.read_log(run_id).await.expect("log reads")),
        ["GraphRunStarted"],
        "a refused fold leaves one event, so there is no boundary sweep to run"
    );
}

/// A graph run's log records only the document's hash, so a continuation must
/// re-supply the document itself. Both halves of that:
///
/// - re-supplying a DIFFERENT document refuses as a divergence at the head and
///   appends nothing, so a changed graph cannot silently resume an old run;
/// - re-supplying the same document with a DIFFERENT `input` argument is
///   harmless, because the recorded input always wins.
#[tokio::test]
async fn resuming_a_graph_run_must_re_supply_the_matching_document() {
    let dir = SweepDir::create("re-supply");
    let model = gate_server().await;
    let shape = gate_shape(&model.uri());
    let control = record_control(&shape, dir.path()).await;

    // Park the run at its gate: the prefix up to (not including) the `Resumed`
    // the control recorded there.
    let park = control
        .iter()
        .position(|envelope| matches!(envelope.event, Event::Resumed { .. }))
        .expect("the control run parked at its gate");
    let store: Arc<dyn EventStore> =
        Arc::new(SqliteStore::open(dir.path().join("re-supply.db")).expect("boundary store opens"));
    for envelope in &control[..park] {
        store.append(envelope).await.expect("prefix event appends");
    }
    let executors = (shape.build)();

    // A different document: the same topology with a different gate prompt, so
    // only the hash changes. The refusal is a divergence at the head.
    let mut altered = gate_shape(&model.uri());
    altered.graph = GraphBuilder::new()
        .agent(AgentSpec::new("research", RESEARCH_HASH))
        .gate(GateSpec::new("approve", approval_schema()).prompt("Approve? (reworded)"))
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("research", "approve")
        .edge("approve", "publish")
        .build();
    let error = drive_once(
        &altered,
        &executors,
        &store,
        control[..park].to_vec(),
        Some(json!({"approved": true})),
    )
    .await
    .expect_err("a changed document must not resume an old run");
    assert!(
        matches!(
            error,
            EngineError::Runtime(RuntimeError::Replay(ReplayError::Divergence { .. }))
        ),
        "expected a divergence at the recorded head, got {error:?}"
    );
    assert_eq!(
        store.read_log(shape.run_id).await.expect("log reads").len(),
        park,
        "the refused resume appended nothing"
    );
    assert_eq!(executors.total(), 0, "the refused resume executed nothing");

    // The matching document with a deliberately wrong `input` argument: the
    // recorded input wins, so the run completes to the byte-identical log.
    let mut wrong_input = gate_shape(&model.uri());
    wrong_input.input = json!({"topic": "not the recorded topic"});
    drive_to_completion(
        &wrong_input,
        &executors,
        &store,
        Some(&control),
        "re-supply",
    )
    .await;
    let log = store.read_log(shape.run_id).await.expect("log reads");
    assert_eq!(
        serde_json::to_string(&log).unwrap(),
        serde_json::to_string(&control).unwrap(),
        "the recorded input wins over the re-supplied one, byte for byte"
    );
}

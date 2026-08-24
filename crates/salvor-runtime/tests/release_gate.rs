//! THE RELEASE GATE: kill the reference run at every
//! possible event boundary, resume it through the full runtime, and assert
//! the continued run reaches a byte-identical final log with zero duplicate
//! `Write` executions. This suite passing is the release gate for v0.1.
//!
//! It is the full-runtime sibling of the engine-level property in
//! `salvor-core/tests/every_boundary.rs`, and the exhaustive complement of
//! the process-level anchor in `salvor-cli/tests/kill_resume.rs` (which
//! SIGKILLs the real binary at one boundary). This suite covers *every*
//! boundary, with the whole stack in the loop: the built-in driver, budget
//! enforcement, suspension, the wire sentinels, and a file-backed
//! `SqliteStore`.
//!
//! # The reference run
//!
//! Twenty loop turns against a scripted model, exercising every runtime
//! behavior: 20 model calls; 8 `search` calls (Read), 5 `store_doc` calls
//! (Idempotent, so each records a `RandomObserved` idempotency-key draw),
//! 5 `publish` calls (Write, the calls that must never duplicate); one
//! `approval` tool that suspends the run (resumed with `{"approved": true}`);
//! a `max_steps = 10` budget crossing at turn 11 (resumed with a recorded
//! extension); and completion at turn 20. The uninterrupted control log has
//! 109 events, so the sweep covers 110 boundaries (every prefix 0..=109).
//!
//! # How a kill is modeled
//!
//! For each boundary, the control log's prefix is copied into a fresh
//! file-backed `SqliteStore` and a fresh `Runtime` (same injected constant
//! clock and random source, same run id) continues the run. That is exactly
//! the state a `kill -9` leaves behind: some prefix of the log durable,
//! everything else gone with the process. The continuation dispatches on
//! derived state precisely as the CLI's `resume` verb does
//! (`salvor-cli/src/commands.rs`): parked runs resume with the recorded
//! input, crashed runs recover, and a dangling write intent refuses.
//!
//! # The expected re-execution table
//!
//! For a cut at prefix length `c`, with a tool intent recorded at sequence
//! `i` (its completion at `i + 1`):
//!
//! - `c <= i`: neither intent nor completion survived; the call runs live
//!   exactly once during the continuation.
//! - `c == i + 1`, effect Read or Idempotent: the intent survived without
//!   its completion, so the call re-executes on recovery. That is the
//!   specified effect-table behavior (Read: re-execute if unrecorded;
//!   Idempotent: safe to re-attempt under the same recorded key), and the
//!   suite asserts the extra execution explicitly rather than ignoring it.
//! - `c == i + 1`, effect Write: recovery REFUSES with the reconciliation
//!   error and executes nothing at all. Five boundaries land here, one per
//!   `publish` call.
//! - `c >= i + 2`: the completed call replays from the log and never
//!   re-executes.
//!
//! Dangling *model* intents are re-issued on recovery by design (an
//! unanswered request had no side effect); the scripted server answers by
//! request shape, so the fresh completion is byte-identical.
//!
//! Whole-scenario write accounting closes the loop: executions represented
//! by the prefix (one per recorded write completion) plus live executions
//! during the continuation must equal the control run's total exactly.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response, tool_use_response,
};
use salvor_core::{Effect, Event, EventEnvelope, ReplayError, RunId, RunStatus, derive_state};
use salvor_runtime::{Agent, Budgets, Runtime, RuntimeError};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::Suspension;
use serde_json::{Value, json};
use wiremock::MockServer;

/// What the model asks for on each of the twenty loop turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// Call the `search` tool (Read).
    Search,
    /// Call the `store_doc` tool (Idempotent; draws a recorded random key).
    StoreDoc,
    /// Call the `publish` tool (Write; must never duplicate).
    Publish,
    /// Call the `approval` tool (Read), which suspends the run.
    Approval,
    /// Answer with final text, completing the run.
    Final,
}

/// The twenty-turn reference plan: 8 reads, 5 idempotent stores, 5 writes,
/// one suspension, then completion. The budget crossing is not a turn; it
/// interposes at the start of turn [`BUDGET_TURN`]'s iteration.
const TURNS: [Turn; 20] = [
    Turn::Search,   // 1
    Turn::StoreDoc, // 2
    Turn::Publish,  // 3
    Turn::Approval, // 4  (suspends; resumed with the approval input)
    Turn::Search,   // 5
    Turn::StoreDoc, // 6
    Turn::Search,   // 7
    Turn::Publish,  // 8
    Turn::Search,   // 9
    Turn::StoreDoc, // 10
    Turn::Publish,  // 11 (the max_steps crossing fires before this call)
    Turn::Search,   // 12
    Turn::StoreDoc, // 13
    Turn::Search,   // 14
    Turn::Publish,  // 15
    Turn::Search,   // 16
    Turn::StoreDoc, // 17
    Turn::Search,   // 18
    Turn::Publish,  // 19
    Turn::Final,    // 20
];

/// The declared step budget: ten completed model calls, so the crossing
/// fires at the start of the eleventh iteration.
const MAX_STEPS: u64 = 10;

/// The 1-based turn whose iteration begins with the budget crossing.
const BUDGET_TURN: usize = 11;

/// Control-run write executions: one per `publish` turn.
const CONTROL_WRITE_EXECUTIONS: usize = 5;

/// The run's initial input.
fn run_input() -> Value {
    json!("survey the archived research notes")
}

/// The schema the suspending `approval` tool demands of its resume input.
fn approval_schema() -> Value {
    json!({"type": "object", "required": ["approved"]})
}

/// The operator's recorded answer to the suspension.
fn approval_input() -> Value {
    json!({"approved": true})
}

/// The operator's recorded budget extension.
fn extension_input() -> Value {
    json!({"extend": {"steps": 100}})
}

/// Mounts the scripted model for the whole sweep. Responses are selected by
/// the number of `messages` in the request (turn `k` carries `2k - 1`), so
/// one stateless server correctly serves the control run and every
/// concurrent boundary continuation, however much of each run replays.
async fn scripted_server() -> MockServer {
    let mut script = Vec::with_capacity(TURNS.len());
    for (index, turn) in TURNS.iter().enumerate() {
        let number = index + 1;
        let messages = 2 * number - 1;
        let id = format!("tu_{number:02}");
        let input_tokens = (100 + number) as u64;
        let output_tokens = (10 + number) as u64;
        let response = match turn {
            Turn::Search => tool_use_response(
                &id,
                "search",
                json!({"q": format!("subtopic {number}")}),
                input_tokens,
                output_tokens,
            ),
            Turn::StoreDoc => tool_use_response(
                &id,
                "store_doc",
                json!({"doc": format!("note {number}")}),
                input_tokens,
                output_tokens,
            ),
            Turn::Publish => tool_use_response(
                &id,
                "publish",
                json!({"finding": format!("finding {number}")}),
                input_tokens,
                output_tokens,
            ),
            Turn::Approval => {
                tool_use_response(&id, "approval", json!({}), input_tokens, output_tokens)
            }
            Turn::Final => text_response("research complete", input_tokens, output_tokens),
        };
        script.push((messages, response));
    }
    ScriptedModel::mount(script).await
}

/// Per-tool execution counters, shared with the live tools of one agent.
struct Counters {
    search: Arc<AtomicUsize>,
    store_doc: Arc<AtomicUsize>,
    publish: Arc<AtomicUsize>,
    approval: Arc<AtomicUsize>,
}

impl Counters {
    fn snapshot(&self) -> ExecutionCounts {
        ExecutionCounts {
            search: self.search.load(Ordering::SeqCst),
            store_doc: self.store_doc.load(Ordering::SeqCst),
            publish: self.publish.load(Ordering::SeqCst),
            approval: self.approval.load(Ordering::SeqCst),
        }
    }
}

/// A comparable per-tool execution count, for oracle assertions.
#[derive(Debug, Default, PartialEq, Eq)]
struct ExecutionCounts {
    search: usize,
    store_doc: usize,
    publish: usize,
    approval: usize,
}

impl ExecutionCounts {
    fn total(&self) -> usize {
        self.search + self.store_doc + self.publish + self.approval
    }

    fn slot(&mut self, tool: &str) -> &mut usize {
        match tool {
            "search" => &mut self.search,
            "store_doc" => &mut self.store_doc,
            "publish" => &mut self.publish,
            "approval" => &mut self.approval,
            other => panic!("unexpected tool in the log: {other}"),
        }
    }
}

/// Builds a fresh reference agent over the scripted server, with fresh
/// execution counters. Same builder inputs every time, so the agent
/// definition hash matches across the control run and every continuation.
fn reference_agent(server_uri: &str) -> (Agent, Counters) {
    let (search, search_calls) = TestTool::new("search", Effect::Read, ToolBehavior::Echo);
    let (store_doc, store_calls) =
        TestTool::new("store_doc", Effect::Idempotent, ToolBehavior::Echo);
    let (publish, publish_calls) = TestTool::new("publish", Effect::Write, ToolBehavior::Echo);
    let (approval, approval_calls) = TestTool::new(
        "approval",
        Effect::Read,
        ToolBehavior::Suspend(Suspension::new(
            "awaiting reviewer approval",
            approval_schema(),
        )),
    );
    let agent = agent_builder(server_uri)
        .tool_dyn(Box::new(search))
        .tool_dyn(Box::new(store_doc))
        .tool_dyn(Box::new(publish))
        .tool_dyn(Box::new(approval))
        .budgets(Budgets {
            max_steps: Some(MAX_STEPS),
            ..Budgets::default()
        })
        .build()
        .expect("the reference agent builds");
    (
        agent,
        Counters {
            search: search_calls,
            store_doc: store_calls,
            publish: publish_calls,
            approval: approval_calls,
        },
    )
}

/// The event-kind sequence the control run must record, derived from the
/// turn plan so the plan is the single source of truth for the run's shape.
fn expected_control_kinds() -> Vec<&'static str> {
    let mut kinds = vec!["RunStarted"];
    for (index, turn) in TURNS.iter().enumerate() {
        let number = index + 1;
        kinds.push("NowObserved");
        if number == BUDGET_TURN {
            kinds.extend(["BudgetExceeded", "Resumed"]);
        }
        kinds.extend(["ModelCallRequested", "ModelCallCompleted"]);
        match turn {
            Turn::Search | Turn::Publish => {
                kinds.extend(["ToolCallRequested", "ToolCallCompleted"]);
            }
            Turn::StoreDoc => {
                kinds.extend(["RandomObserved", "ToolCallRequested", "ToolCallCompleted"]);
            }
            Turn::Approval => {
                kinds.extend([
                    "ToolCallRequested",
                    "ToolCallCompleted", // the suspension sentinel
                    "Suspended",
                    "Resumed",
                ]);
            }
            Turn::Final => kinds.push("RunCompleted"),
        }
    }
    kinds
}

/// Drives a run to completion the way an operator (or the CLI's `resume`
/// verb) would: read the log, derive state, and dispatch. Parked states
/// resume with the scenario's recorded inputs; crashed states recover.
///
/// When `control` is given (boundary continuations), the resume input is
/// cross-checked against the `Resumed` event recorded at the parked
/// position in the control log, proving continuations feed the run exactly
/// what the control run recorded.
async fn drive_to_completion(
    runtime: &Runtime,
    agent: &Agent,
    run_id: RunId,
    store: &Arc<dyn EventStore>,
    control: Option<&[EventEnvelope]>,
) {
    // A completed drive needs at most four actions (start, two park
    // resumes, and the final leg); the cap only bounds a runaway bug.
    for _ in 0..16 {
        let log = store.read_log(run_id).await.expect("log reads");
        let state = derive_state(&log);
        match &state.status {
            RunStatus::NotStarted => {
                runtime
                    .start_with_id(agent, run_id, run_input())
                    .await
                    .expect("the fresh run drives");
            }
            RunStatus::Completed { .. } => return,
            RunStatus::Suspended { .. } => {
                let input = approval_input();
                if let Some(control) = control {
                    assert_recorded_resume(control, log.len(), &input);
                }
                runtime
                    .resume(agent, run_id, input)
                    .await
                    .expect("the suspension resumes");
            }
            RunStatus::BudgetExceeded { .. } => {
                let input = extension_input();
                if let Some(control) = control {
                    assert_recorded_resume(control, log.len(), &input);
                }
                runtime
                    .resume(agent, run_id, input)
                    .await
                    .expect("the budget crossing resumes with an extension");
            }
            RunStatus::Failed { error } => panic!("the run failed: {error}"),
            RunStatus::NeedsReconciliation => {
                panic!("reconciliation must be handled before dispatch")
            }
            // Running / AwaitingModel / AwaitingTool: crashed mid-step.
            _ => {
                runtime
                    .recover(agent, run_id)
                    .await
                    .expect("the crashed run recovers");
            }
        }
    }
    panic!("the run did not complete within the action cap");
}

/// Asserts the control log recorded a `Resumed` event carrying exactly
/// `input` at `position`, the seam a continuation's resume writes into.
fn assert_recorded_resume(control: &[EventEnvelope], position: usize, input: &Value) {
    match &control[position].event {
        Event::Resumed { input: recorded } => assert_eq!(
            recorded, input,
            "the resume input must be the recorded one (control position {position})"
        ),
        other => panic!("expected Resumed at control position {position}, found {other:?}"),
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

/// The oracle: expected live executions per tool for a continuation from
/// prefix length `cut`, per the re-execution table in the module docs.
fn expected_executions(control: &[EventEnvelope], cut: usize) -> ExecutionCounts {
    let mut counts = ExecutionCounts::default();
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
        *counts.slot(tool) += runs;
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

/// A directory of per-boundary SQLite files under the OS temp dir, removed
/// on drop. (std-only so the crate needs no extra dev-dependency.)
struct SweepDir(PathBuf);

impl SweepDir {
    fn create() -> Self {
        let path = std::env::temp_dir().join(format!("salvor-release-gate-{}", std::process::id()));
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

/// Runs the uninterrupted control run on a file-backed store and returns
/// its full log: the oracle every continuation must reproduce exactly.
async fn record_control(server_uri: &str, dir: &Path) -> Vec<EventEnvelope> {
    let store: Arc<dyn EventStore> =
        Arc::new(SqliteStore::open(dir.join("control.db")).expect("control store opens"));
    let (agent, counters) = reference_agent(server_uri);
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(80);

    drive_to_completion(&runtime, &agent, run_id, &store, None).await;

    let log = store.read_log(run_id).await.expect("control log reads");
    assert_eq!(
        event_kinds(&log),
        expected_control_kinds(),
        "the control run's shape matches the turn plan"
    );
    assert_eq!(
        counters.snapshot(),
        ExecutionCounts {
            search: 8,
            store_doc: 5,
            publish: 5,
            approval: 1,
        },
        "the control run executes every tool exactly once per call"
    );
    log
}

/// One boundary of the sweep: copy the prefix into a fresh file-backed
/// store, continue with a fresh runtime, and assert the outcome. Returns
/// whether this boundary was a (correct) reconciliation refusal.
async fn continue_from_boundary(
    cut: usize,
    control: Arc<Vec<EventEnvelope>>,
    server_uri: Arc<String>,
    db_path: PathBuf,
) -> bool {
    let store: Arc<dyn EventStore> =
        Arc::new(SqliteStore::open(&db_path).expect("boundary store opens"));
    for envelope in &control[..cut] {
        store.append(envelope).await.expect("prefix event appends");
    }
    let (agent, counters) = reference_agent(&server_uri);
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(80);

    // The refusal boundaries: a dangling write intent. The write may or may
    // not have landed before the kill; only a human may decide, so recovery
    // must refuse with the reconciliation error and execute nothing.
    if ends_at_dangling_write_intent(&control[..cut]) {
        assert_eq!(
            derive_state(&control[..cut]).status,
            RunStatus::NeedsReconciliation,
            "cut {cut}: a dangling write intent derives to reconciliation"
        );
        let error = runtime
            .recover(&agent, run_id)
            .await
            .expect_err("recovery over a dangling write intent must refuse");
        assert!(
            matches!(
                error,
                RuntimeError::Replay(ReplayError::NeedsReconciliation { .. })
            ),
            "cut {cut}: expected the reconciliation error, got {error:?}"
        );
        assert_eq!(
            counters.snapshot().total(),
            0,
            "cut {cut}: a refused recovery executes nothing"
        );
        return true;
    }

    if cut == control.len() {
        // The full log: recovery is a pure end-to-end replay. It must reach
        // the completed outcome, execute nothing, and append nothing.
        runtime
            .recover(&agent, run_id)
            .await
            .expect("full-log replay is divergence free");
    } else {
        drive_to_completion(&runtime, &agent, run_id, &store, Some(&control)).await;
    }

    let log = store.read_log(run_id).await.expect("final log reads");
    assert_eq!(
        log, *control,
        "cut {cut}: the continued log must equal the control log exactly"
    );

    let expected = expected_executions(&control, cut);
    assert_eq!(
        counters.snapshot(),
        expected,
        "cut {cut}: live executions must match the re-execution table"
    );

    // Whole-scenario write accounting: executions represented by the prefix
    // plus live executions equal the control total. Zero duplicates.
    assert_eq!(
        prefix_write_executions(&control, cut) + counters.publish.load(Ordering::SeqCst),
        CONTROL_WRITE_EXECUTIONS,
        "cut {cut}: write executions across the whole scenario must total the control count"
    );
    false
}

/// THE RELEASE GATE. Records the 109-event control run, then for every
/// prefix boundary 0..=109 continues from a fresh store and asserts:
/// (a) the completed log equals the control log exactly, (b) write
/// executions across the whole scenario total exactly the control count,
/// and (c) the five dangling-write-intent prefixes refuse with the
/// reconciliation error and execute nothing. Boundaries run concurrently;
/// each has its own store file and its own agent counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn release_gate_kill_at_every_event_boundary_resumes_identically() {
    let started = Instant::now();
    let dir = SweepDir::create();
    let server = scripted_server().await;

    let control = Arc::new(record_control(&server.uri(), dir.path()).await);
    assert_eq!(control.len(), 109, "the reference run records 109 events");

    let server_uri = Arc::new(server.uri());
    let mut boundaries = tokio::task::JoinSet::new();
    for cut in 0..=control.len() {
        let control = control.clone();
        let server_uri = server_uri.clone();
        let db_path = dir.path().join(format!("boundary_{cut:03}.db"));
        boundaries
            .spawn(async move { continue_from_boundary(cut, control, server_uri, db_path).await });
    }

    let mut refused = 0;
    let mut completed = 0;
    while let Some(result) = boundaries.join_next().await {
        match result {
            Ok(was_refusal) => {
                refused += usize::from(was_refusal);
                completed += 1;
            }
            // Surface the boundary's own panic message instead of a bare
            // JoinError, so a failure names its cut.
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }
    assert_eq!(completed, control.len() + 1, "every boundary was swept");
    assert_eq!(
        refused, CONTROL_WRITE_EXECUTIONS,
        "exactly the five write-intent prefixes refuse; every other boundary completes"
    );
    eprintln!(
        "release gate: {} boundaries swept in {:.2?}",
        completed,
        started.elapsed()
    );
}

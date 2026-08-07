//! Acceptance for the `fold` node's execution: a bounded loop whose passes run
//! INLINE and SEQUENTIALLY in the parent's own log, each pass folding over the
//! one before it. Because the loop lives in one log, the whole feature is proven
//! by the same single-log replay machinery already proven for linear, branching,
//! and map graphs:
//!
//! - THE PROPERTY TEST FIRST (`fold_holds_the_property_at_every_kill_boundary`):
//!   a multi-pass loop is killed at EVERY event boundary; after recovery the
//!   log is byte-identical to an uninterrupted drive, and every completed body
//!   call executed exactly once;
//! - joins are recorded in pass order, and a completed fold run re-drives with
//!   ZERO live calls and a byte-identical log;
//! - the loop's progress and its convergence read back through
//!   `derive_graph_projection`;
//! - the three join rules: `best_by` is an argmax over ALL passes (a middle pass
//!   can win, ties keep the earliest pass, and no comparable candidate at all is
//!   a typed refusal recorded BEFORE any convergence), `last` takes the final
//!   pass, `all` takes every pass's value in order;
//! - both stop causes: the predicate firing early, and the bound being reached,
//!   each naming itself in the recorded reason;
//! - a parked pass records no join, so a resume re-drives that same pass;
//! - a `subgraph` body, or a body node that is not an `agent` or `tool`, is a
//!   typed `UnsupportedFoldBody` refused before the fold's `NodeEntered`;
//! - the committed `fold-refine.json` fixture drives under the engine with its
//!   `tailor` agent as the per-pass worker, walked once per pass and never as a
//!   node of its own.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{
    PassTool, ScriptedModel, SuspendingTool, agent_builder, event_kinds, fixed_clock, fixed_random,
    fixed_run_id, text_response,
};
use salvor_core::{Effect, Event, EventEnvelope, RunId};
use salvor_engine::{EngineError, ForkError, GraphOutcome, plan_fork, run_graph};
use salvor_graph::{
    FoldBody, FoldJoin, FoldSpec, GateSpec, Graph, GraphBuilder, ToolSpec, validate,
};
use salvor_replay::{NodeState, derive_graph_projection};
use salvor_runtime::{Agent, ParkReason, RunCtx};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::{Value, json};

/// The agent hash `fold-refine.json` registers its `tailor` body under.
const TAILOR_HASH: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

/// A one-fold-node graph: `refine` (the entry) folds through the `worker` tool
/// up to `max_iterations` times. `worker` is the fold's body, so it is never
/// walked independently.
fn fold_only_graph(max_iterations: u32, stop_when: &str, join: FoldJoin) -> Graph {
    GraphBuilder::new()
        .fold(FoldSpec::new(
            "refine",
            FoldBody::Node("worker".into()),
            max_iterations,
            stop_when,
            join,
        ))
        .tool(ToolSpec::new("worker", "refine_tool"))
        .build()
}

/// A registry holding the scripted `refine_tool` body plus its shared execution
/// counter, so a replay's zero-execution claim (and the exactly-once claim) is
/// checkable. Idempotent so it also exercises the position-derived per-pass key
/// and re-issues safely on a dangling intent.
fn worker_tools(scores: Vec<Value>) -> (HashMap<String, Box<dyn DynTool>>, Arc<AtomicUsize>) {
    let (worker, calls) = PassTool::new("refine_tool", Effect::Idempotent, scores);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("refine_tool".to_owned(), Box::new(worker));
    (tools, calls)
}

fn no_agents() -> HashMap<String, Agent> {
    HashMap::new()
}

/// The graph input every scripted fold starts from: pass 0 folds over this, and
/// the tool reads its `pass` count out of it.
fn seed() -> Value {
    json!({"pass": 0})
}

/// Drives `graph` fresh over a new store and returns the produced log and the
/// number of body executions it took.
async fn drive_fresh(
    graph: &Graph,
    scores: Vec<Value>,
    run_id: RunId,
) -> (Vec<EventEnvelope>, usize) {
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let (tools, calls) = worker_tools(scores);
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(&mut ctx, graph, &seed(), &no_agents(), &tools)
        .await
        .expect("graph drives");
    assert!(
        matches!(outcome, GraphOutcome::Completed { .. }),
        "fold graph completes, got {outcome:?}"
    );
    (
        store.read_log(run_id).await.expect("log reads"),
        calls.load(Ordering::SeqCst),
    )
}

/// The recorded convergence: the winner index and the reason the loop stopped.
fn convergence(log: &[EventEnvelope]) -> (u64, String) {
    log.iter()
        .find_map(|envelope| match &envelope.event {
            Event::FoldConverged {
                winner_index,
                reason,
                ..
            } => Some((*winner_index, reason.clone())),
            _ => None,
        })
        .expect("the fold converged")
}

/// The run's final output, off the terminal.
fn final_output(log: &[EventEnvelope]) -> Value {
    match &log.last().expect("a terminal").event {
        Event::RunCompleted { output } => output.clone(),
        other => panic!("expected the terminal, found {other:?}"),
    }
}

/// The event-kind sequence a `passes`-pass loop over a tool body records.
fn expected_kinds(passes: usize) -> Vec<&'static str> {
    let mut kinds = vec!["GraphRunStarted", "NodeEntered"];
    for _ in 0..passes {
        kinds.extend([
            "FoldIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "FoldIterationJoined",
        ]);
    }
    kinds.extend(["FoldConverged", "NodeExited", "RunCompleted"]);
    kinds
}

// ---------------------------------------------------------------------------
// The property test
// ---------------------------------------------------------------------------

/// THE PROPERTY TEST. A three-pass loop is killed at EVERY event boundary; after
/// recovery from the truncated prefix:
///   (a) the resumed log is byte-identical to an uninterrupted drive, and
///   (b) every completed body call executed exactly once: no call whose
///       completion is already in the prefix re-executes, and each remaining call
///       runs exactly once.
///
/// This is the fold sibling of `map_graph.rs`'s fan-out sweep, mechanized the
/// same way: seed a fresh store with the first `k` events of the uninterrupted
/// log (for every `k`), resume a `RunCtx` over that prefix, and drive to
/// completion. A cut inside a pass lands between the pass's `FoldIterationStarted`
/// and its `FoldIterationJoined`, which is exactly the parked-pass shape: the
/// resume re-enters the SAME pass and re-runs its body.
#[tokio::test]
async fn fold_holds_the_property_at_every_kill_boundary() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::BestBy("score".into()));
    let scores = vec![json!(1), json!(5), json!(2)];
    let run_id = fixed_run_id(40);

    // The uninterrupted reference drive.
    let (full, calls) = drive_fresh(&graph, scores.clone(), run_id).await;
    assert_eq!(event_kinds(&full), expected_kinds(3));
    assert_eq!(calls, 3, "three passes, three body calls");

    // For each cut k, the number of body executions the resume must perform: the
    // count of ToolCallCompleted events at seq >= k (calls not yet completed when
    // the kill struck).
    let completions_at_or_after = |k: usize| -> usize {
        full.iter()
            .filter(|env| (env.seq.get() as usize) >= k)
            .filter(|env| matches!(env.event, Event::ToolCallCompleted { .. }))
            .count()
    };

    for k in 0..=full.len() {
        // Seed a fresh store with the first k events: the "kill after k events"
        // state on disk.
        let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
        for env in &full[..k] {
            store.append(env).await.expect("seed append");
        }
        let prefix: Vec<EventEnvelope> = full[..k].to_vec();

        // Resume with a fresh counter so the resume's own executions are isolated.
        let (tools, calls) = worker_tools(scores.clone());
        let mut ctx =
            RunCtx::with_hooks(store.clone(), run_id, prefix, fixed_clock(), fixed_random())
                .expect("resume ctx builds");
        let outcome = run_graph(&mut ctx, &graph, &seed(), &no_agents(), &tools)
            .await
            .unwrap_or_else(|e| panic!("resume from cut {k} drives: {e}"));
        assert!(
            matches!(outcome, GraphOutcome::Completed { .. }),
            "resume from cut {k} completes"
        );

        // (a) Byte-identical log.
        let recovered = store.read_log(run_id).await.expect("log reads");
        assert_eq!(
            serde_json::to_string(&recovered).unwrap(),
            serde_json::to_string(&full).unwrap(),
            "resume from cut {k} must reproduce the byte-identical log"
        );

        // (b) Exactly-once: the resume executed exactly the calls not yet
        // completed in the prefix, and nothing already completed re-ran.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            completions_at_or_after(k),
            "resume from cut {k} executed exactly the not-yet-completed body calls"
        );
    }
}

// ---------------------------------------------------------------------------
// The recorded shape, the replay, and the projection
// ---------------------------------------------------------------------------

/// A fold's joins are recorded in pass order, one per pass, and each pass folds
/// over the pass before it: the tool counts itself up from the graph input, so
/// the recorded outputs are the sequence only a threaded accumulator produces.
#[tokio::test]
async fn fold_joins_in_pass_order_and_threads_each_pass_into_the_next() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::All);
    let (log, _) = drive_fresh(&graph, vec![json!(1), json!(2), json!(3)], fixed_run_id(41)).await;

    let started: Vec<u64> = log
        .iter()
        .filter_map(|env| match &env.event {
            Event::FoldIterationStarted { node, index } if node == "refine" => Some(*index),
            _ => None,
        })
        .collect();
    let joined: Vec<u64> = log
        .iter()
        .filter_map(|env| match &env.event {
            Event::FoldIterationJoined { node, index } if node == "refine" => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(started, [0, 1, 2], "passes started in index order");
    assert_eq!(joined, [0, 1, 2], "joins recorded in index order");

    // The `all` join shows every pass's value: pass 0 folded over the graph
    // input, and each later pass over the one before it.
    assert_eq!(
        final_output(&log),
        json!([
            {"pass": 1, "score": 1},
            {"pass": 2, "score": 2},
            {"pass": 3, "score": 3},
        ]),
        "each pass's input was the previous pass's output"
    );
}

/// A completed fold run re-drives with ZERO live calls and a byte-identical log:
/// the replay proof, extended to the fold markers and the inline body calls.
#[tokio::test]
async fn a_completed_fold_run_replays_free_and_byte_identical() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::BestBy("score".into()));
    let scores = vec![json!(1), json!(5), json!(2)];
    let run_id = fixed_run_id(42);

    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let (tools, calls) = worker_tools(scores.clone());
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    run_graph(&mut ctx, &graph, &seed(), &no_agents(), &tools)
        .await
        .expect("graph drives");
    let live_log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(calls.load(Ordering::SeqCst), 3, "three body calls live");

    let (replay_tools, replay_calls) = worker_tools(scores);
    let mut ctx2 = RunCtx::with_hooks(
        store.clone(),
        run_id,
        live_log.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("replay ctx builds");
    let outcome = run_graph(&mut ctx2, &graph, &seed(), &no_agents(), &replay_tools)
        .await
        .expect("replay drives");
    assert!(matches!(outcome, GraphOutcome::Completed { .. }));
    assert_eq!(
        replay_calls.load(Ordering::SeqCst),
        0,
        "replay makes zero live body calls"
    );
    let replay_log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        serde_json::to_string(&replay_log).unwrap(),
        serde_json::to_string(&live_log).unwrap(),
        "replay leaves the log byte-identical"
    );
}

/// The loop's per-pass progress and its convergence read back through
/// `derive_graph_projection`, and the body node never appears: it is fold-owned,
/// so it is never walked.
#[tokio::test]
async fn fold_projection_reads_back() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::BestBy("score".into()));
    let (log, _) = drive_fresh(&graph, vec![json!(1), json!(5), json!(2)], fixed_run_id(43)).await;

    let projection = derive_graph_projection(&log);
    let refine = projection.node("refine").expect("refine was reached");
    assert_eq!(refine.state, NodeState::Exited, "the fold exited");
    let fold = refine.fold.as_ref().expect("the fold iterated");
    assert_eq!(fold.iterations.len(), 3);
    for (index, iteration) in fold.iterations.iter().enumerate() {
        assert_eq!(iteration.index, index as u64, "passes read back in order");
        assert!(iteration.joined, "every pass joined");
    }
    let (winner_index, reason) = convergence(&log);
    let converged = fold.converged.as_ref().expect("the loop settled");
    assert_eq!(
        (converged.winner_index, converged.reason.as_str()),
        (winner_index, reason.as_str()),
        "the recorded convergence is what the projection reports"
    );
    assert!(
        projection.node("worker").is_none(),
        "the body node is not walked"
    );
}

// ---------------------------------------------------------------------------
// The join rules
// ---------------------------------------------------------------------------

/// `best_by` is an argmax over ALL passes, not a preference for the first or the
/// last: a middle pass wins, and the recorded `winner_index` names it.
#[tokio::test]
async fn best_by_picks_the_best_pass_even_in_the_middle() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::BestBy("score".into()));
    let (log, calls) =
        drive_fresh(&graph, vec![json!(1), json!(5), json!(2)], fixed_run_id(44)).await;

    assert_eq!(calls, 3, "the bound ran every pass");
    let (winner_index, _) = convergence(&log);
    assert_eq!(winner_index, 1, "the middle pass carried the best score");
    assert_eq!(
        final_output(&log),
        json!({"pass": 2, "score": 5}),
        "the fold produces the winning pass's value, not the last pass's"
    );
}

/// A tie keeps the EARLIEST pass: only a strictly greater candidate displaces
/// the incumbent, so the winner of equal scores is the one that ran first.
#[tokio::test]
async fn best_by_breaks_a_tie_to_the_earliest_pass() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::BestBy("score".into()));
    let (log, _) = drive_fresh(&graph, vec![json!(5), json!(5), json!(1)], fixed_run_id(45)).await;

    let (winner_index, _) = convergence(&log);
    assert_eq!(winner_index, 0, "the earliest of the tied passes wins");
    assert_eq!(final_output(&log), json!({"pass": 1, "score": 5}));
}

/// `best_by` orders candidates by the EXPRESSION LANGUAGE's own comparison, so a
/// pass whose reference names something that language does not order (here an
/// object, and a pass with no `score` at all) cannot win, and the ordering
/// between the passes that can is the ordering `>` would give.
#[tokio::test]
async fn best_by_ignores_passes_whose_reference_is_not_comparable() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::BestBy("score".into()));
    // Pass 0 has no score at all, pass 1's score is an object (unordered under
    // the expression language), pass 2's is a plain number.
    let (log, _) = drive_fresh(
        &graph,
        vec![json!(null), json!({"nested": 9}), json!(2)],
        fixed_run_id(46),
    )
    .await;

    let (winner_index, _) = convergence(&log);
    assert_eq!(
        winner_index, 2,
        "the only pass with an orderable score wins, however small"
    );
}

/// A `best_by` join with no comparable candidate in ANY pass is a typed refusal,
/// returned BEFORE `FoldConverged` is recorded: no winner and no reason land in
/// the log for a convergence that did not happen, and the node never exits.
#[tokio::test]
async fn best_by_with_no_comparable_candidate_refuses_before_converging() {
    let graph = fold_only_graph(2, "score >= 99", FoldJoin::BestBy("score".into()));
    let run_id = fixed_run_id(47);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let (tools, calls) = worker_tools(vec![json!(null), json!({"nested": 9})]);
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");

    let error = run_graph(&mut ctx, &graph, &seed(), &no_agents(), &tools)
        .await
        .expect_err("an argmax with no candidate has no answer");
    match error {
        EngineError::FoldNoComparableCandidate { node, reference } => {
            assert_eq!(node, "refine");
            assert_eq!(reference, "score");
        }
        other => panic!("expected FoldNoComparableCandidate, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2, "both passes still ran");

    // The passes are recorded (they happened); the convergence is not.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered",
            "FoldIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "FoldIterationJoined",
            "FoldIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "FoldIterationJoined",
        ],
        "the refusal lands before FoldConverged, NodeExited, and the terminal"
    );
}

/// The `last` join produces the final pass's value and names it as the winner;
/// the `all` join produces every pass's value as a list in pass order, and reads
/// its winner as the pass the loop stopped at, since every pass contributed.
#[tokio::test]
async fn the_last_and_all_joins_produce_their_documented_outputs() {
    let scores = vec![json!(1), json!(5), json!(2)];

    let last = fold_only_graph(3, "score >= 99", FoldJoin::Last);
    let (log, _) = drive_fresh(&last, scores.clone(), fixed_run_id(48)).await;
    assert_eq!(convergence(&log).0, 2, "`last` names the final pass");
    assert_eq!(
        final_output(&log),
        json!({"pass": 3, "score": 2}),
        "`last` produces the final pass's value even though pass 1 scored higher"
    );

    let all = fold_only_graph(3, "score >= 99", FoldJoin::All);
    let (log, _) = drive_fresh(&all, scores, fixed_run_id(49)).await;
    assert_eq!(
        convergence(&log).0,
        2,
        "`all` reads its winner as the pass the loop stopped at"
    );
    assert_eq!(
        final_output(&log),
        json!([
            {"pass": 1, "score": 1},
            {"pass": 2, "score": 5},
            {"pass": 3, "score": 2},
        ]),
        "`all` produces every pass's value in pass order"
    );
}

// ---------------------------------------------------------------------------
// The two stop causes
// ---------------------------------------------------------------------------

/// The predicate firing stops the loop early: fewer passes run than the bound
/// allows, and the recorded reason names the predicate that fired.
#[tokio::test]
async fn stop_when_fires_early_and_records_the_predicate_reason() {
    let graph = fold_only_graph(5, "score >= 5", FoldJoin::Last);
    let (log, calls) = drive_fresh(
        &graph,
        vec![json!(1), json!(5), json!(2), json!(3), json!(4)],
        fixed_run_id(50),
    )
    .await;

    assert_eq!(calls, 2, "the loop stopped two passes into a bound of five");
    assert_eq!(event_kinds(&log), expected_kinds(2));
    let (winner_index, reason) = convergence(&log);
    assert_eq!(winner_index, 1);
    assert_eq!(reason, "stop_when `score >= 5` held after pass 1");
}

/// A predicate that never holds runs the loop to its bound, and the recorded
/// reason names the bound rather than the predicate.
#[tokio::test]
async fn the_bound_stops_the_loop_and_records_the_bound_reason() {
    let graph = fold_only_graph(3, "score >= 99", FoldJoin::Last);
    let (log, calls) =
        drive_fresh(&graph, vec![json!(1), json!(5), json!(2)], fixed_run_id(51)).await;

    assert_eq!(calls, 3, "every pass the bound allows ran");
    let (_, reason) = convergence(&log);
    assert_eq!(
        reason,
        "reached the max_iterations bound of 3 without stop_when `score >= 99` holding"
    );
}

// ---------------------------------------------------------------------------
// Parking inside a pass
// ---------------------------------------------------------------------------

/// A pass that parks records NO join, so the run resumes into the SAME pass: the
/// resume's input becomes that pass's output, the join for it is recorded then,
/// and the loop carries on. Two passes, each parking once, prove the loop does
/// not skip past a pass it never joined.
#[tokio::test]
async fn a_parked_pass_records_no_join_and_resumes_into_the_same_pass() {
    let graph = fold_only_graph(2, "score >= 99", FoldJoin::Last);
    let schema = json!({"type": "object"});
    let (approve, approve_calls) = SuspendingTool::new("refine_tool", "review this draft", schema);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("refine_tool".to_owned(), Box::new(approve));

    let run_id = fixed_run_id(52);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));

    // Drive one: pass 0 suspends and the run parks AT THE FOLD NODE.
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(&mut ctx, &graph, &seed(), &no_agents(), &tools)
        .await
        .expect("the drive parks");
    match &outcome {
        GraphOutcome::Parked {
            node,
            reason: ParkReason::Suspended { reason, .. },
        } => {
            assert_eq!(node, "refine", "the park names the fold, not its body");
            assert_eq!(reason, "review this draft");
        }
        other => panic!("expected a park inside the pass, got {other:?}"),
    }
    let parked = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&parked),
        [
            "GraphRunStarted",
            "NodeEntered",
            "FoldIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "Suspended",
        ],
        "the log ends inside pass 0, with no join recorded for it"
    );

    // Drive two: the resume input is pass 0's output, its join is recorded, and
    // pass 1 starts and parks in turn.
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        parked.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("resume ctx builds");
    ctx.set_resume_input(json!({"pass": 1, "score": 4}));
    let outcome = run_graph(&mut ctx, &graph, &seed(), &no_agents(), &tools)
        .await
        .expect("the resume drives");
    assert!(
        matches!(outcome, GraphOutcome::Parked { .. }),
        "pass 1 parks in its turn, got {outcome:?}"
    );

    // Drive three: pass 1's resume completes the loop.
    let log = store.read_log(run_id).await.expect("log reads");
    let mut ctx =
        RunCtx::with_hooks(store.clone(), run_id, log, fixed_clock(), fixed_random()).expect("ctx");
    ctx.set_resume_input(json!({"pass": 2, "score": 7}));
    let outcome = run_graph(&mut ctx, &graph, &seed(), &no_agents(), &tools)
        .await
        .expect("the second resume drives");
    assert!(matches!(outcome, GraphOutcome::Completed { .. }));

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered",
            "FoldIterationStarted", // 0
            "ToolCallRequested",
            "ToolCallCompleted",
            "Suspended",
            "Resumed",
            "FoldIterationJoined",  // 0, recorded only once the pass finished
            "FoldIterationStarted", // 1
            "ToolCallRequested",
            "ToolCallCompleted",
            "Suspended",
            "Resumed",
            "FoldIterationJoined", // 1
            "FoldConverged",
            "NodeExited",
            "RunCompleted",
        ],
        "each pass joined exactly once, after its own resume"
    );
    // One execution per pass, two passes: a resumed pass replays its recorded
    // suspension rather than calling the body again.
    assert_eq!(approve_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        final_output(&log),
        json!({"pass": 2, "score": 7}),
        "the resume input is the pass's output, and `last` produces it"
    );
}

// ---------------------------------------------------------------------------
// The refusals
// ---------------------------------------------------------------------------

/// A fold whose body is an embedded `subgraph` is a typed `UnsupportedFoldBody`,
/// refused BEFORE the fold's `NodeEntered`, so a lone such fold leaves only the
/// graph head in the log. (The node-body form ships; the subgraph body does not.)
#[tokio::test]
async fn a_fold_with_a_subgraph_body_refuses_with_only_the_head_recorded() {
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

    let run_id = fixed_run_id(53);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    let error = run_graph(
        &mut ctx,
        &graph,
        &json!({"score": 0.9}),
        &no_agents(),
        &tools,
    )
    .await
    .expect_err("a subgraph body must be refused");
    match error {
        EngineError::UnsupportedFoldBody { node, detail } => {
            assert_eq!(node, "refine");
            assert!(
                detail.contains("subgraph"),
                "the detail names the form: {detail}"
            );
        }
        other => panic!("expected UnsupportedFoldBody, got {other:?}"),
    }

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        ["GraphRunStarted"],
        "the refusal leaves only the head; no NodeEntered for the fold, no terminal"
    );
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            Event::NodeEntered { node } if node == "refine"
        )),
        "the fold must not have been entered"
    );
}

/// A fold whose body names a node that is neither an `agent` nor a `tool` is the
/// same typed refusal, also before the fold's `NodeEntered`.
#[tokio::test]
async fn a_fold_whose_body_is_not_an_agent_or_tool_refuses() {
    let graph = GraphBuilder::new()
        .fold(FoldSpec::new(
            "refine",
            FoldBody::Node("approve".into()),
            2,
            "score >= 1",
            FoldJoin::Last,
        ))
        .gate(GateSpec::new("approve", json!({"type": "object"})))
        .build();

    let run_id = fixed_run_id(54);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    let error = run_graph(&mut ctx, &graph, &seed(), &no_agents(), &tools)
        .await
        .expect_err("a gate cannot be a per-pass worker");
    match error {
        EngineError::UnsupportedFoldBody { node, detail } => {
            assert_eq!(node, "refine");
            assert!(
                detail.contains("gate"),
                "the detail names the kind: {detail}"
            );
        }
        other => panic!("expected UnsupportedFoldBody, got {other:?}"),
    }
    assert_eq!(
        event_kinds(&store.read_log(run_id).await.expect("log reads")),
        ["GraphRunStarted"]
    );
}

/// Forking INTO a fold pass is refused precisely: a pass is not a node boundary,
/// so the fold's body node (never framed with a `NodeEntered` of its own) cannot
/// be a fork point, while the fold node itself can.
#[tokio::test]
async fn forking_into_a_fold_pass_is_refused_but_the_fold_node_is_a_boundary() {
    let graph = fold_only_graph(2, "score >= 99", FoldJoin::Last);
    let (log, _) = drive_fresh(&graph, vec![json!(1), json!(2)], fixed_run_id(55)).await;

    let error = plan_fork(&log, "worker").expect_err("worker is not a node boundary");
    match error {
        ForkError::NodeNeverEntered { node } => assert_eq!(node, "worker"),
        other => panic!("expected NodeNeverEntered for the pass body, got {other:?}"),
    }
    let plan = plan_fork(&log, "refine").expect("refine is a real node boundary");
    assert_eq!(plan.from_node(), "refine");
}

// ---------------------------------------------------------------------------
// The committed fixture
// ---------------------------------------------------------------------------

/// The committed `fold-refine.json` fixture, the cross-language pin, validates
/// and now DRIVES under the engine: its `tailor` agent runs once per pass as the
/// fold's per-pass worker and is never walked as a node of its own (no
/// `NodeEntered` names it, and the model is called exactly once per pass rather
/// than four times).
///
/// The fixture's `best_by` join then refuses, and that is the honest reading of
/// what it asks for: a built-in `agent` node's output is the model's reply TEXT,
/// so no pass value carries the `score` the join names, and an argmax with no
/// comparable candidate has no answer. The loop itself is what this pins; the
/// refusal at the join is the documented boundary of what an agent body can
/// currently produce.
#[tokio::test]
async fn the_fold_refine_fixture_drives_without_double_running_its_body() {
    let path = format!(
        "{}/../../examples/graphs/fold-refine.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).expect("the fixture reads");
    let graph: Graph = serde_json::from_str(&text).expect("the fixture parses");
    assert!(
        validate(&graph).is_ok(),
        "the committed fixture validates clean"
    );

    let server = ScriptedModel::mount(vec![(1, text_response("a tailored draft", 5, 3))]).await;
    let mut agents: HashMap<String, Agent> = HashMap::new();
    agents.insert(
        TAILOR_HASH.to_owned(),
        agent_builder(&server.uri()).build().expect("agent builds"),
    );
    let tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();

    let run_id = fixed_run_id(56);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let error = run_graph(
        &mut ctx,
        &graph,
        &json!({"topic": "otters"}),
        &agents,
        &tools,
    )
    .await
    .expect_err("a text reply carries no `score` for the best_by join to order");
    assert!(
        matches!(error, EngineError::FoldNoComparableCandidate { .. }),
        "expected the argmax refusal, got {error:?}"
    );

    let log = store.read_log(run_id).await.expect("log reads");
    // Three passes, each one agent loop inline between the fold's markers.
    let mut kinds = vec!["GraphRunStarted", "NodeEntered"];
    for _ in 0..3 {
        kinds.extend([
            "FoldIterationStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "FoldIterationJoined",
        ]);
    }
    assert_eq!(event_kinds(&log), kinds);
    assert!(
        !log.iter().any(|env| matches!(
            &env.event,
            Event::NodeEntered { node } if node == "tailor"
        )),
        "the body agent is fold-owned: it is never entered as a node of its own"
    );
    assert_eq!(
        log.iter()
            .filter(|env| matches!(env.event, Event::ModelCallRequested { .. }))
            .count(),
        3,
        "one model call per pass: the body did not also run as a walked node"
    );
}

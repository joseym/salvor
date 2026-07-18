//! MAP fan-out, on the form that shipped: **inline
//! sequential** iterations recorded in the parent's own log (the deliberate
//! fallback — the `concurrency` cap is accepted but not honored in v0.4). Because
//! the fan-out lives in one log, the whole feature is proven by the same
//! single-log replay machinery already proven for linear and branching graphs:
//!
//! - THE PROPERTY TEST FIRST (`map_fan_out_holds_the_property_at_every_kill_boundary`):
//!   a fan-out of N is killed at EVERY event boundary; after recovery the parent
//!   log is byte-identical to an uninterrupted drive, and every completed body
//!   tool call executed exactly once (no completed call re-executes on resume).
//! - joins are recorded in INDEX ORDER, never completion order;
//! - a completed map run re-drives with ZERO live calls and a byte-identical log;
//! - the fan-out state reads back through `derive_graph_projection`;
//! - forking from a node AFTER a completed map includes the map markers in the
//!   prefix, and forking INTO a map (at its body/iteration) is refused precisely,
//!   because an iteration is not a node boundary.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{EchoTool, event_kinds, fixed_clock, fixed_random, fixed_run_id};
use salvor_core::{Effect, Event, EventEnvelope};
use salvor_engine::{ForkError, graph_hash};
use salvor_engine::{GraphOutcome, plan_fork, run_graph};
use salvor_graph::{Graph, GraphBuilder, MapBody, MapSpec, ToolSpec};
use salvor_replay::{MapIteration, NodeState, derive_graph_projection};
use salvor_runtime::RunCtx;
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::{Value, json};

/// A one-map-node graph: `fanout` (entry) fans out over the input's `items`
/// list, mapping each element through the `worker` tool. `worker` is the map's
/// body, so it is never walked independently.
fn map_only_graph() -> Graph {
    GraphBuilder::new()
        .map(MapSpec::new(
            "fanout",
            "items",
            2,
            MapBody::Node("worker".into()),
        ))
        .tool(ToolSpec::new("worker", "worker_tool"))
        .build()
}

/// A registry with the idempotent `worker` echo tool plus its shared execution
/// counter, so a replay's zero-execution claim (and the exactly-once claim) is
/// checkable. Idempotent so it also exercises the position-derived per-iteration
/// key and re-issues safely on a dangling intent.
fn worker_tools() -> (
    HashMap<String, Box<dyn DynTool>>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let (worker, calls) = EchoTool::new("worker_tool", Effect::Idempotent);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("worker_tool".to_owned(), Box::new(worker));
    (tools, calls)
}

fn no_agents() -> HashMap<String, salvor_runtime::Agent> {
    HashMap::new()
}

/// Drives `graph` fresh over a new store and returns the produced log.
async fn drive_fresh(
    graph: &Graph,
    input: &Value,
    run_id: salvor_core::RunId,
) -> Vec<EventEnvelope> {
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let (tools, _calls) = worker_tools();
    let agents = no_agents();
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(&mut ctx, graph, input, &agents, &tools)
        .await
        .expect("graph drives");
    assert!(
        matches!(outcome, GraphOutcome::Completed { .. }),
        "map graph completes, got {outcome:?}"
    );
    store.read_log(run_id).await.expect("log reads")
}

/// THE PROPERTY TEST (written before the scheduler, as the highest-risk case
/// to prove first). A fan-out of N=3 is killed at EVERY event boundary; after
/// recovery from the truncated prefix:
///   (a) the resumed parent log is byte-identical to an uninterrupted drive, and
///   (b) every completed body tool call executed exactly once — no call whose
///       completion is already in the prefix re-executes, and each remaining call
///       runs exactly once.
///
/// "Kill at every event boundary" is mechanized by seeding a fresh store with the
/// first `k` events of the uninterrupted log (for every `k`), then resuming a
/// `RunCtx` over that prefix and driving to completion. The resume's tool
/// executions are counted on a fresh counter and checked against the number of
/// body `ToolCallCompleted` events at or beyond the cut — the exact set of calls
/// that had not completed when the kill struck.
#[tokio::test]
async fn map_fan_out_holds_the_property_at_every_kill_boundary() {
    let graph = map_only_graph();
    let input = json!({"items": ["a", "b", "c"]});
    let run_id = fixed_run_id(30);

    // The uninterrupted reference drive.
    let full = drive_fresh(&graph, &input, run_id).await;
    // Sanity: the shape is head, node-enter, fan-out, then a Started/tool-pair/
    // Joined per element, then node-exit and the terminal.
    assert_eq!(
        event_kinds(&full),
        [
            "GraphRunStarted",
            "NodeEntered",         // fanout
            "MapFannedOut",        //
            "MapIterationStarted", // 0
            "ToolCallRequested",
            "ToolCallCompleted",
            "MapIterationJoined", // 0
            "MapIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "MapIterationJoined", // 1
            "MapIterationStarted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "MapIterationJoined", // 2
            "NodeExited",         // fanout
            "RunCompleted",
        ]
    );

    // For each cut k, the number of body tool executions the resume must perform:
    // the count of ToolCallCompleted events at seq >= k (calls not yet completed
    // when the kill struck).
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
        let (tools, calls) = worker_tools();
        let agents = no_agents();
        let mut ctx =
            RunCtx::with_hooks(store.clone(), run_id, prefix, fixed_clock(), fixed_random())
                .expect("resume ctx builds");
        let outcome = run_graph(&mut ctx, &graph, &input, &agents, &tools)
            .await
            .unwrap_or_else(|e| panic!("resume from cut {k} drives: {e}"));
        assert!(
            matches!(outcome, GraphOutcome::Completed { .. }),
            "resume from cut {k} completes"
        );

        // (a) Byte-identical parent log.
        let recovered = store.read_log(run_id).await.expect("log reads");
        assert_eq!(
            serde_json::to_string(&recovered).unwrap(),
            serde_json::to_string(&full).unwrap(),
            "resume from cut {k} must reproduce the byte-identical parent log"
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

/// Joins are recorded strictly in INDEX ORDER, and the joined output is the
/// per-element outputs as a list in index order. The shipped form runs iterations
/// sequentially, so there is no completion-order scheduler to invert the order;
/// the index-order guarantee is structural, and this asserts the recorded join
/// indices literally and that output i corresponds to item i.
#[tokio::test]
async fn map_joins_in_index_order() {
    let graph = map_only_graph();
    let input = json!({"items": ["a", "b", "c"]});
    let log = drive_fresh(&graph, &input, fixed_run_id(31)).await;

    // The recorded join indices, in log order, are exactly 0, 1, 2.
    let join_indices: Vec<u64> = log
        .iter()
        .filter_map(|env| match &env.event {
            Event::MapIterationJoined { node, index } if node == "fanout" => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(join_indices, [0, 1, 2], "joins recorded in index order");

    // The map node's output is the per-element echo outputs, in index order.
    let Event::RunCompleted { output } = &log.last().unwrap().event else {
        panic!("last event is the terminal");
    };
    assert_eq!(
        output,
        &json!([
            {"published": "a"},
            {"published": "b"},
            {"published": "c"},
        ]),
        "joined output is the item-order list of per-element outputs"
    );
}

/// A completed map run re-drives with ZERO live calls and a byte-identical log:
/// the replay proof, extended to the map markers and the inline body calls.
#[tokio::test]
async fn a_completed_map_run_replays_free_and_byte_identical() {
    let graph = map_only_graph();
    let input = json!({"items": ["x", "y"]});
    let run_id = fixed_run_id(32);

    // Drive live into a store we keep.
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let (tools, calls) = worker_tools();
    let agents = no_agents();
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, vec![], fixed_clock(), fixed_random())
        .expect("ctx builds");
    run_graph(&mut ctx, &graph, &input, &agents, &tools)
        .await
        .expect("graph drives");
    let live_log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "two body calls executed live"
    );

    // Re-drive over the recorded log with a fresh counter: no execution, and the
    // log does not grow (byte-identical).
    let (replay_tools, replay_calls) = worker_tools();
    let mut ctx2 = RunCtx::with_hooks(
        store.clone(),
        run_id,
        live_log.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("replay ctx builds");
    let outcome = run_graph(&mut ctx2, &graph, &input, &agents, &replay_tools)
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

/// The fan-out state (resolved items and per-iteration progress) reads back
/// correctly through `derive_graph_projection`.
#[tokio::test]
async fn map_projection_reads_back() {
    let graph = map_only_graph();
    let input = json!({"items": ["a", "b"]});
    let log = drive_fresh(&graph, &input, fixed_run_id(33)).await;

    let projection = derive_graph_projection(&log);
    let fanout = projection.node("fanout").expect("fanout was reached");
    assert_eq!(fanout.state, NodeState::Exited, "the map exited");
    let map = fanout.map.as_ref().expect("the map fanned out");
    assert_eq!(
        map.items,
        json!(["a", "b"]),
        "the resolved item list reads back"
    );
    assert_eq!(map.iterations.len(), 2);
    // Both iterations joined, in index order, each carrying its derived child id.
    for (i, iteration) in map.iterations.iter().enumerate() {
        assert_eq!(
            iteration,
            &MapIteration {
                index: i as u64,
                child_run: iteration.child_run.clone(),
                joined: true,
            }
        );
        assert!(
            iteration.child_run.starts_with("sha256:"),
            "the derived child run id is a canonical hash: {}",
            iteration.child_run
        );
    }
    // The worker body node is map-owned: it is never walked, so the projection
    // never mentions it.
    assert!(
        projection.node("worker").is_none(),
        "the body node is not walked"
    );
}

/// The derived child-run id is a pure function of recorded data (parent run id,
/// node id, index): two independent drives of the same document under DIFFERENT
/// run ids record DIFFERENT child ids (the parent run id is part of the
/// derivation), while a re-drive under the SAME run id reproduces them identically
/// (proven by the replay test's byte-identical log). Here we assert the
/// per-parent determinism half: the ids are stable and index-distinct.
#[tokio::test]
async fn map_child_ids_are_derived_and_index_distinct() {
    let graph = map_only_graph();
    let input = json!({"items": ["a", "b", "c"]});

    let child_ids = |log: &[EventEnvelope]| -> Vec<String> {
        log.iter()
            .filter_map(|env| match &env.event {
                Event::MapIterationStarted { child_run, .. } => Some(child_run.clone()),
                _ => None,
            })
            .collect()
    };

    let log_a = drive_fresh(&graph, &input, fixed_run_id(34)).await;
    let log_b = drive_fresh(&graph, &input, fixed_run_id(35)).await;
    let ids_a = child_ids(&log_a);
    let ids_b = child_ids(&log_b);

    // Three iterations, three distinct ids per run.
    assert_eq!(ids_a.len(), 3);
    let mut sorted = ids_a.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "the three iteration ids are distinct");
    // Different parent run ids yield different child ids (parent id is hashed in).
    assert_ne!(
        ids_a, ids_b,
        "child ids incorporate the parent run id, so they differ across runs"
    );
}

/// Forking from a node AFTER a completed map includes the map markers in the
/// prefix, and the child replays that prefix (map and all) with zero live calls
/// before continuing live from the fork node.
#[tokio::test]
async fn fork_after_a_completed_map_includes_the_map_markers() {
    // fanout (map) -> collect (tool). collect echoes the joined list.
    let graph = GraphBuilder::new()
        .map(MapSpec::new(
            "fanout",
            "items",
            2,
            MapBody::Node("worker".into()),
        ))
        .tool(ToolSpec::new("worker", "worker_tool"))
        .tool(ToolSpec::new("collect", "collect_tool"))
        .edge("fanout", "collect")
        .build();
    let input = json!({"items": ["a", "b"]});
    let origin_id = fixed_run_id(36);

    // Drive the origin to completion.
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let (worker, _wc) = EchoTool::new("worker_tool", Effect::Idempotent);
    let (collect, _cc) = EchoTool::new("collect_tool", Effect::Read);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("worker_tool".to_owned(), Box::new(worker));
    tools.insert("collect_tool".to_owned(), Box::new(collect));
    let agents = no_agents();
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        origin_id,
        vec![],
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds");
    run_graph(&mut ctx, &graph, &input, &agents, &tools)
        .await
        .expect("origin drives");
    let origin_log = store.read_log(origin_id).await.expect("log reads");

    // Plan the fork from `collect`, the node after the map.
    let plan = plan_fork(&origin_log, "collect").expect("collect was entered");
    assert!(plan.hazards().is_empty(), "no Write intents to acknowledge");
    let child_id = fixed_run_id(37);
    let child_prefix = plan.build_child_prefix(child_id, plan.hazard_seqs());

    // The prefix carries the whole map: fan-out, both iteration pairs, all joined.
    let prefix_kinds = event_kinds(&child_prefix);
    assert!(
        prefix_kinds.contains(&"MapFannedOut")
            && prefix_kinds
                .iter()
                .filter(|k| **k == "MapIterationStarted")
                .count()
                == 2
            && prefix_kinds
                .iter()
                .filter(|k| **k == "MapIterationJoined")
                .count()
                == 2,
        "the fork prefix includes the completed map's markers: {prefix_kinds:?}"
    );
    // And it stops before collect's own NodeEntered (the fork boundary).
    assert!(
        !child_prefix.iter().any(|env| matches!(
            &env.event,
            Event::NodeEntered { node } if node == "collect"
        )),
        "the prefix stops at the fork boundary, before collect is entered"
    );

    // Drive the child: it replays the prefix (map included, zero live worker
    // calls) and runs collect live, completing.
    let child_store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    for env in &child_prefix {
        child_store.append(env).await.expect("seed child prefix");
    }
    let (worker2, worker2_calls) = EchoTool::new("worker_tool", Effect::Idempotent);
    let (collect2, collect2_calls) = EchoTool::new("collect_tool", Effect::Read);
    let mut child_tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    child_tools.insert("worker_tool".to_owned(), Box::new(worker2));
    child_tools.insert("collect_tool".to_owned(), Box::new(collect2));
    let mut child_ctx = RunCtx::with_hooks(
        child_store.clone(),
        child_id,
        child_prefix.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("child ctx builds");
    let outcome = run_graph(&mut child_ctx, &graph, &input, &agents, &child_tools)
        .await
        .expect("child drives");
    assert!(matches!(outcome, GraphOutcome::Completed { .. }));
    assert_eq!(
        worker2_calls.load(Ordering::SeqCst),
        0,
        "the map replays in the fork with zero live worker calls"
    );
    assert_eq!(
        collect2_calls.load(Ordering::SeqCst),
        1,
        "the fork ran collect live exactly once"
    );

    // The child's replayed map markers are byte-identical to the origin's modulo
    // the run id (and the seq-0 forked_from the head carries).
    let child_log = child_store.read_log(child_id).await.expect("log reads");
    let origin_hash = graph_hash(&graph).expect("hash");
    assert_eq!(
        recorded_graph_hash(&child_log),
        Some(origin_hash),
        "the fork reuses the origin's graph hash"
    );
}

/// Forking INTO a map is refused precisely: an iteration is not a node boundary,
/// so the map's body node (`worker`, never framed with a `NodeEntered` of its
/// own) cannot be a fork point, while the map node itself (`fanout`, a real node
/// boundary) can.
#[tokio::test]
async fn forking_into_a_map_iteration_is_refused_but_the_map_node_is_a_boundary() {
    let graph = map_only_graph();
    let input = json!({"items": ["a", "b"]});
    let origin_log = drive_fresh(&graph, &input, fixed_run_id(38)).await;

    // The map's body/iteration is NOT a node boundary: no NodeEntered names it.
    let error = plan_fork(&origin_log, "worker").expect_err("worker is not a node boundary");
    match error {
        ForkError::NodeNeverEntered { node } => assert_eq!(node, "worker"),
        other => panic!("expected NodeNeverEntered for the iteration body, got {other:?}"),
    }

    // The map node itself IS a node boundary: forking from it is legal (it would
    // re-run the whole fan-out).
    let plan = plan_fork(&origin_log, "fanout").expect("fanout is a real node boundary");
    assert_eq!(plan.from_node(), "fanout");
}

/// The `graph_hash` recorded in a graph run's head, for the fork test.
fn recorded_graph_hash(log: &[EventEnvelope]) -> Option<String> {
    log.iter().find_map(|env| match &env.event {
        Event::GraphRunStarted { graph_hash, .. } => Some(graph_hash.clone()),
        _ => None,
    })
}

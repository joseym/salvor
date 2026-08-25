//! A graph `delay` node: the run parks on a durable timer, then the walk
//! continues.
//!
//! The delay is the timer counterpart of the gate. A gate waits for a person
//! and takes what they supply; a delay waits for an instant and takes nothing,
//! so its output is its input verbatim. Both park by entering the node and
//! leaving no `NodeExited` behind, and both need no event kind of their own:
//! the gate borrows `Suspended` / `Resumed`, the delay borrows `SleepStarted` /
//! `SleepCompleted`.
//!
//! What the tests below pin, in order: the recorded shape of a park and the
//! wake that ends it, that an early drive records nothing, that the node's
//! output is its input untouched, that a completed run replays free and byte
//! for byte, that the projection reads the park back, and the kill sweep, which
//! cuts the run at every event boundary and demands a byte-identical log and
//! exactly-once tool execution from each recovery.
//!
//! # Why the clock is set by hand rather than fixed
//!
//! Every other sweep in this directory drives on a constant clock, which is
//! what makes two logs comparable byte for byte. A timer cannot: the whole
//! point of the node is that a deadline arrives. So the clock here moves
//! exactly twice, and only ever between the two phases a delay run has: START
//! while the run reaches its sleep, `WAKE_AT` once it is asleep. Every live
//! clock READING happens in the first phase, so the recorded `NowObserved` and
//! the `wake_at` derived from it are the same on every drive, and a recovery
//! re-enters whichever phase its prefix left off in.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{EchoTool, TestClock, event_kinds, fixed_random, fixed_run_id};
use salvor_core::{Effect, Event, EventEnvelope, RunStatus, derive_state};
use salvor_engine::{GraphOutcome, run_graph};
use salvor_graph::{DelaySpec, Graph, GraphBuilder, ToolSpec};
use salvor_replay::{NodeState, ParkReason, derive_graph_projection};
use salvor_runtime::{Agent, RunCtx};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;

/// A registry mapping tool names to their implementations.
type ToolRegistry = HashMap<String, Box<dyn DynTool>>;

/// The instant the run starts at, and the instant a one-hour delay entered at
/// START wakes on. `WAKE_AT` is START plus [`WAIT_SECONDS`], written out rather
/// than computed so the test states the arithmetic the engine performs instead
/// of repeating it.
const START: OffsetDateTime = datetime!(2026-08-14 08:00:00 UTC);
const WAKE_AT: OffsetDateTime = datetime!(2026-08-14 09:00:00 UTC);

/// The wait the document declares. A duration, because a document is run more
/// than once; the instant is the engine's to derive.
const WAIT_SECONDS: u64 = 3600;

/// The graph input, and so also the delay's output: a delay transforms
/// nothing.
fn input() -> Value {
    json!({"order": "A-1"})
}

/// `assess` -> `cooloff` (delay) -> `publish`. The node before the delay is
/// what gives the delay a real input to pass through; the node after it is what
/// proves the walk continues rather than merely stopping politely.
fn delay_graph() -> Graph {
    GraphBuilder::new()
        .tool(ToolSpec::new("assess", "assess_tool"))
        .delay(DelaySpec::new("cooloff", WAIT_SECONDS).name("Cool off before publishing"))
        .tool(ToolSpec::new("publish", "publish_tool"))
        .edge("assess", "cooloff")
        .edge("cooloff", "publish")
        .build()
}

/// The two Read tools the graph resolves, sharing one execution counter each so
/// a replay's zero-execution claim is checkable.
fn tools() -> (ToolRegistry, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (assess, assess_calls) = EchoTool::new("assess_tool", Effect::Read);
    let (publish, publish_calls) = EchoTool::new("publish_tool", Effect::Read);
    let mut registry: ToolRegistry = HashMap::new();
    registry.insert("assess_tool".to_owned(), Box::new(assess));
    registry.insert("publish_tool".to_owned(), Box::new(publish));
    (registry, assess_calls, publish_calls)
}

/// No agent node in this graph, so the resolver is empty.
fn no_agents() -> HashMap<String, Agent> {
    HashMap::new()
}

/// The event sequence an uninterrupted run records, pinned so a change in the
/// delay's recorded shape is a visible diff rather than a silent one.
const EXPECTED_KINDS: [&str; 15] = [
    "GraphRunStarted",
    "NodeEntered", // assess
    "ToolCallRequested",
    "ToolCallCompleted",
    "NodeExited",  // assess
    "NodeEntered", // cooloff
    "NowObserved",
    "SleepStarted",
    "SleepCompleted",
    "NodeExited",  // cooloff
    "NodeEntered", // publish
    "ToolCallRequested",
    "ToolCallCompleted",
    "NodeExited", // publish
    "RunCompleted",
];

/// A delay node parks its run on a timer it derives from a recorded clock
/// reading, an early drive records nothing at all, and a drive at the deadline
/// wakes the node and walks on to the end. The node's output is its input
/// verbatim, and the finished run replays free and byte for byte.
#[tokio::test]
async fn a_delay_node_parks_on_its_own_timer_and_the_walk_continues() {
    let graph = delay_graph();
    let run_id = fixed_run_id(80);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let clock = TestClock::new(START);
    let (registry, assess_calls, publish_calls) = tools();

    // --- Drive 1: reaches the delay and parks. ---
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        vec![],
        clock.injected(),
        fixed_random(),
    )
    .expect("ctx builds");
    let outcome = run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
        .await
        .expect("the graph drives");
    match outcome {
        GraphOutcome::Parked {
            node,
            reason: ParkReason::Sleeping { wake_at },
        } => {
            assert_eq!(node, "cooloff", "the node the run is parked at");
            // THE DERIVATION: the document said "3600 seconds", and the engine
            // turned that into an instant from the reading it recorded.
            assert_eq!(wake_at, WAKE_AT, "an hour past the recorded reading");
        }
        other => panic!("expected a timer park, got {other:?}"),
    }

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        &EXPECTED_KINDS[..8],
        "no NodeExited: the node is still the one the run is in"
    );
    // The reading is recorded BEFORE the sleep derived from it, which is what
    // makes the wake instant a pure function of the log.
    assert!(
        matches!(log[6].event, Event::NowObserved { .. })
            && matches!(log[7].event, Event::SleepStarted { wake_at } if wake_at == WAKE_AT)
    );
    assert_eq!(
        derive_state(&log).status,
        RunStatus::Sleeping { wake_at: WAKE_AT }
    );

    // THE PROJECTION reads the park back: the run sits inside the node, which
    // is entered and never exited.
    let projection = derive_graph_projection(&log);
    assert_eq!(
        projection.current_node.as_deref(),
        Some("cooloff"),
        "the projection shows the run sitting inside the delay"
    );
    assert_eq!(
        projection
            .nodes
            .iter()
            .find(|node| node.node == "cooloff")
            .map(|node| node.state.clone()),
        Some(NodeState::Entered),
        "entered, never exited"
    );

    // --- Drive 2: still early. Records nothing, however often it is asked. ---
    clock.set(WAKE_AT - time::Duration::minutes(1));
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log.clone(),
        clock.injected(),
        fixed_random(),
    )
    .expect("ctx builds");
    let early = run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
        .await
        .expect("an early drive is not an error");
    assert!(
        matches!(
            early,
            GraphOutcome::Parked {
                reason: ParkReason::Sleeping { .. },
                ..
            }
        ),
        "still asleep: {early:?}"
    );
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        log,
        "an early drive appends nothing"
    );
    assert_eq!(
        assess_calls.load(Ordering::SeqCst),
        1,
        "and re-executes nothing"
    );
    assert_eq!(publish_calls.load(Ordering::SeqCst), 0);

    // --- Drive 3: at the deadline. The wake lands and the walk ends. ---
    clock.set(WAKE_AT);
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, log, clock.injected(), fixed_random())
        .expect("ctx builds");
    let outcome = run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
        .await
        .expect("the woken graph drives");
    let GraphOutcome::Completed { output } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };
    // THE PASS-THROUGH, read from the far end: `publish` echoes whatever it was
    // handed, so the final output shows the delay handed on exactly what
    // `assess` produced.
    assert_eq!(
        output,
        json!({"published": {"published": {"order": "A-1"}}}),
        "the delay's output is its input verbatim"
    );

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(event_kinds(&log), EXPECTED_KINDS);
    assert_eq!(assess_calls.load(Ordering::SeqCst), 1);
    assert_eq!(publish_calls.load(Ordering::SeqCst), 1);

    // --- Drive 4: a full replay over the finished log executes nothing and
    // appends nothing, the wake included. ---
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log.clone(),
        clock.injected(),
        fixed_random(),
    )
    .expect("ctx builds");
    let replayed = run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
        .await
        .expect("the replay is divergence free");
    assert!(matches!(replayed, GraphOutcome::Completed { .. }));
    assert_eq!(
        store.read_log(run_id).await.expect("log reads"),
        log,
        "a replay appends nothing"
    );
    assert_eq!(assess_calls.load(Ordering::SeqCst), 1);
    assert_eq!(publish_calls.load(Ordering::SeqCst), 1);
}

/// The delay node's own recorded output is the value that reached it, not a
/// value derived from the timer. Read straight off the edge rather than through
/// the tool downstream: `assess`'s completion and `publish`'s intent carry the
/// same JSON, which is only true if the node between them changed nothing.
#[tokio::test]
async fn a_delay_hands_on_exactly_what_reached_it() {
    let log = uninterrupted_run(fixed_run_id(81)).await;

    let assessed = log
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolCallCompleted { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("assess completed");
    let published = log
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::ToolCallRequested { input, .. } => Some(input.clone()),
            _ => None,
        })
        .nth(1)
        .expect("publish was called");
    assert_eq!(
        published, assessed,
        "the delay passed the upstream output through untouched"
    );
}

/// THE KILL SWEEP. The run is cut at EVERY event boundary; after recovery from
/// the truncated prefix the log is byte-identical to an uninterrupted drive and
/// every tool call executed exactly once. The sibling of `fold_graph.rs`'s and
/// `map_graph.rs`'s sweeps, with one addition a timer forces: a recovery has to
/// re-enter the right clock phase.
///
/// A cut that lands inside the delay (between its `NodeEntered` and its
/// `SleepCompleted`) is the interesting one, and there are four of them. Each
/// leaves a log with no `NodeExited` for the node, so the recovery re-enters
/// the SAME node, replays or re-records the sleep, and waits again. That is why
/// each such cut is driven twice: once with the clock still at START, which
/// must park and append nothing beyond the sleep, and once at the deadline,
/// which must finish. A cut past the wake is already in the second phase and is
/// driven once, from `WAKE_AT`.
#[tokio::test]
async fn a_delay_run_holds_the_property_at_every_kill_boundary() {
    let graph = delay_graph();
    let run_id = fixed_run_id(82);
    let control = uninterrupted_run(run_id).await;
    assert_eq!(event_kinds(&control), EXPECTED_KINDS);

    // The boundary that separates the two clock phases: the prefix has the
    // wake in it exactly when it is longer than the wake's own position.
    let woke_at_index = control
        .iter()
        .position(|envelope| matches!(envelope.event, Event::SleepCompleted {}))
        .expect("the control run woke");

    // Tool executions a recovery from prefix length `k` must perform: the calls
    // whose completion is not yet in the prefix. A dangling Read intent counts,
    // because a Read re-executes on recovery.
    let completions_at_or_after = |k: usize| -> usize {
        control
            .iter()
            .filter(|envelope| (envelope.seq.get() as usize) >= k)
            .filter(|envelope| matches!(envelope.event, Event::ToolCallCompleted { .. }))
            .count()
    };

    for k in 0..=control.len() {
        // A fresh store holding the first `k` events: exactly what a `kill -9`
        // at that boundary leaves on disk.
        let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
        for envelope in &control[..k] {
            store.append(envelope).await.expect("seed append");
        }
        let prefix: Vec<EventEnvelope> = control[..k].to_vec();

        let (registry, assess_calls, publish_calls) = tools();
        let asleep_in_prefix = k > woke_at_index;
        let clock = TestClock::new(if asleep_in_prefix { WAKE_AT } else { START });

        // Phase one. A prefix that has not yet woken must park here, at the
        // delay, having appended nothing past its sleep.
        let mut ctx = RunCtx::with_hooks(
            store.clone(),
            run_id,
            prefix.clone(),
            clock.injected(),
            fixed_random(),
        )
        .expect("resume ctx builds");
        let outcome = run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
            .await
            .unwrap_or_else(|error| panic!("resume from cut {k} drives: {error}"));
        if asleep_in_prefix {
            assert!(
                matches!(outcome, GraphOutcome::Completed { .. }),
                "resume from cut {k}, already past the wake, completes in one drive"
            );
        } else {
            assert!(
                matches!(
                    outcome,
                    GraphOutcome::Parked {
                        reason: ParkReason::Sleeping { .. },
                        ..
                    }
                ),
                "resume from cut {k} must park on the delay, got {outcome:?}"
            );
            // Phase two: the deadline arrives and the run finishes.
            clock.set(WAKE_AT);
            let log = store.read_log(run_id).await.expect("log reads");
            let mut ctx =
                RunCtx::with_hooks(store.clone(), run_id, log, clock.injected(), fixed_random())
                    .expect("woken ctx builds");
            let outcome = run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
                .await
                .unwrap_or_else(|error| panic!("cut {k} does not finish after the wake: {error}"));
            assert!(
                matches!(outcome, GraphOutcome::Completed { .. }),
                "resume from cut {k} completes once the deadline passes"
            );
        }

        // (a) The recovered log is byte-identical to the uninterrupted one.
        let recovered = store.read_log(run_id).await.expect("log reads");
        assert_eq!(
            serde_json::to_string(&recovered).expect("serialize"),
            serde_json::to_string(&control).expect("serialize"),
            "resume from cut {k} must reproduce the byte-identical log"
        );

        // (b) Exactly-once: the recovery ran precisely the calls the prefix had
        // not yet completed, and nothing already completed re-ran.
        assert_eq!(
            assess_calls.load(Ordering::SeqCst) + publish_calls.load(Ordering::SeqCst),
            completions_at_or_after(k),
            "resume from cut {k} executed exactly the not-yet-completed calls"
        );
    }
}

/// One uninterrupted run of [`delay_graph`], driven through both clock phases,
/// returning its whole log: the oracle the sweep compares against.
async fn uninterrupted_run(run_id: salvor_core::RunId) -> Vec<EventEnvelope> {
    let graph = delay_graph();
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let clock = TestClock::new(START);
    let (registry, _, _) = tools();

    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        vec![],
        clock.injected(),
        fixed_random(),
    )
    .expect("ctx builds");
    run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
        .await
        .expect("the graph parks");

    clock.set(WAKE_AT);
    let log = store.read_log(run_id).await.expect("log reads");
    let mut ctx = RunCtx::with_hooks(store.clone(), run_id, log, clock.injected(), fixed_random())
        .expect("ctx builds");
    run_graph(&mut ctx, &graph, &input(), &no_agents(), &registry)
        .await
        .expect("the woken graph completes");

    store.read_log(run_id).await.expect("log reads")
}

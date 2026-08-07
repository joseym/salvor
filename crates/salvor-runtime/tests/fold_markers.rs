//! The fold markers on the public context: `fold_iteration_started`,
//! `fold_iteration_joined`, and `fold_converged` persist exactly the events a
//! fold node's loop narrates, and re-driving the same orchestration over the
//! recorded log appends nothing.
//!
//! The passes run inline in the one log (a fold never fans out into child
//! runs), so the markers are ordinary appends at contiguous positions, and the
//! replay proof is the whole point: a resumed fold must not run a pass twice.

mod common;

use std::sync::Arc;

use common::{event_kinds, fixed_clock, fixed_random, fixed_run_id};
use salvor_core::{RunStatus, derive_state};
use salvor_runtime::{RunCtx, RuntimeError};
use salvor_store::{EventStore, SqliteStore};
use serde_json::json;

/// A fold node that runs two passes and settles on the second, written
/// against nothing but the public context surface. Run it once to record;
/// run it again over the recorded log to replay.
async fn refine_twice(ctx: &mut RunCtx) -> Result<(), RuntimeError> {
    ctx.begin_graph("sha256:graph-fold-v1", &json!({"draft": "otters"}))
        .await?;
    ctx.node_entered("refine").await?;
    for index in 0..2 {
        ctx.fold_iteration_started("refine", index).await?;
        ctx.fold_iteration_joined("refine", index).await?;
    }
    ctx.fold_converged("refine", 1, "stop predicate fired")
        .await?;
    ctx.node_exited("refine").await?;
    ctx.complete_run(&json!({"text": "second pass"})).await
}

#[tokio::test]
async fn the_fold_markers_persist_once_and_replay_free() {
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(60);

    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        Vec::new(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds over an empty log");
    refine_twice(&mut ctx)
        .await
        .expect("the first drive records");
    drop(ctx);

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "GraphRunStarted",
            "NodeEntered",
            "FoldIterationStarted",
            "FoldIterationJoined",
            "FoldIterationStarted",
            "FoldIterationJoined",
            "FoldConverged",
            "NodeExited",
            "RunCompleted",
        ]
    );
    assert!(matches!(
        derive_state(&log).status,
        RunStatus::Completed { .. }
    ));

    // A fresh context over the recorded log, re-driving the same
    // orchestration: every marker is answered from history, so the stored log
    // is untouched.
    let mut ctx = RunCtx::with_hooks(
        store.clone(),
        run_id,
        log.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("ctx builds over the recorded log");
    refine_twice(&mut ctx)
        .await
        .expect("the replay drive succeeds");
    assert!(!ctx.is_replaying(), "history fully consumed");

    let replayed = store.read_log(run_id).await.expect("log reads");
    assert_eq!(replayed, log, "a replayed fold appends nothing");
}

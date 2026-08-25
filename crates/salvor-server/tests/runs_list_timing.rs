//! Timing sanity for the enriched `GET /v1/runs`: the fold already ran once
//! per run for `status` before this change: this test measures whether
//! adding `usage`/`step_count`/`agent_def_hash` (all derived from the log
//! and derived state already in hand, no second store read) moves the needle
//! on a store of a realistic size (dozens of runs, ~20 events each), scaled
//! up for a stable signal.
//!
//! Run with `cargo test -p salvor-server --test runs_list_timing --
//! --ignored --nocapture` to see the printed numbers; it is `#[ignore]`d so
//! the normal `cargo test --workspace` gate stays fast, per repo convention
//! for anything that seeds hundreds of runs.

mod common;

use std::time::{Duration, Instant};

use common::memory_store;
use salvor_core::{Event, EventEnvelope, RunId, SequenceNumber, TokenUsage, derive_state};
use salvor_server::AppState;
use salvor_store::EventStore;
use serde_json::json;
use time::OffsetDateTime;
use time::macros::datetime;

const RUNS: usize = 300;
const MODEL_CALLS_PER_RUN: usize = 9;

/// Appends a synthetic completed run's log directly to `store`: `RunStarted`,
/// `MODEL_CALLS_PER_RUN` request/completion pairs, then `RunCompleted` --
/// the same event shape a real run produces, built directly rather than
/// through a live agent so seeding hundreds of runs is fast and hermetic.
async fn seed_run(store: &dyn EventStore, agent_def_hash: &str) {
    let run_id = RunId::new();
    let base_time: OffsetDateTime = datetime!(2026-07-10 12:00:00 UTC);
    let mut seq = 0u64;

    let envelope = EventEnvelope::new(
        run_id,
        SequenceNumber::new(seq),
        base_time,
        Event::RunStarted {
            agent_def_hash: agent_def_hash.to_owned(),
            input: json!({"topic": "durable execution"}),
            labels: None,
            driven_by: None,
        },
    );
    store.append(&envelope).await.expect("append RunStarted");
    seq += 1;

    for i in 0..MODEL_CALLS_PER_RUN {
        let call_seq = SequenceNumber::new(seq);
        let envelope = EventEnvelope::new(
            run_id,
            call_seq,
            base_time,
            Event::ModelCallRequested {
                seq: call_seq,
                request_hash: format!("sha256:req{i}"),
                request_body: None,
                performed_by: None,
            },
        );
        store.append(&envelope).await.expect("append request");
        seq += 1;

        let envelope = EventEnvelope::new(
            run_id,
            SequenceNumber::new(seq),
            base_time,
            Event::ModelCallCompleted {
                seq: call_seq,
                response: json!({"text": format!("turn {i}")}),
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                },
            },
        );
        store.append(&envelope).await.expect("append completion");
        seq += 1;
    }

    let envelope = EventEnvelope::new(
        run_id,
        SequenceNumber::new(seq),
        base_time,
        Event::RunCompleted {
            output: json!({"summary": "done"}),
        },
    );
    store.append(&envelope).await.expect("append RunCompleted");
}

/// The pre-enrichment `list` body verbatim (git history: `runs.rs` before
/// this change) -- `status`/`event_count`/timestamps only, one read and one
/// fold per run, no `usage`/`step_count`/`agent_def_hash`. Kept here, not in
/// production code, purely as this test's timing baseline.
async fn list_pre_enrichment(store: &dyn EventStore) -> Duration {
    let start = Instant::now();
    let summaries = store.list_runs().await.expect("list_runs");
    let mut runs = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let log = store.read_log(summary.run_id).await.expect("read_log");
        let state = derive_state(&log);
        runs.push(json!({
            "run": summary.run_id.as_uuid().to_string(),
            "status": format!("{:?}", state.status),
            "event_count": summary.event_count,
        }));
    }
    assert_eq!(runs.len(), RUNS);
    start.elapsed()
}

#[tokio::test]
#[ignore = "seeds hundreds of runs; run explicitly with --ignored --nocapture for the timing report"]
async fn enriched_list_does_not_slow_the_seeded_store_pathologically() {
    let store = memory_store();
    for _ in 0..RUNS {
        seed_run(store.as_ref(), "sha256:bench-agent").await;
    }

    // Warm the OS/allocator caches with one untimed pass of each path before
    // measuring, then take the best of several timed runs to damp scheduler
    // noise, same idea as any micro-benchmark over shared CI hardware.
    let _ = list_pre_enrichment(store.as_ref()).await;

    let mut before = Duration::MAX;
    for _ in 0..5 {
        before = before.min(list_pre_enrichment(store.as_ref()).await);
    }

    let state = AppState::new(
        store.clone(),
        std::sync::Arc::new(|_definition| {
            Box::pin(async { Err::<salvor_server::BuiltAgent, String>("unused".to_owned()) })
        }),
    );
    // Warm-up.
    let _ = salvor_server::runs::list(axum::extract::State(state.clone()))
        .await
        .expect("list");

    let mut after = Duration::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let response = salvor_server::runs::list(axum::extract::State(state.clone()))
            .await
            .expect("list");
        let elapsed = started.elapsed();
        // Touch the body so the compiler cannot elide the work being timed.
        let _ = axum::response::IntoResponse::into_response(response);
        after = after.min(elapsed);
    }

    let delta = after.saturating_sub(before);
    let pct = if before.as_secs_f64() > 0.0 {
        100.0 * delta.as_secs_f64() / before.as_secs_f64()
    } else {
        0.0
    };
    println!(
        "[runs_list_timing] {RUNS} runs x {MODEL_CALLS_PER_RUN} model calls: \
         before={before:?} after={after:?} delta={delta:?} ({pct:.1}%)"
    );

    // A generous ceiling, not a tight budget: the point is "not pathological"
    // (a second fold pass, an N+1 store hit, or similar), not a fixed
    // percentage. Reusing the already-loaded log for the new fields should
    // cost low-single-digit percent at most on this shape.
    assert!(
        after < before * 3,
        "enriched list should stay within a small constant factor of the pre-enrichment fold, \
         got before={before:?} after={after:?}"
    );
}

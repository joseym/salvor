//! The facade's own acceptance: that the names it promises are reachable through it, and that they
//! compose without naming a single `salvor-*` crate.
//!
//! A re-export crate fails quietly. It compiles, publishes, and only breaks when someone follows
//! the documentation and finds a path missing, so this test is written the way a user writes code:
//! every import comes from `salvor`, never from a sibling.

use salvor::prelude::*;

/// The store is the one piece with real IO behind it, so exercising it proves the re-export reaches
/// the implementation rather than a lookalike. `EventStore` and `RunId` arrive from two different
/// crates, and the call only type-checks if the facade lands both on the same types.
#[tokio::test]
async fn the_prelude_opens_a_real_store_and_reads_back_an_unknown_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteStore::open(dir.path().join("facade.db")).expect("open the store");

    let run = RunId::new();
    let log = store.read_log(run).await.expect("read an unknown run");
    assert!(
        log.is_empty(),
        "a run with no appended events reads back empty, got {} events",
        log.len()
    );
}

/// The fold is the durability engine's centre. It arrives through `core` while the envelope type
/// arrives through the same module, and an empty log has exactly one honest answer: the run was
/// never started, which is distinct from a run that started and did nothing.
#[test]
fn the_fold_is_reachable_and_calls_an_empty_log_not_started() {
    let empty: Vec<EventEnvelope> = Vec::new();
    let state: RunState = derive_state(&empty);
    assert!(
        matches!(state.status, RunStatus::NotStarted),
        "an empty log folds to NotStarted, got {:?}",
        state.status
    );
}

/// The documented module layout is part of the contract: a reorganisation that quietly moves
/// `salvor::runtime` breaks every doc example at once, and nothing else in the workspace would
/// notice. Named rather than called, since existence is the whole claim here.
#[test]
fn the_documented_module_paths_resolve() {
    let _: fn() -> salvor::runtime::AgentBuilder = salvor::runtime::Agent::builder;
    let _: salvor::tools::RetryPolicy = salvor::tools::RetryPolicy::for_effect(Effect::Read);
    let _: u32 = salvor::core::SCHEMA_VERSION;
}

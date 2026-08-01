//! The fork contract at the engine level: refuse-then-record's mechanics
//! proven end to end over `run_graph` and [`plan_fork`], with full control of
//! both logs.
//!
//! The fixture is `research (agent) -> notify (idempotent tool) -> publish
//! (write tool)`. Forking from `notify` re-walks an idempotent call AND a write,
//! which lets one drive prove:
//!
//! - the hazard set is exactly the downstream write (B);
//! - the child's prefix is byte-identical to the origin's modulo run id and the
//!   seq-0 fork origin (E2);
//! - the write executes exactly once in the child (E3);
//! - the ORIGIN's log is unchanged before and after (E4);
//! - the child log replays standalone with zero live calls (E5);
//! - the re-walked idempotent call carries the SAME recorded key in child and
//!   parent, because the key is derived from graph position, not randomness (C).

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{
    EchoTool, ScriptedModel, agent_builder, fixed_clock, fixed_random, fixed_run_id, text_response,
};
use salvor_core::{Effect, Event, EventEnvelope};
use salvor_engine::{graph_hash, plan_fork, run_graph};
use salvor_graph::{AgentSpec, Graph, GraphBuilder, ToolSpec};
use salvor_replay::ReplayCursor;
use salvor_runtime::RunCtx;
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::DynTool;
use serde_json::{Value, json};

const RESEARCH_HASH: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";

/// research (agent) -> notify (idempotent tool) -> publish (write tool).
fn fork_fixture() -> Graph {
    GraphBuilder::new()
        .agent(AgentSpec::new("research", RESEARCH_HASH))
        .tool(ToolSpec::new("notify", "notify_tool"))
        .tool(ToolSpec::new("publish", "http_post"))
        .edge("research", "notify")
        .edge("notify", "publish")
        .build()
}

/// The idempotency key of the first `notify_tool` intent in a log, if any.
fn notify_key(log: &[EventEnvelope]) -> Option<String> {
    log.iter().find_map(|envelope| match &envelope.event {
        Event::ToolCallRequested {
            tool,
            idempotency_key,
            ..
        } if tool == "notify_tool" => idempotency_key.clone(),
        _ => None,
    })
}

#[tokio::test]
async fn fork_records_the_acknowledgement_and_the_child_replays_standalone() {
    let research_server =
        ScriptedModel::mount(vec![(1, text_response("a draft about otters", 5, 3))]).await;
    let research_agent = agent_builder(&research_server.uri()).build().unwrap();
    let mut agents: HashMap<String, salvor_runtime::Agent> = HashMap::new();
    agents.insert(RESEARCH_HASH.to_owned(), research_agent);

    let (notify, notify_calls) = EchoTool::new("notify_tool", Effect::Idempotent);
    let (publish, publish_calls) = EchoTool::new("http_post", Effect::Write);
    let mut tools: HashMap<String, Box<dyn DynTool>> = HashMap::new();
    tools.insert("notify_tool".to_owned(), Box::new(notify));
    tools.insert("http_post".to_owned(), Box::new(publish));

    let graph = fork_fixture();
    let input = json!({"topic": "otters"});
    let origin_id = fixed_run_id(30);
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));

    // --- Origin: drive to completion. ---
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

    let origin_log = store.read_log(origin_id).await.expect("origin log reads");
    let origin_bytes_before = serde_json::to_string(&origin_log).unwrap();
    let origin_len_before = origin_log.len();
    assert_eq!(
        notify_calls.load(Ordering::SeqCst),
        1,
        "notify ran once live"
    );
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        1,
        "publish ran once live"
    );
    let parent_notify_key = notify_key(&origin_log).expect("notify recorded a key");
    assert!(parent_notify_key.starts_with("sha256:"));

    // --- Plan the fork from `notify`. The hazard is exactly the write. ---
    let plan = plan_fork(&origin_log, "notify").expect("notify was entered");
    let hazards = plan.hazards();
    assert_eq!(hazards.len(), 1, "one write in the re-walked segment");
    assert_eq!(hazards[0].tool, "http_post");
    let publish_seq = origin_log
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolCallRequested { tool, .. } if tool == "http_post" => Some(e.seq.get()),
            _ => None,
        })
        .expect("origin recorded the publish intent");
    assert_eq!(plan.hazard_seqs(), vec![publish_seq]);

    // --- Build the child prefix, acknowledging the write, and write it. ---
    let child_id = fixed_run_id(31);
    let child_prefix = plan.build_child_prefix(child_id, plan.hazard_seqs());
    for envelope in &child_prefix {
        store.append(envelope).await.expect("child prefix appends");
    }

    // Acceptance E2: byte-identical prefix modulo run id and the seq-0 fork
    // origin, constructed explicitly.
    let rewrite_blank = |envelopes: &[EventEnvelope]| -> String {
        let mut copy: Vec<EventEnvelope> = envelopes.to_vec();
        for env in &mut copy {
            env.run_id = child_id;
        }
        if let Event::GraphRunStarted { forked_from, .. } = &mut copy[0].event {
            *forked_from = None;
        }
        serde_json::to_string(&copy).unwrap()
    };
    let parent_prefix = &origin_log[..plan.prefix_len()];
    assert_eq!(
        rewrite_blank(parent_prefix),
        rewrite_blank(&child_prefix),
        "the child prefix is byte-identical to the parent's modulo run id and forked_from"
    );

    // The recorded fork origin carries the acknowledged write permanently.
    match &child_prefix[0].event {
        Event::GraphRunStarted { forked_from, .. } => {
            let origin = forked_from
                .as_ref()
                .expect("child head carries the fork origin");
            assert_eq!(origin.run_id, origin_id);
            assert_eq!(origin.from_node, "notify");
            assert_eq!(origin.acknowledged_writes, vec![publish_seq]);
        }
        other => panic!("child head is not GraphRunStarted: {other:?}"),
    }

    // --- Drive the child from the fork node (recover: no resume input). ---
    let child_start_log = store.read_log(child_id).await.expect("child log reads");
    let notify_before = notify_calls.load(Ordering::SeqCst);
    let publish_before = publish_calls.load(Ordering::SeqCst);
    let research_reqs_before = research_server.received_requests().await.unwrap().len();

    let mut ctx2 = RunCtx::with_hooks(
        store.clone(),
        child_id,
        child_start_log,
        fixed_clock(),
        fixed_random(),
    )
    .expect("child ctx builds");
    run_graph(&mut ctx2, &graph, &Value::Null, &agents, &tools)
        .await
        .expect("child drives from the fork node");

    // Acceptance E3: the write executed EXACTLY once in the child.
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        publish_before + 1,
        "the downstream write executes exactly once in the child"
    );
    // The idempotent notify re-executed (allowed), but under the SAME key.
    assert_eq!(notify_calls.load(Ordering::SeqCst), notify_before + 1);
    // research replayed from the prefix: no new model request.
    assert_eq!(
        research_server.received_requests().await.unwrap().len(),
        research_reqs_before,
        "the replayed agent segment makes no live model call"
    );

    // Acceptance E4: the ORIGIN is unchanged, before and after the fork.
    let origin_after = store.read_log(origin_id).await.expect("origin log reads");
    assert_eq!(
        origin_after.len(),
        origin_len_before,
        "origin event count is identical"
    );
    assert_eq!(
        serde_json::to_string(&origin_after).unwrap(),
        origin_bytes_before,
        "origin log is byte-identical before and after the fork"
    );

    // Acceptance C: the re-walked idempotent call carries the SAME key as the
    // parent's, because it is derived from graph position, not randomness.
    let child_log = store.read_log(child_id).await.expect("child log reads");
    let child_notify_key = notify_key(&child_log).expect("child notify recorded a key");
    assert_eq!(
        child_notify_key, parent_notify_key,
        "the fork's idempotent call carries the same recorded key as the parent"
    );

    // The child recorded the same graph hash it forked under (reused unchanged).
    match &child_log[0].event {
        Event::GraphRunStarted { graph_hash: h, .. } => {
            assert_eq!(h, &graph_hash(&graph).unwrap());
        }
        other => panic!("child head is not GraphRunStarted: {other:?}"),
    }

    // Acceptance E5: the child log replays standalone: a fresh cursor over the
    // child log alone accepts it, and a re-drive makes zero live calls and
    // produces a byte-identical log.
    ReplayCursor::new(child_log.clone()).expect("the child log is a legal standalone run log");
    let child_bytes = serde_json::to_string(&child_log).unwrap();
    let notify_before2 = notify_calls.load(Ordering::SeqCst);
    let publish_before2 = publish_calls.load(Ordering::SeqCst);
    let research_reqs_before2 = research_server.received_requests().await.unwrap().len();

    let mut ctx3 = RunCtx::with_hooks(
        store.clone(),
        child_id,
        child_log.clone(),
        fixed_clock(),
        fixed_random(),
    )
    .expect("child replay ctx builds");
    run_graph(&mut ctx3, &graph, &Value::Null, &agents, &tools)
        .await
        .expect("child replays");
    assert!(!ctx3.is_replaying(), "child history fully consumed");
    assert_eq!(
        notify_calls.load(Ordering::SeqCst),
        notify_before2,
        "no notify on replay"
    );
    assert_eq!(
        publish_calls.load(Ordering::SeqCst),
        publish_before2,
        "no write on replay"
    );
    assert_eq!(
        research_server.received_requests().await.unwrap().len(),
        research_reqs_before2,
        "no model call on replay"
    );
    let child_after = store.read_log(child_id).await.expect("child log reads");
    assert_eq!(
        serde_json::to_string(&child_after).unwrap(),
        child_bytes,
        "the child replay produced a byte-identical log"
    );
}

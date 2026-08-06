//! Structured output: the built-in loop under a declared output schema, where
//! the final answer arrives as a forced call to the synthetic `salvor_answer`
//! tool and the loop's output is that call's validated input.
//!
//! What these tests pin:
//!
//! - the request shape: the answer tool carries the DECLARED schema as its
//!   `input_schema`, and `tool_choice` is `any`, so no bare-text terminal turn
//!   is possible by API contract;
//! - the accept edge: a lone answer call whose input validates ends the loop
//!   with that input verbatim;
//! - every other shape of turn feeds back and the loop asks again, with fixed
//!   templates and this repo's OWN validator verdict as the only variable text
//!   (a violation, an answer beside real tool work, a turn with no call at all);
//! - repeats collapse through the same [`FailureTracker`] streak counter tool
//!   failures use;
//! - the steps budget is the only bound on re-asking: a crossing mid-re-ask
//!   parks exactly like any other crossing and a resume continues;
//! - the property that matters most: a structured run killed at EVERY event
//!   boundary recovers to a byte-identical log, because everything fed back is
//!   a pure function of recorded data;
//! - an agent that already offers a real `salvor_answer` tool is a typed
//!   refusal recorded nowhere, raised before the first model call;
//! - the declaration can live on the agent instead of the call site, and then
//!   `Runtime::start` reaches the structured loop on its own, while an agent
//!   that declares nothing stays on the plain one.
//!
//! [`FailureTracker`]: salvor_runtime::FailureTracker

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use async_trait::async_trait;
use common::{
    ScriptedModel, TestTool, ToolBehavior, agent_builder, blocks_response, event_kinds,
    fixed_clock, fixed_random, fixed_run_id, text_response, tool_result_contents, tool_use_block,
    tool_use_response,
};
use salvor_core::{Effect, Event, EventEnvelope, RunId};
use salvor_runtime::{
    ANSWER_TOOL, Agent, Budgets, LoopOutcome, ParkReason, RunCtx, RunOutcome, Runtime,
    RuntimeError, drive_loop_structured,
};
use salvor_store::{EventStore, RunSummary, SqliteStore, StoreError};
use serde_json::{Value, json};
use wiremock::MockServer;

/// The declared output schema every test here drives under: an object with a
/// required numeric `score`, which the structural validator's `type` and
/// `required` keywords are enough to police.
fn schema() -> Value {
    json!({
        "type": "object",
        "required": ["score"],
        "properties": {"score": {"type": "number"}, "note": {"type": "string"}}
    })
}

/// A response whose only block is a call to the answer tool.
fn answer_response(tool_use_id: &str, input: Value, output_tokens: u64) -> Value {
    tool_use_response(tool_use_id, ANSWER_TOOL, input, 10, output_tokens)
}

/// One drive of the structured loop over whatever history the store already
/// holds: begin, [`drive_loop_structured`], and the terminal on completion.
/// That is exactly what the runtime's own `drive` does for the unstructured
/// loop, written out here because the structured entry point is the library
/// surface, not a `Runtime` mode.
async fn drive_structured(
    store: Arc<dyn EventStore>,
    run_id: RunId,
    agent: &Agent,
    input: Value,
    resume_input: Option<Value>,
) -> Result<LoopOutcome, RuntimeError> {
    let log = store.read_log(run_id).await.expect("log reads");
    let mut ctx = RunCtx::with_hooks(store, run_id, log, fixed_clock(), fixed_random())
        .expect("ctx builds over the recorded log");
    if let Some(resume_input) = resume_input {
        ctx.set_resume_input(resume_input);
    }
    let input = ctx.begin(agent.def_hash(), &input).await?;
    let outcome = drive_loop_structured(&mut ctx, agent, &input, &schema()).await?;
    if let LoopOutcome::Completed(output) = &outcome {
        ctx.complete_run(output).await?;
    }
    Ok(outcome)
}

/// The `tools` array and `tool_choice` of one recorded request body.
fn offered_tools(request_body: &[u8]) -> (Vec<Value>, Option<Value>) {
    let body: Value = serde_json::from_slice(request_body).expect("request body is JSON");
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    (tools, body.get("tool_choice").cloned())
}

#[tokio::test]
async fn a_valid_first_answer_is_the_loops_output_and_the_request_forces_the_call() {
    let server = ScriptedModel::mount(vec![(
        1,
        answer_response("tu_answer", json!({"score": 0.91, "note": "tight"}), 2),
    )])
    .await;
    let agent = agent_builder(&server.uri()).build().expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(60);

    let outcome = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect("the drive succeeds");
    let LoopOutcome::Completed(output) = outcome else {
        panic!("expected completion");
    };
    // The answer call's input, verbatim: not the reply text, and not a
    // re-serialized subset of the schema's properties.
    assert_eq!(output, json!({"score": 0.91, "note": "tight"}));

    // The request offered the answer tool with the DECLARED schema as its
    // input schema, and forced some tool call.
    let requests = server.received_requests().await.expect("requests recorded");
    let (tools, tool_choice) = offered_tools(&requests[0].body);
    assert_eq!(tools.len(), 1, "the agent has no tools of its own");
    assert_eq!(tools[0]["name"], json!(ANSWER_TOOL));
    assert_eq!(tools[0]["input_schema"], schema());
    assert!(
        tools[0]["description"]
            .as_str()
            .is_some_and(|d| !d.is_empty()),
        "the answer tool describes itself: {:?}",
        tools[0]["description"]
    );
    assert_eq!(tool_choice, Some(json!({"type": "any"})));

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted"
        ]
    );
}

#[tokio::test]
async fn a_violating_answer_is_fed_back_and_the_re_ask_replays_byte_for_byte() {
    let server = ScriptedModel::mount(vec![
        (1, answer_response("tu_bad", json!({"score": "high"}), 2)),
        (3, answer_response("tu_good", json!({"score": 0.88}), 3)),
    ])
    .await;
    let agent = agent_builder(&server.uri()).build().expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(61);

    let outcome = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect("the drive succeeds");
    assert!(
        matches!(outcome, LoopOutcome::Completed(ref output) if *output == json!({"score": 0.88})),
        "{outcome:?}"
    );

    // The violation went back as a tool error on the answer call, naming OUR
    // validator's verdict (the string is ours and permanent) and what to do.
    let requests = server.received_requests().await.expect("requests recorded");
    let fed_back = tool_result_contents(&requests[1].body);
    assert_eq!(fed_back.len(), 1, "{fed_back:?}");
    assert!(
        fed_back[0].contains("$.score: expected type number, got string"),
        "{}",
        fed_back[0]
    );
    assert!(
        fed_back[0].contains("does not match its schema"),
        "{}",
        fed_back[0]
    );

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted"
        ]
    );

    // The re-ask is a pure function of recorded data, so re-driving the same
    // function over the recorded log changes nothing and calls nobody.
    let replayed = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect("the replay succeeds");
    assert!(
        matches!(replayed, LoopOutcome::Completed(ref output) if *output == json!({"score": 0.88}))
    );
    assert_eq!(store.read_log(run_id).await.expect("log reads"), log);
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        2,
        "the replayed drive reached the model zero times"
    );
}

#[tokio::test]
async fn an_answer_beside_real_tool_work_is_refused_while_the_tool_still_runs() {
    let server = ScriptedModel::mount(vec![
        (
            1,
            blocks_response(
                "mixed",
                vec![
                    tool_use_block("tu_read", "lookup", json!({"q": "otters"})),
                    tool_use_block("tu_early", ANSWER_TOOL, json!({"score": 0.5})),
                ],
                20,
                4,
            ),
        ),
        (3, answer_response("tu_answer", json!({"score": 0.75}), 3)),
    ])
    .await;
    let (tool, calls) = TestTool::new("lookup", Effect::Read, ToolBehavior::Echo);
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(62);

    let outcome = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect("the drive succeeds");
    assert!(
        matches!(outcome, LoopOutcome::Completed(ref output) if *output == json!({"score": 0.75})),
        "{outcome:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the real call ran normally"
    );

    // Both results went back in one turn: the tool's output, and the answer
    // call told to come back alone.
    let requests = server.received_requests().await.expect("requests recorded");
    let fed_back = tool_result_contents(&requests[1].body);
    assert_eq!(fed_back.len(), 2, "{fed_back:?}");
    assert!(fed_back[0].contains("otters"), "{}", fed_back[0]);
    assert!(fed_back[1].contains("call it alone"), "{}", fed_back[1]);

    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "ToolCallRequested",
            "ToolCallCompleted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted"
        ]
    );
}

#[tokio::test]
async fn a_turn_with_no_tool_call_at_all_is_re_asked() {
    // `tool_choice: any` is supposed to make this impossible; providers vary,
    // so the loop asks again rather than reading prose as an answer it never
    // validated.
    let server = ScriptedModel::mount(vec![
        (1, text_response("the score is about 0.9, roughly", 10, 5)),
        (3, answer_response("tu_answer", json!({"score": 0.9}), 3)),
    ])
    .await;
    let agent = agent_builder(&server.uri()).build().expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(63);

    let outcome = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect("the drive succeeds");
    assert!(
        matches!(outcome, LoopOutcome::Completed(ref output) if *output == json!({"score": 0.9})),
        "{outcome:?}"
    );

    let requests = server.received_requests().await.expect("requests recorded");
    let body: Value = serde_json::from_slice(&requests[1].body).expect("request body is JSON");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "input, the bare-text turn, the re-ask");
    let re_ask = messages[2]["content"].as_str().expect("the re-ask is text");
    assert!(re_ask.contains("That turn called no tool"), "{re_ask}");
}

#[tokio::test]
async fn repeated_identical_violations_collapse_through_the_streak_counter() {
    let bad = json!({"score": "high"});
    let server = ScriptedModel::mount(vec![
        (1, answer_response("tu_bad_1", bad.clone(), 2)),
        (3, answer_response("tu_bad_2", bad.clone(), 2)),
        (5, answer_response("tu_bad_3", bad, 2)),
        (7, answer_response("tu_good", json!({"score": 0.4}), 3)),
    ])
    .await;
    let agent = agent_builder(&server.uri()).build().expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(64);

    let outcome = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect("the drive succeeds");
    assert!(
        matches!(outcome, LoopOutcome::Completed(ref output) if *output == json!({"score": 0.4})),
        "{outcome:?}"
    );

    // The same violation three times over: the first goes back in full, the
    // second and third as the counted summary, exactly as a repeated tool
    // failure does.
    let requests = server.received_requests().await.expect("requests recorded");
    let first = tool_result_contents(&requests[1].body);
    let second = tool_result_contents(&requests[2].body);
    let third = tool_result_contents(&requests[3].body);
    assert!(first[0].contains("expected type number"), "{}", first[0]);
    assert!(
        second
            .last()
            .expect("a result")
            .contains("2 consecutive times"),
        "{second:?}"
    );
    assert!(
        third
            .last()
            .expect("a result")
            .contains("3 consecutive times"),
        "{third:?}"
    );
}

#[tokio::test]
async fn a_steps_budget_crossing_mid_re_ask_parks_and_a_resume_continues() {
    let bad = json!({"score": "high"});
    let server = ScriptedModel::mount(vec![
        (1, answer_response("tu_bad_1", bad.clone(), 2)),
        (3, answer_response("tu_bad_2", bad, 2)),
        (5, answer_response("tu_good", json!({"score": 0.6}), 3)),
    ])
    .await;
    let agent = agent_builder(&server.uri())
        .budgets(Budgets {
            max_steps: Some(2),
            ..Budgets::default()
        })
        .build()
        .expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(65);

    // Two re-asked turns spend the budget; the third iteration crosses before
    // its model call and parks. There is no separate "out of retries" error:
    // the budget IS the bound.
    let outcome = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect("the drive itself succeeds");
    assert!(
        matches!(
            outcome,
            LoopOutcome::Parked(ParkReason::BudgetExceeded { budget, observed })
                if budget.limit == 2.0 && observed == 2.0
        ),
        "{outcome:?}"
    );
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "NowObserved",
            "BudgetExceeded"
        ]
    );

    // An extension resumes the parked run and the next answer validates.
    let outcome = drive_structured(
        store.clone(),
        run_id,
        &agent,
        json!("rate this"),
        Some(json!({"extend": {"steps": 2}})),
    )
    .await
    .expect("the resumed drive succeeds");
    assert!(
        matches!(outcome, LoopOutcome::Completed(ref output) if *output == json!({"score": 0.6})),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_real_tool_named_salvor_answer_refuses_before_any_model_call() {
    let server = ScriptedModel::mount(vec![(
        1,
        answer_response("tu_answer", json!({"score": 0.1}), 2),
    )])
    .await;
    let (tool, calls) = TestTool::new(ANSWER_TOOL, Effect::Read, ToolBehavior::Echo);
    let agent = agent_builder(&server.uri())
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let run_id = fixed_run_id(66);

    let error = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
        .await
        .expect_err("a colliding tool name refuses the drive");
    assert!(
        matches!(error, RuntimeError::AnswerToolNameTaken),
        "{error}"
    );

    // Nothing but the run head: the refusal lands before the first model call,
    // so there is no half-structured turn in the log.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(event_kinds(&log), ["RunStarted"]);
    assert!(
        server
            .received_requests()
            .await
            .expect("requests recorded")
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// The property: a kill at every boundary recovers identically
// ---------------------------------------------------------------------------

/// A store wrapper that allows a budgeted number of appends and then fails
/// every later one WITHOUT touching the wrapped store, simulating a process
/// death at an exact event boundary. Mirrors the wrapper `kill_resume.rs`
/// drives the built-in loop with; the shared test module cannot own it without
/// rewriting that test.
struct KillStore {
    inner: Arc<dyn EventStore>,
    remaining: AtomicI64,
}

impl KillStore {
    fn new(inner: Arc<dyn EventStore>, allow: i64) -> Self {
        Self {
            inner,
            remaining: AtomicI64::new(allow),
        }
    }
}

#[async_trait]
impl EventStore for KillStore {
    async fn append(&self, envelope: &EventEnvelope) -> Result<(), StoreError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) <= 0 {
            return Err(StoreError::Backend("simulated crash".to_owned()));
        }
        self.inner.append(envelope).await
    }

    async fn read_log(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, StoreError> {
        self.inner.read_log(run_id).await
    }

    async fn list_runs(&self) -> Result<Vec<RunSummary>, StoreError> {
        self.inner.list_runs().await
    }

    async fn claim_call(
        &self,
        claimant: salvor_store::CallClaimant<'_>,
    ) -> Result<salvor_store::CallClaim, StoreError> {
        self.inner.claim_call(claimant).await
    }

    async fn lookup_call(
        &self,
        tool: &str,
        idempotency_key: &str,
    ) -> Result<Option<salvor_store::CallCommitment>, StoreError> {
        self.inner.lookup_call(tool, idempotency_key).await
    }

    async fn append_settling_call(
        &self,
        envelope: &EventEnvelope,
        claimant: salvor_store::CallClaimant<'_>,
    ) -> Result<(), StoreError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) <= 0 {
            return Err(StoreError::Backend("simulated crash".to_owned()));
        }
        self.inner.append_settling_call(envelope, claimant).await
    }
}

/// The reference structured run, scripted three turns deep so the sweep cuts
/// through a real tool call AND a re-asked violation: a read tool call, an
/// answer that violates the schema, then an answer that validates.
async fn sweep_server() -> MockServer {
    ScriptedModel::mount(vec![
        (
            1,
            tool_use_response("tu_read", "lookup", json!({"q": "otters"}), 20, 4),
        ),
        (3, answer_response("tu_bad", json!({"score": "high"}), 2)),
        (5, answer_response("tu_good", json!({"score": 0.93}), 3)),
    ])
    .await
}

/// Builds the sweep's agent over a server, with the read tool's counter.
fn sweep_agent(server_uri: &str) -> (Agent, Arc<AtomicUsize>) {
    let (tool, calls) = TestTool::new("lookup", Effect::Read, ToolBehavior::Echo);
    let agent = agent_builder(server_uri)
        .tool_dyn(Box::new(tool))
        .build()
        .expect("agent builds");
    (agent, calls)
}

#[tokio::test]
async fn a_structured_run_recovers_identically_from_every_kill_boundary() {
    // The control run: uninterrupted, the oracle every recovered log must equal.
    let server = sweep_server().await;
    let (agent, _calls) = sweep_agent(&server.uri());
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let control_id = fixed_run_id(70);
    let outcome = drive_structured(store.clone(), control_id, &agent, json!("rate this"), None)
        .await
        .expect("the control run completes");
    assert!(
        matches!(outcome, LoopOutcome::Completed(ref output) if *output == json!({"score": 0.93}))
    );
    let control = store.read_log(control_id).await.expect("log reads");
    assert_eq!(
        control.len(),
        13,
        "the reference structured run records 13 events"
    );

    // Every boundary in it, cut and recovered.
    for allow in 1..control.len() {
        let server = sweep_server().await;
        let (agent, calls) = sweep_agent(&server.uri());
        let store: Arc<dyn EventStore> = Arc::new(SqliteStore::in_memory().expect("store opens"));
        let run_id = fixed_run_id(71);

        let killer: Arc<dyn EventStore> = Arc::new(KillStore::new(
            store.clone(),
            i64::try_from(allow).expect("a small budget"),
        ));
        let error = drive_structured(killer, run_id, &agent, json!("rate this"), None)
            .await
            .expect_err("the kill store aborts the drive");
        assert!(matches!(error, RuntimeError::Store(_)), "{error}");
        assert_eq!(
            store.read_log(run_id).await.expect("log reads").len(),
            allow,
            "exactly the budgeted number of events persisted"
        );

        let outcome = drive_structured(store.clone(), run_id, &agent, json!("rate this"), None)
            .await
            .unwrap_or_else(|error| panic!("recovery after {allow} appends failed: {error}"));
        assert!(
            matches!(
                outcome,
                LoopOutcome::Completed(ref output) if *output == json!({"score": 0.93})
            ),
            "recovery after {allow} appends: {outcome:?}"
        );
        let recovered = store.read_log(run_id).await.expect("log reads");
        assert_eq!(
            recovered.len(),
            control.len(),
            "recovery after {allow} appends produced a different log length"
        );
        for (position, (left, right)) in recovered.iter().zip(control.iter()).enumerate() {
            assert_eq!(
                left.event, right.event,
                "recovery after {allow} appends diverged at seq {position}"
            );
        }
        // The read tool re-executes when its completion is what failed to
        // persist; it never runs more than twice, and never zero times.
        let executions = calls.load(Ordering::SeqCst);
        assert!(
            (1..=2).contains(&executions),
            "the read tool ran {executions} times for a kill after {allow} appends"
        );
    }
}

/// The declaration can live on the agent itself, and then nobody has to ask
/// for the structured loop by name: `Runtime::start` drives it, because
/// `driver::drive` reads `agent.output_schema()`. This is the path a
/// `salvor run` over an `agent.toml` with an `[output_schema]` table takes,
/// end to end, with the scripted model answering through the answer tool.
#[tokio::test]
async fn an_agent_that_declares_a_schema_drives_the_structured_loop_through_the_runtime() {
    let server = ScriptedModel::mount(vec![(
        1,
        answer_response("tu_runtime", json!({"score": 0.42, "note": "thin"}), 2),
    )])
    .await;
    let agent = agent_builder(&server.uri())
        .output_schema(schema())
        .build()
        .expect("agent builds");
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(72);

    let outcome = runtime
        .start_with_id(&agent, run_id, json!("rate this"))
        .await
        .expect("the run completes");
    let RunOutcome::Completed { output, .. } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };
    // The object, not a sentence about it: the whole point of declaring it.
    assert_eq!(output, json!({"score": 0.42, "note": "thin"}));

    // And the run's terminal carries the same object, so a caller reading the
    // log sees the structured answer too.
    let log = store.read_log(run_id).await.expect("log reads");
    assert_eq!(
        event_kinds(&log),
        [
            "RunStarted",
            "NowObserved",
            "ModelCallRequested",
            "ModelCallCompleted",
            "RunCompleted",
        ]
    );
    let Event::RunCompleted { output } = &log.last().expect("a terminal").event else {
        panic!("the last event is the terminal");
    };
    assert_eq!(*output, json!({"score": 0.42, "note": "thin"}));

    // The request the agent's own declaration produced is the same one an
    // explicit `drive_loop_structured` produces: the declared schema on the
    // answer tool, and a forced call.
    let requests = server.received_requests().await.expect("requests recorded");
    let (tools, tool_choice) = offered_tools(&requests[0].body);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], json!(ANSWER_TOOL));
    assert_eq!(tools[0]["input_schema"], schema());
    assert_eq!(tool_choice, Some(json!({"type": "any"})));
}

/// The mirror of the test above: the same agent with no declaration is left
/// exactly where it was, on the plain loop, answering in text. The default is
/// untouched, which is what makes the key safe to add to the format.
#[tokio::test]
async fn an_agent_without_a_schema_still_drives_the_plain_loop_through_the_runtime() {
    let server = ScriptedModel::mount(vec![(1, text_response("about 0.42", 10, 2))]).await;
    let agent = agent_builder(&server.uri()).build().expect("agent builds");
    let store = Arc::new(SqliteStore::in_memory().expect("store opens"));
    let runtime = Runtime::with_hooks(store.clone(), fixed_clock(), fixed_random());
    let run_id = fixed_run_id(73);

    let outcome = runtime
        .start_with_id(&agent, run_id, json!("rate this"))
        .await
        .expect("the run completes");
    let RunOutcome::Completed { output, .. } = outcome else {
        panic!("expected completion, got {outcome:?}");
    };
    assert_eq!(output, json!("about 0.42"));

    let requests = server.received_requests().await.expect("requests recorded");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("tool_choice").is_none(), "{body}");
}

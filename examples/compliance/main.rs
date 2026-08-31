//! Example: a compliance gate on the library-first tier, offline.
//!
//! An agent handles a refund request. Issuing the refund is a consequential
//! action, so it is gated: the run pauses and a compliance officer must sign
//! off in writing before anything is written to the refund ledger. Approve and
//! the refund is issued exactly once; reject and the run stops with no write at
//! all. Either way the whole story (the model's proposal, the approval request,
//! the officer's typed decision, and the executed-or-not action) lands in the
//! append-only event log, which is the audit record.
//!
//! Like `examples/approval-loop`, this is the library-first tier: no
//! [`Runtime`](salvor_runtime::Runtime) and no built-in loop. `compliance_flow`
//! below is an ordinary async function written against the public
//! [`RunCtx`](salvor_runtime::RunCtx) API. The human-approval gate lives here,
//! in the library tier, because a real approval gate needs the suspension
//! primitive: a tool asks to park the run, the run suspends with a typed input
//! schema, and a later invocation resumes it with the officer's decision.
//!
//! # Offline by construction
//!
//! There is no paid API key here. The model is a small scripted stand-in served
//! over a local mock HTTP server (`wiremock`), so the run is deterministic and
//! costs nothing. The example teaches the durability and the approval control,
//! not model quality, so a scripted proposal is the honest choice: the recorded
//! `ModelCallCompleted` is a genuine model boundary in the log, it just came
//! from a canned response instead of the provider.
//!
//! # The gate, concretely
//!
//! One run, driven across separate process invocations that share one SQLite
//! file and one fixed run id:
//!
//! - First invocation (no `SALVOR_DECISION`): begin the run, make one model
//!   call that proposes the refund, then suspend awaiting a compliance
//!   officer's sign-off. The process exits with the run parked durably.
//! - Second invocation (`SALVOR_DECISION=approve`): open a fresh `RunCtx` over
//!   the parked log, replay it without any IO (the model is never called
//!   again), record the officer's approval, and only then execute the
//!   `issue_refund` Write, which appends one line to the ledger.
//! - Or (`SALVOR_DECISION=reject`): record the officer's rejection and complete
//!   the run with no write. The ledger stays empty.
//!
//! # Run it
//!
//! ```sh
//! # 1. propose and park at the approval gate
//! cargo run -p salvor-runtime --example compliance_gate
//! # 2a. a compliance officer approves: the refund is issued exactly once
//! SALVOR_DECISION=approve cargo run -p salvor-runtime --example compliance_gate
//!
//! # to see the reject path, re-park then reject:
//! cargo run -p salvor-runtime --example compliance_gate
//! SALVOR_DECISION=reject cargo run -p salvor-runtime --example compliance_gate
//! ```

use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use salvor_core::{Effect, Event, EventEnvelope, RunId, derive_state};
use salvor_llm::{Client, Config, Message, MessageRequest};
use salvor_runtime::{Resumption, RunCtx, RuntimeError, ToolCallResult};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::{DynTool, HandlerError, ToolCtx, ToolError, ToolOutcome};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The definition hash `begin` records and re-checks on every replay. A stable
/// string is all a hand-written flow needs, since the same flow must present
/// the same hash on every invocation or the resume would refuse to replay.
const DEF_HASH: &str = "sha256:compliance-gate-example-v1";

/// The event log both invocations share. The audit trail lives here.
const STORE_PATH: &str = "/tmp/salvor-compliance.db";

/// The refund ledger the `issue_refund` Write appends to. This is the world
/// side effect the gate protects; the run clears it on a fresh park.
const LEDGER_PATH: &str = "/tmp/salvor-compliance-ledger.txt";

/// The idempotency key stamped on the refund Write. A deterministic key means a
/// duplicate resume can never issue a second refund: the provider (here, our
/// ledger append) would collapse the retry. It is recorded on the
/// `ToolCallRequested` event, so the audit trail shows the guard.
const REFUND_IDEMPOTENCY_KEY: &str = "refund-C-1024-4000";

/// The scripted proposal the mock model returns. In a real deployment this is
/// the provider's answer; here it is canned so the run is offline and
/// deterministic.
const PROPOSAL_TEXT: &str = "I recommend issuing a $40.00 refund to customer C-1024 for the \
duplicate charge. Issuing a refund is a consequential action, so it needs a compliance \
officer's written sign-off before it is written to the ledger.";

/// How one drive of the flow ended.
enum Flow {
    /// Parked at the approval gate; no decision was available this invocation.
    Parked,
    /// A decision landed and the run completed with this output.
    Completed(Value),
}

// --- The gated Write tool -------------------------------------------------
//
// `issue_refund` is an `Effect::Write` tool: the runtime records its intent
// *before* the handler runs, so a crash between intent and completion is
// detectable and the write is never silently retried. Its execution counter is
// shared with `main` so the example can prove the refund is issued exactly once
// across the whole scenario.

/// The consequential action the gate protects: appends one refund line to the
/// ledger. `Effect::Write`, so the runtime write-ahead-logs its intent.
struct IssueRefund {
    ledger_path: PathBuf,
    /// Incremented once per real execution, shared with `main` so exactly-once
    /// can be asserted. On replay the handler never runs, so this never moves.
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl DynTool for IssueRefund {
    fn name(&self) -> &str {
        "issue_refund"
    }

    fn description(&self) -> &str {
        "Issue a refund to a customer. Consequential: appends to the refund ledger."
    }

    fn effect(&self) -> Effect {
        Effect::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["customer", "amount_usd"],
            "properties": {
                "customer": {"type": "string"},
                "amount_usd": {"type": "number"},
            },
        })
    }

    async fn call_json(
        &self,
        _ctx: &ToolCtx,
        input: Value,
    ) -> Result<ToolOutcome<Value>, ToolError> {
        // The one real side effect. The runtime recorded the intent before
        // calling us, so this line is written at most once even across a crash
        // and resume.
        self.executions.fetch_add(1, Ordering::SeqCst);

        let customer = input
            .get("customer")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let amount = input
            .get("amount_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let line = format!("REFUND issued to {customer} for ${amount:.2}");

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.ledger_path)
            .map_err(|error| handler_failure(self.name(), error))?;
        writeln!(file, "{line}").map_err(|error| handler_failure(self.name(), error))?;

        Ok(ToolOutcome::Output(
            json!({"status": "issued", "ledger_line": line}),
        ))
    }
}

/// Wraps an IO error as a tool handler failure the runtime can record.
fn handler_failure(tool: &str, error: std::io::Error) -> ToolError {
    ToolError::Handler {
        tool: tool.to_owned(),
        source: HandlerError::new(error),
    }
}

/// The typed approval schema a resume input must satisfy. The officer identity,
/// the decision, and an optional note are the evidence the audit record keeps.
fn approval_schema() -> Value {
    json!({
        "type": "object",
        "required": ["approver", "decision"],
        "properties": {
            "approver": {"type": "string", "description": "the compliance officer signing off"},
            "decision": {"type": "string", "enum": ["approve", "reject"]},
            "note": {"type": "string", "description": "rationale recorded with the decision"},
        },
    })
}

/// The user-written orchestration, built against public `salvor-runtime` API.
/// Run it once with no staged decision to park at the gate; run it again with
/// an approval or rejection staged on the context to continue past the gate.
async fn compliance_flow(
    ctx: &mut RunCtx,
    client: &Client,
    refund_tool: &dyn DynTool,
) -> Result<Flow, RuntimeError> {
    // `begin` records `RunStarted` live, or on replay verifies the definition
    // hash and returns the recorded input. Either way we get the case back.
    let case = ctx
        .begin(
            DEF_HASH,
            &json!({"customer": "C-1024", "amount_usd": 40.0, "reason": "duplicate charge"}),
        )
        .await?;

    // A recorded clock read: live once, replayed forever after. Orchestration
    // code must never read the wall clock directly, or replay would diverge.
    let _observed_at = ctx.now().await?;

    // The one model call. Live on the first invocation, answered from the log
    // on every rerun, so the scripted provider is contacted exactly once.
    let request = MessageRequest::new("compliance-review-model", 256).push_message(Message::user(
        format!("A customer requested a refund: {case}. Recommend whether to issue it."),
    ));
    let turn = ctx.model_call(client, &request).await?;
    let proposal = turn.response.text();

    // The gate: park the run awaiting a compliance officer's typed decision.
    // `suspend` records `Suspended { reason, input_schema }`; `await_resume` is
    // where the run either continues (a decision was staged) or reports parked.
    ctx.suspend(
        "a compliance officer must sign off before this refund is written to the ledger",
        &approval_schema(),
    )
    .await?;

    match ctx.await_resume().await? {
        // No decision this time. The log holds everything, so the process may
        // stop driving the run; it survives the exit, parked at the gate.
        Resumption::Parked => Ok(Flow::Parked),
        // A decision was recorded (live now, or replayed on a later rerun). The
        // decision is durable, so this branch is deterministic on replay.
        Resumption::Resumed(decision) => {
            let approved = decision.get("decision").and_then(Value::as_str) == Some("approve");
            if approved {
                // Approved: execute the Write exactly once. The intent is
                // recorded before the ledger append, the completion after.
                let refund_input = json!({"customer": "C-1024", "amount_usd": 40.0});
                let receipt = match ctx
                    .tool_call(refund_tool, &refund_input, Some(REFUND_IDEMPOTENCY_KEY))
                    .await?
                {
                    ToolCallResult::Output(value) => value,
                    ToolCallResult::Failed(failure) => {
                        let error = format!("the refund write failed: {failure:?}");
                        ctx.fail_run(&error).await?;
                        return Ok(Flow::Completed(json!({"status": "failed", "error": error})));
                    }
                    // An Effect::Write tool that only ever returns output never
                    // reaches here; the arms keep the match total.
                    ToolCallResult::Suspended(_) | ToolCallResult::Sleeping(_) => {
                        unreachable!("issue_refund neither suspends nor sleeps")
                    }
                };
                let output = json!({
                    "status": "refund_issued",
                    "proposal": proposal,
                    "approval": decision,
                    "receipt": receipt,
                });
                ctx.complete_run(&output).await?;
                Ok(Flow::Completed(output))
            } else {
                // Rejected: record the decision and complete with no write. The
                // ledger is never touched.
                let output = json!({
                    "status": "refund_denied",
                    "proposal": proposal,
                    "approval": decision,
                });
                ctx.complete_run(&output).await?;
                Ok(Flow::Completed(output))
            }
        }
    }
}

/// Mounts the scripted model on a local mock server. One canned proposal for
/// the single model call this flow makes, so no key and no network are needed.
async fn scripted_model() -> MockServer {
    let server = MockServer::start().await;
    let response = json!({
        "id": "msg_compliance_proposal",
        "model": "compliance-review-model",
        "role": "assistant",
        "content": [{"type": "text", "text": PROPOSAL_TEXT}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 48, "output_tokens": 36},
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(&server)
        .await;
    server
}

/// The staged decision for a resume invocation, keyed by `SALVOR_DECISION`.
fn decision_input(decision: &str) -> Value {
    match decision {
        "approve" => json!({
            "approver": "dana@compliance.example",
            "decision": "approve",
            "note": "verified the duplicate charge in the billing system",
        }),
        _ => json!({
            "approver": "dana@compliance.example",
            "decision": "reject",
            "note": "customer was already made whole via a chargeback",
        }),
    }
}

/// Prints the run's event log as the audit trail: every model decision, the
/// approval request, the officer's decision, and the executed-or-not action,
/// in order, exactly as recorded.
fn print_audit_trail(log: &[EventEnvelope]) {
    println!("\n=== AUDIT TRAIL ({} events) ===", log.len());
    for envelope in log {
        let seq = envelope.seq.get();
        let at = envelope.recorded_at;
        let detail = match &envelope.event {
            Event::RunStarted { input, .. } => format!("run started; case = {input}"),
            Event::ModelCallRequested { request_hash, .. } => {
                format!("model call requested ({request_hash})")
            }
            Event::ModelCallCompleted { usage, .. } => format!(
                "model call completed ({} in / {} out tokens)",
                usage.input_tokens, usage.output_tokens
            ),
            Event::NowObserved { .. } => "clock observed".to_owned(),
            Event::RandomObserved { .. } => "random observed".to_owned(),
            Event::Suspended { reason, .. } => format!("SUSPENDED for approval: {reason}"),
            Event::Resumed { input, .. } => format!("RESUMED with officer decision: {input}"),
            Event::ToolCallRequested {
                tool,
                effect,
                idempotency_key,
                input,
                ..
            } => format!(
                "tool intent: {tool} (effect={effect:?}, idempotency_key={}) input={input}",
                idempotency_key.as_deref().unwrap_or("none"),
            ),
            Event::ToolCallCompleted { output, .. } => format!("tool completed: {output}"),
            Event::BudgetExceeded { budget, observed } => {
                format!("budget exceeded: {budget:?} observed {observed}")
            }
            Event::RunCompleted { output } => {
                let status = output.get("status").and_then(Value::as_str).unwrap_or("");
                format!("run completed (status={status})")
            }
            Event::RunFailed { error } => format!("run failed: {error}"),
            // This walkthrough drives a single agent run, so the graph-run
            // events never appear in its log. A wildcard keeps the demo
            // compiling as the event vocabulary grows rather than narrating
            // kinds it cannot emit.
            other => format!("{other:?}"),
        };
        println!("  #{seq:<2} {at}  {detail}");
    }
    println!("=== END AUDIT TRAIL ===\n");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let decision = std::env::var("SALVOR_DECISION").ok();
    if let Some(value) = &decision
        && value != "approve"
        && value != "reject"
    {
        eprintln!("SALVOR_DECISION must be 'approve' or 'reject' (got {value:?})");
        return Ok(());
    }

    // A fresh park invocation (no decision) starts from an empty log and an
    // empty ledger, so clear both (including the SQLite WAL sidecars). A resume
    // invocation keeps them: that recorded log is exactly what it replays.
    if decision.is_none() {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{STORE_PATH}{suffix}"));
        }
        let _ = std::fs::remove_file(LEDGER_PATH);
    }

    // The offline model: a scripted proposal over a local mock server, so no
    // API key is needed. On a resume invocation the single model call replays
    // from the log, so the server is never actually contacted.
    let server = scripted_model().await;
    let client = Client::new(
        Config::new()
            .with_base_url(server.uri())
            .with_max_retries(0),
    )?;

    // One event store, shared across invocations through the file on disk. A
    // fixed run id means a resume invocation addresses the run the park wrote.
    let store: Arc<SqliteStore> = Arc::new(SqliteStore::open(STORE_PATH)?);
    let run_id = RunId::from_uuid("00000000-0000-4000-8000-00000000c0de".parse()?);

    let executions = Arc::new(AtomicUsize::new(0));
    let refund_tool = IssueRefund {
        ledger_path: LEDGER_PATH.into(),
        executions: executions.clone(),
    };

    // Build the context over whatever the log holds now: empty on the park,
    // the parked history on a resume. `Arc<SqliteStore>` coerces to the
    // `Arc<dyn EventStore>` the constructor wants.
    let log = store.read_log(run_id).await?;
    let replaying = log.len();
    let mut ctx = RunCtx::new(store.clone(), run_id, log)?;

    // Stage the officer's decision, if this invocation carries one. The next
    // live `await_resume` records it as the `Resumed` event and returns it.
    if let Some(value) = &decision {
        ctx.set_resume_input(decision_input(value));
    }

    match compliance_flow(&mut ctx, &client, &refund_tool).await? {
        Flow::Parked => {
            println!("run {} parked at the compliance gate.", run_id.as_uuid());
            println!("a compliance officer approves and issues the refund with:");
            println!(
                "  SALVOR_DECISION=approve cargo run -p salvor-runtime --example compliance_gate"
            );
            println!("or rejects it (no refund) with:");
            println!(
                "  SALVOR_DECISION=reject cargo run -p salvor-runtime --example compliance_gate"
            );
        }
        Flow::Completed(output) => {
            let status = output.get("status").and_then(Value::as_str).unwrap_or("");
            println!(
                "run {} completed ({status}); replayed {replaying} recorded events, then continued live.",
                run_id.as_uuid()
            );
        }
    }

    // The event log is the audit record. Dump it as the audit trail, then read
    // the ledger and report how many times the refund actually executed.
    let final_log = store.read_log(run_id).await?;
    print_audit_trail(&final_log);

    let ledger = std::fs::read_to_string(LEDGER_PATH).unwrap_or_default();
    let ledger_lines: Vec<&str> = ledger
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    println!("refund ledger ({LEDGER_PATH}):");
    if ledger_lines.is_empty() {
        println!("  (empty: no refund was written)");
    } else {
        for line in &ledger_lines {
            println!("  {line}");
        }
    }
    println!(
        "issue_refund executed {} time(s) this invocation; ledger holds {} refund line(s).",
        executions.load(Ordering::SeqCst),
        ledger_lines.len(),
    );

    let state = derive_state(&final_log);
    println!(
        "{} events, {} input + {} output tokens (one scripted model call total).",
        final_log.len(),
        state.usage.input_tokens,
        state.usage.output_tokens,
    );
    Ok(())
}

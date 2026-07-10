//! Example: the library-first tier.
//!
//! This is Salvor used as a durability substrate you bring your own loop to.
//! There is no [`Runtime`](salvor_runtime::Runtime) and no built-in agent loop
//! here: `approval_flow` below is an ordinary async function written against
//! the public [`RunCtx`](salvor_runtime::RunCtx) API, and it gets the same
//! durability, crash-exact replay, and suspension guarantees the batteries
//! tier does. This is the 12-factor "own your control flow" posture: the
//! runtime sits under your loop instead of taking it away.
//!
//! The flow is deliberately tiny so you can read it top to bottom: begin the
//! run, read the recorded clock once, make one model call with a one-line
//! prompt, then suspend awaiting a human's approval. That suspension parks the
//! run durably and the process exits. Running the binary again with `APPROVAL`
//! set replays the recorded history live-free (the model is never called a
//! second time) and continues from the suspension to completion.
//!
//! # Replay-on-rerun, concretely
//!
//! Both invocations share one SQLite file and one fixed run id, so the second
//! run opens a `RunCtx` over the log the first one wrote. As `approval_flow`
//! re-executes, `begin`, `now`, and `model_call` each find their event already
//! in the log and return the recorded value without doing any IO. Execution
//! only goes live again at `await_resume`, where the approval this invocation
//! supplied is recorded as the `Resumed` event and the run completes. That is
//! the whole replay contract: events `0..k` replay, then execution continues.
//!
//! # Run it (twice)
//!
//! ```sh
//! export ANTHROPIC_API_KEY=sk-ant-...
//! cargo run -p salvor-runtime --example approval_loop            # parks
//! APPROVAL=looks-good cargo run -p salvor-runtime --example approval_loop  # completes
//! ```

use std::sync::Arc;

use salvor_core::{RunId, derive_state};
use salvor_llm::{Client, Config, Message, MessageRequest};
use salvor_runtime::{Pricing, Resumption, RunCtx, RuntimeError, content_string};
use salvor_store::{EventStore, SqliteStore};
use serde_json::{Value, json};

/// The definition hash `begin` records and re-checks on every replay. A real
/// agent derives this from its configuration; a hand-written flow just needs a
/// stable string, since the same flow must produce the same hash each run.
const DEF_HASH: &str = "sha256:approval-loop-example-v1";

/// The store file both invocations share.
const STORE_PATH: &str = "/tmp/salvor-approval-loop.sqlite";

/// Real `claude-opus-4-8` pricing, for the closing cost report.
const PRICING: Pricing = Pricing {
    input_per_mtok: 5.0,
    output_per_mtok: 25.0,
};

/// How one drive of the flow ended.
enum Flow {
    /// Parked at the suspension; no approval was available this invocation.
    Parked,
    /// The approval landed and the run completed with this output.
    Completed(Value),
}

/// The user-written orchestration, built against nothing but public
/// `salvor-runtime` API (plus the `salvor-llm` types it composes). Run it once
/// to park at the approval gate; run it again over the recorded log, with a
/// resume input set on the context, to continue past the gate.
async fn approval_flow(ctx: &mut RunCtx, client: &Client) -> Result<Flow, RuntimeError> {
    // `begin` records `RunStarted` live, or on replay verifies the definition
    // hash and returns the recorded input. Either way we get the prompt back.
    let prompt = ctx
        .begin(
            DEF_HASH,
            &json!(
                "In one sentence, draft a friendly status update saying the migration is on track."
            ),
        )
        .await?;

    // A recorded clock read. Live it reads the real clock once and persists the
    // instant; on replay it returns that same instant. Orchestration code must
    // never read the wall clock directly, or replay would diverge.
    let _started_at = ctx.now().await?;

    // The one model call. On the first invocation this hits the provider and
    // records the response. On the rerun it is answered entirely from the log,
    // so the provider is never contacted a second time.
    let request = MessageRequest::new("claude-opus-4-8", 256)
        .push_message(Message::user(content_string(&prompt)));
    let turn = ctx.model_call(client, &request).await?;
    let draft = turn.response.text();

    // The suspension: park the run awaiting a human approval that matches this
    // schema. `suspend` records `Suspended`; `await_resume` is where the run
    // either continues (an approval was supplied) or reports that it is parked.
    let schema = json!({
        "type": "object",
        "required": ["approved"],
        "properties": {
            "approved": {"type": "boolean"},
            "note": {"type": "string"},
        },
    });
    ctx.suspend("a human must approve the drafted status update", &schema)
        .await?;

    match ctx.await_resume().await? {
        // No approval this time. The log already holds everything, so the
        // process may simply stop driving the run; it survives the exit.
        Resumption::Parked => Ok(Flow::Parked),
        // An approval was recorded (live now, or replayed on a later rerun).
        Resumption::Resumed(approval) => {
            let output = json!({"draft": draft, "approval": approval});
            ctx.complete_run(&output).await?;
            Ok(Flow::Completed(output))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let approving = std::env::var("APPROVAL").ok();

    // A fresh, no-APPROVAL invocation starts from an empty log, so clear any
    // store left over from a previous demo (including the WAL sidecars). An
    // APPROVAL invocation keeps the file: that recorded log is exactly what it
    // replays.
    if approving.is_none() {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{STORE_PATH}{suffix}"));
        }
    }

    let key = std::env::var("DEMO_ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .ok();
    let Some(key) = key else {
        eprintln!(
            "no API key found. Set ANTHROPIC_API_KEY (or DEMO_ANTHROPIC_API_KEY) and re-run."
        );
        return Ok(());
    };
    let client = Client::new(Config::new().with_api_key(key))?;

    // One Arc<SqliteStore>, shared by both invocations through the file on
    // disk. A fixed run id means the second invocation addresses the same run
    // the first one wrote.
    let store: Arc<SqliteStore> = Arc::new(SqliteStore::open(STORE_PATH)?);
    let run_id = RunId::from_uuid("00000000-0000-4000-8000-00000000a110".parse()?);

    // Build the context over whatever the log holds now: empty on the first
    // run, the parked history on the rerun. `RunCtx::new` takes the store as a
    // trait object; Arc<SqliteStore> coerces to Arc<dyn EventStore>.
    let log = store.read_log(run_id).await?;
    let replaying = log.len();
    let mut ctx = RunCtx::new(store.clone(), run_id, log)?;

    // If this invocation carries an approval, hand it to the context. The next
    // live `await_resume` will record it as the `Resumed` event and return it.
    if let Some(note) = &approving {
        ctx.set_resume_input(json!({"approved": true, "note": note}));
    }

    match approval_flow(&mut ctx, &client).await? {
        Flow::Parked => {
            println!("run {} parked awaiting approval.", run_id.as_uuid());
            println!("rerun exactly this to approve and continue:");
            println!("  APPROVAL=looks-good cargo run -p salvor-runtime --example approval_loop");
        }
        Flow::Completed(output) => {
            println!(
                "run {} completed (replayed {replaying} recorded events, then continued live).",
                run_id.as_uuid()
            );
            println!("output: {output}");
        }
    }

    // The event log is the whole story: count the events and report the model
    // spend, computed from recorded usage and the fixed pricing.
    let final_log = store.read_log(run_id).await?;
    let state = derive_state(&final_log);
    let cost = PRICING.cost_usd(state.usage.input_tokens, state.usage.output_tokens);
    println!(
        "{} events, {} input + {} output tokens, ${cost:.4} spent (one model call total, across both runs)",
        final_log.len(),
        state.usage.input_tokens,
        state.usage.output_tokens,
    );
    Ok(())
}

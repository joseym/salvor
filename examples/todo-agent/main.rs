//! Example: the batteries-included tier.
//!
//! This is Salvor used as a library at its highest-level tier: you describe an
//! agent as data (model, prompt, tools, budgets) with [`Agent::builder`], hand
//! it to a [`Runtime`], and let the built-in loop drive it. You write tools,
//! not orchestration.
//!
//! The agent is a tiny todo-triage assistant. You give it a brain dump; it
//! calls `add_todo` a few times to break the dump into actionable items, then
//! finishes with a one-line summary. Two native tools are defined with
//! `#[derive(Tool)]` plus a hand-written `ToolHandler`, the two canonical
//! shapes: `add_todo` is a `Write`-effect tool (it appends a line
//! to a file) and `list_todos` is a `Read`-effect tool.
//!
//! # The durability story, in one binary
//!
//! Every run is an append-only event log in a SQLite file. Nothing else is
//! state. That is what makes `kill -9` at any point recoverable: the run id is
//! printed before the first step executes, and re-running the same binary with
//! `RESUME_RUN_ID=<id>` set replays the recorded log and continues from the
//! first unrecorded step. Completed model and tool calls are read back from the
//! log, never re-executed, so no todo is written twice.
//!
//! # Run it
//!
//! ```sh
//! export ANTHROPIC_API_KEY=sk-ant-...
//! cargo run -p salvor-runtime --example todo_agent
//! # then, to prove crash-recovery, re-run with the id it printed:
//! RESUME_RUN_ID=<printed-id> cargo run -p salvor-runtime --example todo_agent
//! ```
//!
//! Environment knobs: `ANTHROPIC_API_KEY` (or `DEMO_ANTHROPIC_API_KEY`) for the
//! model, `TODO_FILE` for the output file (default `/tmp/salvor-todos.txt`),
//! `TODO_INPUT` for the brain dump, and `RESUME_RUN_ID` to recover a run.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use salvor_core::{RunId, derive_state};
use salvor_llm::{AuthKind, Config};
use salvor_runtime::{Agent, Budgets, Pricing, RunOutcome, Runtime};
use salvor_store::{EventStore, SqliteStore};
use salvor_tools::{HandlerError, Tool, ToolCtx, ToolHandler, ToolOutcome};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Real Anthropic pricing for `claude-opus-4-8`, in US dollars per million
/// tokens. The cost budget below is computed against this table.
const PRICING: Pricing = Pricing {
    input_per_mtok: 5.0,
    output_per_mtok: 25.0,
};

/// Where the agent's todos land. A `Write`-effect tool appends here.
fn todo_path() -> PathBuf {
    std::env::var("TODO_FILE")
        .unwrap_or_else(|_| "/tmp/salvor-todos.txt".to_owned())
        .into()
}

// --- Tool 1: add_todo (Write effect) ------------------------------------
//
// `#[derive(Tool)]` writes the `ToolMeta` half (name, description, effect)
// from the attributes; the `ToolHandler` impl below is hand-written, and the
// input JSON Schema handed to the model is derived from `AddTodoInput` by
// schemars. The `Write` effect is the interesting one: the runtime records the
// call's intent *before* the handler runs (write-ahead), so a crash between
// intent and completion is detectable and never silently retried.

/// The typed input the model must supply to add one todo.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct AddTodoInput {
    /// One actionable todo item, phrased as a short imperative.
    item: String,
}

/// What `add_todo` returns to the model after writing.
#[derive(Debug, Serialize)]
struct AddTodoOutput {
    /// The item that was written.
    written: String,
    /// How many todos the file holds now.
    total: usize,
}

#[derive(Tool)]
#[tool(
    effect = "write",
    description = "Append one actionable todo item to the todo list. Call once per item."
)]
struct AddTodo;

#[async_trait::async_trait]
impl ToolHandler for AddTodo {
    type Input = AddTodoInput;
    type Output = AddTodoOutput;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        input: AddTodoInput,
    ) -> Result<ToolOutcome<AddTodoOutput>, HandlerError> {
        let path = todo_path();
        // The one real side effect in this example. Because the runtime
        // recorded the intent before calling us, this line is written at most
        // once even across a crash and resume.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(HandlerError::new)?;
        let item = input.item.trim().to_owned();
        writeln!(file, "{item}").map_err(HandlerError::new)?;

        let contents = std::fs::read_to_string(&path).map_err(HandlerError::new)?;
        let total = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        Ok(ToolOutcome::Output(AddTodoOutput {
            written: item,
            total,
        }))
    }
}

// --- Tool 2: list_todos (Read effect) -----------------------------------
//
// A `Read`-effect tool has no persistent side effect, so the runtime is free
// to re-execute it if a crash lands before its result was recorded.

/// `list_todos` takes no arguments; an empty object satisfies its schema.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListTodosInput {}

/// What `list_todos` hands back to the model.
#[derive(Debug, Serialize)]
struct ListTodosOutput {
    /// Every todo currently in the file, in order.
    items: Vec<String>,
}

#[derive(Tool)]
#[tool(
    effect = "read",
    description = "Return every todo currently on the list, in order."
)]
struct ListTodos;

#[async_trait::async_trait]
impl ToolHandler for ListTodos {
    type Input = ListTodosInput;
    type Output = ListTodosOutput;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _input: ListTodosInput,
    ) -> Result<ToolOutcome<ListTodosOutput>, HandlerError> {
        let contents = std::fs::read_to_string(todo_path()).unwrap_or_default();
        let items = contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(ToolOutcome::Output(ListTodosOutput { items }))
    }
}

const SYSTEM_PROMPT: &str = "You are a todo-triage assistant. The user gives you \
a brain dump. Break it into 2 to 4 concrete, actionable todos and record each \
one by calling add_todo exactly once. When every item is recorded, stop calling \
tools and reply with a one-sentence summary of what you filed.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The model key. A real console API key uses the default ApiKey scheme
    // (the `x-api-key` header). DEMO_ANTHROPIC_API_KEY is checked first so this
    // example can run against a demo key without disturbing ANTHROPIC_API_KEY.
    let key = std::env::var("DEMO_ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .ok();
    let Some(key) = key else {
        eprintln!(
            "no API key found. Set ANTHROPIC_API_KEY (or DEMO_ANTHROPIC_API_KEY) and re-run."
        );
        return Ok(());
    };
    let config = Config::new()
        .with_api_key(key)
        .with_auth_kind(AuthKind::ApiKey);

    // The agent is pure data: model, prompt, two tools, and budgets. The cost
    // budget needs pricing (the builder rejects a cost budget without it), so
    // the real per-token rates go in alongside. Budgets are enforced by the
    // runtime between steps, not by the prompt.
    let agent = Agent::builder()
        .model(config, "claude-opus-4-8")
        .system_prompt(SYSTEM_PROMPT)
        .tool(AddTodo)
        .tool(ListTodos)
        .budgets(Budgets {
            max_steps: Some(12),
            max_cost_usd: Some(0.50),
            ..Budgets::default()
        })
        .pricing(PRICING)
        .build()?;

    // The store holds the run's event log and nothing else. A file-backed
    // SQLite store (WAL mode, synchronous=FULL) means every appended event has
    // reached durable storage before the append returns. Drop the runtime,
    // kill the process, redeploy: the log is all the state there is.
    let store: Arc<dyn EventStore> = Arc::new(SqliteStore::open("/tmp/salvor-todo-agent.sqlite")?);
    let runtime = Runtime::new(store.clone());

    // Two ways in: recover a crashed run named by RESUME_RUN_ID, or start a
    // fresh one. `recover` replays the recorded log and continues live from the
    // first unrecorded step; recorded model and tool calls are never re-run.
    let (run_id, outcome) = if let Ok(id_str) = std::env::var("RESUME_RUN_ID") {
        let run_id = RunId::from_uuid(id_str.parse()?);
        println!("recovering run {}", run_id.as_uuid());
        (run_id, runtime.recover(&agent, run_id).await?)
    } else {
        // Mint and print the id BEFORE driving, so a kill -9 at any point still
        // leaves you an id to resume with.
        let run_id = RunId::new();
        println!("started run {}", run_id.as_uuid());
        println!(
            "if this process dies, recover it with: RESUME_RUN_ID={} cargo run -p salvor-runtime --example todo_agent",
            run_id.as_uuid()
        );
        let input = std::env::var("TODO_INPUT").unwrap_or_else(|_| {
            "Ship the launch newsletter, the fridge is empty, and quarterly taxes \
             are due next week."
                .to_owned()
        });
        let outcome = runtime.start_with_id(&agent, run_id, json!(input)).await?;
        (run_id, outcome)
    };

    match outcome {
        RunOutcome::Completed { output, .. } => {
            println!("\ncompleted: {output}");
            let contents = std::fs::read_to_string(todo_path()).unwrap_or_default();
            println!("\ntodo file ({}):\n{contents}", todo_path().display());
        }
        RunOutcome::Parked { reason, .. } => {
            // This agent is not expected to park, but any run can, so the
            // batteries-included tier makes it a first-class outcome.
            println!("\nparked: {reason:?}");
            println!(
                "resume once you can supply the requested input; this binary would \
                 call runtime.resume(&agent, run_id, input)."
            );
        }
    }

    // The event log doubles as the audit log. Derive the run's state from it to
    // report event count, token usage, and the observed cost against the rail.
    let log = store.read_log(run_id).await?;
    let state = derive_state(&log);
    let cost = PRICING.cost_usd(state.usage.input_tokens, state.usage.output_tokens);
    println!(
        "\n{} events, {} input + {} output tokens, ${cost:.4} of the $0.50 budget",
        log.len(),
        state.usage.input_tokens,
        state.usage.output_tokens,
    );
    Ok(())
}

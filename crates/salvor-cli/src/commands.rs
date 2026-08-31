//! The command handlers: one function per verb, each taking parsed arguments
//! and the resolved store path and returning a process exit code.
//!
//! # Exit codes
//!
//! A handler returns `Ok(0)` for success, `Ok(1)` for a deliberate refusal
//! that is not an internal error (resuming a run that needs human
//! reconciliation), and `Err(..)` for a genuine failure the top level reports
//! and turns into a non-zero exit.
//! Parking is **not** a failure: a run that suspends or hits a budget exits
//! `0` with a report telling the operator how to continue it.
//!
//! # Progress and stdout discipline
//!
//! Command output (the final result, the tables, the reports) goes to stdout.
//! Progress goes to the tracing log on stderr, so a caller can pipe stdout
//! cleanly. The run id is printed to stdout **first**, before the run drives,
//! so an operator can copy it and `resume` even after a `kill -9`.
//!
//! Progress streams live. `salvor-runtime` emits one info-level record at each
//! persist (see `salvor_runtime`'s progress module), carrying the run id and
//! sequence number, the instant the event becomes durable. So this module does
//! not walk the log after the drive: it just lets the runtime's records flow to
//! the subscriber on stderr as the run drives. A resumed or recovered run
//! replays its recorded prefix silently (those events are not re-persisted) and
//! streams only its genuinely new activity, which is what progress should mean.
//! The full recorded log, replayed prefix and all, is always available through
//! `salvor history`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error as StdError;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use salvor_core::{
    Event, EventEnvelope, PendingCall, RunId, RunStatus, derive_state, log_is_client_driven,
};
use salvor_engine::{
    EngineError, ForkError, GraphOutcome, ToolResolver, WriteHazard, graph_hash, plan_fork,
    run_graph,
};
use salvor_graph::{Graph, Node, ToolNode};
use salvor_runtime::{
    Agent, ParkReason, RunCtx, RunOutcome, Runtime, RuntimeError, validate_labels,
};
use salvor_server::dispatch::{Disposition, classify};
use salvor_server::{
    AgentDefinition, AgentFactory, AppState, BuiltAgent, ClientToolDecl, ClientToolRegistry,
    DefFormat, LlmModelExecutor, ToolRegistry,
};
use salvor_store::{EventStore, SqliteStore, StoreError};
use salvor_tools::DynTool;
use salvor_tools::mcp::McpServer;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::agent_config::{self, AgentConfig, AgentConfigExt};
use crate::anchor;
use crate::checkout;
use crate::cli::{
    AbandonArgs, AgentHashArgs, AgentValidateArgs, AnchorArgs, BuildArgs, CompletionsArgs,
    ForkArgs, GraphRunArgs, GraphValidateArgs, HistoryArgs, ListArgs, ReplayArgs, ResolveArgs,
    ResumeArgs, RunArgs, ServeArgs, VerifyArgs, WakeArgs,
};
use crate::dev_server::DevServer;
use crate::render;
use crate::serve_kill;

/// `salvor run`: start a fresh run, print its id, drive it, and report.
///
/// `--fixture <DIR>` is the offline variant: the agent, the input, and a
/// recorded model conversation all come from one directory, and a scripted
/// model is stood up locally to serve that conversation (see the `fixture`
/// module in this crate).
/// It changes only where those three things come from: everything below the
/// resolution is the same store, the same runtime, and the same event log a
/// `--agent`/`--input` run gets.
pub async fn run(store_path: &Path, caller: Option<&str>, args: RunArgs) -> Result<u8> {
    // Resolve the two ways of naming a run into the one shape the rest of this
    // handler works in. clap already guarantees exactly one of them is present
    // (see `RunArgs`), so the `None` arms below are unreachable in practice;
    // they still return an actionable message rather than panicking, because a
    // handler is not the place to trust a parse tree absolutely.
    let fixture = args
        .fixture
        .as_deref()
        .map(crate::fixture::Fixture::load)
        .transpose()?;
    let (agent_path, input) = match &fixture {
        Some(fixture) => (fixture.agent_path().to_owned(), fixture.input().clone()),
        None => {
            let agent = args
                .agent
                .clone()
                .context("`salvor run` needs --agent <file>, or --fixture <dir>")?;
            let raw = args
                .input
                .as_deref()
                .context("`salvor run` needs --input <json|@file>, or --fixture <dir>")?;
            (agent, parse_input(raw)?)
        }
    };

    let config = AgentConfig::load(&agent_path)?;
    let store = open_store(store_path)?;

    // The fixture's model binds a local port and exports the agent's declared
    // `base_url_env` at it. This must happen BEFORE the agent is built: the
    // client config reads that variable once, at build time.
    let model = match &fixture {
        Some(fixture) => Some(fixture.start_model(&config).await?),
        None => None,
    };

    // From here the fixture path and the ordinary path are the same code. Any
    // failure between the model starting and the run finishing tears it down
    // first, the way `close_servers` runs regardless of outcome below.
    let (agent, servers) = match agent_config::build_agent(&config, &agent_path, false).await {
        Ok(built) => built,
        Err(error) => {
            shutdown_model(model).await;
            return Err(error);
        }
    };
    // The agent carries the resolved prompt-recording flag (per-agent config
    // over SALVOR_RECORD_PROMPTS over off); hand it to the runtime so the
    // RunCtx driving this run records the body only when opted in.
    let mut runtime = with_caller(
        Runtime::new(store.clone()).with_record_prompts(agent.record_prompts()),
        caller,
    );
    // Labels set on the agent definition (via the Rust builder; there is no
    // TOML surface for them yet) ride along the same way record_prompts does.
    if let Some(labels) = agent.labels() {
        runtime = runtime.with_labels(labels.clone());
    }

    let run_id = RunId::new();
    let uuid = run_id.as_uuid().to_string();
    // Printed first, so a kill mid-run still leaves the operator an id to resume.
    println!("run {uuid}");
    tracing::info!(run_id = %uuid, "starting run");

    // Progress streams live from the runtime as the loop drives; this call
    // returns only once the run has completed or parked. `close_servers` runs
    // regardless of the outcome, so a teardown always happens before the `?`.
    let outcome = runtime.start_with_id(&agent, run_id, input).await;
    close_servers(servers).await;
    shutdown_model(model).await;

    report_outcome(
        outcome.map_err(|error| contextualize_auth_error(error, &config))?,
        &uuid,
        &agent_path,
        store_path,
    )
}

/// Turns a 401 from the Messages API into a message that names the
/// configuration, rather than one that only names the HTTP status.
///
/// A raw `salvor_llm::Error::Api` with `status: 401` says the request was
/// rejected; it says nothing about WHERE Salvor read the key from, because
/// `salvor-llm` builds requests from an already-resolved [`salvor_llm::Config`]
/// and never sees the agent file's `[llm] api_key_env`. That name lives only in
/// the [`AgentConfig`] this handler already loaded, so it is added here, at the
/// one seam where both the error and the config are in scope. Every other
/// `RuntimeError` passes through unchanged.
fn contextualize_auth_error(error: RuntimeError, config: &AgentConfig) -> anyhow::Error {
    let RuntimeError::Model(salvor_llm::Error::Api(ref api)) = error else {
        return error.into();
    };
    if api.status != 401 {
        return error.into();
    }
    let key_env = config.api_key_env();
    anyhow::anyhow!(
        "authentication failed calling the Messages API (HTTP 401: {}). This agent's \
         [llm] block reads the API key from the `{key_env}` environment variable \
         (the default is `{default}` when `api_key_env` is not set in the file). \
         Set `{key_env}` to a valid key and try again.",
        api.message,
        default = agent_config::DEFAULT_API_KEY_ENV,
    )
}

/// `salvor resume`: continue an existing run, dispatching on its derived state.
///
/// The mapping (task §3): a run that needs reconciliation is refused with
/// evidence; a parked run (suspended or budget-exceeded) needs `--input` and
/// resumes; a crashed run (running or awaiting a step) recovers with no input;
/// a finished run is reported and left alone.
pub async fn resume(store_path: &Path, caller: Option<&str>, args: ResumeArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let uuid = run_id.as_uuid().to_string();
    let store = open_store(store_path)?;
    let log = store.read_log(run_id).await?;
    if log.is_empty() {
        bail!("no run {uuid} in this store");
    }
    let state = derive_state(&log);

    // The state-to-verb mapping is the shared `classify` the control-plane
    // server uses too, so the CLI and the HTTP resume endpoint can never
    // disagree on what a given state means. These first arms need no agent and
    // spawn no MCP servers, so decide them before paying to build the agent.
    let disposition = classify(&state);
    match disposition {
        Disposition::Reconcile(_) => {
            // The intent's timestamp is part of the evidence; find it in the
            // log by the pending call's sequence number.
            let recorded_at = match state.pending_call.as_ref() {
                Some(PendingCall::Tool { seq, .. }) => log
                    .iter()
                    .find(|envelope| envelope.seq == *seq)
                    .map(|envelope| envelope.recorded_at),
                _ => None,
            };
            print!(
                "{}",
                render::reconciliation_report(
                    &uuid,
                    state.pending_call.as_ref(),
                    recorded_at,
                    render::DEFAULT_REPORT_WIDTH,
                )
            );
            return Ok(1);
        }
        Disposition::Completed(output) => {
            println!("run {uuid} already completed. Final output:");
            println!("{}", render::pretty_json(&output));
            return Ok(0);
        }
        Disposition::Failed(error) => {
            println!("run {uuid} already failed: {error}");
            return Ok(0);
        }
        Disposition::Abandoned {
            reason,
            unresolved_write,
        } => {
            match reason {
                Some(reason) => println!("run {uuid} was abandoned: {reason}"),
                None => println!("run {uuid} was abandoned"),
            }
            if let Some(write) = unresolved_write {
                println!(
                    "  the write at seq {} ({}) was left unresolved and is recorded as such; \
                     its effect stays unknown",
                    write.seq.get(),
                    write.tool
                );
            }
            return Ok(0);
        }
        // A due run drives on through the recover path below, which is the
        // whole of waking; `salvor wake` reaches it by calling this very
        // function for each run its sweep found due. An early one is refused
        // and told how long is left, because the drive would record nothing
        // and a silent no-op reads like a bug.
        Disposition::Sleeping { wake_at } => {
            let now = OffsetDateTime::now_utc();
            if now < wake_at {
                print!(
                    "{}",
                    render::sleeping_report(
                        &uuid,
                        wake_at,
                        wake_at - now,
                        &args.agents,
                        args.graph.as_deref(),
                        Some(store_path),
                        render::DEFAULT_REPORT_WIDTH,
                    )
                );
                return Ok(1);
            }
        }
        Disposition::NotStarted => bail!("run {uuid} has no recorded events"),
        Disposition::Resume(_) | Disposition::Recover => {}
    }

    // A graph run re-drives over the engine, not the built-in loop. The classify
    // above, the parked-vs-crashed decision, and the input handling are shared;
    // only the re-drive differs, because the log records the graph's hash, not
    // the document. See `resume_graph`.
    if is_graph_run(&log) {
        return resume_graph(store, run_id, &uuid, &log, &args, disposition, store_path).await;
    }

    // An agent run rebuilds its one agent. Exactly one `--agent` is expected.
    let agent_path = single_agent(&args.agents)?;
    let config = AgentConfig::load(agent_path)?;
    let (agent, servers) = agent_config::build_agent(&config, agent_path, false).await?;
    let mut runtime = with_caller(
        Runtime::new(store.clone()).with_record_prompts(agent.record_prompts()),
        caller,
    );
    if let Some(labels) = agent.labels() {
        runtime = runtime.with_labels(labels.clone());
    }

    // A run woken on schedule and a run recovered after a crash both continue
    // through `recover`, so the disposition is the only thing that can tell
    // them apart in the log. It has to: a scheduled wake that logs a crash
    // sends an operator hunting a fault that never happened.
    let waking = matches!(disposition, Disposition::Sleeping { .. });
    let outcome = match disposition {
        Disposition::Resume(_) => {
            let raw = args.input.as_deref().context(
                "this run is parked awaiting input; pass --input <json|@file> to resume it",
            )?;
            let input = parse_input(raw)?;
            tracing::info!(run_id = %uuid, "resuming parked run");
            runtime.resume(&agent, run_id, input).await
        }
        // Recover: the process died mid-step (running or awaiting a step), or
        // its timer came due.
        _ => {
            if args.input.is_some() {
                if waking {
                    tracing::warn!(
                        run_id = %uuid,
                        "a sleeping run takes no input; --input is ignored when waking it"
                    );
                } else {
                    tracing::warn!(
                        run_id = %uuid,
                        "this run crashed mid-step; --input is ignored when recovering"
                    );
                }
            }
            if waking {
                tracing::info!(run_id = %uuid, "waking sleeping run");
            } else {
                tracing::info!(run_id = %uuid, "recovering crashed run");
            }
            runtime.recover(&agent, run_id).await
        }
    };

    close_servers(servers).await;
    report_outcome(
        outcome.map_err(|error| contextualize_auth_error(error, &config))?,
        &uuid,
        agent_path,
        store_path,
    )
}

/// `salvor wake`: drive every run whose durable timer has come due, then exit.
///
/// Nothing wakes a sleeping run on its own. A run parked on a timer is passive
/// data with the deadline recorded in its log, and it moves again only when
/// something re-drives it; `salvor serve` has a sweeper doing that on an
/// interval, and this verb is the same job for an operator with no long-lived
/// server, one pass per invocation, cron-shaped.
///
/// It routes every due run through [`resume`], because waking a run IS resuming
/// it: a sleeping run continues with no input supplied, which is exactly what
/// `resume` already does for a run it classifies as recoverable, and
/// [`RunCtx::await_wake`](salvor_runtime::RunCtx::await_wake) enforces the
/// deadline itself against the clock. So this handler holds no drive loop of
/// its own, and `--agent`/`--graph` are `resume`'s flags with `resume`'s
/// validation: whatever that verb needs to rebuild a run, this one needs too.
///
/// # Exit code
///
/// `0` unless a drive genuinely failed. A run that woke and went straight back
/// to sleep, or woke and parked at a gate, is ordinary operation and reported
/// as such; a cron entry that alerted on either would alert on nothing being
/// wrong. Nor is a run another driver got to first: two sweeps racing at the
/// same due run is exactly what the exactly-once guarantee is for, and the
/// loser's job was done for it. What does fail is a run this invocation could
/// not drive at all: a missing `--agent` or `--graph`, a document whose hash
/// does not match, a divergence. That exits `1`, but only after every other due
/// run has still had its turn, so one unwakeable run never costs the rest their
/// sweep.
///
/// A `--dry-run` follows the same rule against the same question asked without
/// driving: `1` when a due run could not be woken with the files given, or when
/// a file the operator gave could not itself be loaded even if no due run
/// currently needs it (see [`check_wake_files`]), so the crontab line an
/// operator is about to save can be smoke-tested by running it.
pub async fn wake(store_path: &Path, caller: Option<&str>, args: WakeArgs) -> Result<u8> {
    let store = open_store(store_path)?;
    // The real clock, read once, so every run in this sweep is measured
    // against one instant rather than against a time that drifts as the sweep
    // works through the list.
    let now = OffsetDateTime::now_utc();
    let due = salvor_runtime::due_runs(store.as_ref(), now).await?;

    if due.is_empty() {
        match next_sleeping_deadline(store.as_ref()).await? {
            Some(wake_at) => println!(
                "nothing to wake: the next run in {} is due at {} (in {})",
                store_path.display(),
                render::format_ts(wake_at),
                render::format_duration(wake_at - now)
            ),
            None => println!(
                "nothing to wake: no run in {} is sleeping",
                store_path.display()
            ),
        }
        return Ok(0);
    }

    if args.dry_run {
        let file_failures = check_wake_files(&args);
        let previews = preview_due_runs(store.as_ref(), &due, &args).await?;
        print!("{}", render_wake_preview(&file_failures, &previews, now));
        let unwakeable = previews
            .iter()
            .filter(|preview| preview.readiness.is_blocked())
            .count();
        return Ok(u8::from(unwakeable > 0 || !file_failures.is_empty()));
    }

    println!(
        "{} run(s) due as of {now}:",
        due.len(),
        now = render::format_ts(now)
    );
    let mut failures = 0_usize;
    let mut taken = 0_usize;
    for run in &due {
        let uuid = run.run_id.as_uuid().to_string();
        println!("\n{uuid} (due {})", render::format_ts(run.wake_at));
        // The log as this sweep found it. A drive that fails reads the same
        // whether nothing could drive the run or another driver had already
        // driven it; what the run looked like before and what it reads as
        // after is what tells those apart (see `classify_failed_wake`). A read
        // that fails here is this run's own failure, not the whole sweep's: it
        // is reported on this run's line and the sweep still moves on to the
        // next due run, exactly as a drive failure does below.
        let log_before = match store.read_log(run.run_id).await {
            Ok(log) => log,
            Err(error) => {
                failures += 1;
                println!(
                    "  {}",
                    describe_unreadable(&uuid, ReadTiming::BeforeTheDrive, &error)
                );
                continue;
            }
        };
        // A client-driven run's own client wakes it on its own drive lease;
        // re-driving it here would make this sweep a second writer racing
        // that lease for the same positions. The server's wake sweeper
        // already leaves these alone (see `salvor_server::wake`); this sweep
        // must too, so it is neither driven nor counted as a failure below.
        if log_is_client_driven(&log_before) {
            println!("  {uuid} is client-driven; its client wakes it, this sweep left it alone");
            continue;
        }
        let events_before = log_before.len();
        let outcome = resume(
            store_path,
            caller,
            ResumeArgs {
                run_id: uuid.clone(),
                agents: args.agents.clone(),
                graph: args.graph.clone(),
                input: None,
            },
        )
        .await;
        // What the re-drive actually left behind, folded from the log it just
        // wrote, so the report states the run's state rather than assuming the
        // drive's own report covered it. This read can fail on its own, apart
        // from whatever the drive itself did (a store hiccup between the drive
        // and this read, the torn-read failure mode `salvor-store` is fixed
        // against separately); when it does, this run's outcome cannot be
        // told, so it is reported as unreadable rather than aborting every run
        // still waiting for its turn.
        let log = match store.read_log(run.run_id).await {
            Ok(log) => log,
            Err(error) => {
                failures += 1;
                println!(
                    "  {}",
                    describe_unreadable(&uuid, ReadTiming::AfterTheDrive, &error)
                );
                continue;
            }
        };
        let status = derive_state(&log).status;
        match outcome {
            Ok(_) => println!("  {uuid} is now {}", render::status_label(&status)),
            Err(error) => {
                match classify_failed_wake(&error, run.wake_at, events_before, &status, log.len()) {
                    FailedWake::TakenByAnotherDriver => {
                        taken += 1;
                        tracing::info!(
                            run_id = %uuid,
                            %error,
                            "another driver moved this run while this drive was failing"
                        );
                        println!("  {uuid} {}", describe_taken(&status));
                    }
                    FailedWake::NotWoken => {
                        failures += 1;
                        println!("  {uuid} was not woken: {error:#}");
                    }
                }
            }
        }
    }

    if taken > 0 {
        println!(
            "\n{taken} of {} due run(s) were driven by something else while this sweep ran; \
             exactly one driver wakes a run and it was not this one",
            due.len()
        );
    }
    if failures > 0 {
        println!(
            "\n{failures} of {} due run(s) could not be driven; every due run was tried, \
             and each line above says what it needs",
            due.len()
        );
        return Ok(1);
    }
    Ok(0)
}

/// The earliest wake instant among every run this store holds that folds to
/// [`RunStatus::Sleeping`], or `None` when not one of its runs is sleeping at
/// all.
///
/// Called only once a sweep already knows nothing is DUE, to answer the
/// question that "nothing to wake" alone cannot: whether there are no timers
/// in this store to begin with, or whether the nearest one simply has not
/// come due yet. That distinction is exactly what `salvor list` already folds
/// per run (see its handler above), so this walks every run's log the same
/// way `list` does rather than inventing a second way to derive a status.
async fn next_sleeping_deadline(store: &dyn EventStore) -> Result<Option<OffsetDateTime>> {
    let summaries = store.list_runs().await?;
    let mut earliest: Option<OffsetDateTime> = None;
    for summary in summaries {
        let log = store.read_log(summary.run_id).await?;
        if let RunStatus::Sleeping { wake_at } = derive_state(&log).status {
            earliest = Some(match earliest {
                Some(current) if current <= wake_at => current,
                _ => wake_at,
            });
        }
    }
    Ok(earliest)
}

/// What a sweep should say about one due run whose drive returned an error.
///
/// Two very different things arrive as the same `Err`. A run nothing could
/// rebuild, or one whose drive broke partway, is a genuine failure of this
/// invocation. A run another driver was waking at the same moment refuses this
/// one at the store (`database is locked`), which is the exactly-once
/// guarantee doing its job: the run is fine, the work happened, and this
/// process simply lost the race. Reporting the second as the first tells an
/// operator to re-drive a completed run and alerts a crontab on nothing being
/// wrong.
///
/// Nothing here reads the error's text, which is a storage backend's wording
/// and no contract of ours. Two typed signals decide it instead.
///
/// The first is [`StoreError::Conflict`] anywhere in the error's chain: the
/// store refusing this drive's append because that position was already
/// taken. Only another writer on the same run can produce it, so it names the
/// race outright, whatever the run happens to read as at the instant this
/// sweep looks (mid-drive, a winner's run reads `running`).
///
/// The second is the run's own state, for the loser that arrives late enough
/// to find the work already done and never gets as far as an append. It rests
/// on what a FAILED drive can leave behind: a drive that parked a run or
/// finished it returns success, so a run found parked at a gate, out of
/// budget, completed, abandoned, or asleep on a NEW deadline was driven there
/// by something, and that something was not this failing drive. Every other
/// state, this drive could have left itself (a half-driven run reads
/// `running`, and a permanent refusal records `failed`), so it stays the
/// failure it looks like rather than being explained away as someone else's
/// work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedWake {
    /// The drive's error is the news: this sweep failed to do its job, whether
    /// it left the run untouched or broke partway through it.
    NotWoken,
    /// Another driver moved the run while this drive was failing. The run's
    /// own state is the news; the error describes this process, not the run.
    TakenByAnotherDriver,
}

/// Which of the sweep's two reads of one due run's log failed to read at all:
/// see [`describe_unreadable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTiming {
    /// The read the sweep takes before driving the run, to learn how many
    /// events it starts with (see `classify_failed_wake`'s `events_before`).
    BeforeTheDrive,
    /// The read the sweep takes after driving the run, to fold its resulting
    /// status and report it.
    AfterTheDrive,
}

/// What a sweep prints for one due run whose log this invocation could not
/// read at all, before or after the drive.
///
/// A run in this state is not classified against [`classify_failed_wake`]:
/// that decision needs the log this read failed to produce (a folded status,
/// an event count), so there is nothing to classify against. It is reported
/// plainly as not woken instead, the same as any other run this sweep failed
/// to do its job for, and the sweep still moves on to the one after it.
///
/// Split out from the loop so the wording is a unit test rather than a rerun
/// of the whole binary. Proving the sweep truly continues past a run in this
/// state, rather than aborting, needs a store that fails one read mid-sweep;
/// that is the same distance from a hermetic test as staging the real race
/// [`classify_failed_wake`] is proven against, which this module's docs
/// explain is left to a test that injects the failure directly rather than
/// racing two real drivers.
#[must_use]
pub fn describe_unreadable(uuid: &str, when: ReadTiming, error: &StoreError) -> String {
    match when {
        ReadTiming::BeforeTheDrive => {
            format!("{uuid} was not woken: could not read its log before driving it: {error}")
        }
        ReadTiming::AfterTheDrive => {
            format!("{uuid} was not woken: could not re-read {uuid} after the drive: {error}")
        }
    }
}

/// What a sweep says about a run [`FailedWake::TakenByAnotherDriver`] was
/// classified for, once the status folded from its log is in hand.
///
/// A folded status is only trustworthy news here when it is one a FINISHED or
/// PARKED drive leaves behind: [`RunStatus::Completed`], [`RunStatus::Failed`],
/// [`RunStatus::Abandoned`], [`RunStatus::Suspended`],
/// [`RunStatus::BudgetExceeded`], or [`RunStatus::Sleeping`]. Naming the
/// status is safe there because nothing but a driver that got all the way
/// through could have left the run in one of those. Every other status this
/// sweep might read (`Running`, `AwaitingModel`, `AwaitingTool`,
/// `NeedsReconciliation`, `NotStarted`) is one the *other* driver's still-open
/// write could leave behind while it is mid-drive, so naming it would read as
/// a diagnosis (`needs-reconciliation`, say) of a run that is merely being
/// worked on by someone else. That case gets a status-free sentence instead.
#[must_use]
pub fn describe_taken(status: &RunStatus) -> String {
    if matches!(
        status,
        RunStatus::Completed { .. }
            | RunStatus::Failed { .. }
            | RunStatus::Abandoned { .. }
            | RunStatus::Suspended { .. }
            | RunStatus::BudgetExceeded { .. }
            | RunStatus::Sleeping { .. }
    ) {
        format!(
            "was picked up by another driver and is now {}",
            render::status_label(status)
        )
    } else {
        "was picked up by another driver, which is still driving it; this sweep recorded nothing"
            .to_owned()
    }
}

/// Decides which of the two a failed drive was, from the error it returned,
/// the deadline the sweep found the run at, and the log before and after. See
/// [`FailedWake`].
#[must_use]
pub fn classify_failed_wake(
    error: &anyhow::Error,
    due_at: OffsetDateTime,
    events_before: usize,
    status_after: &RunStatus,
    events_after: usize,
) -> FailedWake {
    // The store arbitrating two writers on one run. It says what no reading of
    // the run's state can say on its own: the winner may still be mid-drive,
    // so the run reads `running` and looks for all the world like a run this
    // sweep broke halfway through.
    if lost_a_position_race(error) {
        return FailedWake::TakenByAnotherDriver;
    }
    // Exactly as the sweep found it: nothing appended, still asleep on the
    // deadline this sweep picked it up for. Nothing drove it, this one least
    // of all.
    let untouched = events_after == events_before
        && matches!(status_after, RunStatus::Sleeping { wake_at } if *wake_at == due_at);
    if untouched {
        return FailedWake::NotWoken;
    }
    match status_after {
        RunStatus::Completed { .. }
        | RunStatus::Abandoned { .. }
        | RunStatus::Suspended { .. }
        | RunStatus::BudgetExceeded { .. } => FailedWake::TakenByAnotherDriver,
        // A new deadline means a drive got past this timer and recorded the
        // next one.
        RunStatus::Sleeping { wake_at } if *wake_at != due_at => FailedWake::TakenByAnotherDriver,
        _ => FailedWake::NotWoken,
    }
}

/// Whether this one error IS the store refusing an append because that
/// position was already taken.
///
/// Three shapes, because [`StoreError::Conflict`] reaches a caller wearing
/// whichever coats the layers it passed through put on it: bare from the
/// store, inside [`RuntimeError::Store`] from a persist, and inside
/// [`EngineError::Runtime`] on top of that from a graph drive. None of the
/// three is reachable by walking `source` alone. The runtime's variant carries
/// the store error by value rather than as a `source`, and the engine's is
/// `#[error(transparent)]`, which forwards `source` to the runtime error's own
/// (`None`) and so hides the runtime error as a link too. Every coat has to be
/// opened by name.
fn is_position_conflict(cause: &(dyn StdError + 'static)) -> bool {
    matches!(
        cause.downcast_ref::<StoreError>(),
        Some(StoreError::Conflict { .. })
    ) || matches!(
        cause.downcast_ref::<RuntimeError>(),
        Some(RuntimeError::Store(StoreError::Conflict { .. }))
    ) || matches!(
        cause.downcast_ref::<EngineError>(),
        Some(EngineError::Runtime(RuntimeError::Store(
            StoreError::Conflict { .. }
        )))
    )
}

/// Whether anything in this error's chain is that refusal.
fn lost_a_position_race(error: &anyhow::Error) -> bool {
    error.chain().any(is_position_conflict)
}

/// The same question of a drive error that has not been wrapped in an
/// [`anyhow::Error`] yet, walking its own `source` chain.
fn drive_lost_a_position_race(error: &EngineError) -> bool {
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(cause) = current {
        if is_position_conflict(cause) {
            return true;
        }
        current = cause.source();
    }
    false
}

/// One due run as `--dry-run` reports it.
struct WakePreview {
    /// The run's id.
    uuid: String,
    /// The deadline its log recorded.
    wake_at: OffsetDateTime,
    /// What the log says the run is and what it recorded, ready to print:
    /// `graph run, recorded document sha256:...`.
    identity: String,
    /// Whether the files this invocation was given would wake it.
    readiness: WakeReadiness,
}

/// Whether waking one due run with the files given would get as far as a drive.
enum WakeReadiness {
    /// They would: `with` is the file the run would be rebuilt from.
    Ready {
        /// The `--agent` or `--graph` file that satisfies the run.
        with: String,
    },
    /// They would not, and this is the refusal a real wake gives, verbatim,
    /// because it comes from the same resolution the drive runs.
    Blocked {
        /// The refusal, already formatted with its context chain.
        refusal: String,
    },
    /// The run is client-driven (`driven_by: client` on its `RunStarted`), so
    /// no file given here would ever drive it: a real wake leaves it alone
    /// regardless, the same as [`wake`]'s non-dry-run sweep. Not a
    /// [`Self::Blocked`] refusal, since nothing about the files given
    /// is at fault, and not counted against the dry run's exit code.
    ClientDriven,
}

impl WakeReadiness {
    /// Whether this run could not be woken with the files given, which is what
    /// decides a dry run's exit code.
    fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }
}

/// One `--agent` or `--graph` file the operator gave `wake --dry-run` that
/// could not itself be loaded, checked independently of which run kind
/// actually happens to be due today. See [`check_wake_files`].
struct FileCheckFailure {
    /// The flag and file, e.g. `--agent agents/writer.toml`.
    what: String,
    /// The load or parse error, already formatted with its context chain.
    error: String,
}

/// Opens and parses every file the operator gave `wake --dry-run`, regardless
/// of whether any due run actually needs it: every `--agent` file (through
/// [`AgentConfig::load`], the same parse a real drive performs) and, when
/// `--graph` is given, the graph document (through [`load_and_validate_graph`],
/// the same strict parse `graph validate` runs).
///
/// This is what catches a typo in a flag no run *currently* due happens to
/// need: [`preview_due_runs`]'s per-run readiness check only exercises the
/// files a given run's kind actually asks for (an agent run never looks at
/// `--graph`, a graph run never looks at `--agent`), so a crontab line with a
/// bad path on the side it is not using today would otherwise preview clean
/// and only fail once some other, currently-sleeping run needed it.
fn check_wake_files(args: &WakeArgs) -> Vec<FileCheckFailure> {
    let mut failures = Vec::new();
    for path in &args.agents {
        if let Err(error) = AgentConfig::load(path) {
            failures.push(FileCheckFailure {
                what: format!("--agent {}", path.display()),
                error: format!("{error:#}"),
            });
        }
    }
    if let Some(graph_path) = &args.graph
        && let Err(error) = load_and_validate_graph(graph_path)
    {
        failures.push(FileCheckFailure {
            what: format!("--graph {}", graph_path.display()),
            error: format!("{error:#}"),
        });
    }
    failures
}

/// Asks of every due run what a real wake would ask of it, without driving
/// anything: what kind of run it is, what it recorded, and whether the
/// `--agent`/`--graph` files given satisfy it.
///
/// The refusals are the drive's own ([`resolve_graph_document`],
/// [`single_agent`], the agent-file parse), so a dry run's answer is the real
/// answer and a crontab line can be checked by running it with `--dry-run`
/// before it is saved.
///
/// What a preview deliberately does not do is build anything: no MCP server is
/// spawned and no agent is constructed. So the definition-level checks that
/// need a built agent (an agent run's recorded `agent_def_hash`, which covers
/// the MCP tool schemas, and a graph's `agent` or `tool` nodes resolving to a
/// tool some given agent actually carries) still happen at the drive, and a
/// preview says only that the files are the right shape for the run, not that
/// the run will find every tool it needs inside them.
///
/// One shape of that gap needs no built agent to see, though: a graph whose
/// document has a `tool` node at all, with no `--agent` file given for the
/// drive to ever check that tool name against. That is not a question of
/// whether the right agent was supplied, only of whether any agent was, so
/// this walks the resolved document's nodes and blocks on it directly,
/// naming the tool node, rather than reporting the run "ready" and letting a
/// real wake discover the same gap a step later.
async fn preview_due_runs(
    store: &dyn EventStore,
    due: &[salvor_runtime::DueRun],
    args: &WakeArgs,
) -> Result<Vec<WakePreview>> {
    let mut previews = Vec::with_capacity(due.len());
    for run in due {
        let uuid = run.run_id.as_uuid().to_string();
        let log = store.read_log(run.run_id).await?;
        let is_graph = is_graph_run(&log);
        let identity = if is_graph {
            let recorded = recorded_graph_hash(&log)
                .unwrap_or_else(|| "no document (its log has no GraphRunStarted event)".to_owned());
            format!("graph run, recorded document {recorded}")
        } else {
            let recorded = recorded_agent_def_hash(&log)
                .unwrap_or_else(|| "no definition (its log has no RunStarted event)".to_owned());
            format!("agent run, recorded definition {recorded}")
        };
        // A client-driven run is left alone by a real wake regardless of what
        // files were given, so a dry run says exactly that instead of
        // resolving files against a run no drive will ever touch.
        let readiness = if log_is_client_driven(&log) {
            WakeReadiness::ClientDriven
        } else if is_graph {
            match resolve_graph_document(args.graph.as_deref(), &log, &uuid) {
                Ok((path, document)) => {
                    match tool_node_without_any_agent(&document, &args.agents) {
                        Some(tool) => WakeReadiness::Blocked {
                            refusal: format!(
                                "tool node `{}` names tool `{}`; a graph with tool nodes needs \
                                 at least one --agent file to carry them, and none was given",
                                tool.id, tool.tool
                            ),
                        },
                        None => WakeReadiness::Ready {
                            with: path.display().to_string(),
                        },
                    }
                }
                Err(error) => WakeReadiness::Blocked {
                    refusal: format!("{error:#}"),
                },
            }
        } else {
            match single_agent(&args.agents).and_then(|path| AgentConfig::load(path).map(|_| path))
            {
                Ok(path) => WakeReadiness::Ready {
                    with: path.display().to_string(),
                },
                Err(error) => WakeReadiness::Blocked {
                    refusal: format!("{error:#}"),
                },
            }
        };
        previews.push(WakePreview {
            uuid,
            wake_at: run.wake_at,
            identity,
            readiness,
        });
    }
    Ok(previews)
}

/// The `--dry-run` listing: which files given could not even be loaded, which
/// runs are due, how overdue each is, what each one is, and whether the files
/// given would wake it. Prints and drives nothing, mirroring how `salvor fork
/// --dry-run` previews a fork it does not create.
///
/// `file_failures` (see [`check_wake_files`]) is reported once, right under
/// the header and ahead of the per-run listing, rather than against any one
/// run: it is not any particular run's news, since it holds regardless of
/// which run kind is actually due.
fn render_wake_preview(
    file_failures: &[FileCheckFailure],
    previews: &[WakePreview],
    now: OffsetDateTime,
) -> String {
    let mut out = format!(
        "{} run(s) due as of {} (dry run):\n",
        previews.len(),
        render::format_ts(now)
    );
    for failure in file_failures {
        out.push_str(&format!("  {}: {}\n", failure.what, failure.error));
    }
    let mut unwakeable = 0_usize;
    for preview in previews {
        out.push_str(&format!(
            "  {} due {}, overdue by {}\n",
            preview.uuid,
            render::format_ts(preview.wake_at),
            render::format_duration(now - preview.wake_at)
        ));
        out.push_str(&format!("    {}\n", preview.identity));
        match &preview.readiness {
            WakeReadiness::Ready { with } => {
                out.push_str(&format!("    would wake with {with}\n"));
            }
            WakeReadiness::Blocked { refusal } => {
                unwakeable += 1;
                out.push_str(&format!(
                    "    cannot be woken with these files: {refusal}\n"
                ));
            }
            WakeReadiness::ClientDriven => {
                out.push_str(
                    "    is client-driven; its client wakes it, this sweep left it alone\n",
                );
            }
        }
    }
    if unwakeable == 0 && file_failures.is_empty() {
        out.push_str("nothing was driven. Drop --dry-run to wake these.\n");
    } else {
        out.push_str("nothing was driven.");
        if unwakeable > 0 {
            out.push_str(&format!(
                " {unwakeable} of {} due run(s) could not be woken with the files given; pass \
                 what each one names above.",
                previews.len()
            ));
        }
        if !file_failures.is_empty() {
            out.push_str(&format!(
                " {} file(s) given could not be loaded; see above.",
                file_failures.len()
            ));
        }
        out.push('\n');
    }
    out
}

/// `salvor fork`: fork a graph run from a node boundary into a NEW run, refusing
/// to re-execute a recorded write the operator has not acknowledged.
///
/// The local flavor of the server's `POST /v1/runs/{id}/fork`, mirroring how
/// `graph run` and a graph `resume` re-supply their documents: the origin's log
/// records only the graph's hash, so the document is re-supplied through
/// `--graph` (hash-checked against the recorded one, since a fork reuses the
/// origin's graph unchanged) and its `agent` nodes through `--agent`.
///
/// The fork planning is the shared, pure [`plan_fork`]; only the IO differs from
/// the server. On a hazard the operator has not acknowledged it refuses (exit 1),
/// listing exactly the writes that would re-fire; with `--acknowledge-writes`
/// covering them it writes the child's prefix (the origin's events below the fork
/// node, rewritten under a fresh id, seq-0 carrying the fork origin) and drives
/// the child onward from the fork node exactly as a recovered graph run.
pub async fn fork(store_path: &Path, args: ForkArgs) -> Result<u8> {
    let origin_id = parse_run_id(&args.run_id)?;
    let origin_uuid = origin_id.as_uuid().to_string();
    let store = open_store(store_path)?;
    let origin_log = store.read_log(origin_id).await?;
    if origin_log.is_empty() {
        bail!("no run {origin_uuid} in this store");
    }

    // An origin parked at a dangling write must be resolved first.
    if matches!(
        derive_state(&origin_log).status,
        RunStatus::NeedsReconciliation
    ) {
        bail!(
            "origin run {origin_uuid} is parked at a dangling write; resolve it \
             (salvor resolve {origin_uuid} --output <json>) before forking, so the fork does not \
             inherit an unsettled write"
        );
    }

    // Plan the fork purely: boundary, prefix, and the write hazard set.
    let plan = plan_fork(&origin_log, &args.from_node).map_err(|error| match error {
        ForkError::NotAGraphRun => anyhow::anyhow!(
            "run {origin_uuid} is an agent run, not a graph run; only a graph run has node \
             boundaries to fork from"
        ),
        ForkError::NodeNeverEntered { node } => anyhow::anyhow!(
            "run {origin_uuid} never entered node `{node}`; fork from a node boundary the run reached"
        ),
    })?;

    // The re-supplied document must hash to the origin's recorded graph.
    let graph = load_and_validate_graph(&args.graph)?;
    let supplied_hash = graph_hash(&graph)?;
    if supplied_hash != plan.graph_hash() {
        bail!(
            "the graph in {} hashes to {supplied_hash}, but run {origin_uuid} forked from {}; a fork \
             reuses the SAME document the origin ran (submit a changed graph as a new run instead)",
            args.graph.display(),
            plan.graph_hash()
        );
    }

    // Resolve the acknowledgement: `all` covers the full hazard set, else a
    // comma-separated seq list. Then find what the acknowledgement misses.
    let hazard_seqs = plan.hazard_seqs();
    let acknowledged = parse_acknowledge_writes(args.acknowledge_writes.as_deref(), &hazard_seqs)?;
    let missing: Vec<u64> = hazard_seqs
        .iter()
        .copied()
        .filter(|seq| !acknowledged.contains(seq))
        .collect();

    // dry_run: print the preview, create nothing.
    if args.dry_run {
        print!("{}", render_fork_preview(&plan, &missing));
        return Ok(0);
    }

    // Refuse-then-record: any unacknowledged hazard refuses, listing what is
    // missing (exit 1, as a reconciliation refusal does).
    if !missing.is_empty() {
        let unacked: Vec<&WriteHazard> = plan
            .hazards()
            .iter()
            .filter(|hazard| missing.contains(&hazard.seq))
            .collect();
        print!(
            "{}",
            render_fork_refusal(&origin_uuid, &args.from_node, &unacked)
        );
        return Ok(1);
    }

    // Everything the child references must resolve, before any envelope is written.
    let (agents, servers) = build_graph_agents(&args.agents).await?;
    if let Err(error) = check_graph_resolvable(&graph, &agents) {
        close_servers(servers).await;
        return Err(error);
    }
    let tools = AgentTools(&agents);

    // Mint the child and write its prefix; it exists, standalone, at once.
    let child_id = RunId::new();
    let child_uuid = child_id.as_uuid().to_string();
    let child_prefix = plan.build_child_prefix(child_id, hazard_seqs);
    for envelope in &child_prefix {
        if let Err(error) = store.append(envelope).await {
            close_servers(servers).await;
            return Err(error.into());
        }
    }
    // Printed first, so a kill mid-drive still leaves the operator an id to resume.
    println!(
        "run {child_uuid} (forked from {origin_uuid} at node `{}`)",
        args.from_node
    );
    tracing::info!(run_id = %child_uuid, origin = %origin_uuid, "forking graph run");

    // Continue from the fork node exactly like a recovered graph run.
    let child_log = store.read_log(child_id).await?;
    let mut ctx = RunCtx::new(store, child_id, child_log)?;
    let outcome = run_graph(&mut ctx, &graph, &Value::Null, &agents, &tools).await;
    let outcome = settle_graph_drive(
        &mut ctx,
        &child_uuid,
        &args.graph,
        &args.agents,
        outcome,
        store_path,
    )
    .await;
    close_servers(servers).await;
    report_graph_outcome(outcome?, &child_uuid, &args.graph, &args.agents, store_path)
}

/// `salvor resolve`: record the completion of a dangling write by hand.
///
/// This is the operator side of reconciliation. A run whose log ends at a
/// write intent with no completion (status `NeedsReconciliation`) cannot be
/// recovered automatically: the write may or may not have taken effect. After
/// a human has verified externally what happened, `resolve` records the
/// completion they observed, so a later `resume` replays it and never re-runs
/// the write. It builds no agent and drives nothing; `--agent`/`--graph`, if
/// given, are used only to compose the real resume command the success report
/// prints, matching what a graph run's own parked report already does. The
/// log itself, not the presence of `--graph`, decides whether that command
/// hints at a graph run: see [`render::resolved_report`].
pub async fn resolve(store_path: &Path, caller: Option<&str>, args: ResolveArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let uuid = run_id.as_uuid().to_string();
    let output = parse_input(&args.output)?;
    let store = open_store(store_path)?;
    let log = store.read_log(run_id).await?;
    if log.is_empty() {
        bail!("no run {uuid} in this store");
    }
    // Read off the log itself, not off whether `--graph` happened to be
    // passed: an operator can resolve a graph run without supplying it, and
    // the printed command still needs to hint at `--graph <FILE>` then.
    let graph_run = is_graph_run(&log);
    let client_driven = log_is_client_driven(&log);

    let runtime = with_caller(Runtime::new(store), caller);
    match runtime.resolve(run_id, output).await {
        Ok(_) => {
            print!(
                "{}",
                render::resolved_report(
                    &uuid,
                    &args.agents,
                    args.graph.as_deref(),
                    graph_run,
                    client_driven,
                    Some(store_path),
                    render::DEFAULT_REPORT_WIDTH
                )
            );
            Ok(0)
        }
        // Refusing to resolve a run that is not awaiting reconciliation is a
        // deliberate refusal, not an internal error: exit 1 with an explanation.
        Err(RuntimeError::NotReconcilable { status, .. }) => {
            eprintln!(
                "run {uuid} does not need reconciliation (status: {status}); there is no dangling write to resolve"
            );
            Ok(1)
        }
        Err(error) => Err(error.into()),
    }
}

/// Attaches the caller name to a runtime, when the command resolved one.
///
/// `None` leaves the runtime unnamed, which records nothing. That is what a
/// build with no user account to read passes, and it is honest: a name nobody
/// supplied is not worth inventing.
fn with_caller(runtime: Runtime, caller: Option<&str>) -> Runtime {
    match caller {
        Some(name) => runtime.with_caller(name),
        None => runtime,
    }
}

/// `salvor abandon`: retire a run by hand, appending a terminal `RunAbandoned`.
///
/// The operator's "we do not care about this run anymore" path, for a run that
/// is dead forever or no longer worth carrying. A deliberate sibling of
/// [`resolve`]: it needs no agent and drives nothing, appending exactly one
/// terminal event. It is allowed for any non-terminal run; a run parked at a
/// dangling write is abandoned with the outstanding write recorded as
/// `unresolved_write`, so the receipt states plainly that the write stays
/// unresolved. Refuses (exit 1) a run that is already terminal.
pub async fn abandon(store_path: &Path, caller: Option<&str>, args: AbandonArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let uuid = run_id.as_uuid().to_string();
    let store = open_store(store_path)?;
    if store.read_log(run_id).await?.is_empty() {
        bail!("no run {uuid} in this store");
    }

    let runtime = with_caller(Runtime::new(store.clone()), caller);
    match runtime.abandon(run_id, args.reason).await {
        Ok(_) => {
            // Re-read to report the appended position and the recorded
            // unresolved-write evidence, straight off the terminal event.
            let log = store.read_log(run_id).await?;
            let appended_seq = log.last().map_or(0, |env| env.seq.get());
            let unresolved = match log.last().map(|env| &env.event) {
                Some(Event::RunAbandoned {
                    unresolved_write: Some(write),
                    ..
                }) => Some((write.seq.get(), write.tool.as_str())),
                _ => None,
            };
            print!(
                "{}",
                render::abandoned_report(
                    &uuid,
                    appended_seq,
                    unresolved,
                    render::DEFAULT_REPORT_WIDTH,
                )
            );
            Ok(0)
        }
        // Refusing an already-terminal run is a deliberate refusal, not an
        // internal error: exit 1 with an explanation.
        Err(RuntimeError::AlreadyTerminal { status, .. }) => {
            eprintln!(
                "run {uuid} is already terminal (status: {status}); there is nothing left to abandon"
            );
            Ok(1)
        }
        Err(error) => Err(error.into()),
    }
}

/// `salvor anchor`: write down every run's chain head, as of now.
///
/// The anchor is the one thing the store cannot rewrite along with itself. A
/// writer who can open the database can rewrite a run from its first event and
/// recompute every hash, head included, and nothing inside the file would
/// disagree; a copy of those heads kept elsewhere is what disagrees. See
/// [`crate::anchor`] for what that closes and what it does not.
///
/// The store is opened as the concrete SQLite backend rather than through the
/// `EventStore` trait, because a chain head is bookkeeping that backend keeps
/// beside the rows and is deliberately not part of the trait's promise.
///
/// One thing about the store's own consistency is judged here, and only one:
/// every run's log is read back before anything is written, and a store any
/// run of which the store itself refuses is not anchored ([`anchor::
/// EXIT_TAMPER`]). An anchor over an unreadable run records a head nobody can
/// check anything against. That refusal is the one `--force` does not lift:
/// `--force` is about the file at `--out`, and no answer about that file makes
/// a run readable. Whether the store agrees with itself in every other respect
/// is what `salvor verify` and every ordinary read answer.
///
/// Four ways it declines to write, all before anything is serialized: no store
/// at the path, a store holding no runs (an anchor over nothing commits to
/// nothing), a run this store refuses to read, and a `--out` file it would be
/// destroying evidence to replace. A fifth is decided by the filesystem: a
/// write that fails leaves no anchor, and says so with
/// [`anchor::EXIT_NOT_CHECKED`] rather than as a generic error, so a cron line
/// reading exit 1 as "the store no longer verifies against the file already
/// there" is not handed an unmounted directory under that name. See
/// [`anchor::EXIT_NOT_CHECKED`] and [`anchor::EXIT_TAMPER`].
///
/// `--force` overwrites the file at `--out`, and does not silence what was
/// found there: the comparison against it still runs and its answer prints as
/// a warning. An operator passing `--force` is saying "overwrite it", not "do
/// not tell me what I am overwriting", and this is the last moment anything
/// can say what the old heads were.
pub async fn anchor(store_path: &Path, args: AnchorArgs) -> Result<u8> {
    let store = match open_existing_store(store_path) {
        Ok(store) => store,
        Err(refusal) => {
            eprintln!("salvor anchor: {refusal}");
            return Ok(anchor::EXIT_NOT_CHECKED);
        }
    };

    let heads = store.chain_heads()?;
    // An anchor over zero runs is a file that says nothing and verifies
    // against anything, which is worse than no file at all: it looks like
    // evidence on the shelf. Refused unless the operator says the emptiness is
    // the truth.
    if heads.is_empty() && !args.allow_empty {
        eprintln!(
            "salvor anchor: nothing to anchor: {} holds no runs. Nothing was written; an anchor \
             over zero runs commits to nothing and a later verify against it passes without \
             checking anything. Pass --allow-empty if this store is empty on purpose.",
            store_path.display()
        );
        return Ok(anchor::EXIT_NOT_CHECKED);
    }

    // Before anything is written, and before --force is consulted at all:
    // every run's log is read back through the store, which recomputes its
    // chain. An anchor over a run the store itself refuses records a head
    // nobody can check anything against, and the file then sits on the shelf
    // looking like evidence while every later verify against it reports the
    // same run broken. The overwrite guard below already read every run for
    // its own reasons; this is the same read, on every anchor.
    for (run_id, _) in &heads {
        if let Err(error) = store.read_log(*run_id).await {
            if error.chain_refusal().is_none() {
                return Err(error.into());
            }
            eprintln!(
                "salvor anchor: not anchoring {}: {error}. An anchor must not record a head for \
                 a run nobody can read, so nothing was written{}. Go back to a backup that reads \
                 clean and read docs/OPERATIONS.md, Anchoring the chain. --force does not lift \
                 this.",
                store_path.display(),
                match args.out.as_deref() {
                    Some(out) => format!(" and {} was left as it is", out.display()),
                    None => String::new(),
                }
            );
            return Ok(anchor::EXIT_TAMPER);
        }
    }

    if let Some(out) = args.out.as_deref()
        && out.exists()
    {
        match read_anchor_file(out) {
            // The file already there is an anchor. Re-anchoring a store that
            // no longer verifies against it records the rewrite and destroys
            // the only copy of what the heads used to be, so the write is
            // refused rather than reported afterwards.
            Ok(existing) => {
                let result = verify_store_against(&store, store_path, out, &existing).await?;
                // The recovery command carries `--store`, because the store it
                // has to be run against is the one this command was given, and
                // an operator who is being told to go and look must not have
                // to reconstruct half the line.
                let look = format!(
                    "salvor verify --store {} --against {}",
                    store_path.display(),
                    out.display()
                );
                // Being handed the wrong file looks exactly like a store that
                // lost everything, and the two lead to opposite actions. Say
                // the cheap one when the shape fits, rather than the integrity
                // wording that sends an operator to a backup.
                if result.maybe_wrong_anchor {
                    if args.force {
                        eprintln!(
                            "warning: {} may be the wrong file: every run it records is missing \
                             from {}, which holds {} run(s) it never names; overwriting anyway \
                             as asked.",
                            out.display(),
                            store_path.display(),
                            result.new,
                        );
                    } else {
                        // An anchor that records no `store` is still an
                        // anchor, so the sentence says it does not name one
                        // rather than trailing off after "taken over".
                        let taken_over = if result.anchor_store.is_empty() {
                            "and it does not name the store it was taken over".to_owned()
                        } else {
                            format!("and it was taken over {}", result.anchor_store)
                        };
                        eprintln!(
                            "salvor anchor: not overwriting {}: this may be the wrong file. \
                             Every run it records is missing from {}, which holds {} run(s) it \
                             never names, {taken_over}. Run `{look}` to see the comparison. \
                             Confirm the two belong together before overwriting; pass --force \
                             to overwrite anyway.",
                            out.display(),
                            store_path.display(),
                            result.new,
                        );
                        return Ok(anchor::EXIT_TAMPER);
                    }
                } else if result.failed > 0 {
                    // Under --force the comparison still runs, and its answer
                    // is still printed. An operator who passes --force is
                    // saying "overwrite it", not "do not tell me what I am
                    // overwriting", and this is the last moment anything can
                    // say what the old heads were.
                    if args.force {
                        eprintln!(
                            "warning: this store fails verification against {} ({} of {} \
                             anchored runs); overwriting anyway as asked.",
                            out.display(),
                            result.failed,
                            result.anchored,
                        );
                    } else {
                        eprintln!(
                            "salvor anchor: this store no longer verifies against the anchor \
                             already at {}; not overwriting. {} of {} anchored run(s) failed. \
                             Run `{look}` to see which, and read docs/OPERATIONS.md, Anchoring \
                             the chain, before re-anchoring. Pass --force to overwrite anyway.",
                            out.display(),
                            result.failed,
                            result.anchored,
                        );
                        return Ok(anchor::EXIT_TAMPER);
                    }
                }
                // No branch here for a broken run outside the anchor: every
                // run in this store was read above, and one the store refuses
                // never reaches this point, --force or not.
            }
            // Not an anchor at all: whatever it is, this command did not write
            // it, and replacing an unread file is how an operator loses one.
            Err(why) => {
                if args.force {
                    eprintln!(
                        "warning: {}: {why} Overwriting anyway as asked.",
                        out.display(),
                    );
                } else {
                    eprintln!(
                        "salvor anchor: not overwriting {}: {why} Read it before replacing it, \
                         or pass --force.",
                        out.display(),
                    );
                    return Ok(anchor::EXIT_NOT_CHECKED);
                }
            }
        }
    }

    let document = anchor::Anchor::take(
        &store_path.display().to_string(),
        OffsetDateTime::now_utc(),
        heads,
    );
    let mut json = serde_json::to_string_pretty(&document).context("serializing the anchor")?;
    json.push('\n');
    match &args.out {
        // A write that fails is a fourth way no anchor was taken, and it exits
        // like the other three rather than as a generic error: the documented
        // `case` in a cron line reads 1 as "the store no longer verifies
        // against the file already there", and an unmounted directory is not
        // that. Nothing was written, so nothing was checked.
        Some(path) => {
            if let Err(error) = std::fs::write(path, &json) {
                eprintln!(
                    "salvor anchor: writing the anchor to {}: {error}. No anchor was taken; the \
                     store was not touched. Check the path and the directory it is in.",
                    path.display()
                );
                return Ok(anchor::EXIT_NOT_CHECKED);
            }
        }
        None => print!("{json}"),
    }
    // Before the success line, not after: an operator reads "anchored 2
    // run(s)" as the end of the matter and stops, and the one thing that makes
    // this anchor worthless has to come first. Printed on every anchor,
    // because the hazard is there on every anchor.
    if let Some(out) = args.out.as_deref()
        && shares_directory(out, store_path)
    {
        eprintln!("{}", render::anchor_beside_store_warning(out, store_path));
    }
    // On stderr: with no --out the anchor itself is on stdout, and a caller
    // redirecting that into a file must get the file and nothing else.
    eprintln!(
        "{}",
        render::anchored_line(document.runs.len(), args.out.as_deref())
    );
    Ok(0)
}

/// Whether two paths sit in the same directory, compared canonically so a
/// relative path, a `..`, and a symlinked directory all answer honestly.
///
/// A path whose directory cannot be canonicalized answers `false`: this
/// decides whether to print a warning, and a warning that cannot be justified
/// is not printed.
fn shares_directory(left: &Path, right: &Path) -> bool {
    let directory = |path: &Path| {
        let parent = path.parent().unwrap_or(Path::new(""));
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        parent.canonicalize().ok()
    };
    match (directory(left), directory(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Opens a store that already exists, refusing a path with no file at it.
///
/// [`SqliteStore::open`] creates a database at a path that has none, which is
/// right for a writer and wrong for every verb that only reads. A created
/// store holds no runs, so `anchor` would write a file committing to nothing,
/// `verify` would report every anchored run missing and send an operator to a
/// restore, and `list` would print `no runs in <path>` and exit 0, which is
/// the same words a genuinely empty store prints and the same exit code a
/// clean integrity read prints. All of that over a typo in `--store`. None of
/// them creates a file.
///
/// The verbs that do create are the ones a first run has to be able to use:
/// `run`, `graph run`, and `serve`. Everything else here reads.
///
/// # Errors
///
/// Returns the message to print: either that there is no store at the path, or
/// what went wrong opening the one that is there.
fn open_existing_store(path: &Path) -> Result<SqliteStore, String> {
    if !path.exists() {
        return Err(format!(
            "no store at {}. Nothing was read and nothing was created: check the path, or point \
             --store at the database.",
            path.display()
        ));
    }
    SqliteStore::open(path).map_err(|error| open_refusal(path, &error))
}

/// The message for a store that is there and would not open.
///
/// Two answers, because they are two different problems. SQLite's "file is not
/// a database" means the path holds something else, which is a typo in
/// `--store` and is fixed by pointing it somewhere else; anything else (no
/// permission, a locked file, a migration that failed) is a store this command
/// could not open, and telling an operator their real store is "not a salvor
/// store" sends them looking for a path that was right all along.
///
/// Either way the backend's own words are kept, because they are the half that
/// says which wrong thing happened. What is dropped is the framing: the error
/// reads "storage backend error: ...", and "storage backend" is a sentence
/// about salvor's insides where an operator needs one about their path.
fn open_refusal(path: &Path, error: &StoreError) -> String {
    let detail = match error {
        StoreError::Backend(message) => message.clone(),
        other => other.to_string(),
    };
    // SQLite ends some of its messages with the path it was handed, and this
    // message has already named it: "could not open the store at /x: unable to
    // open database file: /x" reads as two different files. Trimmed only when
    // it is exactly the path just printed.
    let detail = detail
        .strip_suffix(&format!(": {}", path.display()))
        .unwrap_or(&detail)
        .to_owned();
    if detail.contains("file is not a database") {
        format!(
            "{} is not a salvor store ({detail}). Nothing was read and nothing was created: \
             check the path, or point --store at the database.",
            path.display()
        )
    } else {
        format!(
            "could not open the store at {}: {detail}. Nothing was read and nothing was \
             created.",
            path.display()
        )
    }
}

/// Reads and validates an anchor file: it exists, it is JSON, it carries the
/// two specs this binary knows, and every entry is shaped like an anchored
/// head.
///
/// # Errors
///
/// Returns one sentence saying what is wrong with the file, without naming the
/// path: the two callers put the path in different places (`verify` leads with
/// it, `anchor` says it is not overwriting it), and a reason that names the
/// file itself reads as a stutter in both.
fn read_anchor_file(path: &Path) -> Result<anchor::Anchor, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("cannot be read: {error}."))?;
    let document: anchor::Anchor = serde_json::from_str(&text)
        .map_err(|error| format!("not an anchor this salvor can read: {error}."))?;
    document.check()?;
    Ok(document)
}

/// Compares a store against an already-validated anchor, run by run.
///
/// Shared by `verify` and by `anchor`'s overwrite guard, so the question "does
/// this store still verify against that file" has exactly one answer in this
/// binary whichever verb asked it.
async fn verify_store_against(
    store: &SqliteStore,
    store_path: &Path,
    against: &Path,
    document: &anchor::Anchor,
) -> Result<anchor::Verification> {
    let mut findings: Vec<anchor::RunFinding> = Vec::new();
    let mut anchored: HashSet<String> = HashSet::new();
    for run in &document.runs {
        // Already validated as a UUID by `Anchor::check`, so this cannot fail
        // on a document that reached here.
        let run_id = parse_run_id(&run.run)
            .with_context(|| format!("in the anchor file {}", against.display()))?;
        anchored.insert(run.run.clone());
        let observed = observe(store, run_id, run.anchored_len()).await?;
        findings.push(anchor::RunFinding {
            run: run.run.clone(),
            finding: anchor::finding_for(run, &observed),
        });
    }

    for (run_id, head) in store.chain_heads()? {
        let uuid = run_id.as_uuid().to_string();
        if anchored.contains(&uuid) {
            continue;
        }
        // A run this anchor never saw is still read, because reading is what
        // recomputes a chain: there is nothing to compare it against, and a log
        // this store refuses is still worth saying out loud.
        let finding = match observe(store, run_id, head.len).await? {
            anchor::Observed::Broken { seq, detail } => anchor::Finding::Broken { seq, detail },
            anchor::Observed::Present { len, .. } => anchor::Finding::New { len },
            // A recorded head with no rows under it is refused by `read_log`,
            // so this arm is unreachable in practice; skipping is the honest
            // answer if it ever is reached, since there is no run to report.
            anchor::Observed::Missing => continue,
        };
        findings.push(anchor::RunFinding { run: uuid, finding });
    }

    Ok(anchor::Verification::new(
        document,
        store_path,
        against,
        OffsetDateTime::now_utc(),
        findings,
    ))
}

/// Reports a check that did not run, in the form the caller asked for, and
/// returns [`anchor::EXIT_NOT_CHECKED`].
///
/// With `--json` this goes to stdout as a document rather than to stderr as
/// prose, because a consumer that parses stdout has to be able to tell "no
/// store at that path" from "every run intact", and an empty stdout with an
/// exit code is exactly the shape a script mistakes for a pass.
fn not_checked(json: bool, message: &str) -> Result<u8> {
    if json {
        let document = anchor::PreflightFailure::new(message);
        println!(
            "{}",
            serde_json::to_string_pretty(&document).context("serializing the refusal")?
        );
    } else {
        eprintln!("salvor verify: {message}");
    }
    Ok(anchor::EXIT_NOT_CHECKED)
}

/// `salvor verify --against <file>`: check this store against an anchor taken
/// earlier.
///
/// Every run in the anchor is read back through `read_log`, which recomputes
/// its whole chain, and then asked the one question the anchor can answer: does
/// the chain still carry the anchored hash at the anchored length. A run that
/// has grown since is intact, because the anchor commits to the prefix it
/// recorded and says nothing about what came after.
///
/// A log the store itself refuses is a finding here, not a crash: `verify` is
/// the command an operator reaches for when they already suspect something, so
/// it reports every run and then exits non-zero, rather than stopping at the
/// first bad one.
///
/// Three exit codes, because "the check found nothing wrong" and "the check
/// never ran" have to be different answers to a cron line: [`anchor::
/// EXIT_INTACT`] when every anchored run passed, [`anchor::EXIT_TAMPER`] when
/// any is missing, shortened, rewritten, or broken, and
/// [`anchor::EXIT_NOT_CHECKED`] when nothing was compared at all. A run the
/// anchor never saw is reported and changes none of it.
///
/// With `--json` every one of those prints a document on stdout, so a consumer
/// never has to tell an empty stdout apart from a result.
pub async fn verify(store_path: &Path, args: VerifyArgs) -> Result<u8> {
    let store = match open_existing_store(store_path) {
        Ok(store) => store,
        Err(refusal) => return not_checked(args.json, &refusal),
    };

    // Every way the anchor file can fail to be an anchor: gone, unreadable,
    // not JSON, written under another spec, or carrying an entry that is not a
    // head. Each one is refused here rather than compared on a guess, because
    // a comparison against a file this binary does not understand reads as a
    // finding about the store.
    let document = match read_anchor_file(&args.against) {
        Ok(document) => document,
        Err(why) => {
            return not_checked(args.json, &format!("{}: {why}", args.against.display()));
        }
    };

    // A pass over zero runs prints exactly like a pass over a store full of
    // intact ones, which is the problem: the operator asked a question and got
    // a yes that meant nothing.
    if document.runs.is_empty() && !args.allow_empty {
        return not_checked(
            args.json,
            &format!(
                "this anchor commits to nothing: {} records no runs, so a pass against it would \
                 mean nothing was checked. Take an anchor over a store that holds runs, or pass \
                 --allow-empty to accept this one.",
                args.against.display()
            ),
        );
    }

    let result = verify_store_against(&store, store_path, &args.against, &document).await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).context("serializing the verification")?
        );
    } else {
        print!("{}", render::verify_report(&result));
    }
    Ok(result.exit_code())
}

/// Reads one run back for `verify`: the log (which recomputes its chain) and,
/// when it reads, the hash the chain carried at the anchored length.
///
/// A chain refusal is turned into an observation rather than
/// propagated, because a refused log is exactly what this command exists to
/// report.
async fn observe(
    store: &SqliteStore,
    run_id: RunId,
    anchored_len: u64,
) -> Result<anchor::Observed> {
    match store.read_log(run_id).await {
        Ok(log) if log.is_empty() => Ok(anchor::Observed::Missing),
        Ok(log) => Ok(anchor::Observed::Present {
            len: log.len() as u64,
            hash_at_anchored_len: store.chain_hash_at(run_id, anchored_len)?,
        }),
        // Every way the store can refuse a log, in the store's own words minus
        // the run id, which this report has already printed. The position is
        // whatever the refusal has: a rewritten row has one, a recorded head
        // that disagrees with all of them at once does not.
        Err(error) => match error.chain_refusal() {
            Some((seq, detail)) => Ok(anchor::Observed::Broken { seq, detail }),
            None => Err(error.into()),
        },
    }
}

/// `salvor completions <shell>`: print a completion script on stdout.
///
/// Generated from the same `clap::Command` the parser is built from, not a hand-written list, so a
/// new subcommand or flag completes the moment it exists and cannot drift out of date. That is the
/// whole reason to generate rather than ship a static script.
///
/// It takes no store and reads nothing: shells source this at startup, so it must not touch a
/// database, and the exit code is always 0 unless writing to stdout fails.
pub fn completions(args: CompletionsArgs) -> Result<u8> {
    let mut command = <crate::cli::Cli as clap::CommandFactory>::command();
    let name = command.get_name().to_string();
    clap_complete::generate(args.shell, &mut command, name, &mut std::io::stdout());
    Ok(0)
}

/// `salvor list`: one row per run, with status folded from each log, narrowed by whatever filters
/// the caller gave.
///
/// Filtering happens after the fold because status is a replay-time projection, not a stored
/// column: there is nothing to filter on until each log has been read. That also means a filter
/// saves screen space, not work.
///
/// The store is read, never created. `no runs in <path>` and exit 0 is what a
/// genuinely empty store prints, and it is also what a clean integrity read
/// over every run prints, so a store this command invented at a mistyped path
/// would print an all-clear about nothing. A missing path is refused with
/// [`anchor::EXIT_NOT_CHECKED`] instead, the same code and the same words
/// `verify` uses.
pub async fn list(store_path: &Path, args: ListArgs) -> Result<u8> {
    let store = match open_existing_store(store_path) {
        Ok(store) => store,
        Err(refusal) => {
            eprintln!("salvor list: {refusal}");
            return Ok(anchor::EXIT_NOT_CHECKED);
        }
    };
    let mut summaries = store.list_runs().await?;

    // `list_runs` builds its summaries out of the recorded rows, so a run
    // whose rows are gone while its recorded head remains is a run it cannot
    // see at all, and a listing that never mentions it completes and exits 0.
    // That is the one shape of tampering this command was silent about, and it
    // is the shape a deletion takes. Every head with no summary under it is
    // read, and `read_log` is what refuses it: a head with no rows is not a
    // run that went away quietly, it is a run someone cleared the way for.
    let listed: HashSet<String> = summaries
        .iter()
        .map(|summary| summary.run_id.as_uuid().to_string())
        .collect();
    for (run_id, _) in store.chain_heads()? {
        if !listed.contains(&run_id.as_uuid().to_string()) {
            store.read_log(run_id).await?;
        }
    }

    if summaries.is_empty() {
        println!("no runs in {}", store_path.display());
        return Ok(0);
    }
    summaries.sort_by_key(|summary| summary.first_recorded_at);

    let mut rows = Vec::with_capacity(summaries.len());
    for summary in summaries {
        // Status is a replay-time projection, not a stored column, so fold the
        // log to get it. See RunSummary's docs for why status stays out of the
        // store.
        let log = store.read_log(summary.run_id).await?;
        let state = derive_state(&log);
        let status = render::status_label(&state.status).to_owned();
        // Only a sleeping run has a wake instant; every other status leaves
        // the WAKES AT column blank.
        let wake_at = match &state.status {
            RunStatus::Sleeping { wake_at } => Some(*wake_at),
            _ => None,
        };

        if !args.status.is_empty() && !args.status.iter().any(|s| s == &status) {
            continue;
        }
        if let Some(group) = &args.group
            && render::status_group(&status).map(render::StatusGroup::as_str)
                != Some(group.as_str())
        {
            continue;
        }
        if let Some(needle) = &args.agent {
            let identity = agent_identity(&log);
            if !identity.to_lowercase().contains(&needle.to_lowercase()) {
                continue;
            }
        }
        rows.push((summary, status, wake_at));
    }

    // The tail, not the head: a limit exists to show the runs that matter now, and the newest sit
    // at the end of an ascending list. The surviving rows keep that order, so the shape of the
    // table never changes with the flag.
    if let Some(limit) = args.limit
        && rows.len() > limit
    {
        rows.drain(..rows.len() - limit);
    }

    if rows.is_empty() {
        println!("no runs matched");
        return Ok(0);
    }
    // anstream, not print!: it strips the table's styling when stdout is a pipe or a file, and
    // honours NO_COLOR and CLICOLOR, so redirected output stays plain text.
    anstream::print!("{}", render::list_table(&rows));
    Ok(0)
}

/// `salvor history`: the pretty event log, or raw JSON envelopes with `--json`.
///
/// The store is read, never created: against a store this command had just
/// made, every run id in the world reads back as `no run <id> in this store`,
/// which is a typo in `--store` wearing the words of a missing run.
pub async fn history(store_path: &Path, args: HistoryArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let store = match open_existing_store(store_path) {
        Ok(store) => store,
        Err(refusal) => {
            eprintln!("salvor history: {refusal}");
            return Ok(anchor::EXIT_NOT_CHECKED);
        }
    };
    let log = store.read_log(run_id).await?;
    if log.is_empty() {
        bail!("no run {} in this store", run_id.as_uuid());
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&log)?);
    } else {
        for envelope in &log {
            println!("{}", render::history_line(envelope));
        }
    }
    Ok(0)
}

/// `salvor replay`: re-derive state from the log, execute nothing.
///
/// This is the only mode: replay always re-derives state without executing
/// anything, and never has run any other way. `--dry-run` is accepted and
/// ignored, kept only so a script written against an earlier version that
/// passed it does not break.
///
/// The store is read, never created, for the same reason as `history`: a
/// mistyped `--store` must not come back as a missing run.
pub async fn replay(store_path: &Path, args: ReplayArgs) -> Result<u8> {
    let run_id = parse_run_id(&args.run_id)?;
    let store = match open_existing_store(store_path) {
        Ok(store) => store,
        Err(refusal) => {
            eprintln!("salvor replay: {refusal}");
            return Ok(anchor::EXIT_NOT_CHECKED);
        }
    };
    let log = store.read_log(run_id).await?;
    if log.is_empty() {
        bail!("no run {} in this store", run_id.as_uuid());
    }
    let state = derive_state(&log);
    print!("{}", render::replay_summary(&state));
    Ok(0)
}

/// `salvor serve`: run the control-plane HTTP + server-sent-events server.
///
/// The server owns the same store every other command uses and drives runs
/// through the same runtime, so durability is identical to the local verbs.
/// The one piece the server does not own is the agent-definition format: this
/// command supplies the factory that parses a submitted definition (TOML or
/// JSON) with the CLI's own [`AgentConfig`] and builds it, so the schema keeps
/// its single home here. A submitted definition's relative paths (a prompt
/// file, a wasm component) resolve against the server's working directory.
///
/// `--kill` short-circuits this handler before the store opens or a port
/// binds: it never serves in the same invocation. That is a plain early
/// return rather than a clap `conflicts_with`, because `--store` lives on the
/// top-level [`crate::cli::Cli`] as a global flag, not on [`ServeArgs`], so
/// there is no single sibling argument for clap to name; a handler-level
/// check covers `--store` and `--bind` alike with one line, and keeps the
/// process-discovery flow ([`serve_kill`]) unit-testable on its own.
///
/// `--dev` adds a second process to the same invocation: the Angular dev
/// server (`ng serve`) for `bridge/`, hot module reloading included, proxying
/// `/v1` to the API this same command just bound. The API itself binds and
/// serves exactly as plain `serve` does; `--dev` only decides whether a
/// second process joins it and whether this handler waits on a shutdown
/// signal afterward to tear that second process down (see
/// [`DevServer::shutdown`]). The checkout it needs is found through
/// [`checkout::find_repo_root`], the same walk-up `salvor build` uses, so a
/// `--dev` outside a checkout fails before anything binds or spawns.
pub async fn serve(store_path: &Path, args: ServeArgs) -> Result<u8> {
    if let Some(target) = &args.kill {
        // `--kill` with no value arrives as `Some("")` (clap's
        // `default_missing_value`); that is the "no target, discover and
        // maybe prompt" case, not a literal empty target.
        let target = (!target.is_empty()).then_some(target.as_str());
        return serve_kill::run(target).await;
    }

    // Found and validated before the store opens or a port binds, so `--dev`
    // outside a checkout fails fast and honestly, before any other work.
    let bridge_dir = if args.dev {
        Some(
            checkout::find_repo_root()
                .context(
                    "--dev needs a salvor checkout with bridge/; the installed dashboard is \
                     embedded and does not hot-reload",
                )?
                .join("bridge"),
        )
    } else {
        None
    };

    let store = open_store(store_path)?;

    let factory: AgentFactory = Arc::new(|definition: AgentDefinition| {
        Box::pin(async move { build_from_definition(definition).await })
    });

    // The general model executor client-driven runs perform their model step
    // through, wired from the CLI's own client-construction path. The config is
    // built explicitly: the key keeps `Config::from_env`'s semantics
    // (`ANTHROPIC_API_KEY`, absent for a local endpoint, which sends no auth
    // header), and `SALVOR_MODEL_BASE_URL`, when set and non-empty, points the
    // executor at a local or offline endpoint speaking the same wire protocol
    // instead of the public one. Another host injects its own executor; this is
    // the out-of-the-box default, mirroring the agent factory.
    let mut model_config = salvor_llm::Config::from_env();
    if let Ok(base_url) = std::env::var("SALVOR_MODEL_BASE_URL")
        && !base_url.is_empty()
    {
        model_config = model_config.with_base_url(base_url);
    }
    let model_client = salvor_llm::Client::new(model_config)
        .context("building the model client for the client-driven model step")?;

    // The tool registry: EMPTY by default (the mechanism is wired, but
    // salvor serve ships no tools of its own; a tool-step or a graph `tool`
    // node for any name is a clean `unknown_tool` until a host registers
    // one, mirroring how the model executor is wired), or the deterministic
    // demo set when `--demo-tools` opts in. This is the one place that flag
    // is read; every other line of `serve` is unchanged whether it is
    // passed or not, so the stock, no-flag path stays byte-identical.
    let tool_registry = if args.demo_tools {
        #[cfg(feature = "fixture")]
        {
            tracing::info!(
                "demo tools registered: lookup_invoice (read), issue_refund (write), send_email \
                 (idempotent); see salvor_cli::demo_tools"
            );
            crate::demo_tools::registry()
        }
        #[cfg(not(feature = "fixture"))]
        {
            bail!(
                "--demo-tools requires the `fixture` feature (this binary was built with \
                 --no-default-features); rebuild with the default features to use it"
            );
        }
    } else {
        ToolRegistry::new()
    };
    // The client-performed tool declarations, one TOML file per `--client-tool`.
    // These are tools the CLIENT runs in its own process; this server holds no
    // code for any of them, only the operator's word about the effect class, the
    // two schemas, and whether a client may close its own call. Empty without
    // the flag, and an empty set answers every client-tool intent with a clean
    // `unknown_tool`, exactly as an empty `ToolRegistry` does for a tool step.
    //
    // Loaded here and nowhere else on purpose: there is no endpoint that accepts
    // a declaration, because a declaration fixes the effect class and a client
    // that could write its own would be deciding whether its own writes are
    // subject to the write-ahead rule.
    let mut client_tools = ClientToolRegistry::new();
    for path in &args.client_tools {
        client_tools.declare(load_client_tool(path)?);
    }
    if !client_tools.is_empty() {
        tracing::info!(
            tools = ?client_tools.names(),
            "client-performed tool declarations loaded; salvor records these calls, it does not \
             perform them"
        );
    }

    let mut state = AppState::new(store, factory)
        .with_model_executor(Arc::new(LlmModelExecutor::new(model_client)))
        .with_tool_registry(Arc::new(tool_registry))
        .with_client_tools(Arc::new(client_tools))
        // The wake sweeper's interval, on by default at 60s; `0` turns it off
        // for an operator who wakes runs from cron with `salvor wake`.
        .with_wake_interval(std::time::Duration::from_secs(args.wake_interval));
    // The client-driven-run lease TTL: how long a client run reports an attached
    // driver after its last guarded operation. Default 60s (set in AppState);
    // `SALVOR_CLIENT_LEASE_TTL_SECS`, when a positive integer, shortens it so a
    // driverless client run becomes observable quickly (the stalled-run seed uses
    // this). A missing, empty, zero, or unparseable value leaves the default.
    if let Ok(raw) = std::env::var("SALVOR_CLIENT_LEASE_TTL_SECS")
        && let Ok(secs) = raw.parse::<u64>()
        && secs > 0
    {
        state = state.with_client_lease_ttl(std::time::Duration::from_secs(secs));
        tracing::info!(
            secs,
            "client-driven run lease TTL set from SALVOR_CLIENT_LEASE_TTL_SECS"
        );
    }
    // The named-token file, loaded and checked before a port is bound: a file
    // readable by group or other, owned by another user, malformed, or empty
    // is a refusal to start, not a server running on whatever parsed. The
    // store re-reads the file when it changes, so add and revoke need no
    // restart from here on.
    if let Some(path) = &args.token_file {
        let store = salvor_server::TokenStore::load(path)
            .with_context(|| format!("loading the token file {}", path.display()))?;
        let names: Vec<String> = store
            .current()
            .names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        tracing::info!(
            file = %path.display(),
            tokens = ?names,
            "named bearer tokens loaded; the file is re-read when it changes"
        );
        state = state.with_token_file(store);
    }
    if let Some(env_name) = &args.auth_token {
        match std::env::var(env_name) {
            Ok(token) if !token.is_empty() => {
                salvor_server::tokens::check_single_token(&token)
                    .map_err(|detail| anyhow!("--auth-token names ${env_name}, and {detail}"))?;
                state = state.with_auth_token(token);
                tracing::info!("bearer auth required (token read from ${env_name})");
            }
            _ => bail!(
                "--auth-token names ${env_name}, but it is unset or empty; export ${env_name} \
                 with the bearer token before serving, or drop --auth-token to serve without auth"
            ),
        }
    }

    let listener = TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    let addr = listener.local_addr().context("reading the bound address")?;
    println!("salvor control plane listening on http://{addr}");
    tracing::info!(%addr, "serving the control plane");

    let Some(bridge_dir) = bridge_dir else {
        // The plain path, unchanged from before `--dev` existed: no signal
        // handler installed here, so Ctrl-C or a `--kill` SIGTERM stops this
        // process the ordinary way, at the OS's default disposition.
        salvor_server::serve(listener, state)
            .await
            .context("serving the control plane")?;
        return Ok(0);
    };

    let dev = DevServer::start(&bridge_dir, addr).await?;
    println!(
        "dev UI (hot reload): http://localhost:{}/  <- open this one",
        dev.port()
    );
    println!("the API above stays reachable directly, e.g. for curl or an SDK");

    // A signal handler is installed only on this path: `--dev` is the one
    // case with a second process that must not outlive this one, so this is
    // the one case that needs to intercept the shutdown signal instead of
    // letting the OS's default disposition end the process immediately.
    // Whichever finishes first wins the race; the server future is simply
    // dropped on the signal branch (no in-flight request survives a plain
    // kill today either, `--dev` or not), then the dev server is torn down
    // before this returns, so `--kill` reliably reaps both.
    tokio::select! {
        result = salvor_server::serve(listener, state) => {
            result.context("serving the control plane")?;
        }
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received, stopping the dev server");
        }
    }
    dev.shutdown().await;
    Ok(0)
}

/// Reads one `--client-tool` file and parses the declaration it holds.
///
/// One file, one declaration, mirroring `--agent`. The format itself belongs to
/// `salvor_server::ClientToolDecl` (which carries the `Deserialize` derive);
/// this reads the bytes off disk and names the file in every failure, exactly
/// as the agent-definition path does. A malformed or unreadable declaration
/// stops `serve` before a port is bound, so an operator learns about a typo
/// immediately rather than when a client's first call is refused.
fn load_client_tool(path: &Path) -> Result<ClientToolDecl> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the client tool declaration {}", path.display()))?;
    toml::from_str(&text)
        .with_context(|| format!("parsing the client tool declaration {}", path.display()))
}

/// Waits for Ctrl-C (`SIGINT`) or, on Unix, `SIGTERM` (what `salvor serve
/// --kill` sends). Only [`serve`]'s `--dev` path awaits this: it is the one
/// case that needs to run cleanup code before the process exits, rather than
/// letting the OS's default signal disposition end it immediately.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("installing a SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `salvor build`: build the whole product from a checkout.
///
/// It builds the web dashboard (the Bridge's production output) and then the
/// release binary, in that order, so the release binary embeds the dashboard
/// just produced. With `--install` it then installs that binary onto the PATH,
/// so the `salvor` a shell resolves carries the fresh dashboard.
///
/// The dashboard and the binary are built through a login shell (`bash -lc`) so
/// a node toolchain managed by nvm, and cargo under `~/.cargo/bin`, resolve the
/// same way they do at an interactive prompt. Every subprocess inherits this
/// process's streams, so the token gate and the compiler report scroll through
/// live.
pub async fn build(args: BuildArgs) -> Result<u8> {
    let root = checkout::find_repo_root()?;
    println!("salvor build: repo root at {}", root.display());

    let bridge = root.join("bridge");
    checkout::ensure_node_modules(&bridge).await?;
    println!("building the dashboard (npm run build)");
    checkout::run_shell(&bridge, "npm run build").await?;

    println!("building the release binary (cargo build --release -p salvor-cli)");
    checkout::run_shell(&root, "cargo build --release -p salvor-cli").await?;

    if args.install {
        println!("installing salvor onto the PATH (cargo install --path crates/salvor-cli)");
        checkout::run_shell(&root, "cargo install --path crates/salvor-cli").await?;
        println!(
            "salvor installed at {}",
            install_dir().join("salvor").display()
        );
    } else {
        println!("built {}", root.join("target/release/salvor").display());
        println!("run `salvor build --install` to put it on your PATH");
    }
    Ok(0)
}

/// The directory `cargo install` writes binaries to: `$CARGO_HOME/bin`, else
/// `$HOME/.cargo/bin`.
fn install_dir() -> PathBuf {
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        PathBuf::from(cargo_home).join("bin")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cargo").join("bin")
    } else {
        PathBuf::from("~/.cargo/bin")
    }
}

/// `salvor agent hash <FILE>...`: print each agent definition's content hash.
///
/// The value printed is [`Agent::def_hash`], the same string a run records in
/// `RunStarted` and the same key [`graph_run`] resolves an `agent` node
/// against. It is produced by BUILDING each definition through
/// [`build_agents`], the way every other verb that takes `--agent` does, rather
/// than by hashing the file's bytes: tool schemas are part of the definition
/// and an MCP server supplies its own, so a byte hash of the TOML would be a
/// number no graph node could ever resolve.
///
/// One file prints the bare hash and nothing else, so `$(salvor agent hash
/// a.toml)` is usable as a value. Several print `<path>: <hash>` a line, in the
/// order given, since the question several files ask is which hash belongs to
/// which file.
///
/// This reads no store and starts no run. The MCP sessions the build opened are
/// closed before anything is printed, so stdout carries the hashes alone.
pub async fn agent_hash(args: AgentHashArgs) -> Result<u8> {
    let (agents, servers) = build_agents(&args.agents).await?;
    close_servers(servers).await;

    let bare = agents.len() == 1;
    for (path, agent) in args.agents.iter().zip(&agents) {
        if bare {
            println!("{}", agent.def_hash());
        } else {
            println!("{}: {}", path.display(), agent.def_hash());
        }
    }
    Ok(0)
}

/// `salvor agent validate <FILE>...`: build each agent definition and report
/// what it declares, or the precise error that stopped it from building.
///
/// This is the same build [`agent_hash`] runs, named for what it is worth
/// asking on its own: is this file good, and what does it commit an operator
/// to. By default that build CONNECTS: it spawns every declared MCP server
/// (`command`) or dials every declared one (`url`) to introspect its tools,
/// exactly as `salvor run` would before starting. `--no-connect` skips that
/// step entirely and checks fields and shape only: the TOML parses, required
/// fields are present and well-typed, and each MCP server declaration names
/// exactly one transport. No process is spawned and no socket is dialed, so a
/// declared server whose command does not exist on this machine still passes.
///
/// Each file is built independently, rather than all at once, so a file that
/// fails to build does not stop the rest from being checked, matching `graph
/// validate`'s exit-code contract at the per-file level: `Ok(0)` only when
/// every file built, `Ok(1)` when any one of them did not. A single file's
/// report carries no path prefix, since there is nothing to disambiguate;
/// several files each get one, on both the success line and the error line,
/// for the same reason `agent hash` prefixes its multi-file output.
///
/// The MCP sessions each build opens (default mode only) are closed before
/// the next file starts, so no session outlives the file it was opened to
/// validate, let alone the command.
///
/// This reads no store and starts no run.
pub async fn agent_validate(args: AgentValidateArgs) -> Result<u8> {
    let bare = args.agents.len() == 1;
    let mut any_failed = false;

    for path in &args.agents {
        let result: Result<String> = async {
            let config = AgentConfig::load(path)?;
            let (agent, servers) =
                agent_config::build_agent(&config, path, args.no_connect).await?;
            let idempotency_keys = config.declared_idempotency_keys();
            let report = if args.no_connect {
                render::agent_summary_no_connect(
                    &agent,
                    config.mcp_servers.len(),
                    &idempotency_keys,
                )
            } else {
                render::agent_summary(&agent, servers.len(), &idempotency_keys)
            };
            close_servers(servers).await;
            Ok(report)
        }
        .await;

        match result {
            Ok(report) => {
                if bare {
                    print!("{report}");
                } else {
                    print!("{}: {report}", path.display());
                }
            }
            Err(error) => {
                any_failed = true;
                if bare {
                    eprintln!("{error:#}");
                } else {
                    eprintln!("{}: {error:#}", path.display());
                }
            }
        }
    }

    Ok(u8::from(any_failed))
}

/// `salvor graph edit`: fold typed lines into a graph document.
///
/// The editor itself is [`salvor_cli_core::graph_editor`], which performs no IO;
/// everything host-shaped about a session (the prompt, the streams, the three
/// lines that name a file) is [`crate::graph_edit`]. This handler is the seam
/// between the parse tree and that loop, and it takes no store, because an
/// author building a document has not started a run.
pub async fn graph_edit(args: crate::cli::GraphEditArgs) -> Result<u8> {
    crate::graph_edit::edit(args).await
}

/// `salvor graph validate <path>`: parse a graph document strictly and run
/// every validation check.
///
/// The exit-code contract matches the rest of the CLI: `Ok(0)` on a valid
/// document, `Ok(1)` for a document the tool deliberately refuses (unreadable
/// file, non-JSON, or a validation failure). Every failure prints a clear
/// message to stderr and never panics. On success the summary (node/edge counts
/// and entry/terminal nodes) prints to stdout.
///
/// This handler reads no store and drives no run: a graph document is
/// validated in isolation.
pub fn graph_validate(args: GraphValidateArgs) -> Result<u8> {
    let path = args.path.display().to_string();

    let text = match std::fs::read_to_string(&args.path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("cannot read graph file {path}: {error}");
            return Ok(1);
        }
    };

    // Strict parse: a stray field is a rejection, not a warning, because a graph
    // is a control document.
    let graph: salvor_graph::Graph = match serde_json::from_str(&text) {
        Ok(graph) => graph,
        Err(error) => {
            eprintln!("{path} is not a valid graph document: {error}");
            return Ok(1);
        }
    };

    match salvor_graph::validate(&graph) {
        Ok(summary) => {
            print!("{}", render::graph_summary(&summary));
            Ok(0)
        }
        Err(errors) => {
            eprintln!("{path}: {} validation error(s):", errors.len());
            for error in &errors {
                eprintln!("  - {error}");
            }
            Ok(1)
        }
    }
}

/// `salvor graph schema`: print the graph document JSON Schema to stdout.
///
/// This is the single source of truth for the document format, so it is emitted
/// verbatim from the [`salvor_graph`] types with no store and no run involved.
pub fn graph_schema() -> Result<u8> {
    let schema = salvor_graph::graph_schema();
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(0)
}

/// `salvor graph run`: drive a graph document locally over the store, the graph
/// counterpart of [`run`].
///
/// It mirrors `salvor run`: strict-parse and validate the document (refusing an
/// invalid one before any run head is written), build every `--agent` file and
/// key it by its computed definition hash, and drive the engine over a fresh
/// `RunCtx`. The run id is printed first so a `kill -9` mid-run still leaves an
/// id to resume. On a park (a gate, a budget crossing) it prints how to continue
/// with `salvor resume ... --graph`, and so does a TRANSIENT failure, which
/// leaves the same live, resumable run behind (see [`settle_graph_drive`]).
///
/// # Resolution, and how it matches the server
///
/// An `agent` node resolves to a provided `--agent` file by that file's
/// definition hash; a hash matching none of them is a precise error listing what
/// was provided. A `tool` node resolves from the tools the provided agents
/// carry, the local counterpart of the server's tool registry, keeping one
/// honest story: a tool no provided agent carries is refused, named, before the
/// walk reaches it (as [`run_graph`] does through the resolver).
pub async fn graph_run(store_path: &Path, args: GraphRunArgs) -> Result<u8> {
    let graph = load_and_validate_graph(&args.graph)?;
    let input = parse_input(&args.input)?;
    let labels = parse_label_args(&args.labels)?;
    let store = open_store(store_path)?;
    let (agents, servers) = build_graph_agents(&args.agents).await?;
    // Resolve everything the document references up front, before any run head is
    // written, so an unresolvable agent or tool is a precise refusal rather than
    // a run stranded at the offending node. This mirrors the server, which
    // resolves synchronously at graph-run submit.
    if let Err(error) = check_graph_resolvable(&graph, &agents) {
        close_servers(servers).await;
        return Err(error);
    }
    let tools = AgentTools(&agents);

    let run_id = RunId::new();
    let uuid = run_id.as_uuid().to_string();
    // Printed first, so a kill mid-run still leaves the operator an id to resume.
    println!("run {uuid}");
    tracing::info!(run_id = %uuid, "starting graph run");

    let mut ctx = RunCtx::new(store.clone(), run_id, vec![])?;
    if let Some(labels) = labels {
        ctx = ctx.with_labels(labels);
    }
    let outcome = run_graph(&mut ctx, &graph, &input, &agents, &tools).await;
    let outcome = settle_graph_drive(
        &mut ctx,
        &uuid,
        &args.graph,
        &args.agents,
        outcome,
        store_path,
    )
    .await;
    close_servers(servers).await;
    report_graph_outcome(outcome?, &uuid, &args.graph, &args.agents, store_path)
}

/// Re-drives a parked or crashed GRAPH run, for [`resume`]'s graph branch.
///
/// The log records only the graph's hash, so the document is re-supplied through
/// `--graph`; its hash must match the recorded one (a different document could
/// route the same log differently, which would be a silent divergence rather
/// than an honest refusal). The agent nodes' definitions are re-supplied through
/// `--agent`, exactly as an agent run re-supplies its one definition. A parked
/// run consumes `--input` at its suspension; a crashed run recovers with none.
async fn resume_graph(
    store: Arc<dyn EventStore>,
    run_id: RunId,
    uuid: &str,
    log: &[EventEnvelope],
    args: &ResumeArgs,
    disposition: Disposition,
    store_path: &Path,
) -> Result<u8> {
    let (graph_path, graph) = resolve_graph_document(args.graph.as_deref(), log, uuid)?;
    // See the agent branch of `resume`: waking a due run and recovering a
    // crashed one take the same path, and only the disposition can keep the
    // log honest about which one happened.
    let waking = matches!(disposition, Disposition::Sleeping { .. });
    let (agents, servers) = build_graph_agents(&args.agents).await?;
    if let Err(error) = check_graph_resolvable(&graph, &agents) {
        close_servers(servers).await;
        return Err(error);
    }
    let tools = AgentTools(&agents);

    let mut ctx = RunCtx::new(store, run_id, log.to_vec())?;
    match disposition {
        Disposition::Resume(_) => {
            let raw = args.input.as_deref().context(
                "this run is parked awaiting input; pass --input <json|@file> to resume it",
            )?;
            let input = parse_input(raw)?;
            // The gate accept edge, on this layer. The engine enforces the same
            // rule between its `suspend` and `await_resume` (see
            // `salvor_engine::approval`); refusing here just means the operator
            // reads a refusal in the CLI's own voice rather than a drive that
            // stopped. Either way nothing is appended and the run stays parked
            // at the gate, so a corrected `--input` resumes it.
            if let Err(error) = refuse_nonconforming_approval(log, &graph, &input) {
                close_servers(servers).await;
                return Err(error);
            }
            ctx.set_resume_input(input);
            tracing::info!(run_id = %uuid, "resuming parked graph run");
        }
        _ => {
            if args.input.is_some() {
                if waking {
                    tracing::warn!(
                        run_id = %uuid,
                        "a sleeping run takes no input; --input is ignored when waking it"
                    );
                } else {
                    tracing::warn!(
                        run_id = %uuid,
                        "this graph run crashed mid-step; --input is ignored when recovering"
                    );
                }
            }
            if waking {
                tracing::info!(run_id = %uuid, "waking sleeping graph run");
            } else {
                tracing::info!(run_id = %uuid, "recovering crashed graph run");
            }
        }
    }
    // The recorded input wins on replay, so a bare null is fine here.
    let outcome = run_graph(&mut ctx, &graph, &Value::Null, &agents, &tools).await;
    let outcome = settle_graph_drive(
        &mut ctx,
        uuid,
        graph_path,
        &args.agents,
        outcome,
        store_path,
    )
    .await;
    close_servers(servers).await;
    report_graph_outcome(outcome?, uuid, graph_path, &args.agents, store_path)
}

/// Refuses a `--input` that does not satisfy the `approval_schema` of the gate
/// the run is parked at, listing every violation and then showing the shape a
/// conforming approval has.
///
/// A run parked anywhere else (a tool suspension, a budget crossing) or being
/// recovered rather than resumed passes through: this is the gate's rule, and
/// those inputs already go through the runtime's own recorded-schema check.
fn refuse_nonconforming_approval(
    log: &[EventEnvelope],
    graph: &Graph,
    input: &Value,
) -> Result<()> {
    let Some(gate) = salvor_engine::parked_gate(log, graph) else {
        return Ok(());
    };
    let violations = salvor_engine::approval_violations(input, &gate.approval_schema);
    if violations.is_empty() {
        return Ok(());
    }
    let listed: String = violations
        .iter()
        .map(|violation| format!("\n  {violation}"))
        .collect();
    bail!(
        "the approval does not satisfy gate `{}`'s approval_schema:{listed}\n\nnothing was \
         recorded and the run is still parked at that gate. A conforming approval satisfies:\n{}",
        gate.id,
        render::pretty_json(&gate.approval_schema)
    )
}

/// A [`ToolResolver`] over the provided agents' own tools: a graph `tool` node
/// resolves to the first provided agent that carries a tool of that name. This
/// is the local counterpart of the server's tool registry: the CLI has no
/// standalone tool inventory, so the tools come from the real agent definitions
/// the operator supplied.
struct AgentTools<'a>(&'a HashMap<String, Agent>);

impl ToolResolver for AgentTools<'_> {
    fn resolve_tool(&self, name: &str) -> Option<&dyn DynTool> {
        self.0.values().find_map(|agent| agent.tools().get(name))
    }
}

/// The document a graph run's re-drive needs: the `--graph` file, validated,
/// and hash-checked against the head its log recorded.
///
/// One function, because two verbs ask the same question. [`resume_graph`]
/// asks it to drive the run; `wake --dry-run` asks it to say whether the files
/// an operator is about to put in a crontab would wake the run at all. A
/// preview that phrased these refusals in its own words would be a second
/// contract to learn, and the whole worth of a dry run is that what it says is
/// what the real thing will say.
fn resolve_graph_document<'a>(
    graph: Option<&'a Path>,
    log: &[EventEnvelope],
    uuid: &str,
) -> Result<(&'a Path, Graph)> {
    let graph_path = graph.context(
        "this is a graph run; pass --graph <graph.json> (its hash must match the recorded run) to \
         re-drive it, alongside the --agent files its agent nodes reference",
    )?;
    let graph = load_and_validate_graph(graph_path)?;
    let hash = graph_hash(&graph)?;
    let recorded =
        recorded_graph_hash(log).context("this graph run's log has no GraphRunStarted event")?;
    if hash != recorded {
        bail!(
            "the graph in {} hashes to {hash}, but run {uuid} recorded {recorded}; resume needs the \
             SAME document the run started with (submit the changed graph as a new run instead)",
            graph_path.display()
        );
    }
    Ok((graph_path, graph))
}

/// Reads and strictly validates a graph document, refusing an invalid one with a
/// precise, node/edge-level error message (the same checks `graph validate`
/// runs).
fn load_and_validate_graph(path: &Path) -> Result<Graph> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading graph document {}", path.display()))?;
    let graph: Graph = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a valid graph document", path.display()))?;
    match salvor_graph::validate(&graph) {
        Ok(_) => Ok(graph),
        Err(errors) => {
            let mut message = format!("{}: {} validation error(s):", path.display(), errors.len());
            for error in &errors {
                message.push_str(&format!("\n  - {error}"));
            }
            bail!(message)
        }
    }
}

/// Builds every provided agent file, in the order given, and collects their MCP
/// sessions for the caller to keep alive and close.
///
/// The one place a `--agent` path becomes a live [`Agent`], so what
/// [`agent_hash`] prints, what [`build_graph_agents`] keys a graph node
/// against, and what `graph edit` resolves an `add agent --file` line to cannot
/// be three different numbers.
pub(crate) async fn build_agents(paths: &[PathBuf]) -> Result<(Vec<Agent>, Vec<McpServer>)> {
    let mut agents: Vec<Agent> = Vec::with_capacity(paths.len());
    let mut servers: Vec<McpServer> = Vec::new();
    for path in paths {
        let config = AgentConfig::load(path)?;
        let (agent, agent_servers) = agent_config::build_agent(&config, path, false).await?;
        agents.push(agent);
        servers.extend(agent_servers);
    }
    Ok((agents, servers))
}

/// Builds every provided agent file, keyed by its computed definition hash, and
/// collects their MCP sessions for the caller to keep alive and close.
async fn build_graph_agents(paths: &[PathBuf]) -> Result<(HashMap<String, Agent>, Vec<McpServer>)> {
    let (built, servers) = build_agents(paths).await?;
    let agents = built
        .into_iter()
        .map(|agent| (agent.def_hash().to_owned(), agent))
        .collect();
    Ok((agents, servers))
}

/// The first `tool` node in `graph`, when `agents` (the raw `--agent` paths, not
/// yet built) is empty; `None` when at least one `--agent` was given, whatever
/// the graph contains.
///
/// This is coarser than [`check_graph_resolvable`]'s own tool check: that one
/// asks whether some BUILT agent carries the exact tool a node names, which
/// needs a live [`Agent`] with its tool set populated. This one asks the
/// question a preview can still answer with nothing built at all: whether
/// there is any agent to ask in the first place. A `tool` node has no model of
/// its own to fall back on, so with zero `--agent` files given, every `tool`
/// node in the document is unreachable regardless of which one it names, and
/// `wake --dry-run` can say so directly instead of reporting the run "ready".
fn tool_node_without_any_agent<'a>(graph: &'a Graph, agents: &[PathBuf]) -> Option<&'a ToolNode> {
    if !agents.is_empty() {
        return None;
    }
    graph.nodes.iter().find_map(|node| match node {
        Node::Tool(tool) => Some(tool),
        _ => None,
    })
}

/// Checks every agent and tool the graph references resolves from what was
/// provided, before any run head is written. An `agent` node (or a
/// model-decision `branch`) whose hash matches no provided `--agent` fails with
/// the list of hashes that WERE provided; a `tool` node no provided agent
/// carries fails naming the node and tool.
fn check_graph_resolvable(graph: &Graph, agents: &HashMap<String, Agent>) -> Result<()> {
    for node in &graph.nodes {
        let referenced = match node {
            Node::Agent(agent) => Some((agent.id.as_str(), agent.agent_hash.as_str())),
            Node::Branch(branch) => branch
                .agent_hash
                .as_deref()
                .map(|hash| (branch.id.as_str(), hash)),
            _ => None,
        };
        if let Some((node_id, hash)) = referenced
            && !agents.contains_key(hash)
        {
            let provided: Vec<&str> = agents.keys().map(String::as_str).collect();
            let provided = if provided.is_empty() {
                "none".to_owned()
            } else {
                provided.join(", ")
            };
            bail!(
                "node `{node_id}` references agent `{hash}`, which none of the provided --agent \
                 files supply (provided: {provided})"
            );
        }
    }
    for node in &graph.nodes {
        if let Node::Tool(tool) = node
            && !agents
                .values()
                .any(|agent| agent.tools().get(&tool.tool).is_some())
        {
            bail!(
                "the document names tool `{}` in node `{}`, which none of the provided agents carry; \
                 every tool node must resolve before a graph run drives, so pass --agent with an \
                 agent file whose tools include it",
                tool.tool,
                tool.id
            );
        }
    }
    Ok(())
}

/// What `--agent` matches against: the agent definition hash for an ordinary run, or the literal
/// `graph run` for a graph, which has no single agent to name.
///
/// The same distinction the web UI's agent column draws, and for the same reason: a graph run
/// genuinely has no one agent, so naming one would be a convenient lie.
///
/// Shared with [`crate::completion`], which offers these same identities as Tab candidates for the
/// flag: what completion offers and what the filter matches are one definition, so they cannot
/// drift into offering a value the filter would never match.
pub(crate) fn agent_identity(log: &[EventEnvelope]) -> String {
    log.iter()
        .find_map(|envelope| match &envelope.event {
            Event::RunStarted { agent_def_hash, .. } => Some(agent_def_hash.clone()),
            Event::GraphRunStarted { .. } => Some("graph run".to_owned()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Whether a run's log is a graph run: its first event is `GraphRunStarted`.
fn is_graph_run(log: &[EventEnvelope]) -> bool {
    matches!(
        log.first().map(|envelope| &envelope.event),
        Some(Event::GraphRunStarted { .. })
    )
}

/// The `graph_hash` recorded in a graph run's `GraphRunStarted` head.
fn recorded_graph_hash(log: &[EventEnvelope]) -> Option<String> {
    log.iter().find_map(|envelope| match &envelope.event {
        Event::GraphRunStarted { graph_hash, .. } => Some(graph_hash.clone()),
        _ => None,
    })
}

/// The `agent_def_hash` recorded in an agent run's `RunStarted` head.
fn recorded_agent_def_hash(log: &[EventEnvelope]) -> Option<String> {
    log.iter().find_map(|envelope| match &envelope.event {
        Event::RunStarted { agent_def_hash, .. } => Some(agent_def_hash.clone()),
        _ => None,
    })
}

/// The single agent path an agent-run resume needs, or a clear refusal when
/// none or several were passed.
fn single_agent(agents: &[PathBuf]) -> Result<&Path> {
    match agents {
        [one] => Ok(one),
        [] => bail!("resuming an agent run needs its definition; pass --agent <file>"),
        _ => bail!(
            "an agent run resumes under exactly one --agent; pass --graph to resume a graph run \
             with multiple agents"
        ),
    }
}

/// Parses `key=value` label arguments into the map the runtime stamps, checking
/// the same bounds the server does. `None` when no labels were passed.
fn parse_label_args(labels: &[String]) -> Result<Option<BTreeMap<String, String>>> {
    if labels.is_empty() {
        return Ok(None);
    }
    let mut map = BTreeMap::new();
    for label in labels {
        let (key, value) = label
            .split_once('=')
            .with_context(|| format!("label `{label}` must be key=value"))?;
        map.insert(key.to_owned(), value.to_owned());
    }
    validate_labels(&map).map_err(anyhow::Error::msg)?;
    Ok(Some(map))
}

/// Resolves the `--acknowledge-writes` argument to the set of origin log
/// positions the operator acknowledged: `all` expands to the full hazard set,
/// an omitted argument is the empty set, and anything else is a comma-separated
/// list of `u64` seqs. A non-numeric entry is a precise error.
fn parse_acknowledge_writes(arg: Option<&str>, hazard_seqs: &[u64]) -> Result<HashSet<u64>> {
    match arg.map(str::trim) {
        None | Some("") => Ok(HashSet::new()),
        Some("all") => Ok(hazard_seqs.iter().copied().collect()),
        Some(list) => list
            .split(',')
            .map(|item| {
                let item = item.trim();
                item.parse::<u64>().with_context(|| {
                    format!("`{item}` is not a valid log position; use `4,7` or `all`")
                })
            })
            .collect(),
    }
}

/// Renders the dry-run preview: what the fork would do, creating nothing.
fn render_fork_preview(plan: &salvor_engine::ForkPlan, missing: &[u64]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "fork of run {} from node `{}` (dry run):\n",
        plan.origin_run().as_uuid(),
        plan.from_node()
    ));
    out.push_str(&format!(
        "  prefix: {} event(s), through seq {}\n",
        plan.prefix_len(),
        plan.through_seq().get()
    ));
    if plan.hazards().is_empty() {
        out.push_str("  no writes in the re-walked segment; nothing to acknowledge.\n");
    } else {
        out.push_str(&format!(
            "  {} write(s) the re-walked segment would re-execute:\n",
            plan.hazards().len()
        ));
        for hazard in plan.hazards() {
            out.push_str(&render_hazard_line(hazard));
        }
        if missing.is_empty() {
            out.push_str("  all acknowledged; the fork would proceed.\n");
        } else {
            out.push_str(&format!(
                "  would refuse: seq(s) {} still need acknowledgement.\n",
                seq_list(missing)
            ));
        }
    }
    out
}

/// Renders the refusal report for an unacknowledged fork: the writes that would
/// re-fire, and how to acknowledge them. Mirrors the reconciliation report's
/// posture (show the intent, then how to proceed).
fn render_fork_refusal(origin: &str, from_node: &str, unacked: &[&WriteHazard]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "refused: forking run {origin} from node `{from_node}` would re-execute {} recorded \
         write(s) the segment re-walks:\n",
        unacked.len()
    ));
    for hazard in unacked {
        out.push_str(&render_hazard_line(hazard));
    }
    let seqs: Vec<u64> = unacked.iter().map(|hazard| hazard.seq).collect();
    out.push_str(&format!(
        "acknowledge that they may re-fire, then fork:\n  salvor fork {origin} --from-node \
         {from_node} --graph <graph.json> --agent <file>... --acknowledge-writes {}\n",
        seq_list(&seqs)
    ));
    out
}

/// One write-hazard line: its seq, tool, and recorded input.
fn render_hazard_line(hazard: &WriteHazard) -> String {
    format!(
        "    - seq {} `{}` input {}\n",
        hazard.seq,
        hazard.tool,
        render::pretty_json(&hazard.input).trim_end()
    )
}

/// A comma-separated seq list for a command hint.
fn seq_list(seqs: &[u64]) -> String {
    seqs.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Settles what a graph drive returned before it is reported: a PERMANENT
/// engine refusal gets its terminal `RunFailed` recorded here, and the message
/// the operator reads names the triage plainly.
///
/// This is the CLI half of the discipline
/// [`salvor_engine::record_permanent_refusal`] documents. The engine refuses
/// without writing a terminal, on purpose; whether the refused run is dead or
/// merely stuck is the driver's call, and for `graph run`, `resume`, and
/// `graph fork` this is that driver. A permanent refusal (a route no case
/// matched, a fold that reached its bound under `on_bound: fail`, a body form
/// that cannot run) will refuse identically on every future drive, so the run
/// is dead and must say so rather than sitting in `salvor list` as `running`
/// forever. A transient one (an agent nobody registered, a tool call that
/// failed, a store hiccup) is left recoverable and reads exactly as it did.
///
/// A failure to record is deliberately not surfaced in place of the refusal:
/// the refusal is the real news, and a missing terminal only means the run
/// reads as recoverable when it is not. It is logged at `warn` and the drive's
/// own error is returned.
///
/// # The transient counterpart
///
/// A permanent refusal tells the operator the run is dead and where to read
/// that. A TRANSIENT one (a model transport failure, a tool call that failed, a
/// store hiccup) leaves a run that is alive, on disk, and resumable, and saying
/// only what went wrong strands it: the id scrolled past with the run's first
/// line, and nothing on screen says the work so far survived. So the same
/// triage is printed for it, in its own direction: the id, that the run is
/// recorded and resumable, and the exact `salvor resume` command. Only when a
/// run head exists (`next_seq` past zero), because a refusal raised before
/// `GraphRunStarted` has no run to point at.
async fn settle_graph_drive(
    ctx: &mut RunCtx,
    uuid: &str,
    graph_path: &Path,
    agents: &[PathBuf],
    outcome: Result<GraphOutcome, EngineError>,
    store_path: &Path,
) -> Result<GraphOutcome> {
    let error = match outcome {
        Ok(outcome) => return Ok(outcome),
        Err(error) => error,
    };
    // Read before the append below, so a permanent refusal's own terminal never
    // counts as the head this asks about.
    let has_head = ctx.next_seq().get() > 0;
    match salvor_engine::record_permanent_refusal(ctx, &error).await {
        Ok(true) => Err(anyhow::anyhow!(
            "{error}\n\nthis refusal is a pure function of the graph document and the recorded \
             log, so re-driving reproduces it exactly; run {uuid} is recorded as failed, and \
             `salvor list` shows it as failed."
        )),
        // A position conflict is the store arbitrating two writers on one run,
        // so the triage below is the wrong advice twice over: the run is not
        // waiting for anyone to re-drive it, and another driver is on it right
        // now. It also cannot survive being formatted into a message, and a
        // caller that has to tell a lost race from a broken drive (see
        // `classify_failed_wake`) has nothing but the type to tell it by, so
        // this one travels up whole.
        Ok(false) if drive_lost_a_position_race(&error) => Err(error.into()),
        Ok(false) if has_head => Err(anyhow::anyhow!(
            "{error}\n\nrun {uuid} is recorded and resumable: every step it completed is durable, \
             and nothing was recorded past this failure. Re-drive it with:\n  {}",
            graph_resume_command(uuid, graph_path, agents, store_path)
        )),
        Ok(false) => Err(error.into()),
        Err(recording) => {
            tracing::warn!(
                run_id = %uuid,
                %recording,
                "could not record the terminal for a permanent graph refusal"
            );
            Err(error.into())
        }
    }
}

/// The `salvor resume` command line that re-drives one graph run: the id, the
/// store it lives in, the document, and every `--agent` file the caller
/// supplied, in the order they were given. One place, so the parked report and
/// the transient-failure triage can never drift into telling an operator two
/// different commands for the same run. `--store` sits right after the id,
/// matching the form [`render::parked_report`] and its siblings already print
/// for an agent run, so the two verbs read as one convention rather than two.
fn graph_resume_command(
    uuid: &str,
    graph_path: &Path,
    agents: &[PathBuf],
    store_path: &Path,
) -> String {
    format!(
        "salvor resume {uuid} --store {} --graph {}{}",
        store_path.display(),
        graph_path.display(),
        agent_flags(agents)
    )
}

/// Every `--agent` file the caller supplied, in the order they were given, as
/// command-line flags. Shared by the two commands a parked graph run is told to
/// type: the `resume` above, and the `wake` a timer park needs instead.
fn agent_flags(agents: &[PathBuf]) -> String {
    agents
        .iter()
        .map(|path| format!(" --agent {}", path.display()))
        .collect()
}

/// Prints the result of a graph drive: the final output on completion, or a
/// parked report telling the operator how to continue with `salvor resume
/// --graph`. Both are exit code 0, exactly as an agent run's park is.
fn report_graph_outcome(
    outcome: GraphOutcome,
    uuid: &str,
    graph_path: &Path,
    agents: &[PathBuf],
    store_path: &Path,
) -> Result<u8> {
    match outcome {
        GraphOutcome::Completed { output } => {
            println!("{}", render::pretty_json(&output));
            Ok(0)
        }
        GraphOutcome::Parked { node, reason } => {
            println!("graph run {uuid} parked at node `{node}`.");
            match &reason {
                ParkReason::Suspended { reason, .. } => println!("  reason: {reason}"),
                ParkReason::BudgetExceeded { budget, observed } => {
                    println!("  budget crossed: {budget:?} (observed {observed})");
                }
                ParkReason::Sleeping { wake_at } => {
                    println!("  sleeping until: {}", render::format_ts(*wake_at));
                }
            }
            // A timer park takes no input and is not resumed by hand: it is
            // driven again once its deadline passes, which is what `wake`
            // does for every run that is due. `--store` sits right after the
            // verb here, matching how `render::parked_report`'s own sleeping
            // hint places it after `salvor wake`.
            if let ParkReason::Sleeping { .. } = reason {
                println!(
                    "it continues once the deadline passes:\n  salvor wake --store {} --graph {}{}",
                    store_path.display(),
                    graph_path.display(),
                    agent_flags(agents)
                );
                return Ok(0);
            }
            println!(
                "resume it with:\n  {} --input <json>",
                graph_resume_command(uuid, graph_path, agents, store_path)
            );
            Ok(0)
        }
    }
}

/// Builds a live agent from a submitted definition, for the `serve` factory.
/// Turns any failure into a human message the server maps to a `400`.
async fn build_from_definition(definition: AgentDefinition) -> Result<BuiltAgent, String> {
    let text = String::from_utf8(definition.body)
        .map_err(|_| "agent definition is not valid UTF-8".to_owned())?;
    let config = match definition.format {
        DefFormat::Toml => AgentConfig::from_toml_str(&text),
        DefFormat::Json => AgentConfig::from_json_str(&text),
    }
    .map_err(|error| format!("{error:#}"))?;

    // Relative paths in a submitted definition resolve against the server's
    // working directory; the pseudo path's parent is that directory.
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let pseudo_path = base.join("agent-definition");
    let (agent, servers) = agent_config::build_agent(&config, &pseudo_path, false)
        .await
        .map_err(|error| format!("{error:#}"))?;
    Ok(BuiltAgent { agent, servers })
}

/// Prints the final result of a completed run, or the parked report of a
/// suspended one. Both are exit code 0. `store_path` is the store this
/// command resolved (flag, then `SALVOR_STORE`, then the default), printed
/// into the parked report's resume/wake hint so it is the real command to
/// type.
fn report_outcome(
    outcome: RunOutcome,
    uuid: &str,
    agent_path: &Path,
    store_path: &Path,
) -> Result<u8> {
    match outcome {
        RunOutcome::Completed { output, .. } => {
            println!("{}", render::pretty_json(&output));
            Ok(0)
        }
        RunOutcome::Parked { reason, .. } => {
            print!(
                "{}",
                render::parked_report(
                    uuid,
                    &reason,
                    agent_path,
                    Some(store_path),
                    render::DEFAULT_REPORT_WIDTH
                )
            );
            Ok(0)
        }
    }
}

/// Stops the fixture's scripted model, if there was one. A `None` (the
/// ordinary `--agent`/`--input` run) is a no-op, so the call site stays one
/// unconditional line next to `close_servers`.
async fn shutdown_model(model: Option<crate::fixture::FixtureModel>) {
    if let Some(model) = model {
        model.shutdown().await;
    }
}

/// Closes every MCP server session tidily. Errors are logged, not propagated:
/// the run already finished, so a teardown hiccup must not fail the command.
pub(crate) async fn close_servers(servers: Vec<salvor_tools::mcp::McpServer>) {
    for server in servers {
        if let Err(error) = server.close().await {
            tracing::warn!(%error, "MCP server did not shut down cleanly");
        }
    }
}

/// Opens the SQLite store, wrapped as the trait object the runtime holds.
fn open_store(path: &Path) -> Result<Arc<dyn EventStore>> {
    let store =
        SqliteStore::open(path).with_context(|| format!("opening store at {}", path.display()))?;
    Ok(Arc::new(store))
}

/// Parses a `--input` value: `@path` reads JSON from a file, anything else is
/// parsed as a JSON literal.
pub fn parse_input(raw: &str) -> Result<Value> {
    let text = if let Some(path) = raw.strip_prefix('@') {
        std::fs::read_to_string(path).with_context(|| format!("reading input file {path}"))?
    } else {
        raw.to_owned()
    };
    serde_json::from_str(&text).context("parsing --input as JSON (wrap a bare string in quotes)")
}

/// Parses a run id from its UUID string.
fn parse_run_id(text: &str) -> Result<RunId> {
    let uuid = Uuid::parse_str(text)
        .with_context(|| format!("`{text}` is not a valid run id (expected a UUID)"))?;
    Ok(RunId::from_uuid(uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store that will not open has two different answers, because it is two
    /// different problems. "file is not a database" is a typo in `--store`,
    /// fixed by pointing it elsewhere; anything else is a real store this
    /// command could not open, and calling that "not a salvor store" sends an
    /// operator looking for a path that was right all along.
    #[test]
    fn a_store_that_will_not_open_says_which_of_the_two_problems_it_is() {
        let path = Path::new("/var/lib/salvor/salvor.db");

        let not_a_store = open_refusal(
            path,
            &StoreError::Backend("file is not a database".to_owned()),
        );
        assert_eq!(
            not_a_store,
            "/var/lib/salvor/salvor.db is not a salvor store (file is not a database). Nothing \
             was read and nothing was created: check the path, or point --store at the database."
        );

        // SQLite repeats the path it was handed; the message has already
        // named it, so it is not printed twice.
        let unreadable = open_refusal(
            path,
            &StoreError::Backend(
                "unable to open database file: /var/lib/salvor/salvor.db".to_owned(),
            ),
        );
        assert_eq!(
            unreadable,
            "could not open the store at /var/lib/salvor/salvor.db: unable to open database \
             file. Nothing was read and nothing was created."
        );
        assert!(
            !unreadable.contains("is not a salvor store"),
            "a store that cannot be opened is not a store that is not one: {unreadable}"
        );

        // The framing goes either way: "storage backend error" is a sentence
        // about salvor's insides, and neither message repeats it.
        for message in [&not_a_store, &unreadable] {
            assert!(!message.contains("storage backend"), "{message}");
        }
    }
}

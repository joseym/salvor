//! The wake sweeper: the background task that re-drives runs whose durable
//! timer has come due.
//!
//! # Why a sweeper at all
//!
//! A run parked on a timer is passive data. Nothing in this process holds it,
//! nothing is scheduled for its instant, and a restart forgets nothing because
//! there was nothing to forget: the deadline lives in the log. So waking is not
//! a callback firing, it is somebody re-reading the store and re-driving what
//! is overdue. This task is that somebody, for an operator who runs a server;
//! `salvor wake` is the same thing for one who runs cron.
//!
//! # It is the resume path, not a second driver
//!
//! Every due run goes through [`crate::runs::redrive`], the exact function the
//! resume endpoint's recover arm calls. A woken run therefore rebuilds its
//! agent the same way, drives over the same loop (or the same graph engine),
//! records the same events, and reports the same errors as one a person woke
//! over HTTP. There is no wake-specific drive to keep in step with the real
//! one, and no wake-specific verb: the deadline is enforced inside
//! [`RunCtx::await_wake`](salvor_runtime::RunCtx::await_wake) against the
//! injected clock, so a run driven a minute early simply records nothing and
//! stays asleep.
//!
//! # Not fighting the drivers already running
//!
//! The server tracks which runs a task in this process is still driving
//! ([`AppState::is_run_active`]). The sweeper skips those, and it drives
//! sequentially, so within one pass it can never queue a run twice; across
//! passes, a run stays in that set from the moment
//! [`crate::runs::redrive`] spawns its task until the task ends, which is
//! exactly the span during which re-driving it would be wrong. A `sleeping`
//! status and an active driver are contradictory states in any case (the fold
//! reports sleeping only for a log that stopped), so the check is a guard
//! against a stale read, not the normal case.
//!
//! # Nor fighting a client
//!
//! A run opened through `/v1/client-runs` is driven by its caller under a
//! single-writer drive token, not by a task in this process, so
//! [`AppState::is_run_active`] never sees it; a separate check
//! ([`AppState::is_client_run`]) is what keeps the sweeper off it. A
//! client-driven run's timer is the client's to wake, since re-driving one
//! here would be a second writer racing its drive token, so a due one is
//! left asleep here regardless of how overdue it is.
//!
//! That check reads a registry that dies with the process, so the run's own
//! log is consulted too (see
//! [`client_runs::log_is_client_driven`](crate::client_runs::log_is_client_driven)):
//! a client-driven run's `RunStarted` records `driven_by: client`, which
//! survives a restart the leases do not. A restarted server therefore still
//! leaves a napping client-driven run to its client, rather than adopting
//! every one it no longer remembers.
//!
//! # What the sweeper records as its caller
//!
//! Nobody asked for a wake: a deadline passed. So the name this sweep hands
//! the drive is [`SWEEPER_CALLER`], not a person, and a reader of the log can
//! tell a timer firing apart from an operator answering a gate. In practice it
//! reaches no event today: waking a run is a recover (see
//! [`crate::runs::redrive`]), and a recover records no `RunStarted`, `Resumed`,
//! or `RunAbandoned`, which are the only events with a caller field. The name
//! travels with the drive so that stays true by construction rather than by
//! nobody having passed one.
//!
//! # One bad run does not stop the sweep
//!
//! Every failure is per-run: an agent this server has never had registered, a
//! graph it does not hold, a build that will not build. Each is logged and the
//! loop moves to the next run, and the run is left asleep with its log
//! untouched, still due, so registering the missing definition is enough to
//! make the next pass wake it. Only the store listing itself failing ends a
//! pass, and even that only ends the pass: the next one tries again.
//!
//! An unwakeable run logs the same fields every pass, but only the first
//! sighting is loud: [`AppState::mark_unwakeable_warned`] names the first pass
//! `WARN` and every later one `DEBUG`, so an operator learns about the gap
//! once instead of every sweep interval for as long as it stays unregistered,
//! while the fields to find and fix it stay available to anyone watching at
//! debug level. The record clears the moment the run wakes or drops out of
//! the due set, so it never mutes a genuinely new nap.

use std::collections::HashSet;

use salvor_core::RunId;
use tokio::task::JoinHandle;

use crate::state::AppState;

/// The name a wake this sweeper drove records as its caller: a machine, and it
/// says so. Nobody asked for a wake, so the name never claims a person did.
pub const SWEEPER_CALLER: &str = "server:wake";

/// A running sweeper, which stops when this value is dropped.
///
/// A guard rather than a bare [`JoinHandle`] because the task outlives every
/// scope that could remember to stop it otherwise: [`crate::serve`] is itself
/// commonly aborted (a test tearing a server down, a shutdown signal), and an
/// abort runs no cleanup code, only drops. Dropping the guard is therefore the
/// only teardown that always happens.
pub struct Sweeper(Option<JoinHandle<()>>);

impl Drop for Sweeper {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Spawns the sweeper over `state`.
///
/// A zero [`AppState::wake_interval`] is the off switch: no task is spawned,
/// and nothing on this server wakes a timer. The returned guard is inert in
/// that case, so a caller holds it unconditionally.
#[must_use]
pub fn spawn_sweeper(state: AppState) -> Sweeper {
    let interval = state.wake_interval();
    if interval.is_zero() {
        tracing::info!("wake sweeper off; sleeping runs wake only through `salvor wake`");
        return Sweeper(None);
    }
    tracing::info!(
        interval_secs = interval.as_secs_f64(),
        "wake sweeper started"
    );
    Sweeper(Some(tokio::spawn(async move {
        loop {
            // Sleep first. A server that has just started has nothing in flight
            // and every sweep costs a fold of every log, so the interval is the
            // right amount of work to do before the first pass, not after it.
            tokio::time::sleep(interval).await;
            sweep(&state).await;
        }
    })))
}

/// One pass: select the runs whose deadline has passed, re-drive each, and
/// report the ids a drive was started for.
///
/// The loop calls this on its interval; it is public so a host on its own
/// schedule, or a test that must not race one, can run exactly one pass.
pub async fn sweep(state: &AppState) -> Vec<RunId> {
    // The state's own clock, so a test that injects one selects against the
    // same instant the drive will measure the deadline with.
    let now = state.now();
    let store = state.store();
    let due = match salvor_runtime::due_runs(store.as_ref(), now).await {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(%error, "wake sweep could not list runs; retrying next pass");
            return Vec::new();
        }
    };

    // A run that warned on an earlier pass but is not due this pass has
    // nothing left to warn about here; drop its record rather than let it
    // linger and mute a warning about some unrelated later nap.
    let due_ids: HashSet<RunId> = due.iter().map(|run| run.run_id).collect();
    state.prune_unwakeable_warned(&due_ids);

    let mut driven = Vec::new();
    for run in due {
        // A client-driven run (opened through `/v1/client-runs`) holds a
        // single-writer drive token that a caller presents on every append;
        // `runs::redrive` spawning a server task against the same log would
        // be a second writer racing the client for the same sequence
        // numbers. A client-driven run's timer is the client's to wake for
        // exactly that reason, so the sweeper leaves any run with a lease
        // alone, current or lapsed: a
        // lapsed lease still means a client opened this run and may resume
        // driving it, not that this server may. It stays due; the client's
        // own resume path (or a fresh open) is what wakes it.
        if state.is_client_run(run.run_id) {
            tracing::debug!(
                run_id = %run.run_id.as_uuid(),
                "skipping a due run that is client-driven"
            );
            continue;
        }
        if state.is_run_active(run.run_id) {
            tracing::debug!(
                run_id = %run.run_id.as_uuid(),
                "skipping a due run a driver in this process is already on"
            );
            continue;
        }
        let log = match store.read_log(run.run_id).await {
            Ok(log) => log,
            Err(error) => {
                tracing::warn!(
                    run_id = %run.run_id.as_uuid(),
                    %error,
                    "wake sweep could not read a due run's log; leaving it asleep"
                );
                continue;
            }
        };
        // The same skip, on the log's own evidence. The lease check above
        // knows only the runs this process opened, so after a restart it does
        // not recognize a run a client is still driving; the `driven_by` its
        // `RunStarted` records does, because the log outlived the process the
        // leases died with. Without this, the first restart would put this
        // sweeper back to racing clients for their runs' log positions.
        if crate::client_runs::log_is_client_driven(&log) {
            tracing::debug!(
                run_id = %run.run_id.as_uuid(),
                "skipping a due run whose log records it as client-driven"
            );
            continue;
        }
        match crate::runs::redrive(
            state.clone(),
            run.run_id,
            &log,
            Some(SWEEPER_CALLER.to_owned()),
        )
        .await
        {
            Ok(_) => {
                state.clear_unwakeable_warned(run.run_id);
                tracing::info!(
                    run_id = %run.run_id.as_uuid(),
                    wake_at = %run.wake_at,
                    "waking a run whose timer came due"
                );
                driven.push(run.run_id);
            }
            // Not an error of the sweeper's: this server cannot wake a run
            // whose agent or graph it does not hold. The run stays asleep and
            // still due, so registering the definition is all it takes; no
            // rate limiting on whether this is logged, only on how loud: the
            // first sighting is `warn!`, an ongoing, actionable condition an
            // operator should find without turning on debug-level noise;
            // every later pass while the record stands repeats the same
            // fields at `debug!` instead, so a definition left unregistered
            // for a week does not page the same warning every sweep interval.
            Err(error) => {
                if state.mark_unwakeable_warned(run.run_id) {
                    tracing::warn!(
                        run_id = %run.run_id.as_uuid(),
                        ?error,
                        "cannot wake this run here; leaving it asleep; wake it with salvor wake, \
                         passing the --agent/--graph files it was started with"
                    );
                } else {
                    tracing::debug!(
                        run_id = %run.run_id.as_uuid(),
                        ?error,
                        "cannot wake this run here; leaving it asleep; wake it with salvor wake, \
                         passing the --agent/--graph files it was started with"
                    );
                }
            }
        }
    }
    driven
}

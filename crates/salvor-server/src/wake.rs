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
//! ([`AppState::is_client_run`]) is what keeps the sweeper off it. Timers and
//! signals are out of scope for client-driven runs for now, so a due one is
//! left asleep here regardless of how overdue it is.
//!
//! # One bad run does not stop the sweep
//!
//! Every failure is per-run: an agent this server has never had registered, a
//! graph it does not hold, a build that will not build. Each is logged and the
//! loop moves to the next run, and the run is left asleep with its log
//! untouched, still due, so registering the missing definition is enough to
//! make the next pass wake it. Only the store listing itself failing ends a
//! pass, and even that only ends the pass: the next one tries again.

use salvor_core::RunId;
use tokio::task::JoinHandle;

use crate::state::AppState;

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

    let mut driven = Vec::new();
    for run in due {
        // A client-driven run (opened through `/v1/client-runs`) holds a
        // single-writer drive token that a caller presents on every append;
        // `runs::redrive` spawning a server task against the same log would
        // be a second writer racing the client for the same sequence
        // numbers. Timers and signals are out of scope for client-driven
        // runs for exactly that reason (design decision, 2026-08-05), so the
        // sweeper leaves any run with a lease alone, current or lapsed: a
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
        match crate::runs::redrive(state.clone(), run.run_id, &log).await {
            Ok(_) => {
                tracing::info!(
                    run_id = %run.run_id.as_uuid(),
                    wake_at = %run.wake_at,
                    "waking a run whose timer came due"
                );
                driven.push(run.run_id);
            }
            // Not an error of the sweeper's: this server cannot wake a run
            // whose agent or graph it does not hold. Still `warn!`, not
            // `info!`: this fires every sweep interval for as long as the
            // definition stays unregistered, which is exactly the kind of
            // ongoing, actionable condition an operator should be able to
            // find without turning on info-level noise. The run stays
            // asleep and still due, so registering the definition is all it
            // takes; no rate limiting here, a warn per sweep is honest.
            Err(error) => tracing::warn!(
                run_id = %run.run_id.as_uuid(),
                ?error,
                "cannot wake this run here; leaving it asleep; wake it with salvor wake, \
                 passing the --agent/--graph files it was started with"
            ),
        }
    }
    driven
}

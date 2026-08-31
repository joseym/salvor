//! Runtime budgets: the declared limits ([`Budgets`]), the pricing table a
//! cost budget needs ([`Pricing`]), the extensions a human grants at resume
//! time ([`BudgetExtensions`]), and the crossing check itself.
//!
//! Named `Budgets` (plural) deliberately: [`Budget`] is the *event payload*
//! naming which single limit was crossed; this type is the *declaration* of
//! every limit an agent runs under.
//!
//! # Why this lives in the pure crate
//!
//! Every input to a check is replayed data (see the determinism section
//! below), so the check itself never touches a clock, a store, or a network:
//! it is arithmetic over recorded numbers. It sits here rather than in
//! `salvor-runtime` for the same reason [`derive_state`](crate::derive_state)
//! does: the runtime enforces budgets at the IO edge and a browser wants to
//! evaluate the identical rule client-side, and one implementation serving
//! both is the only way the two cannot disagree. `salvor-runtime` re-exports
//! every name here, so `salvor_runtime::Budgets` and the rest keep resolving.
//!
//! [`budget_observations`] closes the loop: it folds a recorded log into the
//! [`BudgetObservations`] the loop would have built at that point, so a caller
//! holding only the log can run the real check rather than approximate it.
//!
//! # Determinism
//!
//! Budget checks run between events, before each model call, and every
//! input to a check is replayed data:
//!
//! - **steps** counts completed model calls in this drive of the loop.
//! - **tokens** and **cost** accumulate the recorded usage of completed
//!   model calls (cost multiplies those integers by the agent's fixed
//!   [`Pricing`]).
//! - **wall time** is derived only from recorded `ctx.now()` observations
//!   taken at each loop-iteration start, never from the ambient clock, minus
//!   any span the run spent asleep on a durable timer (see
//!   [`budget_observations`] for why).
//!
//! So a crossing that fired live recomputes identically on replay, and the
//! cursor matches it against the recorded `BudgetExceeded` event. A check
//! fires when the observed value reaches or passes the effective limit
//! (`observed >= limit`), and checks are evaluated in a fixed documented
//! order: steps, tokens, cost, wall time.
//!
//! # The extension shape
//!
//! A budget crossing parks the run. Resuming it may carry an extension in
//! the resume input, under the reserved `extend` key:
//!
//! ```json
//! {
//!     "extend": {
//!         "steps": 5,
//!         "tokens": 20000,
//!         "cost_usd": 1.5,
//!         "wall_time_seconds": 600.0
//!     }
//! }
//! ```
//!
//! Every field is optional; `steps` and `tokens` are unsigned integers,
//! `cost_usd` and `wall_time_seconds` are numbers. The effective limit for
//! each dimension is the declared limit plus the sum of every recorded
//! extension. Extensions live inside recorded `Resumed` events, so replay
//! sees exactly the extensions the live run saw, in the same order, and the
//! effective budget evolves identically. [`validate_extension_input`] is the
//! shape check `Runtime::resume` applies before recording anything: the top
//! level may contain only `extend`, and `extend` may contain only the four
//! keys above with the right JSON types.

use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;

use crate::{Budget, BudgetKind, Event, EventEnvelope};

/// The limits an agent declares. Every dimension is optional; an absent
/// dimension is never checked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Budgets {
    /// Maximum loop iterations, counted as completed model calls.
    pub max_steps: Option<u64>,
    /// Maximum total recorded tokens (input plus output) across the run.
    pub max_tokens: Option<u64>,
    /// Maximum cost in US dollars, computed from recorded usage and the
    /// agent's [`Pricing`]. Declaring this without pricing is a build-time
    /// error on the agent builder.
    pub max_cost_usd: Option<f64>,
    /// Maximum wall time, measured between recorded `ctx.now()`
    /// observations, never against the ambient clock.
    pub max_wall_time: Option<Duration>,
}

impl Budgets {
    /// Whether any dimension is declared at all.
    #[must_use]
    pub fn any_declared(&self) -> bool {
        self.max_steps.is_some()
            || self.max_tokens.is_some()
            || self.max_cost_usd.is_some()
            || self.max_wall_time.is_some()
    }

    /// The first crossing, if any, in the fixed check order (steps, tokens,
    /// cost, wall time). Returns the crossed [`Budget`] (whose `limit` is
    /// the *effective* limit: declared plus extensions) and the observed
    /// value, both exactly as they will be recorded.
    #[must_use]
    pub fn first_crossing(
        &self,
        extensions: &BudgetExtensions,
        pricing: Option<&Pricing>,
        observations: &BudgetObservations,
    ) -> Option<(Budget, f64)> {
        if let Some(max_steps) = self.max_steps {
            let limit = to_f64(max_steps.saturating_add(extensions.steps));
            let observed = to_f64(observations.steps);
            if observed >= limit {
                return Some((
                    Budget {
                        kind: BudgetKind::Steps,
                        limit,
                    },
                    observed,
                ));
            }
        }
        if let Some(max_tokens) = self.max_tokens {
            let limit = to_f64(max_tokens.saturating_add(extensions.tokens));
            let observed = to_f64(
                observations
                    .input_tokens
                    .saturating_add(observations.output_tokens),
            );
            if observed >= limit {
                return Some((
                    Budget {
                        kind: BudgetKind::Tokens,
                        limit,
                    },
                    observed,
                ));
            }
        }
        if let (Some(max_cost), Some(pricing)) = (self.max_cost_usd, pricing) {
            let limit = max_cost + extensions.cost_usd;
            let observed = pricing.cost_usd(observations.input_tokens, observations.output_tokens);
            if observed >= limit {
                return Some((
                    Budget {
                        kind: BudgetKind::CostUsd,
                        limit,
                    },
                    observed,
                ));
            }
        }
        if let Some(max_wall) = self.max_wall_time {
            let limit = max_wall.as_secs_f64() + extensions.wall_time_seconds;
            let observed = observations.elapsed_seconds;
            if observed >= limit {
                return Some((
                    Budget {
                        kind: BudgetKind::WallTime,
                        limit,
                    },
                    observed,
                ));
            }
        }
        None
    }
}

/// Per-token pricing, in US dollars per million tokens. Required by the
/// agent builder whenever a cost budget is declared.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pricing {
    /// Dollars per million input tokens.
    pub input_per_mtok: f64,
    /// Dollars per million output tokens.
    pub output_per_mtok: f64,
}

impl Pricing {
    /// The cost of the given recorded token counts under this pricing. A
    /// pure function of integers and the fixed rates, so it reproduces bit
    /// for bit on replay.
    #[must_use]
    pub fn cost_usd(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        to_f64(input_tokens) / 1_000_000.0 * self.input_per_mtok
            + to_f64(output_tokens) / 1_000_000.0 * self.output_per_mtok
    }
}

/// The replay-derived quantities a budget check consumes. The loop builds
/// one of these at each iteration start, exclusively from recorded data.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BudgetObservations {
    /// Completed model calls so far.
    pub steps: u64,
    /// Recorded input tokens accumulated so far.
    pub input_tokens: u64,
    /// Recorded output tokens accumulated so far.
    pub output_tokens: u64,
    /// Seconds between the first recorded `ctx.now()` observation and the
    /// latest one, less every recorded sleep span (see
    /// [`budget_observations`]). Never negative.
    pub elapsed_seconds: f64,
}

/// Folds a recorded log into the observations a budget check consumes.
///
/// The runtime's loop accumulates these as it drives, but every quantity it
/// accumulates is itself recorded, so the same numbers can be read back out
/// of the log afterwards. That is what makes a check reproducible off the
/// event stream alone, which is what a browser has:
///
/// - **steps** is the count of `ModelCallCompleted` events, the loop's
///   "completed model calls".
/// - **tokens** are the recorded `usage` totals on those same events.
/// - **elapsed** is the span between the first and the last `NowObserved`,
///   the loop's baseline and its latest reading, less every span the run spent
///   asleep. Fewer than two observations means no span has elapsed, which is
///   zero.
///
/// Pass the prefix the check would have seen. The loop checks *before* each
/// model call, so the observations behind a recorded
/// [`Event::BudgetExceeded`] at position `n` are the fold of `log[..n]`.
///
/// # Why recorded sleep is excluded from wall time
///
/// A wall-time budget bounds how long a run may take, and a durable timer is
/// time the run deliberately did not take: a run told to sleep a week would
/// cross any declared `max_wall_time` the instant it woke, before doing a
/// single further step, which would turn every timer into a budget crossing.
/// So each span between an [`Event::SleepStarted`] and its
/// [`Event::SleepCompleted`] is summed and subtracted, using the two events'
/// recorded envelope timestamps as the span's endpoints. A sleep still open at
/// the end of the log contributes the span from its start to the last
/// observation, so a prefix cut mid-sleep excludes what it has seen of the
/// sleep so far.
///
/// Gate-wait time is deliberately not excluded. A run waiting on a human is
/// blocked on the outside world with no promised end, which is exactly the
/// thing a wall-time budget is there to catch, and every log recorded before
/// timers existed holds none of these events, so its elapsed figure and its
/// budget verdict are unchanged to the byte.
///
/// # Scope
///
/// This is the accounting of one agent run's log: what `salvor run` and
/// `salvor resume` record, and what the loop counts. It is the whole log
/// because the loop replays: on resume the driver re-enters at iteration
/// zero and every recorded call is replayed through it, so its counters
/// arrive at the live edge holding the run's full recorded history.
///
/// A graph run is deliberately not that shape. Its engine drives each node
/// through its own loop with its own counters, so folding a graph log whole
/// would sum quantities no single check ever saw. Fold one node's span, or
/// do not use this on a graph log.
#[must_use]
pub fn budget_observations(log: &[EventEnvelope]) -> BudgetObservations {
    let mut observations = BudgetObservations::default();
    let mut first_now = None;
    let mut last_now = None;
    let mut slept_seconds = 0.0;
    let mut sleeping_since: Option<OffsetDateTime> = None;

    for envelope in log {
        match &envelope.event {
            Event::ModelCallCompleted { usage, .. } => {
                observations.steps = observations.steps.saturating_add(1);
                observations.input_tokens = observations
                    .input_tokens
                    .saturating_add(u64::from(usage.input_tokens));
                observations.output_tokens = observations
                    .output_tokens
                    .saturating_add(u64::from(usage.output_tokens));
            }
            Event::NowObserved { now } => {
                first_now.get_or_insert(*now);
                last_now = Some(*now);
            }
            // The sleep span's endpoints are the two events' recorded
            // timestamps, the one instant each of them carries. A sleep with
            // no start before it closes nothing: the fold stays total over
            // every prefix, including one cut between the two.
            Event::SleepStarted { .. } => {
                sleeping_since = Some(envelope.recorded_at);
            }
            Event::SleepCompleted {} => {
                if let Some(started) = sleeping_since.take() {
                    slept_seconds += span_seconds(started, envelope.recorded_at);
                }
            }
            _ => {}
        }
    }

    if let (Some(first), Some(last)) = (first_now, last_now) {
        // A sleep the log never closed still ran until the last thing the run
        // observed, so it excludes what elapsed counted of it: no more, since
        // elapsed itself stops at that observation.
        if let Some(started) = sleeping_since {
            slept_seconds += span_seconds(started, last);
        }
        observations.elapsed_seconds = (span_seconds(first, last) - slept_seconds).max(0.0);
    }
    observations
}

/// The seconds from `from` to `to`, floored at zero.
///
/// Recorded timestamps come off the wire and this fold is total over whatever
/// a log holds, so a span that runs backwards contributes nothing rather than
/// crediting a run with time it never spent.
fn span_seconds(from: OffsetDateTime, to: OffsetDateTime) -> f64 {
    (to - from).as_seconds_f64().max(0.0)
}

/// The accumulated budget extensions granted by recorded resume inputs.
/// See the module docs for the JSON shape they are parsed from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BudgetExtensions {
    /// Extra steps granted.
    pub steps: u64,
    /// Extra tokens granted.
    pub tokens: u64,
    /// Extra dollars granted.
    pub cost_usd: f64,
    /// Extra wall-time seconds granted.
    pub wall_time_seconds: f64,
}

impl BudgetExtensions {
    /// Folds one resume input's `extend` object (if present) into the
    /// accumulated totals. Unknown or ill-typed fields are ignored here;
    /// rejecting them is [`validate_extension_input`]'s job, applied before
    /// the input was ever recorded.
    pub fn absorb(&mut self, resume_input: &Value) {
        let Some(extend) = resume_input.get("extend").and_then(Value::as_object) else {
            return;
        };
        if let Some(steps) = extend.get("steps").and_then(Value::as_u64) {
            self.steps = self.steps.saturating_add(steps);
        }
        if let Some(tokens) = extend.get("tokens").and_then(Value::as_u64) {
            self.tokens = self.tokens.saturating_add(tokens);
        }
        if let Some(cost) = extend.get("cost_usd").and_then(Value::as_f64) {
            self.cost_usd += cost;
        }
        if let Some(seconds) = extend.get("wall_time_seconds").and_then(Value::as_f64) {
            self.wall_time_seconds += seconds;
        }
    }
}

/// Folds a recorded log into the extensions a budget check has been granted.
///
/// The loop absorbs an extension exactly when a resume answers a budget
/// crossing, so this absorbs the input of a [`Event::Resumed`] whose
/// immediately preceding event is a [`Event::BudgetExceeded`], and no other.
/// That adjacency is not a heuristic: `ctx.budget_exceeded` records the
/// crossing and the `await_resume` that follows it records the answer, with
/// nothing in between. A resume answering a *suspension* is a different
/// conversation and is deliberately not absorbed here, even if its input
/// happens to carry a key spelled `extend`.
///
/// Like [`budget_observations`], this is the whole log because the loop
/// replays: on resume the driver re-absorbs every recorded extension in
/// order before it reaches the live edge.
#[must_use]
pub fn budget_extensions(log: &[EventEnvelope]) -> BudgetExtensions {
    let mut extensions = BudgetExtensions::default();
    for pair in log.windows(2) {
        if let (Event::BudgetExceeded { .. }, Event::Resumed { input, .. }) =
            (&pair[0].event, &pair[1].event)
        {
            extensions.absorb(input);
        }
    }
    extensions
}

/// Validates a resume input against the budget-extension shape documented
/// at module level. Applied by `Runtime::resume` when the run parked on a
/// budget crossing, *before* the input is recorded.
///
/// # Errors
///
/// Returns a human-readable description of the first violation: a non-object
/// input, an unexpected top-level key, a non-object `extend`, an unknown
/// key inside `extend`, or a field with the wrong JSON type.
pub fn validate_extension_input(input: &Value) -> Result<(), String> {
    let Some(top) = input.as_object() else {
        return Err("a budget-crossing resume input must be a JSON object".to_owned());
    };
    for key in top.keys() {
        if key != "extend" {
            return Err(format!(
                "unexpected top-level key `{key}`; a budget-crossing resume input may only carry `extend`"
            ));
        }
    }
    let Some(extend) = top.get("extend") else {
        return Ok(());
    };
    let Some(extend) = extend.as_object() else {
        return Err("`extend` must be a JSON object".to_owned());
    };
    for (key, value) in extend {
        match key.as_str() {
            "steps" | "tokens" => {
                if value.as_u64().is_none() {
                    return Err(format!("`extend.{key}` must be an unsigned integer"));
                }
            }
            "cost_usd" | "wall_time_seconds" => {
                if value.as_f64().is_none() {
                    return Err(format!("`extend.{key}` must be a number"));
                }
            }
            other => {
                return Err(format!(
                    "unknown key `extend.{other}`; expected steps, tokens, cost_usd, or wall_time_seconds"
                ));
            }
        }
    }
    Ok(())
}

/// Widens an integer count to `f64` for the wire's numeric budget fields.
/// Exact for every count below 2^53, far beyond any real run.
#[allow(clippy::cast_precision_loss)]
fn to_f64(count: u64) -> f64 {
    count as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{RunId, SequenceNumber};
    use serde_json::json;
    use time::macros::datetime;
    use uuid::Uuid;

    /// The run every log in these tests belongs to.
    fn run_id() -> RunId {
        RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000008").unwrap())
    }

    /// The baseline instant every scripted timestamp is an offset from.
    fn base() -> OffsetDateTime {
        datetime!(2026-07-09 12:00:00 UTC)
    }

    /// Wraps events, each carrying the recorded timestamp its `(seconds after
    /// base)` says. The envelope timestamp matters here: it is the endpoint a
    /// sleep span is measured from.
    fn log(events: Vec<(i64, Event)>) -> Vec<EventEnvelope> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, (offset, event))| {
                EventEnvelope::new(
                    run_id(),
                    SequenceNumber::new(i as u64),
                    base() + time::Duration::seconds(offset),
                    event,
                )
            })
            .collect()
    }

    fn started() -> Event {
        Event::RunStarted {
            agent_def_hash: "sha256:agent".into(),
            input: json!({}),
            labels: None,
            driven_by: None,
            caller: None,
        }
    }

    /// A one-minute wall-time budget, the limit every wall-time case below
    /// asks about.
    fn one_minute() -> Budgets {
        Budgets {
            max_wall_time: Some(Duration::from_secs(60)),
            ..Budgets::default()
        }
    }

    /// Whether the declared wall-time budget is crossed by these observations.
    fn crosses_wall_time(observations: &BudgetObservations) -> bool {
        matches!(
            one_minute().first_crossing(&BudgetExtensions::default(), None, observations),
            Some((budget, _)) if budget.kind == BudgetKind::WallTime
        )
    }

    const WEEK: i64 = 7 * 24 * 60 * 60;

    /// The same span as a count of seconds, for comparing against a derived
    /// `elapsed_seconds` without casting an integer by hand.
    fn seconds(count: i64) -> f64 {
        time::Duration::seconds(count).as_seconds_f64()
    }

    /// A run that slept a week and then worked for five seconds has five
    /// seconds of wall time, and does not cross a one-minute budget. The same
    /// log with the two sleep events removed does cross it, which is what the
    /// exclusion is for: without it every timer longer than the budget would
    /// park the run the moment it woke.
    #[test]
    fn a_week_long_sleep_is_excluded_from_wall_time() {
        let slept = budget_observations(&log(vec![
            (0, started()),
            (0, Event::NowObserved { now: base() }),
            (
                0,
                Event::SleepStarted {
                    wake_at: base() + time::Duration::seconds(WEEK),
                },
            ),
            (WEEK, Event::SleepCompleted {}),
            (
                WEEK + 5,
                Event::NowObserved {
                    now: base() + time::Duration::seconds(WEEK + 5),
                },
            ),
        ]));
        assert!((slept.elapsed_seconds - 5.0).abs() < 1e-9);
        assert!(!crosses_wall_time(&slept));

        let without_the_sleep = budget_observations(&log(vec![
            (0, started()),
            (0, Event::NowObserved { now: base() }),
            (
                WEEK + 5,
                Event::NowObserved {
                    now: base() + time::Duration::seconds(WEEK + 5),
                },
            ),
        ]));
        assert!(
            (without_the_sleep.elapsed_seconds - seconds(WEEK + 5)).abs() < 1e-9,
            "the same span, uncredited, is the whole week"
        );
        assert!(
            crosses_wall_time(&without_the_sleep),
            "without the exclusion this run crosses the budget on waking"
        );
    }

    /// A sleep the log never closed is handled at both shapes a prefix can cut
    /// it at: a log ending at the sleep start has nothing observed after it, so
    /// elapsed stops where it already stopped; a log that observed the clock
    /// again without recording a completion excludes the span it saw.
    #[test]
    fn an_open_sleep_at_the_end_of_a_log_is_handled() {
        let parked = budget_observations(&log(vec![
            (0, started()),
            (0, Event::NowObserved { now: base() }),
            (
                2,
                Event::SleepStarted {
                    wake_at: base() + time::Duration::seconds(WEEK),
                },
            ),
        ]));
        assert_eq!(
            parked.elapsed_seconds, 0.0,
            "one observation is no span, and the open sleep never credits one back"
        );

        let observed_while_open = budget_observations(&log(vec![
            (0, started()),
            (0, Event::NowObserved { now: base() }),
            (
                0,
                Event::SleepStarted {
                    wake_at: base() + time::Duration::seconds(WEEK),
                },
            ),
            (
                WEEK,
                Event::NowObserved {
                    now: base() + time::Duration::seconds(WEEK),
                },
            ),
        ]));
        assert_eq!(
            observed_while_open.elapsed_seconds, 0.0,
            "the open sleep runs to the last observation, so nothing is left over"
        );
        assert!(!crosses_wall_time(&observed_while_open));
    }

    /// The no-change proof: a run that waited a week on a human gate still
    /// counts every second of it. Gate waiting is exactly what a wall-time
    /// budget is meant to catch, and no log recorded before durable timers
    /// existed changes its verdict.
    #[test]
    fn a_gate_suspension_still_counts_as_wall_time() {
        let waited = budget_observations(&log(vec![
            (0, started()),
            (0, Event::NowObserved { now: base() }),
            (
                0,
                Event::Suspended {
                    reason: "awaiting approval".into(),
                    input_schema: json!({"type": "object"}),
                    kind: None,
                },
            ),
            (
                WEEK,
                Event::Resumed {
                    input: json!({"approved": true}),
                    caller: None,
                },
            ),
            (
                WEEK,
                Event::NowObserved {
                    now: base() + time::Duration::seconds(WEEK),
                },
            ),
        ]));
        assert!((waited.elapsed_seconds - seconds(WEEK)).abs() < 1e-9);
        assert!(
            crosses_wall_time(&waited),
            "a week spent waiting on a human is a week of wall time"
        );
    }

    /// The two rules together, in one log: a run waits ten minutes on a
    /// human gate, then sleeps for three hours before waking. Only the sleep
    /// span is excluded; the gate wait counts in full, so elapsed wall time
    /// is the whole span minus just the sleep.
    #[test]
    fn a_gate_wait_and_a_sleep_in_the_same_log_are_treated_differently() {
        const GATE_MINUTES: i64 = 10 * 60;
        const SLEEP_HOURS: i64 = 3 * 60 * 60;

        let mixed = budget_observations(&log(vec![
            (0, started()),
            (0, Event::NowObserved { now: base() }),
            (
                0,
                Event::Suspended {
                    reason: "awaiting approval".into(),
                    input_schema: json!({"type": "object"}),
                    kind: None,
                },
            ),
            (
                GATE_MINUTES,
                Event::Resumed {
                    input: json!({"approved": true}),
                    caller: None,
                },
            ),
            (
                GATE_MINUTES,
                Event::SleepStarted {
                    wake_at: base() + time::Duration::seconds(GATE_MINUTES + SLEEP_HOURS),
                },
            ),
            (GATE_MINUTES + SLEEP_HOURS, Event::SleepCompleted {}),
            (
                GATE_MINUTES + SLEEP_HOURS,
                Event::NowObserved {
                    now: base() + time::Duration::seconds(GATE_MINUTES + SLEEP_HOURS),
                },
            ),
        ]));
        assert!(
            (mixed.elapsed_seconds - seconds(GATE_MINUTES)).abs() < 1e-9,
            "the total span minus the sleep is just the gate wait"
        );
        assert!(
            crosses_wall_time(&mixed),
            "the ten-minute gate wait alone crosses a one-minute budget"
        );
    }

    /// Checks fire on reaching the limit and honor absorbed extensions.
    #[test]
    fn crossing_fires_at_limit_and_extensions_raise_it() {
        let budgets = Budgets {
            max_steps: Some(2),
            ..Budgets::default()
        };
        let mut extensions = BudgetExtensions::default();
        let observations = BudgetObservations {
            steps: 2,
            ..BudgetObservations::default()
        };

        let (budget, observed) = budgets
            .first_crossing(&extensions, None, &observations)
            .expect("steps crossing fires at the limit");
        assert_eq!(budget.kind, BudgetKind::Steps);
        assert_eq!(budget.limit, 2.0);
        assert_eq!(observed, 2.0);

        extensions.absorb(&json!({"extend": {"steps": 3}}));
        assert_eq!(
            budgets.first_crossing(&extensions, None, &observations),
            None,
            "the extension raises the effective limit past the observation"
        );
    }

    /// The documented check order: steps beats tokens when both cross.
    #[test]
    fn check_order_is_steps_first() {
        let budgets = Budgets {
            max_steps: Some(1),
            max_tokens: Some(10),
            ..Budgets::default()
        };
        let observations = BudgetObservations {
            steps: 1,
            input_tokens: 100,
            output_tokens: 100,
            ..BudgetObservations::default()
        };
        let (budget, _) = budgets
            .first_crossing(&BudgetExtensions::default(), None, &observations)
            .expect("a crossing fires");
        assert_eq!(budget.kind, BudgetKind::Steps);
    }

    /// Cost uses pricing over recorded token counts.
    #[test]
    fn cost_crossing_uses_pricing() {
        let budgets = Budgets {
            max_cost_usd: Some(1.0),
            ..Budgets::default()
        };
        let pricing = Pricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        let observations = BudgetObservations {
            input_tokens: 200_000,
            output_tokens: 40_000,
            ..BudgetObservations::default()
        };
        // 0.2 mtok * 3 + 0.04 mtok * 15 = 0.6 + 0.6 = 1.2 >= 1.0.
        let (budget, observed) = budgets
            .first_crossing(&BudgetExtensions::default(), Some(&pricing), &observations)
            .expect("cost crossing fires");
        assert_eq!(budget.kind, BudgetKind::CostUsd);
        assert!((observed - 1.2).abs() < 1e-12);
    }

    /// The extension validator accepts the documented shape and rejects
    /// obviously wrong ones.
    #[test]
    fn extension_validation_rejects_wrong_shapes() {
        assert!(validate_extension_input(&json!({})).is_ok());
        assert!(validate_extension_input(&json!({"extend": {"steps": 2}})).is_ok());
        assert!(
            validate_extension_input(&json!({
                "extend": {"steps": 1, "tokens": 2, "cost_usd": 0.5, "wall_time_seconds": 60}
            }))
            .is_ok()
        );
        assert!(validate_extension_input(&json!("more please")).is_err());
        assert!(validate_extension_input(&json!({"other": 1})).is_err());
        assert!(validate_extension_input(&json!({"extend": 5})).is_err());
        assert!(validate_extension_input(&json!({"extend": {"stepz": 1}})).is_err());
        assert!(validate_extension_input(&json!({"extend": {"steps": -1}})).is_err());
        assert!(validate_extension_input(&json!({"extend": {"cost_usd": "1"}})).is_err());
    }
}

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
//!   taken at each loop-iteration start, never from the ambient clock.
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
    /// latest one.
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
///   the loop's baseline and its latest reading. Fewer than two observations
///   means no span has elapsed, which is zero.
///
/// Pass the prefix the check would have seen. The loop checks *before* each
/// model call, so the observations behind a recorded
/// [`Event::BudgetExceeded`] at position `n` are the fold of `log[..n]`.
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
            _ => {}
        }
    }

    if let (Some(first), Some(last)) = (first_now, last_now) {
        observations.elapsed_seconds = (last - first).as_seconds_f64();
    }
    observations
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
        if let (Event::BudgetExceeded { .. }, Event::Resumed { input }) =
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
    use serde_json::json;

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

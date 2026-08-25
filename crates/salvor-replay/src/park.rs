//! [`ParkReason`]: why a run stopped short of completing.
//!
//! A run parks for exactly three reasons, and all three are recorded in the
//! log before anything stops: a tool suspended the run
//! ([`Event::Suspended`](crate::Event::Suspended)), a declared budget was
//! crossed ([`Event::BudgetExceeded`](crate::Event::BudgetExceeded)), or a
//! durable timer was started and has not come due
//! ([`Event::SleepStarted`](crate::Event::SleepStarted)). This type names that
//! set and carries the fields a caller needs in order to act on it: the schema
//! a resume input must satisfy, the budget and the observed value that crossed
//! it, or the instant the run may continue at.
//!
//! The three differ in what ends the park, and a caller has to know which:
//! two of them wait for input, and the third waits for an instant and takes no
//! input at all. Who owes that input is a further split inside the suspension,
//! which is what its `kind` carries: a gate waits for a person, a signal waits
//! for a webhook or a callback, and only the first is anyone's task.
//!
//! # Purity
//!
//! Every field here is read back out of recorded events, so the vocabulary
//! belongs with the event model rather than at the IO edge that produced it.
//! A renderer that turns a parked run into text needs these names without
//! needing the runtime, the store, or an executor behind them.

use serde_json::Value;
use time::OffsetDateTime;

use crate::event::{Budget, SuspensionKind};

/// Why a run parked instead of completing.
#[derive(Debug, Clone)]
pub enum ParkReason {
    /// A tool suspended the run, awaiting input matching the schema.
    Suspended {
        /// The recorded suspension reason.
        reason: String,
        /// The JSON Schema the resume input must satisfy.
        input_schema: Value,
        /// What the run is waiting on, carried from the recorded event.
        ///
        /// `None` is a person: someone reads the reason, decides, and supplies
        /// the input. A [`SuspensionKind`] names a wait an external system
        /// answers instead, which a report about the park has to say out loud,
        /// because telling an operator to go and approve something no operator
        /// can approve is worse than saying nothing.
        kind: Option<SuspensionKind>,
    },
    /// A declared budget was crossed. Resume may carry an extension.
    BudgetExceeded {
        /// The crossed budget, with its effective limit.
        budget: Budget,
        /// The observed value that crossed it.
        observed: f64,
    },
    /// A durable timer is running and its instant has not arrived. Nothing is
    /// awaited from anyone: the run continues when something re-drives it at
    /// or after `wake_at`, which is what `salvor wake` and the server's wake
    /// sweeper do. A resume carrying input is not what this park wants.
    Sleeping {
        /// The recorded instant the run may continue at.
        wake_at: OffsetDateTime,
    },
}

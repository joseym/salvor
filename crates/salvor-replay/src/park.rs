//! [`ParkReason`]: why a run stopped short of completing.
//!
//! A run parks for exactly two reasons, and both are recorded in the log
//! before anything stops: a tool suspended the run
//! ([`Event::Suspended`](crate::Event::Suspended)), or a declared budget was
//! crossed ([`Event::BudgetExceeded`](crate::Event::BudgetExceeded)). This
//! type names that pair and carries the fields a caller needs in order to act
//! on it: the schema a resume input must satisfy, or the budget and the
//! observed value that crossed it.
//!
//! # Purity
//!
//! Every field here is read back out of recorded events, so the vocabulary
//! belongs with the event model rather than at the IO edge that produced it.
//! A renderer that turns a parked run into text needs these names without
//! needing the runtime, the store, or an executor behind them.

use serde_json::Value;

use crate::event::Budget;

/// Why a run parked instead of completing.
#[derive(Debug, Clone)]
pub enum ParkReason {
    /// A tool suspended the run, awaiting input matching the schema.
    Suspended {
        /// The recorded suspension reason.
        reason: String,
        /// The JSON Schema the resume input must satisfy.
        input_schema: Value,
    },
    /// A declared budget was crossed. Resume may carry an extension.
    BudgetExceeded {
        /// The crossed budget, with its effective limit.
        budget: Budget,
        /// The observed value that crossed it.
        observed: f64,
    },
}

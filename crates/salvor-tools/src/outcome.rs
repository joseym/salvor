//! The success side of a tool call: a normal output, or a request to park the
//! run, on a human gate or on a durable timer.

use salvor_core::SuspensionKind;
use serde_json::Value;
use time::OffsetDateTime;

/// A tool's request to park the run and wait for a human (or any out-of-band)
/// input before continuing.
///
/// Suspension is a value a tool
/// *returns*, not a runtime call available to orchestration. A tool that needs
/// approval returns [`ToolOutcome::Suspend`] carrying one of these. The run
/// parks durably; `salvor resume` later supplies an input that is validated
/// against [`input_schema`](Self::input_schema) before the run continues.
///
/// The schema is a raw JSON Schema [`Value`] rather than a typed handle,
/// because the tool decides at runtime what shape the resume input must take,
/// and that shape can differ from one suspension to the next. A tool that
/// wants a typed resume input can build the schema with
/// `serde_json::to_value(schemars::schema_for!(T))`; the layer stores whatever
/// `Value` it is given and does not interpret it.
///
/// # Waiting on a person, or on a system
///
/// A tool that parks the run on a webhook, a callback, or another service
/// reporting back builds the suspension with [`on_signal`](Self::on_signal).
/// The park is mechanically identical either way, down to the recorded
/// events; what changes is the [`SuspensionKind`] the runtime records, which
/// is how a listing keeps a wait nobody can answer out of an approval inbox.
/// Build it with [`new`](Self::new) (or leave `kind` at `None`) for the
/// ordinary case, a person deciding.
#[derive(Clone, Debug, PartialEq)]
pub struct Suspension {
    /// Why the run is parking, in human-readable form. This is what the
    /// approval inbox shows the person who has to act.
    pub reason: String,
    /// The JSON Schema the resume input must satisfy. `salvor resume` validates
    /// the supplied input against this before recording it and continuing.
    pub input_schema: Value,
    /// What the run is waiting on, when it is not a person.
    ///
    /// `None` is the human gate and is what a suspension built before this
    /// field existed meant, so it stays the default and is omitted from the
    /// recorded event entirely. See [`SuspensionKind`].
    pub kind: Option<SuspensionKind>,
}

impl Suspension {
    /// A suspension awaiting a person: the run parks, someone reads `reason`,
    /// and the input they supply is validated against `input_schema`.
    #[must_use]
    pub fn new(reason: impl Into<String>, input_schema: Value) -> Self {
        Self {
            reason: reason.into(),
            input_schema,
            kind: None,
        }
    }

    /// The same suspension, awaiting an external system rather than a person.
    ///
    /// Chained onto [`new`](Self::new) rather than taking a kind argument,
    /// so the ordinary case names nothing and the exception names itself:
    /// `Suspension::new(reason, schema).on_signal()`.
    #[must_use]
    pub fn on_signal(mut self) -> Self {
        self.kind = Some(SuspensionKind::Signal);
        self
    }
}

/// A tool's request to park the run on a durable timer until an instant
/// arrives.
///
/// The timer counterpart of [`Suspension`], and a value a tool *returns* for
/// the same reason: a tool that has started work it cannot finish yet (a
/// rate-limited backend, a settlement window, a retry-after header) says when
/// to come back and returns [`ToolOutcome::Sleep`]. The run parks durably and
/// continues when something re-drives it at or after `wake_at`, with no input
/// and nothing for a human to supply.
///
/// # An instant, never a duration
///
/// The runtime records an instant (`SleepStarted { wake_at }`) and replay
/// matches it exactly, so the deadline has to be a value that reproduces. A
/// duration is not one: resolving it needs a clock, and a clock read on the
/// second drive gives a second, later deadline. So this carries the instant
/// the tool decided on.
///
/// That decision is a *live* read and is allowed to be one. A tool executes
/// only live; on every later drive its completion is replayed, never
/// re-executed. So a tool computing `now + 30 minutes` from the ambient clock
/// reads that clock exactly once in the run's life, and the instant it
/// produced is recorded in the completion (see the runtime's sleep sentinel)
/// and read back from the log forever after. That is the same property the
/// suspension sentinel gives a reason and a schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sleep {
    /// The instant the run may continue at.
    pub wake_at: OffsetDateTime,
}

impl Sleep {
    /// A request to park until `wake_at`.
    #[must_use]
    pub fn until(wake_at: OffsetDateTime) -> Self {
        Self { wake_at }
    }
}

/// The `Ok` side of a tool call: the tool's normal output, or one of the two
/// ways it can ask to park the run ([`Suspension`], [`Sleep`]).
///
/// Both parks are modeled here, on the success side, and not as a
/// [`ToolError`](crate::ToolError). A parked run is a normal, expected outcome
/// of a human-in-the-loop or a wait-and-retry tool, not a failure, and the
/// runtime loop treats the branches differently: an [`Output`](Self::Output)
/// feeds the next model turn, a [`Suspend`](Self::Suspend) records a
/// `Suspended` event and waits for an input, a [`Sleep`](Self::Sleep) records
/// a `SleepStarted` and waits for an instant. Encoding that split in the type
/// keeps the loop from having to guess.
///
/// The typed layer produces `ToolOutcome<Self::Output>`; the type-erased layer
/// produces `ToolOutcome<serde_json::Value>`. The two park branches are
/// identical across both, so either crosses the erasure boundary unchanged.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcome<T> {
    /// The tool finished and produced this output.
    Output(T),
    /// The tool asks to park the run and wait for the described input.
    Suspend(Suspension),
    /// The tool asks to park the run until an instant.
    Sleep(Sleep),
}

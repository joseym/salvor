//! The success side of a tool call: a normal output, or a request to suspend
//! the run.

use serde_json::Value;

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
#[derive(Clone, Debug, PartialEq)]
pub struct Suspension {
    /// Why the run is parking, in human-readable form. This is what the
    /// approval inbox shows the person who has to act.
    pub reason: String,
    /// The JSON Schema the resume input must satisfy. `salvor resume` validates
    /// the supplied input against this before recording it and continuing.
    pub input_schema: Value,
}

/// The `Ok` side of a tool call: either the tool's normal output, or a
/// [`Suspension`].
///
/// Suspension is modeled here, on the success side, and not as a
/// [`ToolError`](crate::ToolError). A parked run is a normal, expected outcome
/// of a human-in-the-loop tool, not a failure, and the runtime loop treats the two
/// branches differently: an [`Output`](Self::Output) feeds the next model
/// turn, a [`Suspend`](Self::Suspend) records a `Suspended` event and parks the
/// run. Encoding that split in the type keeps the loop from having to guess.
///
/// The typed layer produces `ToolOutcome<Self::Output>`; the type-erased layer
/// produces `ToolOutcome<serde_json::Value>`. The [`Suspend`](Self::Suspend)
/// branch is identical across both, so a suspension crosses the erasure
/// boundary unchanged.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcome<T> {
    /// The tool finished and produced this output.
    Output(T),
    /// The tool asks to park the run and wait for the described input.
    Suspend(Suspension),
}

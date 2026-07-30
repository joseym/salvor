//! The event vocabulary: the envelope every event is wrapped in, the payload
//! enum that names what happened, and the small supporting types a few
//! payloads carry.
//!
//! Everything here is pure data. No constructor reads the clock, draws
//! randomness, or performs IO: the recorded timestamp and any identity are
//! passed in by the caller. That purity is load-bearing, because these types
//! live in the IO-free `salvor-replay` crate that the runtime and the v0.3
//! browser inspector both fold events with.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::effect::Effect;
use crate::id::{RunId, SequenceNumber};

/// The schema version stamped onto every serialized event.
///
/// Present from the first event ever written, so an old log is always
/// self-describing and a future reader can branch on it. Start at 1.
///
/// # Why adding event variants does not bump this
///
/// `schema_version` exists so a reader knows how to interpret events that are
/// already on disk. Adding a variant to [`Event`] changes nothing about how
/// any previously written event is encoded or understood: a log written
/// before the addition contains none of the new kinds, and every event in it
/// parses to the identical value under the new build. The
/// deterministic-context events ([`Event::NowObserved`] and
/// [`Event::RandomObserved`]) were added this way and the version stayed 1.
///
/// A bump is reserved for changes that alter the meaning or shape of events a
/// version-1 writer may have already produced: renaming a field, changing the
/// envelope, or re-encoding a payload. An older binary cannot read a log that
/// contains the newer variants, but that direction is not part of the
/// contract: the store is embedded, so the reader always upgrades together
/// with the binary that owns the log.
///
/// # Why an additive optional field does not bump this either
///
/// The optional `request_body` on [`Event::ModelCallRequested`] follows the
/// same rule. It carries `#[serde(default, skip_serializing_if =
/// "Option::is_none")]`, so with recording off (the default) the field is
/// omitted from the wire form entirely and the event serializes byte for byte
/// as it did before the field existed. An old log, written before the field,
/// deserializes with the field defaulted to `None`. An older reader that meets
/// a log where the field *is* present ignores the unknown `request_body` key.
/// So no version-1 event changes shape or meaning, and the version stays 1.
///
/// The optional `labels` on [`Event::RunStarted`] is the identical contract:
/// `#[serde(default, skip_serializing_if = "Option::is_none")]`, absent by
/// default, so an unlabeled run's `RunStarted` serializes byte for byte as it
/// did before the field existed, and the version stays 1 for the same reason.
///
/// # Why the graph events do not bump this
///
/// The graph-run events ([`Event::GraphRunStarted`] and the node/branch/map/fold
/// markers) are new variants, added the same read-compatible way the
/// deterministic-context events were: a log written before them contains none
/// of the new kinds, so every event in it parses to the identical value under
/// the new build. The fold markers ([`Event::FoldIterationStarted`],
/// [`Event::FoldIterationJoined`], and [`Event::FoldConverged`]) were added
/// last, the same way and for the same reason, and the version stayed 1. [`Event::GraphRunStarted`]'s own `labels` and `forked_from`
/// are additive-optional under the same
/// `#[serde(default, skip_serializing_if = "Option::is_none")]` contract, so a
/// graph run that is neither labeled nor forked omits both keys entirely.
/// [`Event::GraphRunStarted`] is a separate variant rather than a field on
/// [`Event::RunStarted`] because `RunStarted`'s `agent_def_hash` is required and
/// names one agent, while a graph run has many agent hashes and none at its
/// head; folding the two into one variant would have forced `agent_def_hash`
/// optional and changed the existing `RunStarted` bytes, which this discipline
/// forbids.
pub const SCHEMA_VERSION: u32 = 1;

/// Who performed a tool call: the trust distinction a reader of the log needs
/// to tell "salvor witnessed this" apart from "the client says this happened".
///
/// A [`ToolCallRequested`](Event::ToolCallRequested) with no [`Performer`]
/// recorded (the field is `None`) is the default and, until this variant's
/// client side is wired to any endpoint, the only case that exists: salvor
/// made the call itself, in its own process, so the log entry is direct
/// evidence. A recorded [`Performer::Client`] is different in kind, not just
/// in origin: it is the client's own claim that it ran the call in its
/// process and is now telling salvor it happened. Salvor did not witness that
/// execution; it is trusting the report. The log keeps that distinction
/// explicit rather than flattening both into "a tool call happened", so a
/// later reader (a human auditing the log, or code deciding how much to trust
/// an entry) can tell a witnessed fact from an asserted one.
///
/// Serializes lowercase (`"server"`, `"client"`), matching the wire style
/// [`Effect`] uses.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Performer {
    /// Salvor performed the call itself, in its own process. The log entry is
    /// direct evidence: salvor witnessed the call because it made it.
    Server,
    /// The client performed the call in its own process and reported back
    /// that it happened. The log entry is the client's claim, not something
    /// salvor witnessed directly.
    Client,
}

/// One record in a run's append-only log.
///
/// The envelope carries run identity, the event's position in the log, the
/// schema version, when it was recorded, and the payload that says what
/// happened. Serializing an envelope always includes `schema_version`, so the
/// wire form is self-describing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// The run this event belongs to.
    pub run_id: RunId,
    /// This event's monotonic position in the run's log.
    pub seq: SequenceNumber,
    /// The event schema version. Always [`SCHEMA_VERSION`] for events this
    /// build writes; older values may appear when reading an old log.
    pub schema_version: u32,
    /// When the event was recorded, as an RFC 3339 timestamp on the wire.
    ///
    /// Passed in by the caller. Nothing in this crate reads the clock.
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
    /// What happened.
    pub event: Event,
}

impl EventEnvelope {
    /// Wraps a payload with its run identity, log position, and recorded
    /// timestamp, stamping the current [`SCHEMA_VERSION`].
    ///
    /// The timestamp is a parameter on purpose: this constructor never reads
    /// the clock, so it stays usable from the pure replay path.
    #[must_use]
    pub const fn new(
        run_id: RunId,
        seq: SequenceNumber,
        recorded_at: OffsetDateTime,
        event: Event,
    ) -> Self {
        Self {
            run_id,
            seq,
            schema_version: SCHEMA_VERSION,
            recorded_at,
            event,
        }
    }
}

/// Everything that can happen in a run.
///
/// Adjacently tagged: each event serializes as `{"kind": "...", "payload":
/// {...}}`. The tag (`kind`) and the content (`payload`) live in separate
/// keys, which is the deliberate choice for a durable format. It never
/// collides with a payload field (a payload could legitimately contain a
/// field named `kind`), and it does not constrain payloads to be JSON objects
/// the way internal tagging would. The wire shape is a durability contract, so
/// it is spelled out rather than left to a default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum Event {
    /// A run began. Records the hash of the agent definition it ran under and
    /// the input it started with.
    RunStarted {
        /// Content hash of the agent definition (model, prompt, tools,
        /// budget) this run executed.
        agent_def_hash: String,
        /// The input the run started with.
        input: serde_json::Value,
        /// Optional operator-supplied correlation tags for the run (for
        /// example a build id or environment), set once at creation and
        /// never rewritten. `BTreeMap` so the wire form serializes with keys
        /// in sorted order regardless of insertion order, matching the
        /// deterministic-serialization discipline the rest of this crate
        /// holds to (see `salvor_runtime::hash::canonical_json`, which this
        /// field is deliberately never fed into: labels are a tag, not part
        /// of the run's identity, and never enter `agent_def_hash` or
        /// `request_hash`).
        ///
        /// Absent by default. `#[serde(default, skip_serializing_if =
        /// "Option::is_none")]` follows the identical additive contract
        /// `request_body` on [`Event::ModelCallRequested`] set: with no
        /// labels supplied at creation, this field is omitted from the wire
        /// form entirely, so an unlabeled run's `RunStarted` serializes byte
        /// for byte as it did before this field existed. Sanity bounds (at
        /// most 16 labels, keys under 64 bytes, values under 256 bytes) are
        /// enforced where a run is created, never here: a log already on
        /// disk is trusted and replayed as recorded, whatever it holds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        labels: Option<BTreeMap<String, String>>,
    },
    /// A model call was requested. Records the correlating sequence number and
    /// the hash of the request, so a later completion can be matched to it.
    ModelCallRequested {
        /// Correlates this request with its [`Event::ModelCallCompleted`].
        seq: SequenceNumber,
        /// Content hash of the request sent to the model. This is the sole
        /// replay-correlation key for the call; it is computed the same way
        /// whether or not `request_body` is recorded, and the body never feeds
        /// into it.
        request_hash: String,
        /// The full model request body, verbatim, recorded only when prompt
        /// recording is opted into (per-agent `record_prompts` or the
        /// `SALVOR_RECORD_PROMPTS` default). It exists so the inspector can
        /// show the exact prompt sent.
        ///
        /// Off by default, and for a reason: the body can hold user data and
        /// secrets. When recording is off the field is `None` and, thanks to
        /// `skip_serializing_if`, is omitted from the wire form, so the event
        /// serializes byte for byte as it did before this field existed. The
        /// body is purely informational: replay correlates on `request_hash`
        /// alone and ignores whatever is (or is not) recorded here, so a log
        /// captured with bodies replays identically to one captured without.
        // A future redaction pass, if one is ever built, would belong here at
        // the recording edge, transforming the value before it is stored. No
        // such transform exists today; recording is all-or-nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_body: Option<serde_json::Value>,
    },
    /// A model call completed. This is the captured nondeterministic boundary:
    /// once recorded, replay reads the response from here and never calls the
    /// model again.
    ModelCallCompleted {
        /// Correlates this completion with its [`Event::ModelCallRequested`].
        seq: SequenceNumber,
        /// The model response, stored inline.
        ///
        /// Inline is the deliberate choice: the full response
        /// lives in the log. Large multimodal outputs may later force a
        /// content-addressed blob store, at which point this field becomes the
        /// seam, holding a blob reference instead of the response itself.
        response: serde_json::Value,
        /// Token usage reported for the call.
        usage: TokenUsage,
    },
    /// A tool call was requested. Records intent before execution, which is
    /// what lets an unrecorded-but-attempted [`Effect::Write`] be detected on
    /// resume.
    ToolCallRequested {
        /// Correlates this request with its [`Event::ToolCallCompleted`].
        seq: SequenceNumber,
        /// The tool's name.
        tool: String,
        /// The typed input passed to the tool.
        input: serde_json::Value,
        /// The tool's declared side-effect class, which governs retry and
        /// resume behavior for a call that did not complete.
        effect: Effect,
        /// The idempotency key for this attempt, when the tool has one.
        ///
        /// A [`Effect::Read`] call needs none; an [`Effect::Idempotent`] retry
        /// reuses this exact key so the provider collapses duplicates.
        idempotency_key: Option<String>,
        /// Who performed this call: see [`Performer`] for what the
        /// distinction means. Absent means salvor performed the call itself,
        /// which is every [`Event::ToolCallRequested`] ever recorded before
        /// this field existed.
        ///
        /// `#[serde(default, skip_serializing_if = "Option::is_none")]` under
        /// the identical additive-optional contract `request_body` on
        /// [`Event::ModelCallRequested`] set (see that field's doc and the
        /// [`SCHEMA_VERSION`] docs for the full argument): with no performer
        /// recorded, the field is omitted from the wire form entirely, so a
        /// server-performed call serializes byte for byte as it did before
        /// this field existed, and a log written before this field
        /// deserializes with it defaulted to `None`. The pinned-JSON tests in
        /// this module's test suite (`tool_call_requested_without_performer_
        /// serializes_to_pinned_json` and its sibling with a performer
        /// present) check this directly, the same way
        /// `model_call_requested_without_body_omits_the_key` checks
        /// `request_body`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        performed_by: Option<Performer>,
    },
    /// A tool call completed. Once recorded, replay reads the output from here
    /// and never calls the tool again, whatever its effect class.
    ToolCallCompleted {
        /// Correlates this completion with its [`Event::ToolCallRequested`].
        seq: SequenceNumber,
        /// The tool's output.
        output: serde_json::Value,
    },
    /// The value `ctx.now()` returned, captured once while the run executed
    /// live. On replay the recorded value is returned again, bit for bit; the
    /// clock is never consulted a second time. This is how orchestration code
    /// gets to observe time without breaking the determinism constraint.
    ///
    /// # Wire compatibility
    ///
    /// This variant (together with [`Event::RandomObserved`]) was added after
    /// the original ten. Adding variants is a read-compatible change, so
    /// [`SCHEMA_VERSION`] stayed at 1; the constant's docs carry the full
    /// argument.
    NowObserved {
        /// The observed time. RFC 3339 on the wire with nanosecond
        /// precision, so the value replays exactly as recorded.
        #[serde(with = "time::serde::rfc3339")]
        now: OffsetDateTime,
    },
    /// The value `ctx.random()` returned, captured once while the run
    /// executed live. On replay the recorded value is returned again, bit for
    /// bit; the random source is never consulted a second time.
    RandomObserved {
        /// Sixty-four raw bits from the runtime's random source.
        ///
        /// A `u64` is the deliberate representation: JSON integers carry the
        /// full 64-bit range exactly, so replay returns the identical bits.
        /// Richer values (a float in a range, a choice from a list) must be
        /// derived from these bits deterministically by the caller, never
        /// drawn fresh.
        value: u64,
    },
    /// The run parked durably, awaiting input (for example, human approval).
    /// Records why and the schema the resume input must satisfy.
    Suspended {
        /// Why the run suspended.
        reason: String,
        /// JSON Schema the [`Event::Resumed`] input is validated against.
        input_schema: serde_json::Value,
    },
    /// A suspended run resumed with the given input.
    Resumed {
        /// The input supplied on resume.
        input: serde_json::Value,
    },
    /// A declared budget was exceeded. The run suspends rather than dies, so a
    /// human can raise the limit and resume.
    BudgetExceeded {
        /// The budget dimension and the limit that was crossed.
        budget: Budget,
        /// The observed value, interpreted in the units of `budget.kind`. An
        /// `f64` for the same reason [`Budget::limit`] is: exact for integral
        /// token and step counts up to 2^53, and inherently fractional for
        /// cost and wall time.
        observed: f64,
    },
    /// The run finished successfully with this output.
    RunCompleted {
        /// The run's final output.
        output: serde_json::Value,
    },
    /// The run terminated with an error.
    RunFailed {
        /// A description of the failure.
        error: String,
    },
    /// The run was abandoned by an operator: deliberately retired without ever
    /// finishing or failing. A terminal event, appended by hand through the
    /// server's abandon endpoint, never emitted by orchestration.
    ///
    /// # Abandonment is not failure
    ///
    /// This is a separate terminal from [`Event::RunFailed`] on purpose. A
    /// failure says the run tried to continue and could not; an abandonment
    /// says a human decided it should stop mattering (a husk that is dead
    /// forever, or a run whose noise is no longer worth carrying in the
    /// inbox). The two read differently everywhere downstream: the fold gives
    /// abandonment its own status, and the surfaces treat it as a muted
    /// resting state, never the failure ink. Keeping [`Event::RunFailed`]
    /// untouched is the point: its recorded meaning must not shift.
    ///
    /// # Wire compatibility
    ///
    /// Added the same read-compatible way the deterministic-context and graph
    /// events were: a new variant, so a log written before it contains none of
    /// the kind and every event in it parses to the identical value under the
    /// new build. [`SCHEMA_VERSION`] stays 1; the constant's docs carry the
    /// full argument. Both fields are additive-optional under the identical
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` contract
    /// the other optional payloads hold to, so a bare abandonment (no reason,
    /// no dangling write) serializes with an empty payload object:
    /// `{"kind":"RunAbandoned","payload":{}}`.
    RunAbandoned {
        /// The operator's optional note for why the run was abandoned.
        /// Absent by default: an abandonment with no reason omits the key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Set only when the abandoned run was parked at a dangling write
        /// (status `NeedsReconciliation`): the outstanding write intent's
        /// position and tool, recorded as evidence.
        ///
        /// The abandonment never claims the write question was answered.
        /// Abandoning a needs-reconciliation run is allowed precisely because
        /// this field carries the honesty forward: the write may or may not
        /// have taken effect, and the record says so by naming the intent that
        /// was left unsettled rather than pretending a completion. Absent for
        /// any run abandoned from a state with no dangling write, under the
        /// same additive-optional contract as `reason`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unresolved_write: Option<UnresolvedWrite>,
    },
    /// A graph run began. The head of a run that executes a graph document
    /// rather than a single agent loop.
    ///
    /// A separate variant from [`Event::RunStarted`], not a field on it:
    /// `RunStarted` requires exactly one `agent_def_hash`, but a graph run has
    /// many agent hashes (one per agent node) and none at its head. It records
    /// the hash of the frozen graph document and the run's input; the node,
    /// branch, and map markers below then narrate the walk. Downstream of this
    /// crate a `graph_hash` is `sha256:<64 lowercase hex>` over the canonical
    /// graph document, but this crate treats it as an opaque string and never
    /// depends on `salvor-graph` to interpret it.
    GraphRunStarted {
        /// Content hash of the frozen graph document this run executes. Opaque
        /// here; the runtime forms and checks it against `salvor-graph`.
        graph_hash: String,
        /// The input the graph run started with.
        input: serde_json::Value,
        /// Optional operator-supplied correlation tags, carried for parity
        /// with [`Event::RunStarted::labels`]: grouping (a build id, an
        /// environment) matters for graph runs exactly as it does for agent
        /// runs. Same additive-optional contract as that field: absent by
        /// default via `#[serde(default, skip_serializing_if =
        /// "Option::is_none")]`, so an unlabeled graph run omits the key
        /// entirely. A tag, never part of identity: never fed into any hash.
        /// Sanity bounds are enforced where a run is created, never here; a log
        /// on disk is replayed as recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        labels: Option<BTreeMap<String, String>>,
        /// Set only when this run is a fork of an earlier run: the recorded
        /// link back to its origin (see [`ForkOrigin`]). A fork is a new run
        /// whose log opens with the origin's prefix rewritten under the new
        /// id, and this field at seq 0 is the durable fact that it descends
        /// from that origin. Absent by default under the same additive-optional
        /// contract, so an ordinary (non-forked) graph run omits the key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forked_from: Option<ForkOrigin>,
    },
    /// Execution entered a graph node. Marks the node as the run's current
    /// position; a later [`Event::NodeExited`] closes it.
    NodeEntered {
        /// The id of the node entered, unique within the graph document.
        node: String,
    },
    /// Execution left a graph node, having produced its output.
    NodeExited {
        /// The id of the node exited.
        node: String,
    },
    /// A graph node was skipped: reached on the walk but deliberately not run
    /// (for example, a branch case that did not fire routes past its node). A
    /// skipped node WAS reached: it is recorded precisely so a projection can
    /// tell "skipped" apart from "never reached", which is the absence of any
    /// event naming the node.
    NodeSkipped {
        /// The id of the node skipped.
        node: String,
        /// Why it was skipped, recorded for the audit trail.
        reason: String,
    },
    /// A branch node routed: the named case fired. This is the sole recorded
    /// authority for which way a branch went. The executed path is read from
    /// these events, never inferred from which sibling nodes were skipped.
    BranchTaken {
        /// The id of the branch node that routed.
        node: String,
        /// The name of the case that fired, matching a branch-case name in the
        /// graph document (realized by the like-named edge). Opaque here.
        case: String,
    },
    /// A map node fanned out: the resolved list of items to map over was
    /// determined and recorded. Recording the items here (rather than
    /// re-resolving on replay) is what makes the fan-out deterministic: the
    /// per-iteration child ids are derived from this recorded data.
    MapFannedOut {
        /// The id of the map node.
        node: String,
        /// The resolved list of items, one sub-run per element. Recorded
        /// verbatim so replay reproduces the same fan-out.
        items: serde_json::Value,
    },
    /// One iteration of a map fan-out started, as a child run. The child's id
    /// is derived deterministically from recorded data (the parent run, the
    /// node, and the index), so replay reconstructs the identical id without
    /// drawing a fresh one.
    MapIterationStarted {
        /// The id of the map node this iteration belongs to.
        node: String,
        /// The zero-based position of this iteration in the fanned-out list.
        index: u64,
        /// The derived id of the child run executing this iteration.
        child_run: String,
    },
    /// One iteration of a map fan-out joined back: its child run's result was
    /// folded into the map node's output. Joins are recorded in index order,
    /// never completion order, so the concurrency of the fan-out never
    /// influences the parent log's byte sequence.
    MapIterationJoined {
        /// The id of the map node this iteration belongs to.
        node: String,
        /// The zero-based position of the iteration that joined.
        index: u64,
    },
    /// A fold node began one bounded iteration: a single revision pass of the
    /// accumulate-and-refine loop the node models. A fold's passes run
    /// sequentially in the one log (never as child runs, unlike a map's
    /// iterations), so `index` is both the pass position and its recorded
    /// order.
    FoldIterationStarted {
        /// The id of the fold node this iteration belongs to.
        node: String,
        /// The zero-based position of this pass in the fold loop.
        index: u64,
    },
    /// A fold iteration joined back: its pass result was folded into the fold
    /// node's accumulated value. Recorded in index order, exactly like
    /// [`Event::MapIterationJoined`]; because a fold's passes are sequential,
    /// index order already is completion order.
    FoldIterationJoined {
        /// The id of the fold node this iteration belongs to.
        node: String,
        /// The zero-based position of the iteration that joined.
        index: u64,
    },
    /// A fold node settled: its loop stopped and its `join` rule selected the
    /// winning iteration. This is the sole recorded authority for WHICH pass
    /// the fold's output came from: the argmax of a `best_by` join is read
    /// from `winner_index`, never inferred from the iteration order, exactly
    /// as [`Event::BranchTaken`] is the sole authority for a branch's route.
    /// `reason` records WHY the loop ended (its stop predicate fired, the
    /// iteration bound was reached, or a pass failed to improve), an opaque
    /// audit string like [`Event::NodeSkipped`]'s.
    FoldConverged {
        /// The id of the fold node that settled.
        node: String,
        /// The zero-based index of the iteration whose value the `join` rule
        /// selected as the fold's output.
        winner_index: u64,
        /// Why the loop ended, recorded for the audit trail.
        reason: String,
    },
}

/// The recorded link from a forked run back to the run it forked from.
///
/// A fork is a new run, never a mutation of the origin: its log opens with the
/// origin's prefix (every event with `seq` below the fork boundary) rewritten
/// under the fork's own run id, and [`ForkOrigin`] rides on the fork's
/// [`Event::GraphRunStarted`] at seq 0 as the durable fact of that descent. The
/// origin is immutable and never points forward at its forks; "forks of this
/// run" is a derived server-side index over this field, not something the
/// origin records.
///
/// Carries no floats, so it derives [`Eq`] (unlike [`Budget`]); every field is
/// plain recorded data.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ForkOrigin {
    /// The run this fork descends from.
    pub run_id: RunId,
    /// The boundary the prefix was taken through: the fork's log carries the
    /// origin's events with `seq` below the [`Event::NodeEntered`] that the
    /// fork restarts from. A genuine log position, so it rides as a
    /// [`SequenceNumber`].
    pub through_seq: SequenceNumber,
    /// The id of the node the fork restarts execution from.
    pub from_node: String,
    /// The origin's graph hash, which the fork must reuse unchanged. A fork may
    /// not edit the graph: a different document could route the shared prefix
    /// differently, turning a clean refusal into an arbitrary mid-replay
    /// divergence. Opaque here, exactly like [`Event::GraphRunStarted::graph_hash`].
    pub graph_hash: String,
    /// The origin log positions of the [`Effect::Write`] intents the operator
    /// acknowledged when forking past them. Raw `u64` positions, not
    /// [`SequenceNumber`]s: this is an operator acknowledgement list recorded
    /// as evidence, not a set of correlation keys this crate dereferences. An
    /// empty vector means the fork boundary sat before any write intent, so
    /// nothing needed acknowledging.
    pub acknowledged_writes: Vec<u64>,
}

/// The outstanding write an abandonment left unsettled.
///
/// Rides on [`Event::RunAbandoned::unresolved_write`] when a run parked at a
/// dangling [`Effect::Write`] intent is abandoned. It names the intent's log
/// position and the tool it called, mirroring the evidence the reconciliation
/// refusal (`409 needs_reconciliation`) surfaces, so the abandonment record
/// points at exactly the write whose effect stays unknown.
///
/// Deliberately minimal: `seq` and `tool` are the evidence, not the whole
/// recorded intent. The full intent (input, effect, idempotency key) is still
/// in the log at `seq` for anyone who wants it; this struct is the pointer to
/// it, not a copy. Carries no floats, so it derives [`Eq`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct UnresolvedWrite {
    /// The log position of the write intent that was left unresolved. A genuine
    /// log position, so it rides as a [`SequenceNumber`], exactly as the
    /// correlation seqs on the call events do.
    pub seq: SequenceNumber,
    /// The name of the tool the unresolved write called.
    pub tool: String,
}

/// Token counts reported for a model call.
///
/// Minimal on purpose (input and output counts). Budget enforcement
/// owns anything richer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens in the request (prompt).
    pub input_tokens: u32,
    /// Tokens in the response (completion).
    pub output_tokens: u32,
}

/// Which declared budget dimension a run can bump against.
///
/// Serializes in snake_case (`"tokens"`, `"cost_usd"`, `"wall_time"`,
/// `"steps"`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    /// Total tokens across the run.
    Tokens,
    /// Total cost in US dollars.
    CostUsd,
    /// Wall-clock time.
    WallTime,
    /// Number of steps taken.
    Steps,
}

/// A budget limit: which dimension, and the ceiling set for it.
///
/// Minimal on purpose. Budget enforcement owns the richer
/// model; this type exists only so [`Event::BudgetExceeded`] can name what was
/// crossed. `limit` is interpreted in the units implied by `kind`.
///
/// # Why every dimension is an `f64`
///
/// [`BudgetKind::CostUsd`] and [`BudgetKind::WallTime`] are inherently
/// fractional, so they need a float. [`BudgetKind::Tokens`] and
/// [`BudgetKind::Steps`] are integral in nature, yet they ride the wire as
/// `f64` too, and that is a deliberate, kept decision rather than an
/// oversight.
///
/// The concern with a float for a counter is silent rounding. It does not
/// arise here: an IEEE 754 double represents every integer exactly up to
/// 2^53, which is 9_007_199_254_740_992 (about 9.0e15). A run's token and step
/// counts do not approach that bound. A billion-token run is 1e9, six orders
/// of magnitude below the point where consecutive integers stop being exactly
/// representable, and a step is one model turn. So a limit or an observed
/// value that is integral in these dimensions round-trips through the wire and
/// through every comparison exactly. The one caller-facing rule is that a
/// declared token or step limit stay under 2^53, which no realistic budget
/// violates.
///
/// The alternative, splitting the type so integral dimensions carried a `u64`
/// and fractional ones an `f64`, was weighed and declined: it would fracture
/// one uniform budget value into two, complicate the single crossing-check
/// comparison that treats every dimension the same way, and change the wire
/// format (with the snapshot and stored-log churn that implies) for no
/// practical gain, since f64 already holds these integers exactly. The uniform
/// `f64` is the honest representation given the bound above.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Budget {
    /// The dimension this limit applies to.
    pub kind: BudgetKind,
    /// The ceiling, in the units implied by `kind`. An `f64` for every
    /// dimension; integral for `Tokens`/`Steps` and exact there up to 2^53
    /// (see the type docs).
    pub limit: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;
    use uuid::Uuid;

    fn run_id() -> RunId {
        RunId::from_uuid(Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap())
    }

    fn envelope(event: Event) -> EventEnvelope {
        EventEnvelope::new(
            run_id(),
            SequenceNumber::new(3),
            datetime!(2026-07-09 12:00:00 UTC),
            event,
        )
    }

    /// Serializing then deserializing an envelope yields an equal value.
    fn assert_round_trips(event: Event) {
        let original = envelope(event);
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored, "round trip changed the value: {json}");
    }

    #[test]
    fn every_variant_round_trips_through_the_envelope() {
        assert_round_trips(Event::RunStarted {
            agent_def_hash: "sha256:abc".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
        });
        assert_round_trips(Event::RunStarted {
            agent_def_hash: "sha256:abc".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: Some(BTreeMap::from([
                ("build".to_owned(), "42".to_owned()),
                ("env".to_owned(), "prod".to_owned()),
            ])),
        });
        assert_round_trips(Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: "sha256:req".into(),
            request_body: None,
        });
        assert_round_trips(Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: "sha256:req".into(),
            request_body: Some(serde_json::json!({"model": "test", "messages": []})),
        });
        assert_round_trips(Event::ModelCallCompleted {
            seq: SequenceNumber::new(1),
            response: serde_json::json!({"text": "hello"}),
            usage: TokenUsage {
                input_tokens: 12,
                output_tokens: 7,
            },
        });
        assert_round_trips(Event::ToolCallRequested {
            seq: SequenceNumber::new(2),
            tool: "create_ticket".into(),
            input: serde_json::json!({"title": "bug"}),
            effect: Effect::Write,
            idempotency_key: Some("key-123".into()),
            performed_by: None,
        });
        assert_round_trips(Event::ToolCallRequested {
            seq: SequenceNumber::new(2),
            tool: "create_ticket".into(),
            input: serde_json::json!({"title": "bug"}),
            effect: Effect::Write,
            idempotency_key: Some("key-123".into()),
            performed_by: Some(Performer::Client),
        });
        assert_round_trips(Event::ToolCallCompleted {
            seq: SequenceNumber::new(2),
            output: serde_json::json!({"id": "TICKET-1"}),
        });
        assert_round_trips(Event::NowObserved {
            now: datetime!(2026-07-09 12:00:00.123456789 UTC),
        });
        assert_round_trips(Event::RandomObserved { value: u64::MAX });
        assert_round_trips(Event::Suspended {
            reason: "awaiting approval".into(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        assert_round_trips(Event::Resumed {
            input: serde_json::json!({"approved": true}),
        });
        assert_round_trips(Event::BudgetExceeded {
            budget: Budget {
                kind: BudgetKind::CostUsd,
                limit: 2.0,
            },
            observed: 2.5,
        });
        assert_round_trips(Event::RunCompleted {
            output: serde_json::json!({"summary": "done"}),
        });
        assert_round_trips(Event::RunFailed {
            error: "provider timeout".into(),
        });
        assert_round_trips(Event::RunAbandoned {
            reason: None,
            unresolved_write: None,
        });
        assert_round_trips(Event::RunAbandoned {
            reason: Some("husk is dead forever".into()),
            unresolved_write: Some(UnresolvedWrite {
                seq: SequenceNumber::new(5),
                tool: "create_ticket".into(),
            }),
        });
        assert_round_trips(Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
            forked_from: None,
        });
        assert_round_trips(Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: Some(BTreeMap::from([("env".to_owned(), "prod".to_owned())])),
            forked_from: Some(ForkOrigin {
                run_id: run_id(),
                through_seq: SequenceNumber::new(7),
                from_node: "review".into(),
                graph_hash: "sha256:graph".into(),
                acknowledged_writes: vec![3, 5],
            }),
        });
        assert_round_trips(Event::NodeEntered {
            node: "research".into(),
        });
        assert_round_trips(Event::NodeExited {
            node: "research".into(),
        });
        assert_round_trips(Event::NodeSkipped {
            node: "publish".into(),
            reason: "branch case did not fire".into(),
        });
        assert_round_trips(Event::BranchTaken {
            node: "gate".into(),
            case: "approved".into(),
        });
        assert_round_trips(Event::MapFannedOut {
            node: "fanout".into(),
            items: serde_json::json!([1, 2, 3]),
        });
        assert_round_trips(Event::MapIterationStarted {
            node: "fanout".into(),
            index: 0,
            child_run: "sha256:child".into(),
        });
        assert_round_trips(Event::MapIterationJoined {
            node: "fanout".into(),
            index: 0,
        });
        assert_round_trips(Event::FoldIterationStarted {
            node: "refine".into(),
            index: 0,
        });
        assert_round_trips(Event::FoldIterationJoined {
            node: "refine".into(),
            index: 1,
        });
        assert_round_trips(Event::FoldConverged {
            node: "refine".into(),
            winner_index: 1,
            reason: "score >= threshold".into(),
        });
    }

    /// Pins the exact serialized form of a representative envelope. A change to
    /// field names, tag shape, or the presence of `schema_version` fails here
    /// loudly, because the wire form is a durability contract.
    #[test]
    fn envelope_serializes_to_pinned_json() {
        let envelope = envelope(Event::ModelCallCompleted {
            seq: SequenceNumber::new(2),
            response: serde_json::json!({"text": "hi"}),
            usage: TokenUsage {
                input_tokens: 12,
                output_tokens: 7,
            },
        });
        let json = serde_json::to_string(&envelope).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ModelCallCompleted","payload":{"seq":2,"response":{"text":"hi"},"usage":{"input_tokens":12,"output_tokens":7}}}}"#
        );
    }

    /// With prompt recording off, `ModelCallRequested` serializes with no
    /// `request_body` key at all: byte for byte what it produced before the
    /// field existed. This is the additive-optional contract the
    /// [`SCHEMA_VERSION`] docs promise, checked directly.
    #[test]
    fn model_call_requested_without_body_omits_the_key() {
        let env = envelope(Event::ModelCallRequested {
            seq: SequenceNumber::new(2),
            request_hash: "sha256:req".into(),
            request_body: None,
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ModelCallRequested","payload":{"seq":2,"request_hash":"sha256:req"}}}"#
        );
        assert!(
            !json.contains("request_body"),
            "recording-off must not emit the key: {json}"
        );
    }

    /// With recording on, the body rides alongside the hash under its own key.
    #[test]
    fn model_call_requested_with_body_carries_it() {
        let env = envelope(Event::ModelCallRequested {
            seq: SequenceNumber::new(2),
            request_hash: "sha256:req".into(),
            request_body: Some(serde_json::json!({"model": "m"})),
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(json.contains(r#""request_body":{"model":"m"}"#), "{json}");
    }

    /// With no performer recorded, `ToolCallRequested` serializes with no
    /// `performed_by` key at all: byte for byte what it produced before the
    /// field existed. This is the additive-optional contract the
    /// [`SCHEMA_VERSION`] docs promise, checked directly, the same way
    /// `model_call_requested_without_body_omits_the_key` above checks
    /// `request_body`. It is also the backward-compatibility proof: an old
    /// log with no `performed_by` key deserializes to this exact value, with
    /// the field defaulted to `None`.
    #[test]
    fn tool_call_requested_without_performer_serializes_to_pinned_json() {
        let env = envelope(Event::ToolCallRequested {
            seq: SequenceNumber::new(2),
            tool: "create_ticket".into(),
            input: serde_json::json!({"title": "bug"}),
            effect: Effect::Write,
            idempotency_key: Some("key-123".into()),
            performed_by: None,
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ToolCallRequested","payload":{"seq":2,"tool":"create_ticket","input":{"title":"bug"},"effect":"write","idempotency_key":"key-123"}}}"#
        );
        assert!(
            !json.contains("performed_by"),
            "an unattributed call must not emit the key: {json}"
        );

        let restored: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        let Event::ToolCallRequested { performed_by, .. } = restored.event else {
            panic!("expected ToolCallRequested");
        };
        assert_eq!(
            performed_by, None,
            "a log with no performed_by key must deserialize to None"
        );
    }

    /// With a performer recorded, the key rides after `idempotency_key` with
    /// the lowercase string [`Performer`] serializes to.
    #[test]
    fn tool_call_requested_with_performer_serializes_to_pinned_json() {
        let env = envelope(Event::ToolCallRequested {
            seq: SequenceNumber::new(2),
            tool: "create_ticket".into(),
            input: serde_json::json!({"title": "bug"}),
            effect: Effect::Write,
            idempotency_key: Some("key-123".into()),
            performed_by: Some(Performer::Client),
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ToolCallRequested","payload":{"seq":2,"tool":"create_ticket","input":{"title":"bug"},"effect":"write","idempotency_key":"key-123","performed_by":"client"}}}"#
        );
    }

    /// A `ToolCallRequested` JSON that predates `performed_by` (no such key in
    /// the payload) deserializes with the field defaulted to `None`. This is
    /// the load-bearing backward-compatibility proof for every log recorded
    /// before this field existed.
    #[test]
    fn tool_call_requested_without_performer_key_deserializes_to_none() {
        let json = r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ToolCallRequested","payload":{"seq":2,"tool":"create_ticket","input":{"title":"bug"},"effect":"write","idempotency_key":"key-123"}}}"#;
        let restored: EventEnvelope = serde_json::from_str(json).expect("deserialize");
        let Event::ToolCallRequested { performed_by, .. } = restored.event else {
            panic!("expected ToolCallRequested");
        };
        assert_eq!(performed_by, None);
    }

    /// `Performer` round-trips through the envelope in both variants.
    #[test]
    fn performer_round_trips() {
        assert_round_trips(Event::ToolCallRequested {
            seq: SequenceNumber::new(2),
            tool: "create_ticket".into(),
            input: serde_json::json!({"title": "bug"}),
            effect: Effect::Write,
            idempotency_key: None,
            performed_by: Some(Performer::Server),
        });
        assert_round_trips(Event::ToolCallRequested {
            seq: SequenceNumber::new(2),
            tool: "create_ticket".into(),
            input: serde_json::json!({"title": "bug"}),
            effect: Effect::Write,
            idempotency_key: None,
            performed_by: Some(Performer::Client),
        });
    }

    /// `Performer` serializes lowercase, matching [`Effect`]'s wire style.
    #[test]
    fn performer_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Performer::Server).unwrap(),
            r#""server""#
        );
        assert_eq!(
            serde_json::to_string(&Performer::Client).unwrap(),
            r#""client""#
        );
    }

    /// Pins `RunStarted` with no labels: byte for byte the shape `RunStarted`
    /// had before `labels` existed, no `labels` key at all. This is the
    /// unchanged-wire-shape half of the additive contract [`SCHEMA_VERSION`]
    /// documents, checked directly against a fixed string the way
    /// [`envelope_serializes_to_pinned_json`] pins `ModelCallCompleted`.
    #[test]
    fn run_started_without_labels_serializes_to_pinned_json() {
        let env = envelope(Event::RunStarted {
            agent_def_hash: "sha256:abc".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RunStarted","payload":{"agent_def_hash":"sha256:abc","input":{"topic":"otters"}}}}"#
        );
        assert!(
            !json.contains("labels"),
            "an unlabeled run must not emit the key: {json}"
        );
    }

    /// Pins `RunStarted` with labels present: the `labels` key rides after
    /// `input`, and the map serializes with its keys already in sorted order
    /// (a `BTreeMap`'s own iteration order), independent of insertion order.
    #[test]
    fn run_started_with_labels_serializes_to_pinned_json() {
        let env = envelope(Event::RunStarted {
            agent_def_hash: "sha256:abc".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: Some(BTreeMap::from([
                ("env".to_owned(), "prod".to_owned()),
                ("build".to_owned(), "42".to_owned()),
            ])),
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RunStarted","payload":{"agent_def_hash":"sha256:abc","input":{"topic":"otters"},"labels":{"build":"42","env":"prod"}}}}"#
        );
    }

    /// Pins a bare `RunAbandoned`: both optional fields absent, so the payload
    /// is an empty object and neither key appears. This is the additive-optional
    /// contract the [`SCHEMA_VERSION`] docs promise for the new terminal,
    /// checked directly.
    #[test]
    fn run_abandoned_bare_serializes_to_pinned_json() {
        let env = envelope(Event::RunAbandoned {
            reason: None,
            unresolved_write: None,
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RunAbandoned","payload":{}}}"#
        );
        assert!(
            !json.contains("reason") && !json.contains("unresolved_write"),
            "a bare abandonment must omit both keys: {json}"
        );
    }

    /// Pins a `RunAbandoned` carrying both a reason and an unresolved write:
    /// the reason rides first, then the `unresolved_write` object with its
    /// `seq` and `tool`. This is the honesty record an abandoned
    /// needs-reconciliation run leaves behind.
    #[test]
    fn run_abandoned_with_unresolved_write_serializes_to_pinned_json() {
        let env = envelope(Event::RunAbandoned {
            reason: Some("husk is dead forever".into()),
            unresolved_write: Some(UnresolvedWrite {
                seq: SequenceNumber::new(5),
                tool: "create_ticket".into(),
            }),
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RunAbandoned","payload":{"reason":"husk is dead forever","unresolved_write":{"seq":5,"tool":"create_ticket"}}}}"#
        );
    }

    /// Pins the exact serialized form of the two deterministic-context
    /// events. These variants were added after the original ten, which is a
    /// read-compatible change (see [`SCHEMA_VERSION`]); this test extends the
    /// pinned-snapshot coverage to them deliberately, choosing values that
    /// stress the representation: a timestamp with all nine fractional
    /// digits, and the largest `u64`.
    #[test]
    fn context_events_serialize_to_pinned_json() {
        let now_env = envelope(Event::NowObserved {
            now: datetime!(2026-07-09 12:00:00.123456789 UTC),
        });
        let json = serde_json::to_string(&now_env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"NowObserved","payload":{"now":"2026-07-09T12:00:00.123456789Z"}}}"#
        );

        let random_env = envelope(Event::RandomObserved { value: u64::MAX });
        let json = serde_json::to_string(&random_env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RandomObserved","payload":{"value":18446744073709551615}}}"#
        );
    }

    /// Pins `GraphRunStarted` with neither labels nor a fork origin: both
    /// optional keys are absent, so the payload is just `graph_hash` and
    /// `input`. This is the additive-optional contract the [`SCHEMA_VERSION`]
    /// docs promise for the new head variant, checked directly.
    #[test]
    fn graph_run_started_bare_serializes_to_pinned_json() {
        let env = envelope(Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: None,
            forked_from: None,
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"GraphRunStarted","payload":{"graph_hash":"sha256:graph","input":{"topic":"otters"}}}}"#
        );
        assert!(
            !json.contains("labels") && !json.contains("forked_from"),
            "an unlabeled, unforked graph run must omit both keys: {json}"
        );
    }

    /// Pins `GraphRunStarted` carrying both labels and a fork origin: the
    /// labels ride sorted after `input`, and `forked_from` carries the full
    /// [`ForkOrigin`] shape (run id, seq, node, graph hash, acknowledged
    /// writes in order).
    #[test]
    fn graph_run_started_forked_serializes_to_pinned_json() {
        let env = envelope(Event::GraphRunStarted {
            graph_hash: "sha256:graph".into(),
            input: serde_json::json!({"topic": "otters"}),
            labels: Some(BTreeMap::from([
                ("env".to_owned(), "prod".to_owned()),
                ("build".to_owned(), "42".to_owned()),
            ])),
            forked_from: Some(ForkOrigin {
                run_id: run_id(),
                through_seq: SequenceNumber::new(7),
                from_node: "review".into(),
                graph_hash: "sha256:graph".into(),
                acknowledged_writes: vec![3, 5],
            }),
        });
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(
            json,
            r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"GraphRunStarted","payload":{"graph_hash":"sha256:graph","input":{"topic":"otters"},"labels":{"build":"42","env":"prod"},"forked_from":{"run_id":"00000000-0000-4000-8000-000000000001","through_seq":7,"from_node":"review","graph_hash":"sha256:graph","acknowledged_writes":[3,5]}}}}"#
        );
    }

    /// Pins the exact serialized form of each graph node/branch/map marker.
    /// These variants were added additively (see [`SCHEMA_VERSION`]); the pins
    /// lock their wire shape as the durability contract it is.
    #[test]
    fn graph_markers_serialize_to_pinned_json() {
        let prefix = r#"{"run_id":"00000000-0000-4000-8000-000000000001","seq":3,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":"#;

        let cases: Vec<(Event, &str)> = vec![
            (
                Event::NodeEntered {
                    node: "research".into(),
                },
                r#"{"kind":"NodeEntered","payload":{"node":"research"}}"#,
            ),
            (
                Event::NodeExited {
                    node: "research".into(),
                },
                r#"{"kind":"NodeExited","payload":{"node":"research"}}"#,
            ),
            (
                Event::NodeSkipped {
                    node: "publish".into(),
                    reason: "branch case did not fire".into(),
                },
                r#"{"kind":"NodeSkipped","payload":{"node":"publish","reason":"branch case did not fire"}}"#,
            ),
            (
                Event::BranchTaken {
                    node: "gate".into(),
                    case: "approved".into(),
                },
                r#"{"kind":"BranchTaken","payload":{"node":"gate","case":"approved"}}"#,
            ),
            (
                Event::MapFannedOut {
                    node: "fanout".into(),
                    items: serde_json::json!([1, 2, 3]),
                },
                r#"{"kind":"MapFannedOut","payload":{"node":"fanout","items":[1,2,3]}}"#,
            ),
            (
                Event::MapIterationStarted {
                    node: "fanout".into(),
                    index: 0,
                    child_run: "sha256:child".into(),
                },
                r#"{"kind":"MapIterationStarted","payload":{"node":"fanout","index":0,"child_run":"sha256:child"}}"#,
            ),
            (
                Event::MapIterationJoined {
                    node: "fanout".into(),
                    index: 0,
                },
                r#"{"kind":"MapIterationJoined","payload":{"node":"fanout","index":0}}"#,
            ),
            (
                Event::FoldIterationStarted {
                    node: "refine".into(),
                    index: 0,
                },
                r#"{"kind":"FoldIterationStarted","payload":{"node":"refine","index":0}}"#,
            ),
            (
                Event::FoldIterationJoined {
                    node: "refine".into(),
                    index: 1,
                },
                r#"{"kind":"FoldIterationJoined","payload":{"node":"refine","index":1}}"#,
            ),
            (
                Event::FoldConverged {
                    node: "refine".into(),
                    winner_index: 1,
                    reason: "score >= threshold".into(),
                },
                r#"{"kind":"FoldConverged","payload":{"node":"refine","winner_index":1,"reason":"score >= threshold"}}"#,
            ),
        ];

        for (event, expected_event_json) in cases {
            let json = serde_json::to_string(&envelope(event)).expect("serialize");
            assert_eq!(json, format!("{prefix}{expected_event_json}}}"));
        }
    }

    /// Effect serializes lowercase.
    #[test]
    fn effect_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Effect::Read).unwrap(), r#""read""#);
        assert_eq!(
            serde_json::to_string(&Effect::Idempotent).unwrap(),
            r#""idempotent""#
        );
        assert_eq!(serde_json::to_string(&Effect::Write).unwrap(), r#""write""#);
    }

    /// Sequence numbers order by their underlying position.
    #[test]
    fn sequence_numbers_order_by_position() {
        assert!(SequenceNumber::new(1) < SequenceNumber::new(2));
        assert!(SequenceNumber::new(10) > SequenceNumber::new(2));
        assert_eq!(SequenceNumber::new(5), SequenceNumber::new(5));
        assert_eq!(SequenceNumber::new(4).next(), SequenceNumber::new(5));

        let mut seqs = [
            SequenceNumber::new(3),
            SequenceNumber::new(1),
            SequenceNumber::new(2),
        ];
        seqs.sort();
        assert_eq!(
            seqs,
            [
                SequenceNumber::new(1),
                SequenceNumber::new(2),
                SequenceNumber::new(3),
            ]
        );
    }
}

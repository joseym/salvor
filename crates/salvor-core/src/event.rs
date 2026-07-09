//! The event vocabulary: the envelope every event is wrapped in, the payload
//! enum that names what happened, and the small supporting types a few
//! payloads carry.
//!
//! Everything here is pure data. No constructor reads the clock, draws
//! randomness, or performs IO: the recorded timestamp and any identity are
//! passed in by the caller. That purity is load-bearing, because the replay
//! engine must later move into an IO-free crate.

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
pub const SCHEMA_VERSION: u32 = 1;

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
    },
    /// A model call was requested. Records the correlating sequence number and
    /// the hash of the request, so a later completion can be matched to it.
    ModelCallRequested {
        /// Correlates this request with its [`Event::ModelCallCompleted`].
        seq: SequenceNumber,
        /// Content hash of the request sent to the model.
        request_hash: String,
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
        /// The observed value, interpreted in the units of `budget.kind`.
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
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Budget {
    /// The dimension this limit applies to.
    pub kind: BudgetKind,
    /// The ceiling, in the units implied by `kind`.
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
        });
        assert_round_trips(Event::ModelCallRequested {
            seq: SequenceNumber::new(1),
            request_hash: "sha256:req".into(),
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

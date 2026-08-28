//! The anchor document, and the comparison `salvor verify` makes against it.
//!
//! Every run's log is a hash chain, and the store keeps one head per run: how
//! many events the run holds and the hash that commits to all of them. That
//! chain catches any quiet revision of a run, but it is unkeyed, so it cannot
//! catch a writer who rewrites a run from its first event forward and
//! recomputes every hash, head included. Everything the verifier would use is
//! sitting in the database beside the rows.
//!
//! An anchor is the one thing that closes that: a copy of those heads, taken
//! at a moment, and kept somewhere the store cannot reach. A rewriter can
//! recompute the whole database, and a file on another machine still says what
//! the heads used to be.
//!
//! This module is the document and the judgement, with no IO of its own. The
//! reads (the store, the anchor file) and the writes (the report, the exit
//! code) are in [`crate::commands`], and the report's text is in
//! [`crate::render`].
//!
//! # What an anchor does and does not close
//!
//! It closes rewriting and back-dating: an anchored run that no longer carries
//! the anchored hash at the anchored length is named, whatever the store's own
//! chain now says about itself.
//!
//! It does not close anything about events recorded after the anchor was
//! taken, because there is nothing to compare them against. A run that has
//! grown since the anchor verifies as intact, and the events it grew by are
//! covered by the next anchor, not this one. Nor is it signed: an anchor is a
//! plain JSON file, so it proves what it says only to whoever can vouch for
//! where the file has been. Salvor ships no signing.

use std::path::Path;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The anchor document's own spec string, naming its shape and version.
///
/// A file that does not carry this is refused rather than read on a guess: an
/// anchor is evidence, and reading a document whose fields might mean
/// something else would turn a mismatch into a shrug. A future anchor that
/// records different fields (a signature, say) is a new spec string.
pub const ANCHOR_SPEC: &str = "salvor.anchor.v1";

/// The verification result's spec string, for a caller reading `--json`.
pub const VERIFY_SPEC: &str = "salvor.verify.v1";

/// An anchor: every run's chain head in the store, as of a moment.
///
/// Field order here is the field order in the file, so the two spec strings
/// come first and a reader can tell what they are holding from the first two
/// lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anchor {
    /// This document's own spec: [`ANCHOR_SPEC`].
    pub anchor: String,
    /// The chain spec the hashes below were built under, which is
    /// [`salvor_store::chain::CHAIN_SPEC`]. Recorded because a hash means
    /// nothing without the rule that produced it.
    pub chain: String,
    /// The store path as it was given on the command line. A note to a person
    /// reading the file later, never something `verify` matches on: an anchor
    /// is checked against whatever store the operator points it at, which
    /// after a restore is usually a different path.
    pub store: String,
    /// When the anchor was taken, RFC 3339 in UTC, from the wall clock.
    pub taken_at: String,
    /// One entry per run, ordered by run id.
    pub runs: Vec<AnchoredRun>,
}

/// One run's anchored head: its id, how many events it held, and the hash that
/// commits to exactly those events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchoredRun {
    /// The run id, lowercase and hyphenated.
    pub run: String,
    /// How many events the run held when the anchor was taken.
    pub len: u64,
    /// The chain's head hash at that length: 64 lowercase hex characters.
    pub hash: String,
}

impl Anchor {
    /// Builds an anchor over the heads a store handed back, ordered by run id.
    ///
    /// `store` is the path as the operator gave it, and `taken_at` is the wall
    /// clock. Neither is derived from the log: an anchor is an operator's
    /// record of when they looked, not a replayable fact about a run.
    #[must_use]
    pub fn take(
        store: &str,
        taken_at: OffsetDateTime,
        heads: Vec<(salvor_core::RunId, salvor_store::chain::OwnedChainHead)>,
    ) -> Self {
        let mut runs: Vec<AnchoredRun> = heads
            .into_iter()
            .map(|(run_id, head)| AnchoredRun {
                run: run_id.as_uuid().to_string(),
                len: head.len,
                hash: head.hash,
            })
            .collect();
        runs.sort_by(|a, b| a.run.cmp(&b.run));
        Anchor {
            anchor: ANCHOR_SPEC.to_owned(),
            chain: salvor_store::chain::CHAIN_SPEC.to_owned(),
            store: store.to_owned(),
            taken_at: format_time(taken_at),
            runs,
        }
    }

    /// Checks that this file's two spec strings are the ones this binary
    /// knows, returning the refusal to print when they are not.
    ///
    /// Both are refused for the same reason. An anchor document under another
    /// spec may name its fields the same way and mean something else, and a
    /// chain spec this binary does not implement means the hashes were built
    /// by a rule it cannot reproduce, so every comparison below would be
    /// between two unrelated numbers.
    ///
    /// # Errors
    ///
    /// Returns the message to print, naming the spec found and the spec
    /// expected.
    pub fn check_specs(&self) -> Result<(), String> {
        if self.anchor != ANCHOR_SPEC {
            return Err(format!(
                "this file says it is an anchor of kind `{}`, and this salvor only reads \
                 `{ANCHOR_SPEC}`. Nothing was checked. Verify with the salvor that wrote it.",
                self.anchor
            ));
        }
        if self.chain != salvor_store::chain::CHAIN_SPEC {
            return Err(format!(
                "this anchor's hashes were built under chain `{}`, and this salvor builds \
                 `{}`. Nothing was checked: the two hashes would not be comparable.",
                self.chain,
                salvor_store::chain::CHAIN_SPEC
            ));
        }
        Ok(())
    }
}

/// What the store says about one run, as `verify` reads it back.
///
/// The three cases a read can end in, and nothing about the anchor: keeping
/// the observation separate from the judgement is what lets [`finding_for`]
/// be a pure function of the two.
#[derive(Debug, Clone)]
pub enum Observed {
    /// The store holds no events for this run at all.
    Missing,
    /// The store refused the run's log: its recorded rows do not match its own
    /// chain, at `seq`.
    Broken {
        /// The position the chain first disagrees at.
        seq: u64,
        /// What the store said it expected and what it found there.
        detail: String,
    },
    /// The run reads, with `len` events, and the chain carried
    /// `hash_at_anchored_len` at the length the anchor recorded (`None` when
    /// the run is shorter than that, or holds no row at that position).
    Present {
        /// How many events the run holds now.
        len: u64,
        /// The head hash the chain carried at the anchored length.
        hash_at_anchored_len: Option<String>,
    },
}

/// What verification concluded about one run. The name of the variant is the
/// word the report and the JSON both print.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "finding", rename_all = "kebab-case")]
pub enum Finding {
    /// The run still carries the anchored hash at the anchored length. It may
    /// have grown since; `events_since` says by how much, and growth is
    /// ordinary appending, not a discrepancy.
    Intact {
        /// The length the anchor recorded.
        anchored_len: u64,
        /// How many events the run holds now.
        len: u64,
        /// How many of those were appended after the anchor was taken.
        events_since: u64,
    },
    /// The anchor records this run and the store does not hold it.
    Missing {
        /// The length the anchor recorded.
        anchored_len: u64,
    },
    /// The store holds fewer events than the anchor recorded.
    Shortened {
        /// The length the anchor recorded.
        anchored_len: u64,
        /// How many events the run holds now.
        len: u64,
    },
    /// The store holds at least as many events as the anchor recorded, and the
    /// hash at the anchored length is not the anchored one: the events the
    /// anchor covered are not the events the store now holds.
    Rewritten {
        /// The length the anchor recorded.
        anchored_len: u64,
        /// The hash the anchor recorded at that length.
        anchored_hash: String,
        /// The hash the store carries there now, or `None` if it holds no
        /// event at that position.
        found_hash: Option<String>,
    },
    /// The store refused the run's log: it does not match its own chain.
    Broken {
        /// The position the chain first disagrees at.
        seq: u64,
        /// What the store said it expected and what it found.
        detail: String,
    },
    /// The store holds this run and the anchor does not: it was started after
    /// the anchor was taken. Informational, never a failure.
    New {
        /// How many events the run holds now.
        len: u64,
    },
}

impl Finding {
    /// Whether this finding is a failure: the anchored events are not the
    /// events the store holds, or the store cannot vouch for them at all.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Finding::Missing { .. }
                | Finding::Shortened { .. }
                | Finding::Rewritten { .. }
                | Finding::Broken { .. }
        )
    }

    /// The word this finding prints, in the report and in the JSON.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Finding::Intact { .. } => "intact",
            Finding::Missing { .. } => "missing",
            Finding::Shortened { .. } => "shortened",
            Finding::Rewritten { .. } => "rewritten",
            Finding::Broken { .. } => "broken",
            Finding::New { .. } => "new",
        }
    }
}

/// One run and what verification concluded about it.
#[derive(Debug, Clone, Serialize)]
pub struct RunFinding {
    /// The run id.
    pub run: String,
    /// The conclusion, flattened so the JSON reads
    /// `{"run": ..., "finding": "intact", ...}`.
    #[serde(flatten)]
    pub finding: Finding,
}

/// Judges one anchored run against what the store showed.
///
/// The order of the checks is the order of the questions: is the run there at
/// all, does its own chain read, does it still hold the events it was anchored
/// at, and only then is the anchored hash still the hash at that position. A
/// run that grew since the anchor passes the last check unchanged, because the
/// anchored prefix is what the anchor commits to.
#[must_use]
pub fn finding_for(anchored: &AnchoredRun, observed: &Observed) -> Finding {
    match observed {
        Observed::Missing => Finding::Missing {
            anchored_len: anchored.len,
        },
        Observed::Broken { seq, detail } => Finding::Broken {
            seq: *seq,
            detail: detail.clone(),
        },
        Observed::Present { len, .. } if *len < anchored.len => Finding::Shortened {
            anchored_len: anchored.len,
            len: *len,
        },
        Observed::Present {
            len,
            hash_at_anchored_len,
        } => {
            if hash_at_anchored_len.as_deref() == Some(anchored.hash.as_str()) {
                Finding::Intact {
                    anchored_len: anchored.len,
                    len: *len,
                    events_since: len - anchored.len,
                }
            } else {
                Finding::Rewritten {
                    anchored_len: anchored.len,
                    anchored_hash: anchored.hash.clone(),
                    found_hash: hash_at_anchored_len.clone(),
                }
            }
        }
    }
}

/// The whole result of one `salvor verify`: what was checked, what was found,
/// and the counts the summary line reports.
#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    /// This document's own spec: [`VERIFY_SPEC`].
    pub verify: String,
    /// The store that was checked, as the path was given.
    pub store: String,
    /// The anchor file that was checked against.
    pub against: String,
    /// When the anchor was taken, copied from it.
    pub anchor_taken_at: String,
    /// When this check ran, RFC 3339 in UTC.
    pub checked_at: String,
    /// One entry per run, ordered by run id: every anchored run, plus every
    /// run the store holds that the anchor does not.
    pub runs: Vec<RunFinding>,
    /// How many runs the anchor recorded.
    pub anchored: usize,
    /// How many of those still carry their anchored events.
    pub intact: usize,
    /// How many runs the store holds that the anchor does not.
    pub new: usize,
    /// Whether every run passed. False if any run is missing, shortened,
    /// rewritten, or broken; a new run never makes this false.
    pub ok: bool,
}

impl Verification {
    /// Assembles the result from the findings, counting what the summary line
    /// reports and deciding the exit code.
    #[must_use]
    pub fn new(
        anchor: &Anchor,
        store: &Path,
        against: &Path,
        checked_at: OffsetDateTime,
        mut runs: Vec<RunFinding>,
    ) -> Self {
        runs.sort_by(|a, b| a.run.cmp(&b.run));
        let intact = runs
            .iter()
            .filter(|r| matches!(r.finding, Finding::Intact { .. }))
            .count();
        let new = runs
            .iter()
            .filter(|r| matches!(r.finding, Finding::New { .. }))
            .count();
        let ok = !runs.iter().any(|r| r.finding.is_failure());
        Verification {
            verify: VERIFY_SPEC.to_owned(),
            store: store.display().to_string(),
            against: against.display().to_string(),
            anchor_taken_at: anchor.taken_at.clone(),
            checked_at: format_time(checked_at),
            runs,
            anchored: anchor.runs.len(),
            intact,
            new,
            ok,
        }
    }

    /// The process exit code: `0` when every run passed, `1` when any did not.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        u8::from(!self.ok)
    }
}

/// An instant as RFC 3339 in UTC, the form both documents record time in.
///
/// A timestamp that will not format is a clock outside the representable
/// range, which no wall clock is; the empty string rather than a panic keeps
/// an anchor from being lost to it.
fn format_time(when: OffsetDateTime) -> String {
    when.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchored(len: u64, hash: &str) -> AnchoredRun {
        AnchoredRun {
            run: "6d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
            len,
            hash: hash.to_owned(),
        }
    }

    #[test]
    fn a_run_that_grew_since_the_anchor_is_intact() {
        let finding = finding_for(
            &anchored(3, "aa"),
            &Observed::Present {
                len: 7,
                hash_at_anchored_len: Some("aa".to_owned()),
            },
        );
        match finding {
            Finding::Intact {
                anchored_len,
                len,
                events_since,
            } => {
                assert_eq!((anchored_len, len, events_since), (3, 7, 4));
            }
            other => panic!("expected intact, got {other:?}"),
        }
        assert!(
            !finding_for(
                &anchored(3, "aa"),
                &Observed::Present {
                    len: 3,
                    hash_at_anchored_len: Some("aa".to_owned()),
                },
            )
            .is_failure()
        );
    }

    /// The case the anchor exists for: a store whose own chain reads perfectly
    /// and whose anchored prefix is not what it was.
    #[test]
    fn a_recomputed_rewrite_is_named_rewritten() {
        let finding = finding_for(
            &anchored(3, "aa"),
            &Observed::Present {
                len: 3,
                hash_at_anchored_len: Some("bb".to_owned()),
            },
        );
        match &finding {
            Finding::Rewritten {
                anchored_len,
                anchored_hash,
                found_hash,
            } => {
                assert_eq!(*anchored_len, 3);
                assert_eq!(anchored_hash, "aa");
                assert_eq!(found_hash.as_deref(), Some("bb"));
            }
            other => panic!("expected rewritten, got {other:?}"),
        }
        assert!(finding.is_failure());
    }

    #[test]
    fn a_shorter_run_is_shortened_and_an_absent_one_is_missing() {
        assert!(matches!(
            finding_for(
                &anchored(3, "aa"),
                &Observed::Present {
                    len: 2,
                    hash_at_anchored_len: None,
                },
            ),
            Finding::Shortened { len: 2, .. }
        ));
        assert!(matches!(
            finding_for(&anchored(3, "aa"), &Observed::Missing),
            Finding::Missing { anchored_len: 3 }
        ));
    }

    /// A log the store itself refuses is reported at the position it refused,
    /// rather than crashing the command.
    #[test]
    fn a_refused_log_is_broken_at_the_position_the_store_named() {
        let finding = finding_for(
            &anchored(3, "aa"),
            &Observed::Broken {
                seq: 2,
                detail: "expected x, found y".to_owned(),
            },
        );
        assert!(matches!(finding, Finding::Broken { seq: 2, .. }));
        assert!(finding.is_failure());
    }

    #[test]
    fn a_foreign_spec_is_refused_by_name() {
        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.anchor = "someone.else.v1".to_owned();
        let refusal = anchor.check_specs().expect_err("a foreign anchor spec");
        assert!(refusal.contains("someone.else.v1"), "{refusal}");
        assert!(refusal.contains(ANCHOR_SPEC), "{refusal}");

        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.chain = "salvor.chain.v9".to_owned();
        let refusal = anchor.check_specs().expect_err("a foreign chain spec");
        assert!(refusal.contains("salvor.chain.v9"), "{refusal}");
    }

    /// The document's shape is the contract an operator's own tooling reads,
    /// so the field names and their order are pinned here.
    #[test]
    fn the_anchor_document_has_the_documented_shape() {
        let anchor = Anchor::take(
            "/var/lib/salvor/salvor.db",
            OffsetDateTime::UNIX_EPOCH,
            vec![(
                salvor_core::RunId::from_uuid(
                    uuid::Uuid::parse_str("6d1b7a7e-0000-4000-8000-00000000abcd").expect("uuid"),
                ),
                salvor_store::chain::OwnedChainHead {
                    len: 4,
                    hash: "a".repeat(64),
                },
            )],
        );
        let json = serde_json::to_string(&anchor).expect("anchor serializes");
        assert_eq!(
            json,
            format!(
                r#"{{"anchor":"salvor.anchor.v1","chain":"salvor.chain.v1","store":"/var/lib/salvor/salvor.db","taken_at":"1970-01-01T00:00:00Z","runs":[{{"run":"6d1b7a7e-0000-4000-8000-00000000abcd","len":4,"hash":"{}"}}]}}"#,
                "a".repeat(64)
            )
        );
    }
}

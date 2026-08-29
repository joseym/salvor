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
//!
//! # Three exit codes
//!
//! The split that matters to a cron line is not "pass or fail" but "did the
//! check run at all". A verify that never opened a store and a verify that
//! read every run and found them intact are both silent otherwise, and they
//! mean opposite things. So [`EXIT_INTACT`], [`EXIT_TAMPER`] and
//! [`EXIT_NOT_CHECKED`] are three codes, not two: page on the second,
//! investigate the third, and never read the third as an all-clear.

use std::collections::{HashMap, HashSet};
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

/// The check ran and every anchored run still holds the events it was
/// anchored at.
pub const EXIT_INTACT: u8 = 0;

/// The check ran and found at least one anchored run missing, shortened,
/// rewritten, or broken. This is the code to page on.
pub const EXIT_TAMPER: u8 = 1;

/// The check did not run: no store at the path given, an anchor file that is
/// missing, unreadable, not JSON, written under another spec, carrying a
/// malformed entry, or committing to no runs at all. Nothing was compared, so
/// this says nothing about the store either way.
pub const EXIT_NOT_CHECKED: u8 = 2;

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
    ///
    /// Optional on read, and always written. Nothing is compared against it,
    /// so a file that omits it is still an anchor this binary can check a
    /// store against; refusing one would turn a note into a requirement. The
    /// one place it is read is the wrong-anchor hint, which says the anchor
    /// does not name a store rather than naming an empty one.
    #[serde(default)]
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
    pub len: RecordedLen,
    /// The chain's head hash at that length: 64 lowercase hex characters.
    pub hash: String,
}

impl AnchoredRun {
    /// The length this entry records, for an entry [`Anchor::check`] has
    /// already accepted.
    ///
    /// Zero for an entry that never passed that check, which is not a length
    /// any comparison here is reached with: the file is refused before a
    /// single run is read.
    #[must_use]
    pub fn anchored_len(&self) -> u64 {
        self.len.count().unwrap_or(0)
    }
}

/// A `len` as the file records it, before anything has agreed it is a length.
///
/// A plain `u64` field makes serde the thing that refuses `-5`, and serde
/// refuses it in serde's words: `invalid value: integer -5, expected u64 at
/// line 1 column 318`. That is a parser talking about a byte offset, in the
/// place an operator most needs a sentence about which entry of their anchor
/// is wrong. So the value is taken as written and judged in
/// [`Anchor::check_entries`], beside the entry it came from, with the same
/// care a `len` of 0 already got.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordedLen(serde_json::Value);

impl RecordedLen {
    /// The length, when the file recorded one: a whole number of events, at
    /// least one. `None` for anything else, including a negative number, a
    /// fraction, and a value that is not a number at all.
    #[must_use]
    pub fn count(&self) -> Option<u64> {
        self.0.as_u64().filter(|count| *count > 0)
    }
}

impl From<u64> for RecordedLen {
    fn from(count: u64) -> Self {
        RecordedLen(serde_json::Value::from(count))
    }
}

/// As the file wrote it, for a refusal that has to quote what it found: `-5`,
/// `1.5`, `"twelve"`.
impl std::fmt::Display for RecordedLen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
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
                len: head.len.into(),
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

    /// Everything that has to hold before a single run is read: the two spec
    /// strings, then every entry's own fields.
    ///
    /// One call, because the caller's decision is the same for all of it. A
    /// file that fails any of these is not an anchor this binary can compare
    /// anything against, so the check does not run and the exit code is
    /// [`EXIT_NOT_CHECKED`].
    ///
    /// # Errors
    ///
    /// Returns the message to print, naming what was wrong with the file.
    pub fn check(&self) -> Result<(), String> {
        self.check_specs()?;
        self.check_entries()
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

    /// Checks every entry's fields before any of them is compared to a store.
    ///
    /// An entry that is not shaped like an anchored head is a broken file, not
    /// a finding about the store. `"hash": "deadbeef"` compared to a real head
    /// would come out `rewritten`, which reads as tampering and sends an
    /// operator to a backup over a typo, so it is refused here instead, by the
    /// entry it is in.
    ///
    /// The three fields are checked against what the store can produce: a run
    /// id is a UUID, a chain hash is 64 lowercase hex characters (see
    /// [`salvor_store::chain`]), and an anchored run holds a whole number of
    /// events, at least one, since a run with no events has no head to record.
    /// The length is judged here rather than by the deserializer for the same
    /// reason the hash is: see [`RecordedLen`].
    ///
    /// A run named twice is refused as well, because the counts are what the
    /// summary reports and a duplicate inflates them: an anchor listing one
    /// run twice reports "2 anchored, 2 intact" over a store holding one run,
    /// which is a pass that counted the same evidence twice. The store writes
    /// one entry per run, so a second entry for the same run came from
    /// somewhere else.
    ///
    /// # Errors
    ///
    /// Returns the message to print, naming the entry by its position in the
    /// file and by the run it claims.
    pub fn check_entries(&self) -> Result<(), String> {
        let mut first_seen: HashMap<&str, usize> = HashMap::new();
        for (index, entry) in self.runs.iter().enumerate() {
            let position = index + 1;
            if uuid::Uuid::parse_str(&entry.run).is_err() {
                return Err(format!(
                    "entry {position} names run `{}`, which is not a run id (a UUID). Nothing \
                     was checked: this file is not an anchor this salvor can read.",
                    entry.run
                ));
            }
            // Every shape a `len` can arrive in that is not a length gets one
            // sentence, naming the entry and quoting what the file wrote. The
            // alternative is a `u64` field, where serde refuses `-5` before
            // this function is reached and refuses it as a parser: "invalid
            // value: integer -5, expected u64 at line 1 column 318".
            if entry.len.count().is_none() {
                return Err(format!(
                    "entry {position}, run {}, records a length of {}, and a length is a whole \
                     number of events, at least one: a run with no events has no head to anchor. \
                     Nothing was checked: this file is not an anchor this salvor can read.",
                    entry.run, entry.len,
                ));
            }
            if !is_chain_hash(&entry.hash) {
                return Err(format!(
                    "entry {position}, run {}, records `{}` where a chain hash goes, and a chain \
                     hash is 64 lowercase hex characters. Nothing was checked: this file is not \
                     an anchor this salvor can read.",
                    entry.run, entry.hash
                ));
            }
            if let Some(first) = first_seen.insert(entry.run.as_str(), position) {
                return Err(format!(
                    "entries {first} and {position} both name run {}, and an anchor records one \
                     entry per run. Nothing was checked: a run named twice is checked twice and \
                     counted twice, so a pass against this file would report more runs standing \
                     than this store holds.",
                    entry.run
                ));
            }
        }
        Ok(())
    }
}

/// Whether `text` is a chain hash as the store writes one: exactly 64
/// characters, each a lowercase hex digit. Uppercase is refused rather than
/// folded, because the store never writes it and a hash that differs in case
/// differs from the one recorded.
fn is_chain_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
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
    /// chain.
    Broken {
        /// The position the chain first disagrees at, and `None` when there is
        /// no row to blame: a recorded head that disagrees with every row at
        /// once, or with rows that are no longer there.
        seq: Option<u64>,
        /// What the store said disagreed with what.
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
        /// How many events the run holds now. Recorded alongside the anchored
        /// length because "rewritten" says nothing about the current size on
        /// its own, and a run rewritten and extended reads differently from
        /// one rewritten in place.
        len: u64,
        /// The hash the anchor recorded at that length.
        anchored_hash: String,
        /// The hash the store carries there now, or `None` if it holds no
        /// event at that position.
        found_hash: Option<String>,
    },
    /// The store refused the run's log: it does not match its own chain.
    Broken {
        /// The position the chain first disagrees at, and `None` when no
        /// single row is the problem: a recorded head that commits to a
        /// different count, or that outlived the rows under it. A report that
        /// prints a position anyway names a line that is not where to look.
        seq: Option<u64>,
        /// What the store said disagreed with what.
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
    let anchored_len = anchored.anchored_len();
    match observed {
        Observed::Missing => Finding::Missing { anchored_len },
        Observed::Broken { seq, detail } => Finding::Broken {
            seq: *seq,
            detail: detail.clone(),
        },
        Observed::Present { len, .. } if *len < anchored_len => Finding::Shortened {
            anchored_len,
            len: *len,
        },
        Observed::Present {
            len,
            hash_at_anchored_len,
        } => {
            if hash_at_anchored_len.as_deref() == Some(anchored.hash.as_str()) {
                Finding::Intact {
                    anchored_len,
                    len: *len,
                    events_since: len - anchored_len,
                }
            } else {
                Finding::Rewritten {
                    anchored_len,
                    len: *len,
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
    /// The store path the anchor itself recorded, copied from it. Never
    /// matched against `store`: a restore to a new path is ordinary. It is
    /// here so a report that finds every anchored run missing can say which
    /// two stores are being compared.
    pub anchor_store: String,
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
    /// How many anchored runs did not: missing, shortened, rewritten, or
    /// broken. Anchored runs only, so `intact + failed` is never more than
    /// `anchored`; a broken run the anchor never named is counted in
    /// [`Verification::broken_unanchored`] instead.
    pub failed: usize,
    /// How many runs the anchor never named whose log this store now refuses.
    ///
    /// Its own number rather than part of `failed`, because the anchor says
    /// nothing about these runs: the finding is the store refusing itself, not
    /// a comparison against the anchor. It still exits non-zero, because a log
    /// the store will not read is a log nobody can read.
    pub broken_unanchored: usize,
    /// How many runs the store holds that the anchor does not, and whose logs
    /// read. A run the anchor never named whose log the store refuses is in
    /// `broken_unanchored`, not here.
    pub new: usize,
    /// Whether this reads like the wrong anchor rather than a loss: every run
    /// the anchor names is missing here while the store holds runs the anchor
    /// never named. A suspicion, not a verdict; see
    /// [`Verification::looks_like_the_wrong_anchor`]. In the document because
    /// a caller reading `--json` has to be able to reach the same conclusion
    /// the human report prints.
    pub maybe_wrong_anchor: bool,
    /// Always true on this document: the check ran. A pre-flight refusal
    /// prints [`PreflightFailure`] instead, which carries `false`, so a
    /// consumer reads one field to tell "checked and clean" from "never
    /// checked".
    pub checked: bool,
    /// Whether every run passed. False if any anchored run is missing,
    /// shortened, rewritten, or broken, and false if any run outside the
    /// anchor is broken; a new run that reads never makes this false.
    pub ok: bool,
}

/// What `--json` prints when the check did not run at all.
///
/// The same first field as [`Verification`] and the same `ok`, so a consumer
/// parsing stdout gets one shape whatever happened, and `checked` is the field
/// that separates "read the store and found it clean" from "never read the
/// store". Printed on stdout with [`EXIT_NOT_CHECKED`].
#[derive(Debug, Clone, Serialize)]
pub struct PreflightFailure {
    /// This document's own spec: [`VERIFY_SPEC`], the same string a real
    /// result carries.
    pub verify: String,
    /// Always false: nothing was compared, so nothing passed.
    pub ok: bool,
    /// Always false: the check did not run.
    pub checked: bool,
    /// Why, in the words the human report would have printed on stderr.
    pub error: String,
}

impl PreflightFailure {
    /// The document for a check that did not run, carrying `error`.
    #[must_use]
    pub fn new(error: impl Into<String>) -> Self {
        PreflightFailure {
            verify: VERIFY_SPEC.to_owned(),
            ok: false,
            checked: false,
            error: error.into(),
        }
    }
}

impl Verification {
    /// Assembles the result from the findings, counting what the summary line
    /// reports and deciding the exit code.
    ///
    /// The counts are split by whether the anchor named the run, so they
    /// close: `intact + failed` never exceeds `anchored`, and a broken run the
    /// anchor never named lands in `broken_unanchored` rather than inflating a
    /// number the summary presents as a fraction of the anchored ones.
    #[must_use]
    pub fn new(
        anchor: &Anchor,
        store: &Path,
        against: &Path,
        checked_at: OffsetDateTime,
        mut runs: Vec<RunFinding>,
    ) -> Self {
        runs.sort_by(|a, b| a.run.cmp(&b.run));
        let anchored_runs: HashSet<&str> = anchor.runs.iter().map(|r| r.run.as_str()).collect();
        let intact = runs
            .iter()
            .filter(|r| matches!(r.finding, Finding::Intact { .. }))
            .count();
        let new = runs
            .iter()
            .filter(|r| matches!(r.finding, Finding::New { .. }))
            .count();
        let failed = runs
            .iter()
            .filter(|r| r.finding.is_failure() && anchored_runs.contains(r.run.as_str()))
            .count();
        let broken_unanchored = runs
            .iter()
            .filter(|r| {
                matches!(r.finding, Finding::Broken { .. })
                    && !anchored_runs.contains(r.run.as_str())
            })
            .count();
        let missing = runs
            .iter()
            .filter(|r| matches!(r.finding, Finding::Missing { .. }))
            .count();
        let anchored = anchor.runs.len();
        Verification {
            verify: VERIFY_SPEC.to_owned(),
            store: store.display().to_string(),
            against: against.display().to_string(),
            anchor_store: anchor.store.clone(),
            anchor_taken_at: anchor.taken_at.clone(),
            checked_at: format_time(checked_at),
            runs,
            anchored,
            intact,
            failed,
            broken_unanchored,
            new,
            maybe_wrong_anchor: anchored > 0 && new > 0 && missing == anchored,
            checked: true,
            ok: failed == 0 && broken_unanchored == 0,
        }
    }

    /// Whether every anchored run this store holds is missing while the store
    /// holds runs the anchor never saw, which is what pointing `verify` at the
    /// wrong pair of files looks like.
    ///
    /// It is a suspicion, not a verdict: a store really can lose every run it
    /// had and gain new ones. The report says so before it says anything about
    /// restoring from a backup, because the two answers are a minute apart in
    /// effort and hours apart in consequence.
    #[must_use]
    pub fn looks_like_the_wrong_anchor(&self) -> bool {
        self.maybe_wrong_anchor
    }

    /// How many anchored runs this store does not hold at all.
    #[must_use]
    pub fn missing(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| matches!(r.finding, Finding::Missing { .. }))
            .count()
    }

    /// The process exit code: [`EXIT_INTACT`] when every anchored run passed,
    /// [`EXIT_TAMPER`] when any did not. [`EXIT_NOT_CHECKED`] never comes from
    /// here, because a check that did not run produces no result to ask.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        if self.ok { EXIT_INTACT } else { EXIT_TAMPER }
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
            len: len.into(),
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
        // Rewritten and extended, so the current length is a fact of its own:
        // "rewritten" alone does not say whether the run is the size it was.
        let finding = finding_for(
            &anchored(3, "aa"),
            &Observed::Present {
                len: 5,
                hash_at_anchored_len: Some("bb".to_owned()),
            },
        );
        match &finding {
            Finding::Rewritten {
                anchored_len,
                len,
                anchored_hash,
                found_hash,
            } => {
                assert_eq!(*anchored_len, 3);
                assert_eq!(*len, 5, "the length the store holds now is recorded too");
                assert_eq!(anchored_hash, "aa");
                assert_eq!(found_hash.as_deref(), Some("bb"));
            }
            other => panic!("expected rewritten, got {other:?}"),
        }
        assert!(finding.is_failure());
        let json = serde_json::to_value(&finding).expect("serializes");
        assert_eq!(json["len"], serde_json::json!(5));
        assert_eq!(json["anchored_len"], serde_json::json!(3));
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
                seq: Some(2),
                detail: "expected x, found y".to_owned(),
            },
        );
        assert!(matches!(finding, Finding::Broken { seq: Some(2), .. }));
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

    /// A file whose entries are not shaped like anchored heads is refused as a
    /// file, by the entry that is wrong, rather than compared and reported as
    /// tampering.
    #[test]
    fn a_malformed_entry_is_refused_by_its_position() {
        let good = AnchoredRun {
            run: "6d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
            len: 4u64.into(),
            hash: "a".repeat(64),
        };

        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.runs = vec![good.clone()];
        anchor.check().expect("a well-formed entry passes");

        let mut short_hash = anchor.clone();
        short_hash.runs[0].hash = "deadbeef".to_owned();
        let refusal = short_hash.check().expect_err("a short hash is refused");
        assert!(refusal.contains("entry 1"), "{refusal}");
        assert!(refusal.contains("deadbeef"), "{refusal}");
        assert!(refusal.contains("64 lowercase hex"), "{refusal}");

        let mut upper = anchor.clone();
        upper.runs[0].hash = "A".repeat(64);
        assert!(
            upper.check().is_err(),
            "the store never writes uppercase, so an uppercase hash is not the recorded one"
        );

        let mut not_a_uuid = anchor.clone();
        not_a_uuid.runs[0].run = "run-7".to_owned();
        let refusal = not_a_uuid.check().expect_err("a non-uuid run is refused");
        assert!(refusal.contains("run-7"), "{refusal}");
        assert!(refusal.contains("UUID"), "{refusal}");

        let mut zero_len = anchor.clone();
        zero_len.runs[0].len = 0u64.into();
        let refusal = zero_len.check().expect_err("a length of 0 is refused");
        assert!(refusal.contains("length of 0"), "{refusal}");

        // The position named is the entry's, not always the first.
        let mut second_bad = anchor.clone();
        second_bad.runs.push(AnchoredRun {
            run: "7d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
            len: 1u64.into(),
            hash: "zz".repeat(32),
        });
        let refusal = second_bad.check().expect_err("the second entry is refused");
        assert!(refusal.contains("entry 2"), "{refusal}");
    }

    /// A foreign spec is refused before the entries are looked at, so an
    /// operator is told the one thing that stops everything else.
    #[test]
    fn the_spec_is_checked_before_the_entries() {
        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.anchor = "someone.else.v1".to_owned();
        anchor.runs = vec![AnchoredRun {
            run: "not a uuid".to_owned(),
            len: 0u64.into(),
            hash: String::new(),
        }];
        let refusal = anchor.check().expect_err("a foreign spec is refused");
        assert!(refusal.contains("someone.else.v1"), "{refusal}");
    }

    /// The exit code is a function of the findings alone: an intact store is
    /// [`EXIT_INTACT`], any failure is [`EXIT_TAMPER`], and a run the anchor
    /// never saw moves neither.
    #[test]
    fn the_exit_code_separates_intact_from_failed() {
        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.runs = vec![anchored(3, &"a".repeat(64))];
        let store = Path::new("s.db");
        let against = Path::new("anchor.json");

        let intact = Verification::new(
            &anchor,
            store,
            against,
            OffsetDateTime::UNIX_EPOCH,
            vec![
                RunFinding {
                    run: anchored(3, "").run,
                    finding: Finding::Intact {
                        anchored_len: 3,
                        len: 5,
                        events_since: 2,
                    },
                },
                RunFinding {
                    run: "9d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
                    finding: Finding::New { len: 1 },
                },
            ],
        );
        assert_eq!(intact.exit_code(), EXIT_INTACT);
        assert!(intact.ok && intact.checked);
        assert_eq!((intact.intact, intact.failed, intact.new), (1, 0, 1));
        assert_eq!(intact.anchor_store, "s.db");

        let failed = Verification::new(
            &anchor,
            store,
            against,
            OffsetDateTime::UNIX_EPOCH,
            vec![RunFinding {
                run: anchored(3, "").run,
                finding: Finding::Shortened {
                    anchored_len: 3,
                    len: 1,
                },
            }],
        );
        assert_eq!(failed.exit_code(), EXIT_TAMPER);
        assert_eq!(failed.failed, 1);
        assert_eq!(
            (EXIT_INTACT, EXIT_TAMPER, EXIT_NOT_CHECKED),
            (0, 1, 2),
            "the three codes are the ones the help text and the docs promise"
        );
    }

    /// Every anchored run missing while the store holds runs of its own is the
    /// shape of pointing verify at the wrong anchor, and is called out as a
    /// suspicion rather than decided.
    #[test]
    fn every_run_missing_beside_unknown_runs_looks_like_the_wrong_anchor() {
        let mut anchor = Anchor::take("old.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.runs = vec![anchored(3, &"a".repeat(64))];
        let missing_and_new = Verification::new(
            &anchor,
            Path::new("new.db"),
            Path::new("anchor.json"),
            OffsetDateTime::UNIX_EPOCH,
            vec![
                RunFinding {
                    run: anchored(3, "").run,
                    finding: Finding::Missing { anchored_len: 3 },
                },
                RunFinding {
                    run: "9d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
                    finding: Finding::New { len: 1 },
                },
            ],
        );
        assert!(missing_and_new.looks_like_the_wrong_anchor());

        // The same loss in a store holding nothing else is just a loss.
        let only_missing = Verification::new(
            &anchor,
            Path::new("new.db"),
            Path::new("anchor.json"),
            OffsetDateTime::UNIX_EPOCH,
            vec![RunFinding {
                run: anchored(3, "").run,
                finding: Finding::Missing { anchored_len: 3 },
            }],
        );
        assert!(!only_missing.looks_like_the_wrong_anchor());
    }

    /// An anchor that names one run twice verifies it twice and counts it
    /// twice, so a store holding one of two anchored runs comes back "2
    /// anchored, 2 intact" against evidence for one. Refused as a file.
    #[test]
    fn a_run_named_twice_refuses_the_file_by_both_positions() {
        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        let entry = AnchoredRun {
            run: "6d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
            len: 4u64.into(),
            hash: "a".repeat(64),
        };
        let other = AnchoredRun {
            run: "7d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
            len: 2u64.into(),
            hash: "b".repeat(64),
        };
        anchor.runs = vec![entry.clone(), other, entry.clone()];
        let refusal = anchor.check().expect_err("a run named twice is refused");
        assert!(refusal.contains("entries 1 and 3"), "{refusal}");
        assert!(refusal.contains(&entry.run), "{refusal}");

        // The same lengths and hashes across two runs are not a duplicate:
        // what is refused is one run named twice.
        let mut distinct = anchor.clone();
        distinct.runs = vec![
            entry.clone(),
            AnchoredRun {
                run: "7d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
                ..entry
            },
        ];
        distinct
            .check()
            .expect("two runs, same head, is not a duplicate");
    }

    /// An entry's own fields are checked before it is compared with any other,
    /// so a malformed run id is named as malformed rather than as a repeat of
    /// the malformed one above it.
    #[test]
    fn a_malformed_entry_is_named_before_a_duplicate_is() {
        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        let bad = AnchoredRun {
            run: "run-7".to_owned(),
            len: 1u64.into(),
            hash: "a".repeat(64),
        };
        anchor.runs = vec![bad.clone(), bad];
        let refusal = anchor.check().expect_err("a non-uuid run is refused");
        assert!(refusal.contains("not a run id"), "{refusal}");
    }

    /// The counts have to close: `intact` plus `failed` is the anchored total,
    /// and a broken run the anchor never named is its own number rather than a
    /// failure counted against runs it does not describe.
    #[test]
    fn a_broken_run_outside_the_anchor_is_counted_on_its_own() {
        let mut anchor = Anchor::take("s.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.runs = vec![anchored(3, &"a".repeat(64))];

        let result = Verification::new(
            &anchor,
            Path::new("s.db"),
            Path::new("anchor.json"),
            OffsetDateTime::UNIX_EPOCH,
            vec![
                RunFinding {
                    run: anchored(3, "").run,
                    finding: Finding::Shortened {
                        anchored_len: 3,
                        len: 1,
                    },
                },
                RunFinding {
                    run: "9d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
                    finding: Finding::Broken {
                        seq: Some(0),
                        detail: "expected x, found y".to_owned(),
                    },
                },
            ],
        );
        assert_eq!(result.anchored, 1);
        assert_eq!(result.failed, 1, "the anchored run, and only it");
        assert_eq!(result.broken_unanchored, 1);
        assert_eq!(result.new, 0, "a broken run is not a new one");
        assert!(
            result.intact + result.failed <= result.anchored,
            "the counts close: {result:?}"
        );
        assert_eq!(result.exit_code(), EXIT_TAMPER);

        // A broken run outside the anchor on its own still fails the check:
        // the store refuses a log, and nobody can read it whatever the anchor
        // says.
        let only_outside = Verification::new(
            &anchor,
            Path::new("s.db"),
            Path::new("anchor.json"),
            OffsetDateTime::UNIX_EPOCH,
            vec![
                RunFinding {
                    run: anchored(3, "").run,
                    finding: Finding::Intact {
                        anchored_len: 3,
                        len: 3,
                        events_since: 0,
                    },
                },
                RunFinding {
                    run: "9d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
                    finding: Finding::Broken {
                        seq: Some(0),
                        detail: "expected x, found y".to_owned(),
                    },
                },
            ],
        );
        assert_eq!((only_outside.intact, only_outside.failed), (1, 0));
        assert_eq!(only_outside.broken_unanchored, 1);
        assert!(!only_outside.ok);
        assert_eq!(only_outside.exit_code(), EXIT_TAMPER);
    }

    /// The suspicion the human report prints is a field on the document, so a
    /// caller reading `--json` reaches the same conclusion without parsing
    /// prose.
    #[test]
    fn the_wrong_anchor_suspicion_is_a_field_on_the_document() {
        let mut anchor = Anchor::take("old.db", OffsetDateTime::UNIX_EPOCH, Vec::new());
        anchor.runs = vec![anchored(3, &"a".repeat(64))];
        let result = Verification::new(
            &anchor,
            Path::new("new.db"),
            Path::new("anchor.json"),
            OffsetDateTime::UNIX_EPOCH,
            vec![
                RunFinding {
                    run: anchored(3, "").run,
                    finding: Finding::Missing { anchored_len: 3 },
                },
                RunFinding {
                    run: "9d1b7a7e-0000-4000-8000-00000000abcd".to_owned(),
                    finding: Finding::New { len: 1 },
                },
            ],
        );
        assert!(result.maybe_wrong_anchor);
        assert_eq!(
            result.looks_like_the_wrong_anchor(),
            result.maybe_wrong_anchor,
            "the method and the field are one answer"
        );
        let json = serde_json::to_value(&result).expect("serializes");
        assert_eq!(json["maybe_wrong_anchor"], serde_json::json!(true));
        assert_eq!(json["broken_unanchored"], serde_json::json!(0));
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

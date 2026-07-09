//! Error compaction: what a failed tool call puts into the model's context
//! window for the built-in loop.
//!
//! The event log always records the **full** error (inside the tool-call
//! completion; see [`crate::wire`]). Compaction governs only the
//! `tool_result` content handed back to the model, in two deterministic
//! steps:
//!
//! 1. **Truncation.** A message longer than [`COMPACT_MESSAGE_CAP`]
//!    characters keeps its first [`COMPACT_HEAD_CHARS`] and last
//!    [`COMPACT_TAIL_CHARS`] characters around an elision marker naming how
//!    many characters were dropped. Head and tail both survive because
//!    error text tends to put the *what* up front and the *why* (a cause
//!    chain) at the end.
//! 2. **Repeat collapse.** When the same tool fails with the identical full
//!    message consecutively (no other dispatch result in between), the
//!    second and every later occurrence is replaced by a one-line summary
//!    naming the repeat count, instead of the same wall of text again.
//!
//! Both steps are pure functions of recorded data. That is not a style
//! preference: the compacted content flows into the next model request,
//! the request is hashed into `ModelCallRequested`, and the hash must
//! reproduce bit for bit on replay. Any nondeterminism here would poison
//! every replay downstream of a tool failure.
//!
//! Counting is in `char`s, not bytes, so truncation never splits a UTF-8
//! sequence. `RunCtx` users own their context window and their own policy;
//! these functions are exported for them to reuse or ignore.

/// The maximum length, in characters, of an error message handed to the
/// model uncompacted.
pub const COMPACT_MESSAGE_CAP: usize = 512;

/// How many leading characters survive truncation.
pub const COMPACT_HEAD_CHARS: usize = 320;

/// How many trailing characters survive truncation.
pub const COMPACT_TAIL_CHARS: usize = 128;

/// Truncates a long error message, keeping head and tail around an elision
/// marker. Messages at or under [`COMPACT_MESSAGE_CAP`] characters pass
/// through unchanged.
#[must_use]
pub fn compact_error_message(message: &str) -> String {
    let total = message.chars().count();
    if total <= COMPACT_MESSAGE_CAP {
        return message.to_owned();
    }
    let head: String = message.chars().take(COMPACT_HEAD_CHARS).collect();
    let tail: String = {
        let skip = total - COMPACT_TAIL_CHARS;
        message.chars().skip(skip).collect()
    };
    let elided = total - COMPACT_HEAD_CHARS - COMPACT_TAIL_CHARS;
    format!("{head} [... {elided} chars elided ...] {tail}")
}

/// Tracks consecutive identical tool failures and produces the model-facing
/// content for each dispatch result.
///
/// The loop owns one tracker per drive. Feed it every dispatch outcome in
/// order: [`content_for_failure`](Self::content_for_failure) for failures,
/// [`record_success`](Self::record_success) for anything else. Because it is
/// rebuilt from the same recorded sequence on every replay, its output is
/// identical across replays.
#[derive(Debug, Default)]
pub struct FailureTracker {
    /// The (tool, full message) of the last failure, when the last dispatch
    /// was a failure.
    last: Option<(String, String)>,
    /// How many consecutive times that failure has occurred.
    count: u32,
}

impl FailureTracker {
    /// A fresh tracker with no failure history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes a dispatch that did not fail, breaking any repeat streak.
    pub fn record_success(&mut self) {
        self.last = None;
        self.count = 0;
    }

    /// Notes a failure and returns the content the model should see: the
    /// compacted message the first time, a repeat summary from the second
    /// consecutive identical failure onward.
    pub fn content_for_failure(&mut self, tool: &str, full_message: &str) -> String {
        let same = self.last.as_ref().is_some_and(|(last_tool, last_message)| {
            last_tool == tool && last_message == full_message
        });
        if same {
            self.count += 1;
            format!(
                "tool `{tool}` has failed {count} consecutive times with the same error; \
                 the full error is recorded in the run log",
                count = self.count
            )
        } else {
            self.last = Some((tool.to_owned(), full_message.to_owned()));
            self.count = 1;
            compact_error_message(full_message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Short messages pass through untouched.
    #[test]
    fn short_messages_are_unchanged() {
        let message = "connection refused";
        assert_eq!(compact_error_message(message), message);
        let at_cap = "x".repeat(COMPACT_MESSAGE_CAP);
        assert_eq!(compact_error_message(&at_cap), at_cap);
    }

    /// Long messages keep head and tail around the elision marker, and the
    /// marker names the exact number of characters dropped.
    #[test]
    fn long_messages_keep_head_and_tail() {
        let head_part = "H".repeat(COMPACT_HEAD_CHARS);
        let middle = "M".repeat(1000);
        let tail_part = "T".repeat(COMPACT_TAIL_CHARS);
        let message = format!("{head_part}{middle}{tail_part}");

        let compacted = compact_error_message(&message);
        assert!(compacted.starts_with(&head_part));
        assert!(compacted.ends_with(&tail_part));
        assert!(compacted.contains("[... 1000 chars elided ...]"));
    }

    /// Truncation counts characters, so multi-byte text never splits.
    #[test]
    fn truncation_is_char_safe() {
        let total = COMPACT_MESSAGE_CAP + 100;
        let elided = total - COMPACT_HEAD_CHARS - COMPACT_TAIL_CHARS;
        let message = "\u{00e9}".repeat(total);
        let compacted = compact_error_message(&message);
        assert!(compacted.contains(&format!("[... {elided} chars elided ...]")));
    }

    /// Identical consecutive failures collapse into a counted summary; a
    /// success or a different failure resets the streak.
    #[test]
    fn repeats_collapse_and_streaks_reset() {
        let mut tracker = FailureTracker::new();
        let first = tracker.content_for_failure("search", "boom");
        assert_eq!(first, "boom");
        let second = tracker.content_for_failure("search", "boom");
        assert!(second.contains("2 consecutive times"), "{second}");
        let third = tracker.content_for_failure("search", "boom");
        assert!(third.contains("3 consecutive times"), "{third}");

        // A different message is a fresh failure, not a repeat.
        let different = tracker.content_for_failure("search", "bang");
        assert_eq!(different, "bang");

        // A success breaks the streak entirely.
        tracker.record_success();
        let after_success = tracker.content_for_failure("search", "bang");
        assert_eq!(after_success, "bang");
    }

    /// The same message from a different tool is not a repeat.
    #[test]
    fn repeats_are_per_tool() {
        let mut tracker = FailureTracker::new();
        let _ = tracker.content_for_failure("search", "boom");
        let other_tool = tracker.content_for_failure("fetch", "boom");
        assert_eq!(other_tool, "boom");
    }
}

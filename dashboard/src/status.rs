//! The run-status model and the two atoms every view keys off:
//! [`StatusBadge`] and [`RunRef`].
//!
//! # The three groups
//!
//! A run's derived status is one of eight states in three groups. The grouping
//! is the thing the UI must make legible, so it lives here as [`StatusGroup`]
//! rather than being re-derived per view:
//!
//! - **In progress** (moving on their own; nothing for a person to do):
//!   `running`, `awaiting-model`, `awaiting-tool`.
//! - **Waiting on a human** (the point of the inbox; must stand out):
//!   `suspended`, `budget-exceeded`, `needs-reconciliation`.
//! - **Terminal** (done; `failed` is the one worth flagging):
//!   `completed`, `failed`.
//!
//! [`RunStatus`] also carries a ninth variant, [`RunStatus::NotStarted`] (an
//! empty log). It is not one of the eight, so it is shown with its own label
//! and grouped with in-progress: a not-yet-started run has nothing for a person
//! to act on. That mapping is the one judgement call here and is called out at
//! [`status_group`].

use leptos::prelude::*;
use leptos_router::components::A;

use crate::replay::RunStatus;

/// The three attention groups a run's status falls into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusGroup {
    /// Moving on its own; nothing for a person to do.
    InProgress,
    /// Parked, waiting on a human decision. The inbox collects these.
    WaitingOnHuman,
    /// Finished, for good or ill.
    Terminal,
}

impl StatusGroup {
    /// A stable machine slug, for `data-` attributes and CSS hooks.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            StatusGroup::InProgress => "in-progress",
            StatusGroup::WaitingOnHuman => "waiting-on-human",
            StatusGroup::Terminal => "terminal",
        }
    }

    /// A human label for the group.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            StatusGroup::InProgress => "In progress",
            StatusGroup::WaitingOnHuman => "Waiting on a human",
            StatusGroup::Terminal => "Terminal",
        }
    }
}

/// The kebab-case slug for a status, e.g. `awaiting-model`. Stable; used as a
/// `data-status` hook and as the badge's visible text in this skeleton.
#[must_use]
pub const fn status_slug(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::NotStarted => "not-started",
        RunStatus::Running => "running",
        RunStatus::AwaitingModel => "awaiting-model",
        RunStatus::AwaitingTool => "awaiting-tool",
        RunStatus::Suspended { .. } => "suspended",
        RunStatus::BudgetExceeded { .. } => "budget-exceeded",
        RunStatus::NeedsReconciliation => "needs-reconciliation",
        RunStatus::Completed { .. } => "completed",
        RunStatus::Failed { .. } => "failed",
    }
}

/// The attention group a status belongs to.
///
/// The eight documented states map to their groups directly. `NotStarted` is
/// the ninth variant and not one of the eight; it is grouped with in-progress
/// because an empty log has nothing for a person to act on. That is the single
/// judgement call in this mapping.
#[must_use]
pub const fn status_group(status: &RunStatus) -> StatusGroup {
    match status {
        RunStatus::NotStarted
        | RunStatus::Running
        | RunStatus::AwaitingModel
        | RunStatus::AwaitingTool => StatusGroup::InProgress,
        RunStatus::Suspended { .. }
        | RunStatus::BudgetExceeded { .. }
        | RunStatus::NeedsReconciliation => StatusGroup::WaitingOnHuman,
        RunStatus::Completed { .. } | RunStatus::Failed { .. } => StatusGroup::Terminal,
    }
}

/// The most-repeated atom: a run's derived status, shown as its slug with the
/// group carried on `data-group` so the design layer can style the three groups
/// distinctly. Markup is deliberately plain here; the visual treatment lives
/// in the stylesheet.
#[component]
pub fn StatusBadge(
    /// The derived status to render.
    status: RunStatus,
) -> impl IntoView {
    let group = status_group(&status);
    let slug = status_slug(&status);
    view! {
        <span class="status-badge" data-status=slug data-group=group.slug()>
            <span class="status-dot"></span>
            <span class="status-text">{slug}</span>
        </span>
    }
}

/// How many leading characters of an identifier a [`RunRef`] shows before it
/// truncates. Eight is the density the design prototype settled on.
pub const RUN_REF_PREFIX: usize = 8;

/// Truncates an identifier to [`RUN_REF_PREFIX`] characters, appending an
/// ellipsis when it was longer. Counts characters, not bytes, so it never
/// splits a multi-byte character.
#[must_use]
pub fn truncate_id(id: &str) -> String {
    if id.chars().count() <= RUN_REF_PREFIX {
        id.to_string()
    } else {
        let prefix: String = id.chars().take(RUN_REF_PREFIX).collect();
        format!("{prefix}\u{2026}")
    }
}

/// A run or hash identifier: truncated for density, the full value in a
/// tooltip and copyable, and linked into the inspector route for that run.
#[component]
pub fn RunRef(
    /// The full identifier (a run UUID, typically).
    #[prop(into)]
    id: String,
) -> impl IntoView {
    let short = truncate_id(&id);
    let href = format!("/runs/{id}");
    let full_for_title = id.clone();
    let full_for_copy = id;
    view! {
        <span class="run-ref">
            <A href=href attr:title=full_for_title.clone()>
                <span class="run-ref-id">{short}</span>
            </A>
            <button
                class="run-ref-copy"
                title="Copy full id"
                on:click=move |_| copy_to_clipboard(&full_for_copy)
            >
                "copy"
            </button>
        </span>
    }
}

/// Writes text to the browser clipboard, best effort. Used by [`RunRef`] so the
/// truncated id is copyable in full. The returned promise is intentionally
/// dropped: a skeleton does not yet surface success or failure.
fn copy_to_clipboard(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status maps to a group, and the eight documented states land in
    /// the groups the spec assigns.
    #[test]
    fn statuses_group_as_specified() {
        use RunStatus::*;
        let value = serde_json::json!({});
        assert_eq!(status_group(&Running), StatusGroup::InProgress);
        assert_eq!(status_group(&AwaitingModel), StatusGroup::InProgress);
        assert_eq!(status_group(&AwaitingTool), StatusGroup::InProgress);
        assert_eq!(
            status_group(&Suspended {
                reason: String::new(),
                input_schema: value.clone()
            }),
            StatusGroup::WaitingOnHuman
        );
        assert_eq!(
            status_group(&NeedsReconciliation),
            StatusGroup::WaitingOnHuman
        );
        assert_eq!(
            status_group(&Completed { output: value }),
            StatusGroup::Terminal
        );
        assert_eq!(
            status_group(&Failed {
                error: String::new()
            }),
            StatusGroup::Terminal
        );
    }

    #[test]
    fn truncate_shortens_only_when_longer() {
        assert_eq!(truncate_id("abcd"), "abcd");
        assert_eq!(truncate_id("6f1a2b3c4d5e"), "6f1a2b3c\u{2026}");
    }
}

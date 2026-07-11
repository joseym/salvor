//! The four views, as named empty placeholders.
//!
//! This module builds the shell and the plumbing only. Each view comes later:
//! the run list, the inspector and its scrubber, the approval inbox, and the
//! spend overview. Each component here is a named seam the router already
//! targets, so wiring a view in later means filling one body, not touching the
//! router.

use leptos::prelude::*;

use crate::inspector::RunInspector;

/// Run list (landing), route `/`. To come: the run table.
#[component]
pub fn RunList() -> impl IntoView {
    view! { <p class="view-placeholder">"Run list (slice 2)"</p> }
}

/// Run inspector, route `/runs/:id`. Now real: it delegates to
/// [`RunInspector`](crate::inspector::RunInspector), which reads the `:id`
/// param, opens the run's stream, and renders the header, timeline, and
/// scrubber. The remaining views stay placeholders.
#[component]
pub fn Inspector() -> impl IntoView {
    view! { <RunInspector /> }
}

/// Approval & reconciliation inbox, route `/inbox`. To come: the parked-run
/// queues, schema-to-form resume, and reconciliation resolve.
#[component]
pub fn Inbox() -> impl IntoView {
    view! { <p class="view-placeholder">"Approval inbox (slice 4)"</p> }
}

/// Spend overview, route `/spend`. To come: burn-down and aggregates.
#[component]
pub fn Spend() -> impl IntoView {
    view! { <p class="view-placeholder">"Spend (slice 5)"</p> }
}

/// Fallback for an unmatched route.
#[component]
pub fn NotFound() -> impl IntoView {
    view! { <p class="view-placeholder">"Not found"</p> }
}

//! The four views, as named empty placeholders.
//!
//! This module builds the shell and the plumbing only. Each view comes later:
//! the run list, the inspector and its scrubber, the approval inbox, and the
//! spend overview. Each component here is a named seam the router already
//! targets, so wiring a view in later means filling one body, not touching the
//! router.

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

/// Run list (landing), route `/`. To come: the run table.
#[component]
pub fn RunList() -> impl IntoView {
    view! { <p class="view-placeholder">"Run list (slice 2)"</p> }
}

/// Run inspector, route `/runs/:id`. To come: the event timeline, the
/// per-event rows, and the scrubber bound to [`crate::replay::state_as_of`].
///
/// Reads the `:id` param already, so the placeholder names the run it will
/// inspect; nothing else about the view is implemented.
#[component]
pub fn Inspector() -> impl IntoView {
    let params = use_params_map();
    let run_id = move || params.read().get("id").unwrap_or_default();
    view! {
        <p class="view-placeholder">"Run inspector (slice 3) for run " {run_id}</p>
    }
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

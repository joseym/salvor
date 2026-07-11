//! The app shell and the routing skeleton.
//!
//! [`App`] is the mounted root: it resolves the [`Config`], puts it and a
//! shared connection signal into context, and hands off to the [`Router`]. The
//! shell draws the cross-view navigation and the connection indicator; the
//! [`Routes`] block maps the four view routes onto their components, which are
//! empty placeholders for now (see [`crate::views`]).
//!
//! # The connection indicator binding
//!
//! The indicator is bound to a shared `RwSignal<ConnectionState>` held in
//! context as [`AppConnection`]. The SSE client drives a stream's own
//! connection signal (see [`crate::sse`]); the inspector mirrors its
//! stream state into this shared signal, so the shell shows the live
//! connected / reconnecting / offline state of the run being watched. This
//! module wires the signal and renders all three states; it starts
//! [`ConnectionState::Offline`] because no run is being streamed yet.

use leptos::prelude::*;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::path;

use crate::config::Config;
use crate::sse::ConnectionState;
use crate::views::{Inbox, Inspector, NotFound, RunList, Spend};

/// The shared, app-wide stream connection state the shell's indicator reads.
/// A newtype so it is unambiguous in context.
#[derive(Clone, Copy)]
pub struct AppConnection(pub RwSignal<ConnectionState>);

/// The mounted root component.
#[component]
pub fn App() -> impl IntoView {
    // Resolve the control-plane address once and share it. Every module reads
    // the same Config out of context rather than re-reading the env.
    provide_context(Config::from_env());
    // The shell's connection indicator binds to this; a stream drives it.
    provide_context(AppConnection(RwSignal::new(ConnectionState::Offline)));

    view! {
        <Router>
            <AppShell />
        </Router>
    }
}

/// The frame around every view: cross-view navigation and the connection
/// indicator, with the routed view rendered into `main`.
#[component]
fn AppShell() -> impl IntoView {
    view! {
        <div class="app-shell">
            <nav class="app-nav">
                <ul class="app-nav-links">
                    <li><A href="/">"Runs"</A></li>
                    // The inspector opens per-run (via a RunRef), so it has no
                    // standalone target; it is named here so all four views
                    // appear in the nav.
                    <li class="app-nav-passive" title="Open a run from the list">"Inspector"</li>
                    <li><A href="/inbox">"Inbox"</A></li>
                    <li><A href="/spend">"Spend"</A></li>
                </ul>
                <ConnectionIndicator />
            </nav>
            <main class="app-main">
                <Routes fallback=|| view! { <NotFound /> }>
                    <Route path=path!("/") view=RunList />
                    <Route path=path!("/runs/:id") view=Inspector />
                    <Route path=path!("/inbox") view=Inbox />
                    <Route path=path!("/spend") view=Spend />
                </Routes>
            </main>
        </div>
    }
}

/// The live-stream connection indicator, bound to the shared [`AppConnection`]
/// signal. Renders the state slug on `data-state` and as text.
#[component]
fn ConnectionIndicator() -> impl IntoView {
    let AppConnection(connection) = expect_context();
    let slug = move || match connection.get() {
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Offline => "offline",
    };
    view! {
        <span class="connection-indicator" data-state=slug>
            <span class="connection-dot"></span>
            <span class="connection-text">{slug}</span>
        </span>
    }
}

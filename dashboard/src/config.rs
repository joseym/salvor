//! Which control-plane server the dashboard talks to.
//!
//! The dashboard is a static wasm bundle; it holds no server address of its
//! own beyond this one knob. The base URL is resolved once at startup and put
//! into Leptos context, so every module reads the same value.
//!
//! # The knob
//!
//! Two layers, compile-time default then runtime override:
//!
//! - **Compile time.** `SALVOR_API_BASE`, read with [`option_env!`], bakes a
//!   default into the bundle. A build for a fixed deployment sets it once
//!   (`SALVOR_API_BASE=https://salvor.example.com trunk build`). Left unset it
//!   falls back to [`DEFAULT_API_BASE`], the local `salvor serve` address.
//! - **Runtime.** [`Config`] is a plain value put into context, so it can
//!   later be built from a `?server=` query parameter or a settings screen
//!   without touching call sites. This module only wires the compile-time
//!   default through [`Config::from_env`].
//!
//! An empty base means "same origin": paths are used as-is, which is the right
//! default when the dashboard is served by the control plane itself behind one
//! reverse proxy.

/// The control-plane base URL baked in when `SALVOR_API_BASE` is unset at build
/// time. The local `salvor serve` default; override per deployment.
pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:8080";

/// The resolved control-plane address, shared through Leptos context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Base URL every control-plane path is joined onto. No trailing slash.
    /// Empty means same-origin (paths used verbatim).
    pub api_base: String,
}

impl Config {
    /// Resolves the base URL from the compile-time `SALVOR_API_BASE`, falling
    /// back to [`DEFAULT_API_BASE`]. A trailing slash is trimmed so joining is
    /// uniform.
    #[must_use]
    pub fn from_env() -> Self {
        let raw = option_env!("SALVOR_API_BASE").unwrap_or(DEFAULT_API_BASE);
        Self {
            api_base: raw.trim_end_matches('/').to_string(),
        }
    }

    /// Joins an absolute control-plane path (starting with `/`) onto the base.
    ///
    /// With an empty base the path is returned unchanged (same-origin).
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base, path)
    }

    /// The server-sent-events URL for one run, optionally from a cursor.
    ///
    /// `from_seq` appends `?from_seq=<n>`, the non-browser cursor the API
    /// documents, so a manual reconnect resumes exactly past the last event
    /// the client already holds. `None` requests a full replay from seq 0.
    #[must_use]
    pub fn events_url(&self, run_id: &str, from_seq: Option<u64>) -> String {
        let base = self.url(&format!("/v1/runs/{run_id}/events"));
        match from_seq {
            Some(seq) => format!("{base}?from_seq={seq}"),
            None => base,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_env()
    }
}

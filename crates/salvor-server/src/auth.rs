//! Bearer-token auth, the whole of it.
//!
//! The posture is single-tenant: either a bearer guards every endpoint, or the
//! server trusts its caller and a reverse proxy owns auth. There is no user
//! model and no RBAC.
//!
//! Two ways to configure a bearer, and they union. `--auth-token` names an
//! environment variable holding one shared secret, hashed once at startup and
//! never held in the process as plaintext; `--token-file` names a TOML file of
//! named tokens, each stored as the SHA-256 of the token (see
//! [`crate::tokens`]). A request that matches either one is let through. With
//! neither set, the middleware is a pass-through.
//!
//! Every comparison is between two SHA-256 digests, run through
//! [`subtle::ConstantTimeEq`], so the time a refusal takes carries nothing
//! about how many bytes of the presented token were right.
//!
//! # What a caller gets on the way in
//!
//! A request that is let through carries a [`Caller`] in its extensions,
//! holding the name the token was declared under, or [`SINGLE_TOKEN_CALLER`],
//! `shared:token`, for the `--auth-token` secret, which has no name of its
//! own. That name sits outside the `[a-z0-9-]` a minted name is held to, so
//! the shared secret and a named token never record one caller. Handlers read
//! it with `Extension<Caller>`.
//!
//! # A check that outlives the request
//!
//! The middleware checks once, on the way in, which is the whole story for a
//! request that answers and closes. A handler that holds a connection open
//! for hours needs the check again: [`Auth::capture`] hands it a
//! [`StreamCredential`], and [`StreamCredential::still_verifies`] re-runs
//! [`Auth::verify`] over the token file as it reads at that moment. The event
//! stream is the one caller today; see [`crate::sse`] for its cadence.
//!
//! # What a refusal costs
//!
//! Every refusal logs one `WARN` with the source address and an outcome, and
//! never the presented value. Repeated refusals from one source are delayed
//! before the `401`: [`FIRST_DELAY`] on the first, doubling to
//! [`MAX_DELAY`], and the count for a source is dropped once it has been
//! quiet for [`FAILURE_DECAY`] or has presented a token that verified. There
//! is no lockout at any count, so an operator can never be shut out of their
//! own server by someone else's traffic.
//!
//! The source is the peer address of the TCP connection, which
//! [`crate::serve`] supplies through axum's `ConnectInfo`. Behind a reverse
//! proxy that is the proxy's address, so every caller shares one counter.
//! This build reads no `X-Forwarded-For` and writes none: an operator whose
//! proxy sets that header and who wants per-client counters has a header
//! salvor leaves untouched to work with, and a salvor exposed directly has no
//! caller-supplied header steering its throttle.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;
use crate::tokens::{self, TokenStore};

/// The caller name recorded for the `--auth-token` shared secret, which is
/// configured by location rather than by name.
///
/// The colon is what keeps this name to itself. A name in a token file is
/// minted by `salvor token new`, which holds a name to `[a-z0-9-]{1,64}`, so
/// no colon can appear in one and no minted token can ever record the same
/// caller as the shared secret. A bare `token` was reachable: `salvor token
/// new token` is a legal name, and two credentials then wrote one name into
/// the log.
pub const SINGLE_TOKEN_CALLER: &str = "shared:token";

/// How long the first refusal from a source is held before its `401`.
pub const FIRST_DELAY: Duration = Duration::from_millis(100);

/// The longest a refusal is ever held, however many came before it.
pub const MAX_DELAY: Duration = Duration::from_secs(2);

/// How long a source must go without a refusal before its count is dropped.
pub const FAILURE_DECAY: Duration = Duration::from_secs(60);

/// The name of the token a request came in under.
///
/// Present in the extensions of every request the auth layer let through when
/// a bearer is configured, and absent on a server running the pass-through
/// posture, where there is no caller to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    name: String,
}

impl Caller {
    /// Names a caller.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// The token name this request came in under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Why a request was refused. Logged as the `outcome` field, and never
/// accompanied by the presented value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The request carried no `Authorization` header at all.
    MissingHeader,
    /// The header is present but is not a `Bearer <token>`, or its bytes are
    /// not text.
    BadScheme,
    /// A bearer was presented and no configured token has that hash: it was
    /// never valid, or it was revoked.
    UnknownToken,
}

impl Refusal {
    /// The stable string this refusal is logged under.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::MissingHeader => "missing_header",
            Refusal::BadScheme => "bad_scheme",
            Refusal::UnknownToken => "unknown_token",
        }
    }
}

/// How many refusals a source has run up, and when the last one landed.
#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    last: Instant,
}

/// Every configured bearer, plus the per-source refusal counters.
///
/// Held by [`AppState`] when at least one bearer is configured. A state with
/// none holds no `Auth` at all, which is what makes the pass-through posture
/// a missing value rather than an empty set.
#[derive(Debug, Default)]
pub struct Auth {
    /// SHA-256 of the `--auth-token` shared secret, hashed at startup.
    single: Option<[u8; 32]>,
    /// The named-token file, re-read when it changes.
    tokens: Option<TokenStore>,
    /// Refusals per source address, keyed by the peer IP. `None` covers a
    /// request that arrived without connection info, so those share a
    /// counter rather than escaping the throttle.
    failures: Mutex<HashMap<Option<IpAddr>, Failures>>,
}

impl Auth {
    /// An `Auth` with nothing configured yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the `--auth-token` shared secret by its hash. The plaintext is
    /// consumed here and never stored.
    pub fn set_single(&mut self, token: &str) {
        self.single = Some(tokens::digest(token));
    }

    /// Records the named-token file this server verifies against.
    pub fn set_token_file(&mut self, store: TokenStore) {
        self.tokens = Some(store);
    }

    /// The named-token file, when one is configured.
    #[must_use]
    pub fn token_file(&self) -> Option<&TokenStore> {
        self.tokens.as_ref()
    }

    /// Whether a shared secret is configured.
    #[must_use]
    pub fn has_single(&self) -> bool {
        self.single.is_some()
    }

    /// Checks a presented `Authorization` header value, naming the caller on
    /// a match.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] to log and answer `401` with.
    pub fn check(&self, presented: Option<&str>) -> Result<Caller, Refusal> {
        self.verify(&bearer_digest(presented)?)
    }

    /// Checks a bearer's digest against every configured bearer, naming the
    /// caller on a match.
    ///
    /// The named-token file is consulted first so a digest that matches both
    /// a named token and the shared secret is attributed to the name, which is
    /// the more specific answer.
    ///
    /// This is the one place a digest is compared. [`check`](Self::check)
    /// runs it for a request, and [`StreamCredential::still_verifies`] runs
    /// it again for a stream that is already open, so both read the same
    /// token file through the same constant-time comparison.
    ///
    /// # Errors
    ///
    /// [`Refusal::UnknownToken`], the only refusal a digest can earn: it was
    /// never valid, or it was revoked.
    pub fn verify(&self, digest: &[u8; 32]) -> Result<Caller, Refusal> {
        if let Some(store) = &self.tokens
            && let Some(name) = store.current().match_name(digest)
        {
            return Ok(Caller::new(name));
        }
        if let Some(single) = &self.single
            && tokens::digests_equal(single, digest)
        {
            return Ok(Caller::new(SINGLE_TOKEN_CALLER));
        }
        Err(Refusal::UnknownToken)
    }

    /// Captures the credential behind a request, for a handler that holds a
    /// connection open past the one check the middleware ran on the way in.
    ///
    /// `None` when the value does not verify, which is a request
    /// [`require_bearer`] already refused, so a handler behind that layer
    /// always gets a credential.
    #[must_use]
    pub fn capture(&self, presented: Option<&str>) -> Option<StreamCredential> {
        let digest = bearer_digest(presented).ok()?;
        let caller = self.verify(&digest).ok()?;
        Some(StreamCredential {
            name: caller.name,
            digest,
        })
    }

    /// Records a refusal from `source` and returns how long to hold the `401`.
    ///
    /// [`FIRST_DELAY`] doubles per consecutive refusal up to [`MAX_DELAY`]. A
    /// source that has been quiet for [`FAILURE_DECAY`] starts over at the
    /// first delay rather than resuming where it left off.
    pub fn record_failure(&self, source: Option<IpAddr>) -> Duration {
        let now = Instant::now();
        let mut failures = self.failures.lock().expect("auth failure counters lock");
        let entry = failures.entry(source).or_insert(Failures {
            count: 0,
            last: now,
        });
        if now.duration_since(entry.last) >= FAILURE_DECAY {
            entry.count = 0;
        }
        entry.count = entry.count.saturating_add(1);
        entry.last = now;
        delay_for(entry.count)
    }

    /// Drops a source's refusal count: it has just presented a token that
    /// verified, so the next mistake starts over at the first delay.
    pub fn clear_failures(&self, source: Option<IpAddr>) {
        self.failures
            .lock()
            .expect("auth failure counters lock")
            .remove(&source);
    }
}

/// The SHA-256 of the bearer in an `Authorization` header value.
///
/// # Errors
///
/// [`Refusal::MissingHeader`] for no header at all and
/// [`Refusal::BadScheme`] for a value that is not a `Bearer <token>`.
fn bearer_digest(presented: Option<&str>) -> Result<[u8; 32], Refusal> {
    let Some(value) = presented else {
        return Err(Refusal::MissingHeader);
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(Refusal::BadScheme);
    };
    Ok(tokens::digest(token))
}

/// The credential an open stream re-checks, so revoking a token ends the
/// streams that token opened.
///
/// A stream that lives for hours holds the caller's name and the SHA-256 of
/// the bearer that verified, and never the bearer itself: the same 32 bytes
/// [`Auth::verify`] compares a request against, and no plaintext.
///
/// Only the token file can make a captured credential stop verifying while
/// the process runs. The `--auth-token` shared secret is hashed once at
/// startup and never re-read, so its digest goes on matching for as long as
/// the server is up, and revoking it is a restart, which ends every stream on
/// its own. Both follow the one rule below; in practice the rule is about the
/// token file.
#[derive(Debug, Clone)]
pub struct StreamCredential {
    /// The token name this stream opened under.
    name: String,
    /// The SHA-256 of the bearer that verified.
    digest: [u8; 32],
}

impl StreamCredential {
    /// The token name the stream opened under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the credential still verifies against the tokens configured
    /// now.
    ///
    /// A token file re-read that dropped this token's entry makes this
    /// `false`, which is what ends the stream. The comparison is
    /// [`Auth::verify`], so a re-check costs what a request's check costs and
    /// reads the file through the same stat-and-reload path.
    #[must_use]
    pub fn still_verifies(&self, auth: &Auth) -> bool {
        auth.verify(&self.digest).is_ok()
    }
}

/// How long the `count`-th consecutive refusal is held.
#[must_use]
pub fn delay_for(count: u32) -> Duration {
    if count == 0 {
        return Duration::ZERO;
    }
    let shift = (count - 1).min(16);
    FIRST_DELAY.saturating_mul(1u32 << shift).min(MAX_DELAY)
}

/// Rejects a request whose bearer is missing or unknown, when a bearer is
/// configured; otherwise passes it straight through.
pub async fn require_bearer(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(auth) = state.auth() else {
        return next.run(request).await;
    };
    let source = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    match auth.check(presented) {
        Ok(caller) => {
            tracing::info!(
                caller = %caller.name(),
                source = %SourceLabel(source),
                "bearer accepted"
            );
            auth.clear_failures(source);
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(refusal) => {
            let delay = auth.record_failure(source);
            tracing::warn!(
                source = %SourceLabel(source),
                outcome = %refusal.as_str(),
                delay_ms = delay.as_millis() as u64,
                "bearer refused"
            );
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            ApiError::Unauthorized.into_response()
        }
    }
}

/// Renders a source address for a log line, or `unknown` for a request that
/// arrived without connection info.
struct SourceLabel(Option<IpAddr>);

impl std::fmt::Display for SourceLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(addr) => write!(f, "{addr}"),
            None => f.write_str("unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> String {
        tokens::digest(text)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn the_shared_secret_names_its_caller_and_nothing_else_verifies() {
        let mut auth = Auth::new();
        auth.set_single("a-long-enough-secret");
        assert_eq!(
            auth.check(Some("Bearer a-long-enough-secret"))
                .expect("accepted")
                .name(),
            SINGLE_TOKEN_CALLER
        );
        assert_eq!(auth.check(None), Err(Refusal::MissingHeader));
        assert_eq!(auth.check(Some("Basic abc")), Err(Refusal::BadScheme));
        assert_eq!(
            auth.check(Some("Bearer a-long-enough-secre")),
            Err(Refusal::UnknownToken)
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_named_token_wins_over_the_shared_secret_for_the_caller_name() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.toml");
        let mut file = std::fs::File::create(&path).expect("create");
        write!(file, "[tokens.ci]\nhash = \"{}\"\n", hex("shared")).expect("write");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let mut auth = Auth::new();
        auth.set_single("shared");
        auth.set_token_file(TokenStore::load(&path).expect("load"));
        assert_eq!(
            auth.check(Some("Bearer shared")).expect("accepted").name(),
            "ci"
        );
    }

    #[test]
    fn the_shared_secrets_name_is_outside_the_class_a_minted_name_is_held_to() {
        // `salvor token new` holds a name to `[a-z0-9-]{1,64}`, so a name
        // carrying anything else is one no token file entry can be given.
        assert_eq!(SINGLE_TOKEN_CALLER, "shared:token");
        assert!(
            SINGLE_TOKEN_CALLER
                .bytes()
                .any(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')),
            "no minted name can collide with {SINGLE_TOKEN_CALLER}"
        );
    }

    #[test]
    fn the_delay_grows_and_stops_at_the_cap() {
        assert_eq!(delay_for(0), Duration::ZERO);
        assert_eq!(delay_for(1), FIRST_DELAY);
        assert_eq!(delay_for(2), Duration::from_millis(200));
        assert_eq!(delay_for(3), Duration::from_millis(400));
        assert_eq!(delay_for(6), MAX_DELAY);
        assert_eq!(delay_for(60), MAX_DELAY);
    }

    #[test]
    fn a_verified_token_drops_the_source_count() {
        let auth = Auth::new();
        let source = Some("10.0.0.7".parse::<IpAddr>().expect("ip"));
        assert_eq!(auth.record_failure(source), FIRST_DELAY);
        assert_eq!(auth.record_failure(source), Duration::from_millis(200));
        auth.clear_failures(source);
        assert_eq!(auth.record_failure(source), FIRST_DELAY);
    }

    #[test]
    #[cfg(unix)]
    fn a_captured_credential_stops_verifying_once_the_file_drops_it() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.toml");
        let write = |entries: &[(&str, &str)]| {
            let mut file = std::fs::File::create(&path).expect("create");
            for (name, token) in entries {
                write!(file, "[tokens.{name}]\nhash = \"{}\"\n\n", hex(token)).expect("write");
            }
            drop(file);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        };

        write(&[("ci", "one"), ("ops", "two")]);
        let mut auth = Auth::new();
        auth.set_token_file(TokenStore::load(&path).expect("load"));

        let kept = auth.capture(Some("Bearer one")).expect("captured");
        let revoked = auth.capture(Some("Bearer two")).expect("captured");
        assert_eq!(revoked.name(), "ops", "the credential holds the name");
        assert!(revoked.still_verifies(&auth));

        // Revoking is deleting the entry, and the file is shorter for it, so
        // the store's stamp differs and the next check re-reads.
        write(&[("ci", "one")]);
        assert!(
            !revoked.still_verifies(&auth),
            "the dropped entry no longer verifies"
        );
        assert!(
            kept.still_verifies(&auth),
            "the entry the rewrite kept still does"
        );
    }

    #[test]
    fn the_shared_secrets_credential_verifies_for_the_life_of_the_process() {
        let mut auth = Auth::new();
        auth.set_single("a-long-enough-secret");
        let credential = auth
            .capture(Some("Bearer a-long-enough-secret"))
            .expect("captured");
        assert_eq!(credential.name(), SINGLE_TOKEN_CALLER);
        // Hashed once at startup and never re-read: nothing short of a
        // restart changes this answer.
        assert!(credential.still_verifies(&auth));
        assert!(auth.capture(Some("Bearer something-else")).is_none());
        assert!(auth.capture(None).is_none());
    }

    #[test]
    fn sources_are_counted_apart() {
        let auth = Auth::new();
        let one = Some("10.0.0.7".parse::<IpAddr>().expect("ip"));
        let two = Some("10.0.0.8".parse::<IpAddr>().expect("ip"));
        assert_eq!(auth.record_failure(one), FIRST_DELAY);
        assert_eq!(auth.record_failure(one), Duration::from_millis(200));
        assert_eq!(auth.record_failure(two), FIRST_DELAY);
        assert_eq!(auth.record_failure(None), FIRST_DELAY);
    }
}

//! The embedded web dashboard, served alongside the API on one origin.
//!
//! [`static_handler`] is the router's fallback: every request that no `/v1`
//! route matched lands here. It answers three kinds of request:
//!
//! - a real asset (`/main-ABC123.js`, `/salvor_replay_wasm_bg.wasm`) with the
//!   file's bytes, its content-type, and a cache policy chosen from the name;
//! - a client-routed path (`/runs`, `/inspector/<id>`) with `index.html`, so a
//!   deep link cold-loads and the app router takes over;
//! - anything else (an unknown `/v1` path, a missing asset with an extension,
//!   a non-GET) with a 404 or 405, never the SPA shell.
//!
//! The asset bytes come from [`Assets`], a rust-embed folder over the built
//! Angular app. In a debug build rust-embed reads that folder from disk at
//! request time, so a rebuilt app is visible without recompiling this crate; a
//! release build bakes the bytes in. When the folder was empty at compile time
//! (a checkout that never ran `salvor build`), `index.html` is absent and the
//! server says so in plain text rather than serving a broken page.

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The built dashboard, embedded from the Bridge's production output.
///
/// The folder is relative to this crate's manifest. `build.rs` guarantees it
/// exists so the derive never fails to compile; an empty folder simply embeds
/// nothing.
#[derive(RustEmbed)]
#[folder = "../../bridge/dist/bridge/browser"]
struct Assets;

/// The router fallback: serve a static asset, fall back to the SPA shell for a
/// client route, or refuse.
pub async fn static_handler(method: Method, uri: Uri) -> Response {
    // Only GET (and HEAD, which axum serves from the GET body) reaches the app;
    // a write to a non-API path is a 405, not an accidental SPA shell.
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let path = uri.path();

    // `/v1` keeps absolute priority: an unmatched path under it is a genuine
    // 404 from the API, never the dashboard. Serving `index.html` here would
    // hand an SDK an HTML page where it expects an error envelope.
    if path == "/v1" || path.starts_with("/v1/") {
        return StatusCode::NOT_FOUND.into_response();
    }

    // The empty checkout: nothing was embedded, so there is no app to serve.
    if Assets::get("index.html").is_none() {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "salvor was built without the dashboard: run salvor build to embed it\n",
        )
            .into_response();
    }

    let trimmed = path.trim_start_matches('/');

    // The root and any real asset are served directly.
    let asset_path = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    if let Some(file) = Assets::get(asset_path) {
        return serve_asset(asset_path, file.data.into_owned());
    }

    // A missing path that names a file (has an extension) is a real 404: a
    // stale hashed chunk or a typo'd asset should fail, not silently become the
    // shell. A missing path with no extension is a client route, so the app
    // shell cold-loads it and Angular's router resolves it.
    if has_extension(asset_path) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let index = Assets::get("index.html").expect("presence checked above");
    serve_asset("index.html", index.data.into_owned())
}

/// Builds the response for one asset: its bytes, a content-type from the file
/// extension, and a cache policy from the file name.
fn serve_asset(name: &str, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(name))
        .header(header::CACHE_CONTROL, cache_control(name))
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The content-type for a file name, from its extension.
///
/// The table is explicit rather than guessed so `.wasm` is always
/// `application/wasm`: the fold module the Inspector and Spend views stream
/// only compiles through `WebAssembly.instantiateStreaming` when the response
/// carries that exact type.
fn content_type(name: &str) -> HeaderValue {
    let value = match extension(name) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("ico") => "image/x-icon",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("map") => "application/json; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(value)
}

/// The cache policy for a file name.
///
/// `index.html` (and the shell served for a client route) must never be cached:
/// it is the one file that names the current hashed chunks, so a stale copy
/// pins the browser to a past build. A content-hashed chunk is immutable by
/// construction (a new build gets a new name), so it caches for a year.
/// Everything else (a favicon, an unhashed asset) takes a conservative default.
fn cache_control(name: &str) -> HeaderValue {
    if name == "index.html" {
        HeaderValue::from_static("no-cache")
    } else if is_hashed(name) {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("public, max-age=3600")
    }
}

/// Whether a file name carries a build hash, the shape Angular emits:
/// `main-MSUQQNP3.js`, `chunk-2UEFNF5G.js`, `styles-6FLNUR4M.css`. The test is
/// a `-` segment before the extension made of at least eight uppercase base-32
/// characters, which no hand-authored asset name matches.
fn is_hashed(name: &str) -> bool {
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) => stem,
        None => name,
    };
    let last = stem.rsplit(&['-', '/'][..]).next().unwrap_or(stem);
    last.len() >= 8
        && last
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The lowercase extension of a file name, if any.
fn extension(name: &str) -> Option<&str> {
    name.rsplit('/')
        .next()?
        .rsplit_once('.')
        .map(|(_, ext)| ext)
}

/// Whether the final path segment has a file extension. A path with none is
/// treated as a client route, not a missing file.
fn has_extension(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .map(|segment| segment.contains('.'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_chunks_are_recognized() {
        assert!(is_hashed("main-MSUQQNP3.js"));
        assert!(is_hashed("chunk-2UEFNF5G.js"));
        assert!(is_hashed("styles-6FLNUR4M.css"));
    }

    #[test]
    fn plain_names_are_not_hashed() {
        assert!(!is_hashed("index.html"));
        assert!(!is_hashed("favicon.ico"));
        assert!(!is_hashed("salvor_replay_wasm_bg.wasm"));
    }

    #[test]
    fn wasm_is_served_as_a_component_type() {
        assert_eq!(content_type("x.wasm"), "application/wasm");
    }

    #[test]
    fn extensionless_paths_are_routes() {
        assert!(!has_extension("/runs"));
        assert!(!has_extension("/inspector/abc-123"));
        assert!(has_extension("/main-MSUQQNP3.js"));
    }
}

//! The Angular dev server (`ng serve`) behind `salvor serve --dev`: hot
//! module reloading for `bridge/` alongside the API, so a UI/UX iteration
//! never needs a stop/start of either process.
//!
//! # Proxying
//!
//! `ng serve`'s dev server (the Vite-based `@angular/build:dev-server`
//! builder, current since Angular's esbuild application builder) is given a
//! generated proxy config forwarding `/v1` to the API's resolved bind
//! address. That target is written fresh for every `--dev` run (never the
//! committed `bridge/proxy.conf.json`, which is a convenience default for a
//! plain `ng serve`/`npm start` against the CLI's own default bind and is
//! not read on this path), so a custom `--bind` is always proxied correctly.
//! Both of the app's streamed HTTP surfaces under `/v1` — the run-events SSE
//! tail and the client SDK's fetch-based model-step stream (see
//! `sdks/typescript/src/sse.ts`; the app never uses a browser `EventSource`,
//! since one of those two endpoints is a POST, which `EventSource` cannot
//! issue) — proxy through untouched: Vite's proxy streams the response body
//! by default, and neither surface needs a WebSocket upgrade, so no `ws:
//! true` is required in the generated config.
//!
//! # Process lifecycle
//!
//! The child is spawned as `bash -lc 'exec node_modules/.bin/ng serve ...'`
//! (see `checkout::run_shell`'s doc comment for why a login shell): `exec`
//! replaces `bash` with the `ng` process in place, so the pid this module
//! tracks is `ng serve` itself, not a shell sitting above it. On Unix that
//! process is also made the leader of a fresh process group
//! (`process_group(0)`), detached from this CLI's own terminal group, so a
//! Ctrl-C at the terminal does not casually reach it — shutdown is instead
//! always explicit, through [`DevServer::shutdown`], which signals the whole
//! group (not just the tracked pid), reaching any subprocess `ng serve`
//! spawns of its own (esbuild's persistent build-service process) too.
//! [`commands::serve`](crate::commands::serve) calls that from both of its
//! exit paths: a Ctrl-C/SIGTERM caught by its own shutdown-signal wait, and
//! (indirectly, since that SIGTERM is the same signal caught there) a
//! `salvor serve --kill` aimed at the API process from another terminal.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

use crate::checkout;

/// How long [`DevServer::shutdown`] waits for a graceful exit after SIGTERM
/// before escalating to SIGKILL. Short and deliberate: `salvor serve --kill`
/// itself only waits ~2s for the API pid to disappear (see
/// `serve_kill::terminate`), and both processes are asked to stop at the same
/// instant, so this budget must leave room for the escalation and the reap
/// underneath that outer deadline.
const GRACEFUL_SHUTDOWN: Duration = Duration::from_millis(1200);

/// A running `ng serve` dev server for `bridge/`.
pub struct DevServer {
    child: Child,
    port: u16,
    /// The generated proxy config, removed on [`DevServer::shutdown`]. Lives
    /// outside `bridge/` (the OS temp directory) so a `--dev` run never
    /// writes into the checkout.
    proxy_config: PathBuf,
}

impl DevServer {
    /// The port the dev server bound. Combine with `http://localhost:` for
    /// the URL a human should open; `ng serve` binds `localhost` by default.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Starts `ng serve` in `bridge_dir`, hot reload on, proxying `/v1` to
    /// `api_addr`. Installs node dependencies first (`npm ci`) if
    /// `node_modules` is missing, printing a progress line while that runs.
    pub async fn start(bridge_dir: &Path, api_addr: SocketAddr) -> Result<DevServer> {
        checkout::ensure_node_modules(bridge_dir).await?;

        let port = free_port().context("finding a free port for the Angular dev server")?;
        let proxy_config = write_proxy_config(api_addr)
            .context("writing the dev server's generated proxy config")?;

        println!("starting the Angular dev server (ng serve) for hot reload");
        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(format!(
                "exec node_modules/.bin/ng serve --port {port} --proxy-config {}",
                shell_quote(&proxy_config)
            ))
            .current_dir(bridge_dir)
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // A fresh process group (see the module doc comment): shutdown is
            // always explicit through `DevServer::shutdown`, never an
            // incidental side effect of this terminal's own foreground-group
            // signal delivery.
            command.process_group(0);
        }
        let child = command
            .spawn()
            .with_context(|| format!("spawning `ng serve` in {}", bridge_dir.display()))?;

        Ok(DevServer {
            child,
            port,
            proxy_config,
        })
    }

    /// Tears the dev server down: SIGTERM to its whole process group (Unix),
    /// a short grace period, then SIGKILL if it is still alive; a plain child
    /// kill elsewhere. Removes the generated proxy config file, best effort.
    /// Never fails: this runs during shutdown, where a teardown hiccup must
    /// not stop `serve` from exiting.
    pub async fn shutdown(mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            terminate_group(pid as i32);
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.start_kill();
        }

        match tokio::time::timeout(GRACEFUL_SHUTDOWN, self.child.wait()).await {
            Ok(_) => {}
            Err(_elapsed) => {
                tracing::warn!("ng serve did not exit within the grace period; sending SIGKILL");
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
            }
        }

        let _ = std::fs::remove_file(&self.proxy_config);
    }
}

/// Sends `SIGTERM` to the negative pid, i.e. the whole process group: `bash`
/// (exec'd into `ng`, see the module doc comment) is that group's leader, so
/// this reaches `ng serve` and any subprocess it spawned, not just the one
/// pid this module tracks.
#[cfg(unix)]
fn terminate_group(pid: i32) {
    // SAFETY: `kill` with a negative pid signals the process group; this
    // takes no pointers and has no preconditions beyond a valid signal
    // number, which `SIGTERM` is.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
}

/// Binds an ephemeral port on loopback to learn a free one, then releases it
/// immediately so `ng serve` can bind it in turn. A small time-of-check race
/// (another process could take the port in between) is accepted here, the
/// same tradeoff `TcpListener::bind("...:0")` callers make throughout this
/// codebase's own tests.
fn free_port() -> Result<u16> {
    let listener = StdTcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .context("binding an ephemeral port")?;
    let port = listener
        .local_addr()
        .context("reading the ephemeral port")?
        .port();
    drop(listener);
    Ok(port)
}

/// Writes the dev server's proxy config (`/v1` -> `api_addr`) to a fresh file
/// in the OS temp directory, named with this process's pid plus a call
/// counter so concurrent `--dev` runs (and, within one process, concurrent
/// calls to this function) never collide. `0.0.0.0` (an unspecified bind
/// host, meaning "every interface") is not itself a valid connect target on
/// every platform, so it is normalized to loopback: the dev server and the
/// API it proxies to run on the same machine either way.
fn write_proxy_config(api_addr: SocketAddr) -> Result<PathBuf> {
    let host = if api_addr.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        api_addr.ip()
    };
    let target = format!("http://{host}:{port}", port = api_addr.port());
    let config = serde_json::json!({
        "/v1": {
            "target": target,
            "secure": false,
            "changeOrigin": true,
        }
    });

    // The pid alone would collide if this process ever wrote two of these
    // (concurrent tests calling this function directly, notably), so a
    // monotonic counter rides alongside it for a name unique per call, not
    // just per process.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "salvor-dev-proxy-{}-{unique}.json",
        std::process::id()
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Wraps `path` in single quotes for interpolation into a `bash -lc` command
/// line, escaping any embedded single quote. The OS temp directory this path
/// lives in is not expected to contain one, but the escape is cheap enough to
/// do unconditionally rather than assume it.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unspecified host (`0.0.0.0`, "every interface") is normalized to
    /// loopback in the generated target, since that is what a same-machine
    /// dev server needs to connect back to.
    #[test]
    fn proxy_config_normalizes_unspecified_host_to_loopback() {
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let path = write_proxy_config(addr).expect("writes the config");
        let text = std::fs::read_to_string(&path).expect("reads the config back");
        std::fs::remove_file(&path).ok();

        assert!(
            text.contains("http://127.0.0.1:8080"),
            "expected the loopback target in {text}"
        );
        assert!(
            text.contains("\"/v1\""),
            "expected the /v1 context in {text}"
        );
    }

    /// A concrete host (not `0.0.0.0`) passes through unchanged, so a
    /// `--bind` naming a specific loopback address or a real interface is
    /// proxied to exactly what the API actually bound.
    #[test]
    fn proxy_config_preserves_a_concrete_host() {
        let addr: SocketAddr = "127.0.0.1:9090".parse().unwrap();
        let path = write_proxy_config(addr).expect("writes the config");
        let text = std::fs::read_to_string(&path).expect("reads the config back");
        std::fs::remove_file(&path).ok();

        assert!(
            text.contains("http://127.0.0.1:9090"),
            "expected the concrete target in {text}"
        );
    }

    /// A path containing a single quote is escaped rather than breaking out
    /// of the quoted shell argument.
    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        let quoted = shell_quote(Path::new("/tmp/o'brien/proxy.json"));
        assert_eq!(quoted, r"'/tmp/o'\''brien/proxy.json'");
    }
}

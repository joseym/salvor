//! `salvor serve --dev`, end to end: spawn a real instance, prove the
//! Angular dev server actually started as a live child process and that its
//! `/v1` proxy really forwards to the API, then `salvor serve --kill` it (the
//! same path an operator uses on a plain `serve`) and prove BOTH the API and
//! the dev server child are gone, and the API's port freed.
//!
//! Marked `#[ignore]`: this drives a real `bash -lc` -> `ng serve` -> esbuild
//! toolchain (not a fixture), so it depends on wall-clock bundling time and
//! on `bridge/`'s `node_modules` already existing in this checkout (present
//! locally; not guaranteed reproducible or fast on an arbitrary CI runner,
//! which may lack node/npm entirely or need `npm ci` first). Run it by hand
//! after `cd bridge && npm ci` once:
//!
//!     cargo test -p salvor-cli --test dev_serve -- --ignored --nocapture
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, RefreshKind, System};
use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");

#[test]
#[ignore = "spawns a real ng serve/esbuild toolchain; wall-clock timing and a node/npm \
            toolchain are not safe assumptions for a shared CI runner"]
fn dev_starts_both_processes_proxies_and_kill_reaps_both() {
    let dir = tempdir().expect("tempdir");
    let store_path = dir.path().join("salvor.db");

    // Bind port 0: the OS assigns a free port for the API, same as
    // serve_kill's live test. The dev server's own port is chosen the same
    // way, internally, by `DevServer::start`.
    let mut child = Command::new(SALVOR_BIN)
        .args(["--store", store_path.to_str().unwrap()])
        .args(["serve", "--bind", "127.0.0.1:0", "--dev"])
        .env("RUST_LOG", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn salvor serve --dev");
    let salvor_pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");

    let (api_addr, dev_port) = read_banners(stdout);

    // The dev server is a real, live child of this exact salvor pid in the
    // process table -- not a shell wrapper sitting above it (`bash -lc
    // 'exec ...'` replaces bash with `ng` in place; see dev_server's module
    // doc comment). Polled rather than checked once: the "dev UI" banner
    // prints right after the spawn call returns, which can be a beat ahead
    // of `bash`'s `exec` actually landing and this process appearing in a
    // fresh process-table scan.
    assert!(
        wait_for_ng_child(salvor_pid, Duration::from_secs(5)).is_some(),
        "ng serve should be a live child of pid {salvor_pid}"
    );

    // The "dev UI" banner prints as soon as the child is spawned, not once
    // Vite has actually finished its first bundle and started accepting
    // connections (a real, observed wall-clock gap; see the module doc).
    // Wait for the port itself before asking it for anything.
    let dev_addr = format!("127.0.0.1:{dev_port}");
    wait_for_port(&dev_addr, Duration::from_secs(60));

    // The proxy actually forwards: the same JSON comes back whether asked of
    // the API directly or through the dev server's /v1 proxy.
    let direct = http_get(&api_addr, "/v1/runs");
    let proxied = http_get(&dev_addr, "/v1/runs");
    assert!(
        direct.contains("\"runs\":[]"),
        "direct API response should list zero runs: {direct}"
    );
    assert!(
        proxied.contains("\"runs\":[]"),
        "the dev server's proxied response should match the direct one: {proxied}"
    );

    // The same path an operator uses on a plain `serve`: a second,
    // independent invocation, `--kill <pid>`, direct-targeted so this can
    // never touch any other process on the machine.
    let pid_arg = salvor_pid.to_string();
    let kill = Command::new(SALVOR_BIN)
        .args(["serve", "--kill", &pid_arg])
        .env("RUST_LOG", "off")
        .output()
        .expect("run salvor serve --kill");
    assert!(
        kill.status.success(),
        "salvor serve --kill exits 0: {kill:?}"
    );
    let kill_stdout = String::from_utf8_lossy(&kill.stdout);
    assert!(
        kill_stdout.contains(&format!("killed pid {salvor_pid}")),
        "kill output names the pid: {kill_stdout}"
    );

    // Both the API pid and the dev server child are actually gone.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let salvor_exited = child.try_wait().expect("try_wait").is_some();
        let ng_gone = find_ng_child(salvor_pid).is_none();
        if salvor_exited && ng_gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "salvor (exited={salvor_exited}) and/or ng serve (gone={ng_gone}) \
             did not both exit in time"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // The API's port was actually freed.
    assert!(
        TcpStream::connect(&api_addr).is_err(),
        "port {api_addr} should be free after the kill"
    );
}

/// Reads `child`'s stdout on a background thread (so a slow-to-print child
/// cannot stall the deadline logic below) until both of `--dev`'s banner
/// lines are seen, returning the API's `host:port` and the dev server's
/// port. Fails with a clear message if either is missing after a generous
/// deadline (an `npm ci` plus the first Angular bundle both take real
/// wall-clock time).
///
/// The background thread keeps draining the pipe for the child's whole
/// lifetime, even after this function's channel receiver is dropped. That is
/// load-bearing, not tidiness: `ng serve` inherits salvor's stdout, so if
/// the read end of that pipe closed here, ng's next build-progress write
/// would take an EPIPE and node would crash on it (observed directly: an
/// unhandled `write EPIPE` out of ng's own logger), silently killing the
/// very process this test is about.
fn read_banners(stdout: std::process::ChildStdout) -> (String, u16) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    // A failed send only means the banners were already
                    // found and the receiver dropped; keep draining so the
                    // pipe never backs up or closes on the child.
                    let _ = tx.send(line.clone());
                }
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut api_addr = None;
    let mut dev_port = None;
    while api_addr.is_none() || dev_port.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "salvor --dev did not print both banners within 90s \
             (api_addr={api_addr:?}, dev_port={dev_port:?})"
        );
        let line = rx
            .recv_timeout(remaining)
            .expect("salvor --dev exited before printing both banners");
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("salvor control plane listening on http://") {
            api_addr = Some(rest.to_owned());
        } else if let Some(rest) = trimmed.strip_prefix("dev UI (hot reload): http://localhost:") {
            let port_str = rest.split('/').next().unwrap_or("");
            dev_port = port_str.parse::<u16>().ok();
        }
    }
    (api_addr.unwrap(), dev_port.unwrap())
}

/// Finds a live process that is both a direct child of `parent_pid` and
/// looks like the `ng serve` invocation `DevServer::start` spawns (`bash -lc
/// 'exec node_modules/.bin/ng serve ...'`). Matched by substring on the joined
/// argv rather than an exact argv0 comparison: on macOS, `sysinfo`'s argv
/// parsing of this exact shape (a login shell `exec`ing into a shebang'd
/// node script) is observed to sometimes fold the whole invocation into one
/// element (occasionally with trailing environment-variable-looking noise
/// appended) rather than a clean `["ng", "serve", ...]` split, so a strict
/// `cmd[0] == "ng"` check is not reliable there even though the process
/// itself is perfectly healthy (confirmed independently against the
/// system's own `ps`). `bash -lc "... exec ... ng serve ..."` before the
/// `exec` lands, and the post-exec form either way, both still contain the
/// literal substring `"ng serve"` somewhere in argv, so that is what this
/// looks for.
fn find_ng_child(parent_pid: u32) -> Option<u32> {
    let specifics = ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always);
    let system = System::new_with_specifics(RefreshKind::nothing().with_processes(specifics));
    system
        .processes()
        .values()
        .find(|process| {
            let is_child = process.parent().map(|pp| pp.as_u32()) == Some(parent_pid);
            let looks_like_ng = process
                .cmd()
                .iter()
                .any(|arg| arg.to_string_lossy().contains("ng serve"));
            is_child && looks_like_ng
        })
        .map(|process| process.pid().as_u32())
}

/// Polls [`find_ng_child`] until it finds a match or `timeout` elapses.
fn wait_for_ng_child(parent_pid: u32, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(pid) = find_ng_child(parent_pid) {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Polls `addr` with a plain TCP connect until it accepts one or `timeout`
/// elapses, then panics naming `addr`. Vite's first bundle (behind `ng
/// serve`) takes real wall-clock time before its port accepts connections.
fn wait_for_port(addr: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{addr} never started accepting connections"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A minimal blocking HTTP/1.1 GET, close-connection, over a raw
/// [`TcpStream`]: enough to prove a JSON body came back, without pulling in
/// an HTTP client dependency just for this one test.
fn http_get(host_port: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(host_port)
        .unwrap_or_else(|error| panic!("connecting to {host_port}: {error}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let host = host_port.split(':').next().unwrap_or("localhost");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .expect("write the request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");
    response
}

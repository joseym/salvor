//! A hermetic scripted model server for recording the demo.
//!
//! This is demo-support code behind the crate's default-on `fixture` feature,
//! not part of the `salvor` product binary. It exists for one reason: the demo
//! GIF must record without a network or an API key, deterministically, every
//! time. It serves the same twenty-turn scripted conversation as the
//! `demo_run` integration test (both read it from [`salvor_cli::demo_script`],
//! so the recording and the test that guards it cannot disagree), over HTTP,
//! speaking just enough of the Messages API for the demo agent to drive.
//!
//! The switch between this and the real model is one environment variable.
//! `demo/agent.toml` declares `base_url_env = "SALVOR_DEMO_BASE_URL"`; export
//! it pointing at this server and every model call lands here with no key,
//! unset it and export `ANTHROPIC_API_KEY` and the same file targets the
//! public endpoint.
//!
//! # What it speaks
//!
//! One route, `POST /v1/messages`. It counts the `messages` array in the
//! request body and returns the scripted response for that count (turn `k`
//! carries `2k - 1` messages; see [`salvor_cli::demo_script`]). An unmatched
//! count returns a `500` shaped like the API's error envelope, the same
//! contract the test's wiremock model uses, so a script or endpoint mismatch
//! fails loudly rather than hanging. Selection is stateless, which is what
//! lets one running server serve both the pre-kill run and the resume: the
//! resume's first live turn arrives with exactly the message count it would
//! have had uninterrupted.
//!
//! # The per-turn delay
//!
//! Each response is held for a configurable delay (default a few hundred
//! milliseconds) so the run is slow enough to watch and to `kill -9` on
//! camera. Because the delay sits on the *model* call and the demo's tool
//! calls are near-instant local file operations, a kill at an arbitrary wall
//! clock lands almost always while the process waits on a model call, which
//! is a recoverable crash (awaiting-model), never the one unrecoverable
//! window of a write tool call in flight.
//!
//! # Configuration
//!
//! Port and delay come from flags, then environment, then a default:
//! `--port` / `SALVOR_DEMO_MODEL_PORT` (default `8899`), and `--delay-ms` /
//! `SALVOR_DEMO_MODEL_DELAY_MS` (default `300`). Binding `--port 0` picks a
//! free port; the chosen port is printed so a caller can read it back.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use salvor_cli::demo_script;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// The default port, matching what `docs/demo.tape` points the agent at.
const DEFAULT_PORT: u16 = 8899;

/// The default per-turn delay in milliseconds: watchable, and long enough to
/// fire a `kill -9` while the run waits on a model call.
const DEFAULT_DELAY_MS: u64 = 300;

/// The resolved settings this server runs with.
struct Settings {
    port: u16,
    delay: Duration,
}

/// Resolves a `u64` setting from a flag, then an environment variable, then a
/// default. Panics with a clear message on an unparsable value, since a
/// mistyped port or delay should fail loudly at startup, not silently.
fn resolve_u64(args: &[String], flag: &str, env: &str, default: u64) -> u64 {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        let raw = args
            .get(index + 1)
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        return raw
            .parse()
            .unwrap_or_else(|_| panic!("{flag} value `{raw}` is not a number"));
    }
    if let Ok(raw) = std::env::var(env)
        && !raw.is_empty()
    {
        return raw
            .parse()
            .unwrap_or_else(|_| panic!("{env}=`{raw}` is not a number"));
    }
    default
}

/// Reads flags and environment into [`Settings`].
fn settings() -> Settings {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = resolve_u64(
        &args,
        "--port",
        "SALVOR_DEMO_MODEL_PORT",
        u64::from(DEFAULT_PORT),
    );
    let delay = resolve_u64(
        &args,
        "--delay-ms",
        "SALVOR_DEMO_MODEL_DELAY_MS",
        DEFAULT_DELAY_MS,
    );
    Settings {
        port: u16::try_from(port).expect("port fits in u16"),
        delay: Duration::from_millis(delay),
    }
}

/// The scripted response for a request whose body holds `count` messages, and
/// whether it was a scripted hit. Mirrors the wiremock model in the tests: a
/// hit is `200` with the response, a miss is `500` with an error envelope.
fn response_for(script: &[(usize, Value)], count: usize) -> (u16, Value) {
    for (expected, response) in script {
        if *expected == count {
            return (200, response.clone());
        }
    }
    (
        500,
        json!({
            "error": {
                "type": "demo_script",
                "message": format!("no scripted response for {count} messages")
            }
        }),
    )
}

/// Serves one keep-alive connection: read a request, answer it, repeat until
/// the client closes. reqwest (the client `salvor` uses) pools connections, so
/// a run's twenty requests can arrive on one socket; the loop handles that.
async fn serve_connection(
    stream: TcpStream,
    script: Arc<Vec<(usize, Value)>>,
    delay: Duration,
    requests: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        // The request line (e.g. `POST /v1/messages HTTP/1.1`). A zero-length
        // read is a clean client close: end the connection.
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await? == 0 {
            return Ok(());
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_owned();

        // Headers until the blank line; the only one we need is Content-Length.
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }

        // The JSON body, exactly Content-Length bytes.
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;

        let (status, payload) = if path == "/v1/messages" {
            let count = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("messages")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                })
                .unwrap_or(0);
            let nth = requests.fetch_add(1, Ordering::SeqCst) + 1;
            let (status, response) = response_for(&script, count);
            eprintln!(
                "[salvor-demo-model] request #{nth}: {count} messages -> {}",
                if status == 200 {
                    "scripted"
                } else {
                    "unscripted (500)"
                }
            );
            (status, response)
        } else {
            (
                404,
                json!({ "error": { "type": "not_found", "message": path } }),
            )
        };

        // The demo's watchable pace: hold every model answer for the delay.
        tokio::time::sleep(delay).await;

        let serialized = serde_json::to_vec(&payload).expect("response serializes");
        let reason = if status == 200 { "OK" } else { "Error" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             content-type: application/json\r\n\
             content-length: {}\r\n\
             connection: keep-alive\r\n\r\n",
            serialized.len()
        );
        write_half.write_all(head.as_bytes()).await?;
        write_half.write_all(&serialized).await?;
        write_half.flush().await?;
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let settings = settings();
    let script = Arc::new(demo_script::script());
    let requests = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind(("127.0.0.1", settings.port)).await?;
    let port = listener.local_addr()?.port();
    // Printed so a caller that bound port 0 can read the chosen port back.
    println!("salvor-demo-model listening on http://127.0.0.1:{port}");
    eprintln!(
        "[salvor-demo-model] serving {} scripted turns, {:?} per turn",
        script.len(),
        settings.delay
    );

    loop {
        let (stream, _) = listener.accept().await?;
        let script = script.clone();
        let requests = requests.clone();
        let delay = settings.delay;
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, script, delay, requests).await {
                eprintln!("[salvor-demo-model] connection ended: {error}");
            }
        });
    }
}

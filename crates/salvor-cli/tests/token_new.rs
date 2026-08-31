//! `salvor token new`, end to end through the real `salvor` binary.
//!
//! The mint-and-verify test spawns a second real `salvor serve --token-file`
//! process bound to a fixed loopback port in 18830-18839 (never the default
//! `127.0.0.1:8080`, and never an OS-assigned one, so this suite is
//! reproducible in a sandbox that only opens a narrow port range) and tears
//! it down through the `Child` handle's own recorded pid (`Child::kill`),
//! never a broad `pkill`.
//!
//! Every refusal this verb documents gets its own test: a missing file
//! without `--create`, a file readable by group or other, a name already
//! present, a name outside `[a-z0-9-]{1,64}`, and a `--stdin` value under the
//! 16-byte floor. The checksum a minted token carries is not checked anywhere
//! outside `mint` itself (verification hashes the whole string; see
//! `salvor_server::tokens`'s module docs), so there is no second place here
//! to prove a corrupted checksum is rejected. What IS tested, in
//! `salvor-server`'s own unit tests (`tokens::tests::mint_produces_the_documented_wire_shape`
//! and `..._almost_always_changes_the_checksum`), is the round-trip shape:
//! `mint`'s checksum is exactly what `checksum(payload)` recomputes, and a
//! one-character change to the payload changes it.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command as AssertCommand;
use tempfile::tempdir;

/// The `salvor` binary under test, located by Cargo.
const SALVOR_BIN: &str = env!("CARGO_BIN_EXE_salvor");

/// Fixed loopback ports this suite is allowed to bind, tried in order so a
/// leftover listener from an interrupted prior run does not wedge the whole
/// file. Never `8080`, and never `127.0.0.1:0`: both would leave the actual
/// bound address up to something other than this list.
const CANDIDATE_PORTS: [u16; 10] = [
    18830, 18831, 18832, 18833, 18834, 18835, 18836, 18837, 18838, 18839,
];

/// A fresh `assert_cmd` handle for `salvor token new`, with tracing quieted.
fn token_new(args: &[&str]) -> AssertCommand {
    let mut command = AssertCommand::cargo_bin("salvor").expect("salvor binary builds");
    command.args(["token", "new"]).args(args);
    command.env("RUST_LOG", "warn");
    command
}

/// Spawns `salvor serve --token-file <file>` on the first candidate port that
/// answers with its banner within a few seconds, and returns the child (kept
/// alive; kill it by its own pid when done) plus the bound address.
///
/// Modeled on `serve_auth.rs`'s `spawn_serve_with_token_set`: read stdout
/// until the banner line names the bound address, then keep draining stdout
/// in a background thread so the child never blocks on a full pipe.
fn spawn_serve_with_token_file(store: &Path, token_file: &Path) -> (Child, String) {
    for port in CANDIDATE_PORTS {
        let addr = format!("127.0.0.1:{port}");
        let mut child = Command::new(SALVOR_BIN)
            .args([
                "--store",
                store.to_str().unwrap(),
                "serve",
                "--bind",
                &addr,
                "--token-file",
                token_file.to_str().unwrap(),
            ])
            .env("RUST_LOG", "off")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn salvor serve --token-file");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let bound = loop {
            if Instant::now() >= deadline {
                break None;
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break None, // the process exited: this port was taken
                Ok(_) => {
                    let trimmed = line.trim();
                    if let Some(rest) =
                        trimmed.strip_prefix("salvor control plane listening on http://")
                    {
                        break Some(rest.to_owned());
                    }
                }
                Err(_) => break None,
            }
        };
        match bound {
            Some(addr) => {
                std::thread::spawn(move || {
                    let mut sink = String::new();
                    loop {
                        sink.clear();
                        if reader.read_line(&mut sink).unwrap_or(0) == 0 {
                            break;
                        }
                    }
                });
                return (child, addr);
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    panic!("no port in {CANDIDATE_PORTS:?} produced a banner");
}

/// A minimal blocking HTTP/1.1 GET, with an optional bearer token, over a raw
/// [`TcpStream`]. Returns the status code. Mirrors `serve_auth.rs`'s `get`.
fn get_status(addr: &str, path: &str, auth: Option<&str>) -> u16 {
    let mut stream = TcpStream::connect(addr).unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let auth_header = auth
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth_header}Connection: close\r\n\r\n"
    )
    .expect("write the request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");
    let text = String::from_utf8_lossy(&raw);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// The whole happy path: mint a token into a fresh, `--create`d file, then
/// prove it against a real server started with that exact file. A wrong
/// bearer and no bearer both still refuse; only the minted one lets a request
/// through.
#[test]
fn a_minted_token_verifies_against_a_live_server_started_with_the_file() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("tokens.toml");
    let store = dir.path().join("store.db");

    let output = token_new(&["ci", "--file", file.to_str().unwrap(), "--create"])
        .output()
        .expect("runs");
    assert!(
        output.status.success(),
        "mint exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(token.starts_with("sv_"), "{token}");
    assert_eq!(token.len(), 3 + 43 + 1 + 6, "{token}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(&token),
        "the token is never echoed anywhere but stdout: {stderr}"
    );
    assert!(
        stderr.contains("shown once"),
        "stderr says it will not be shown again: {stderr}"
    );

    let contents = fs::read_to_string(&file).expect("read the token file");
    assert!(contents.contains("[tokens.ci]"), "{contents}");
    assert!(
        !contents.contains(&token),
        "the file never carries the raw token: {contents}"
    );
    let expected_hash: String = salvor_server::tokens::digest(&token)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(
        contents.contains(&expected_hash),
        "the file carries the token's own SHA-256: {contents}"
    );

    #[cfg(unix)]
    assert_eq!(mode_of(&file), 0o600, "--create makes the file 0600");

    let (mut child, addr) = spawn_serve_with_token_file(&store, &file);

    assert_eq!(get_status(&addr, "/v1/runs", None), 401, "no bearer at all");
    assert_eq!(
        get_status(&addr, "/v1/runs", Some("sv_definitely_wrong")),
        401,
        "a wrong bearer"
    );
    assert_eq!(
        get_status(&addr, "/v1/runs", Some(&token)),
        200,
        "the minted token verifies against the server started with the same file"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_missing_file_without_create_is_refused_and_names_the_flag() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("tokens.toml");

    let output = token_new(&["ci", "--file", file.to_str().unwrap()])
        .output()
        .expect("runs");
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(stderr.contains("--create"), "{stderr}");
    assert!(!file.exists(), "nothing is created without --create");
}

#[test]
#[cfg(unix)]
fn a_file_readable_by_group_or_other_is_refused_with_the_servers_own_message() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("tokens.toml");
    fs::write(&file, "").expect("write");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod");
    }

    let output = token_new(&["ci", "--file", file.to_str().unwrap()])
        .output()
        .expect("runs");
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("0644"), "names the mode: {stderr}");
    assert!(
        stderr.contains("chmod 0600"),
        "says what to do, the server's own words: {stderr}"
    );
}

#[test]
fn a_name_already_present_is_refused() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("tokens.toml");
    fs::write(
        &file,
        format!("[tokens.ci]\nhash = \"{}\"\n", "a".repeat(64)),
    )
    .expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    let output = token_new(&["ci", "--file", file.to_str().unwrap()])
        .output()
        .expect("runs");
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already has a `ci` entry"), "{stderr}");
}

#[test]
fn a_name_outside_the_shape_is_refused() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("tokens.toml");

    for bad in [
        "CI",
        "has space",
        "under_score",
        "way-too-long-a-name-for-the-sixty-four-character-ceiling-right-here",
    ] {
        let output = token_new(&[bad, "--file", file.to_str().unwrap(), "--create"])
            .output()
            .expect("runs");
        assert!(!output.status.success(), "{bad}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not a valid token name"), "{bad}: {stderr}");
        assert!(!file.exists(), "{bad}: an invalid name creates nothing");
    }
}

#[test]
fn stdin_under_the_entropy_floor_is_refused_and_never_echoed() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("tokens.toml");

    let mut command = token_new(&[
        "ci",
        "--file",
        file.to_str().unwrap(),
        "--create",
        "--stdin",
    ]);
    command.write_stdin("short\n");
    let output = command.output().expect("runs");

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("5 bytes"), "{stderr}");
    assert!(stderr.contains("16"), "names the floor: {stderr}");
    assert!(!stderr.contains("short"), "never the value: {stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("short"), "never the value: {stdout}");
}

#[test]
fn stdin_at_or_over_the_floor_is_imported_and_hashed_not_minted() {
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("tokens.toml");
    let imported = "a-token-minted-elsewhere-with-plenty-of-bytes";

    let mut command = token_new(&[
        "elsewhere",
        "--file",
        file.to_str().unwrap(),
        "--create",
        "--stdin",
    ]);
    command.write_stdin(format!("{imported}\n"));
    let output = command.output().expect("runs");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert_eq!(printed, imported, "the imported value comes back verbatim");

    let contents = fs::read_to_string(&file).expect("read");
    let expected_hash: String = salvor_server::tokens::digest(imported)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(contents.contains(&expected_hash), "{contents}");
    assert!(!contents.contains(imported), "{contents}");
}

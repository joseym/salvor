//! What becomes of an MCP server process when the client that started it goes
//! away.
//!
//! The field report these tests answer: an MCP server child blocked in a write
//! syscall survived a `kill -9` of the salvor process, reparented to init, and
//! sat there still holding the run's tool call. The two mechanisms that usually
//! clean a server up on their own had both been defeated, and neither is
//! something a client can rely on: a server only sees EOF on stdin if it reads
//! stdin again, and only dies of `SIGPIPE` if it writes to stdout again.
//!
//! So the fixture server grows a `SALVOR_MCP_FIXTURE_STUBBORN` mode that does
//! neither and ignores every catchable signal besides, and these tests ask, of
//! each way a connection can end, whether the process is actually gone
//! afterwards. `SALVOR_MCP_FIXTURE_GRANDCHILD` adds the other half of the
//! question: a real server usually *is* a launcher, and a kill aimed at the one
//! pid the client tracks leaves the process doing the actual work running.
//!
//! Unix only. Every assertion here is about process groups and signals, and the
//! Windows story is a different mechanism (a job object) that Salvor does not
//! ship a spawn path for.
//!
//! Nothing here kills a pid it did not record itself.

#![cfg(all(feature = "mcp", unix))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use salvor_tools::mcp::{EffectOverrides, IdempotencyKeys, McpServer};
use tokio::process::Command;

/// How long these tests wait for something to *start*: a fixture to write its
/// pid file, a stand-in parent to report a live session.
///
/// Absurdly generous, and the two deadlines are separate for a reason worth
/// writing down, because collapsing them into one is what made this file flaky
/// the first time. A startup wait costs nothing when things are working (it
/// ends the instant the condition holds) and its only job is to turn "the
/// process never started" into a named failure instead of a hang. Spawning a
/// debug-built process, running an MCP handshake, and listing tools was
/// measured taking over ninety seconds on a machine that was busy compiling the
/// rest of the workspace, so a budget tight enough to expire there reports a
/// lifecycle bug that is not present. Minutes here buy nothing but honesty.
const START_DEADLINE: Duration = Duration::from_secs(300);

/// How long these tests wait for a process to *go away* after something that
/// should have ended it.
///
/// Tight, and deliberately so: this one is a real assertion about the code
/// under test, not a tolerance for a slow machine. The teardown it bounds
/// closes the child's stdin, waits about three seconds for a voluntary exit,
/// then kills the process group, so anything past this is a failure to reap.
/// It is also what the platform-split test below spends proving the macOS
/// residual, which is why it is not simply set to a minute.
const REAP_DEADLINE: Duration = Duration::from_secs(15);

/// Whether a process with this pid exists, including as an unreaped zombie.
///
/// `kill` with signal 0 performs the permission and existence checks and sends
/// nothing, which is the portable way to ask. A zombie still answers yes, so a
/// test that asserts a process is gone is also asserting somebody reaped it,
/// which is the stronger and more useful claim.
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 delivers nothing; the call takes no pointers and cannot
    // affect this process. A pid this test recorded itself is the only input.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Sends `SIGKILL` to one pid, ignoring the result.
///
/// Cleanup only, and only ever against a pid a test recorded. A failure here
/// means the process was already gone, which is the outcome cleanup wanted.
fn kill_now(pid: u32) {
    // SAFETY: same as `alive`, with a real signal. `SIGKILL` to a pid this test
    // spawned and recorded.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// Blocks until `path` holds a pid, or the deadline passes.
///
/// The fixture writes its pid files before it starts serving, so in practice
/// this returns on the first or second poll; the loop exists for the case where
/// the process failed to start at all, where a timeout with a named path is a
/// far better report than a parse error on an empty file.
async fn wait_for_pid(path: &Path) -> u32 {
    let start = Instant::now();
    while start.elapsed() < START_DEADLINE {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "no pid was written to {} within the deadline",
        path.display()
    );
}

/// Blocks until the process is gone, then reports whether it made it.
///
/// Returns rather than asserts so a caller can say what the failure means in
/// its own words, and so the platform-split test below can use the same wait
/// for the case where survival is the documented outcome.
async fn wait_until_gone(pid: u32) -> bool {
    let start = Instant::now();
    while start.elapsed() < REAP_DEADLINE {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    !alive(pid)
}

/// Blocks until the stand-in parent reports its MCP session is live.
///
/// It prints one line, `ready`, after `McpServer::connect` returns. Waiting for
/// that rather than for a duration is what keeps the kill below landing on an
/// established connection instead of somewhere inside the handshake.
async fn wait_for_ready(parent: &mut tokio::process::Child) {
    use tokio::io::AsyncBufReadExt;

    let stdout = parent.stdout.take().expect("the parent's stdout is piped");
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let line = tokio::time::timeout(START_DEADLINE, lines.next_line())
        .await
        .expect("the parent reports readiness within the deadline")
        .expect("reading the parent's stdout");
    assert_eq!(
        line.as_deref(),
        Some("ready"),
        "the stand-in parent did not report a live session"
    );
}

/// A temporary directory plus the pid-file paths inside it that the fixture
/// writes. Held by each test so the directory outlives the child.
struct PidFiles {
    _dir: tempfile::TempDir,
    server: PathBuf,
    grandchild: PathBuf,
}

impl PidFiles {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp directory for the fixture's pid files");
        let server = dir.path().join("server.pid");
        let grandchild = dir.path().join("grandchild.pid");
        Self {
            _dir: dir,
            server,
            grandchild,
        }
    }
}

/// A command that launches the fixture in stubborn mode, recording its own pid
/// and (when `grandchild` is set) the pid of a subprocess it starts.
fn stubborn_fixture(pids: &PidFiles, grandchild: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_salvor-mcp-fixture"));
    command
        .env("SALVOR_MCP_FIXTURE_STUBBORN", "1")
        .env("SALVOR_MCP_FIXTURE_PIDFILE", &pids.server);
    if grandchild {
        command.env("SALVOR_MCP_FIXTURE_GRANDCHILD", &pids.grandchild);
    }
    command
}

// --- The controlled paths -----------------------------------------------

/// `close` is the tidy shutdown, and the one an operator's `salvor run`
/// actually takes: every CLI verb that builds MCP servers runs `close_servers`
/// on the way out, whatever the run's outcome was.
///
/// The server it closes here reads its stdin only until the session ends and
/// then never again, so the usual "the client went away, I saw EOF, I left"
/// exit is unavailable to it, and it ignores `SIGTERM` besides. If `close`
/// returns and the process is still there, an operator's finished run has left
/// something behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_reaps_a_server_that_will_not_leave_on_its_own() {
    let pids = PidFiles::new();
    let server = McpServer::connect(
        stubborn_fixture(&pids, false),
        &EffectOverrides::new(),
        &IdempotencyKeys::new(),
    )
    .await
    .expect("the stubborn fixture still serves a normal MCP session");

    let pid = wait_for_pid(&pids.server).await;
    assert!(
        alive(pid),
        "the server is running while the session is live"
    );

    server.close().await.expect("the session closes cleanly");

    let gone = wait_until_gone(pid).await;
    if !gone {
        kill_now(pid);
    }
    assert!(
        gone,
        "pid {pid} outlived the close that was supposed to end it"
    );
}

/// The same guarantee on the path nobody chooses: a handle dropped without
/// `close`, which is what an error unwinding past it produces.
///
/// The teardown is asynchronous here (rmcp reaps the child from a task it
/// spawns on drop) so this waits rather than asserting immediately, but the
/// outcome must be the same. A `?` on the way out of a build step must not be
/// the difference between a reaped server and a stray one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_reaps_a_server_that_will_not_leave_on_its_own() {
    let pids = PidFiles::new();
    let server = McpServer::connect(
        stubborn_fixture(&pids, false),
        &EffectOverrides::new(),
        &IdempotencyKeys::new(),
    )
    .await
    .expect("the stubborn fixture still serves a normal MCP session");

    let pid = wait_for_pid(&pids.server).await;
    assert!(
        alive(pid),
        "the server is running while the session is live"
    );

    drop(server);

    let gone = wait_until_gone(pid).await;
    if !gone {
        kill_now(pid);
    }
    assert!(
        gone,
        "pid {pid} outlived the drop that was supposed to end it"
    );
}

/// The reason the child is spawned into a process group of its own: a real MCP
/// server is very often a launcher, and the process doing the work is its
/// child, not the pid the client holds.
///
/// The fixture stands in for that with a plain `sleep`, which is exactly the
/// shape of the case that matters: it does not share the server's stdio, so no
/// pipe closing anywhere reaches it, and it would happily outlive everything.
/// Killing the group reaches it; killing the one tracked pid does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_reaps_a_subprocess_the_server_started() {
    let pids = PidFiles::new();
    let server = McpServer::connect(
        stubborn_fixture(&pids, true),
        &EffectOverrides::new(),
        &IdempotencyKeys::new(),
    )
    .await
    .expect("the stubborn fixture still serves a normal MCP session");

    let server_pid = wait_for_pid(&pids.server).await;
    let grandchild_pid = wait_for_pid(&pids.grandchild).await;
    assert!(
        alive(grandchild_pid),
        "the server's own subprocess is running"
    );

    server.close().await.expect("the session closes cleanly");

    let server_gone = wait_until_gone(server_pid).await;
    let grandchild_gone = wait_until_gone(grandchild_pid).await;
    if !server_gone {
        kill_now(server_pid);
    }
    if !grandchild_gone {
        kill_now(grandchild_pid);
    }
    assert!(server_gone, "the server (pid {server_pid}) outlived close");
    assert!(
        grandchild_gone,
        "the server is gone but its subprocess (pid {grandchild_pid}) is still running: \
         the kill reached one pid instead of the group"
    );
}

// --- The uncontrolled path: the field report itself ----------------------

/// The reported scenario, reproduced: a parent holding a live MCP session is
/// killed with `SIGKILL`, so no code of ours runs, and the question is whether
/// the server is still there afterwards.
///
/// The parent is `salvor-mcp-parent`, which connects through the real
/// `McpServer::connect` and then does nothing at all, so whatever happens to
/// the server is what the spawn arranged, not what a shutdown path did.
///
/// **The assertion is deliberately different per platform, because the truth
/// is.** On Linux the child arms `PR_SET_PDEATHSIG` for itself between `fork`
/// and `exec`, so the kernel kills it the moment its parent dies, and this
/// asserts it is gone. On macOS there is no parent-death signal and no code
/// runs after `SIGKILL`, so nothing can reap it from this side; the server
/// survives, and this pins that as the documented residual rather than
/// pretending otherwise. If a future macOS ever gains the capability, this
/// test fails and someone gets to delete a paragraph of `SECURITY.md`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_sigkill_reaps_the_server_on_linux_only() {
    let pids = PidFiles::new();

    let mut parent = Command::new(env!("CARGO_BIN_EXE_salvor-mcp-parent"))
        .arg(env!("CARGO_BIN_EXE_salvor-mcp-fixture"))
        .env("SALVOR_MCP_FIXTURE_STUBBORN", "1")
        .env("SALVOR_MCP_FIXTURE_PIDFILE", &pids.server)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the stand-in parent starts");

    // Wait for the session, not just for the process. The fixture writes its
    // pid file before it starts serving, so killing on the pid file alone would
    // land during the initialize handshake and prove nothing about a *live*
    // connection. The parent prints `ready` once `connect` has returned.
    wait_for_ready(&mut parent).await;

    let server_pid = wait_for_pid(&pids.server).await;
    assert!(alive(server_pid), "the server is running under its parent");

    // `SIGKILL`, not the child's own kill path: this is the crash, and the
    // whole point is that the parent gets no chance to tidy up.
    let parent_pid = parent.id().expect("the parent has not been reaped yet");
    kill_now(parent_pid);
    parent.wait().await.expect("reap the killed parent");

    let gone = wait_until_gone(server_pid).await;
    // Recorded pid, and the test's own responsibility either way: on the
    // platform where it survives, this is what stops the suite leaking a
    // process that ignores every signal but this one.
    if !gone {
        kill_now(server_pid);
    }

    #[cfg(target_os = "linux")]
    assert!(
        gone,
        "pid {server_pid} survived its parent's SIGKILL; the parent-death signal did not fire"
    );

    #[cfg(not(target_os = "linux"))]
    assert!(
        !gone,
        "pid {server_pid} did not survive its parent's SIGKILL on a platform with no \
         parent-death signal. That is better than documented; update the macOS residual \
         in SECURITY.md and the mcp module docs, then make this test assert the good news."
    );
}

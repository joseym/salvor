//! `salvor serve --kill`: find a running `salvor serve` and stop it.
//!
//! There are no pid files (`serve` stays a plain foreground process, and a
//! crashed server must never leave a stale file for this to trip over), so
//! discovery works by inspecting the live process table instead: every
//! process whose executable is named exactly `salvor` and whose argv runs the
//! `serve` verb, other than this process itself. `--bind` and `--store` are
//! recovered from that same argv, falling back to the CLI's own defaults
//! (`127.0.0.1:8080`, `./salvor.db`) when a flag was not given explicitly,
//! since that is what the target process itself would be using.
//!
//! [`sysinfo`] does the enumeration and the actual signal delivery on all
//! three platforms the dist pipeline ships (macOS, Linux, Windows), so there
//! is one code path here rather than a Unix `ps` parser plus a Windows stub.

use std::io::{self, IsTerminal, Write as _};
use std::time::{Duration, Instant};

use anyhow::Result;
use sysinfo::{
    Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, RefreshKind, Signal, System,
};

use crate::render;

/// The default bind address `salvor serve` uses when `--bind` is not given
/// (mirrors [`crate::cli::ServeArgs::bind`]'s `default_value`).
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// The default store path `salvor` uses when `--store` is not given (mirrors
/// [`crate::cli::Cli::store`]'s `default_value`).
pub const DEFAULT_STORE: &str = "./salvor.db";

/// How long `--kill` waits for a SIGTERM'd server to exit before it reports the process as still
/// running. Generous rather than tight: a graceful shutdown finishes in milliseconds when the
/// machine is idle, but the same shutdown on a loaded machine (a CI runner, a busy laptop) can take
/// seconds, and reporting failure for a shutdown that is merely slow sends the operator to `kill -9`
/// for no reason.
const TERMINATE_GRACE: Duration = Duration::from_secs(10);

/// One discovered `salvor serve` process: its pid, and the `--bind` / `--store`
/// it was launched with (recovered from argv, defaults substituted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningServer {
    /// The process id.
    pub pid: u32,
    /// The bind address, e.g. `127.0.0.1:8080`.
    pub bind: String,
    /// The event store path, e.g. `./salvor.db`.
    pub store: String,
}

/// How a termination attempt landed.
enum TerminateOutcome {
    /// The process was gone before this ran (it had already exited).
    AlreadyGone,
    /// The signal was sent and the process exited within the grace period.
    Exited,
    /// The signal was sent but the process was still alive after the grace
    /// period. Not escalated automatically; the caller reports the pid so a
    /// human can decide to force it.
    StillRunning,
    /// This platform has no way to deliver the requested signal.
    NotSupported,
    /// The signal could not be delivered (permissions, or the OS refused).
    Failed,
}

/// `salvor serve --kill [target]`: the whole flow, called instead of binding
/// and serving. `target` is the flag's optional value: `None` when the flag
/// carried no argument (`--kill` alone), `Some` for a direct `--kill <pid or
/// port>`.
pub async fn run(target: Option<&str>) -> Result<u8> {
    let servers = discover(std::process::id());

    if servers.is_empty() {
        println!("no salvor serve is running");
        return Ok(0);
    }

    if let Some(target) = target {
        return kill_direct(target, &servers).await;
    }

    match servers.as_slice() {
        [] => unreachable!("checked non-empty above"),
        [only] => kill_one(only).await,
        many => kill_interactive(many).await,
    }
}

/// Finds every `salvor serve` process besides `exclude_pid` (this process's
/// own pid, so `--kill` never targets itself), sorted by pid for a stable,
/// readable table.
fn discover(exclude_pid: u32) -> Vec<RunningServer> {
    let specifics = ProcessRefreshKind::nothing()
        .with_cmd(sysinfo::UpdateKind::Always)
        .with_exe(sysinfo::UpdateKind::Always);
    let system = System::new_with_specifics(RefreshKind::nothing().with_processes(specifics));

    let mut servers: Vec<RunningServer> = system
        .processes()
        .values()
        .filter(|process| process.pid().as_u32() != exclude_pid)
        .filter(|process| is_salvor_binary(process))
        .filter_map(|process| {
            let argv = argv_of(process);
            if !argv_runs_serve(&argv) {
                return None;
            }
            let (bind, store) = bind_and_store_from_argv(&argv);
            Some(RunningServer {
                pid: process.pid().as_u32(),
                bind,
                store,
            })
        })
        .collect();
    servers.sort_by_key(|server| server.pid);
    servers
}

/// True when `process`'s executable is named exactly `salvor` (`salvor.exe`
/// on Windows), the release binary this repo ships, regardless of install
/// location (`target/debug`, `~/.cargo/bin`, a dist install). Falls back to
/// the reported process name when the executable path is unavailable (a
/// process this one cannot introspect, on some platforms).
fn is_salvor_binary(process: &Process) -> bool {
    if let Some(exe) = process.exe() {
        return exe.file_stem().and_then(|stem| stem.to_str()) == Some("salvor");
    }
    process.name().to_str() == Some("salvor")
}

/// `process.cmd()` as owned `String`s, lossily: argv is expected to be plain
/// ASCII flags and paths, and a lossy re-encode of anything stranger still
/// lets pid-based discovery work even if bind/store recovery degrades.
fn argv_of(process: &Process) -> Vec<String> {
    process
        .cmd()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// True when `argv` (a process's full command line, program name at index 0)
/// runs the `serve` subcommand: the bare token `serve` appears somewhere
/// after the program name. Global flags like `--store` may sit on either
/// side of it (`--store` is `global = true` in the clap tree), so this looks
/// for the token itself rather than a fixed position.
fn argv_runs_serve(argv: &[String]) -> bool {
    argv.iter().skip(1).any(|arg| arg == "serve")
}

/// Recovers the `--bind` and `--store` a `serve` process was launched with
/// from its argv, substituting the CLI's own defaults for whichever flag is
/// absent. Handles both `--flag value` and `--flag=value`, the two forms
/// clap accepts.
fn bind_and_store_from_argv(argv: &[String]) -> (String, String) {
    let bind = find_flag_value(argv, "--bind").unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let store = find_flag_value(argv, "--store").unwrap_or_else(|| DEFAULT_STORE.to_owned());
    (bind, store)
}

/// Finds the value of `flag` in `argv`, as either `--flag value` (two
/// tokens) or `--flag=value` (one token). Returns the first match.
fn find_flag_value(argv: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_owned());
        }
        if arg == flag {
            return iter.next().cloned();
        }
    }
    None
}

/// Finds the discovered server a `--kill <target>` value names: tried first
/// as a pid (exact match), then as the port segment of a `host:port` bind
/// address. `None` when the target parses as neither, or matches nothing.
fn find_target<'a>(target: &str, servers: &'a [RunningServer]) -> Option<&'a RunningServer> {
    let number: u32 = target.parse().ok()?;
    if let Some(server) = servers.iter().find(|server| server.pid == number) {
        return Some(server);
    }
    let port = u16::try_from(number).ok()?;
    servers
        .iter()
        .find(|server| bind_port(&server.bind) == Some(port))
}

/// The port segment of a `host:port` bind address.
fn bind_port(bind: &str) -> Option<u16> {
    bind.rsplit_once(':')?.1.parse().ok()
}

/// `--kill <target>`: kill exactly the named server, with no prompt.
async fn kill_direct(target: &str, servers: &[RunningServer]) -> Result<u8> {
    match find_target(target, servers) {
        Some(server) => kill_one(server).await,
        None => {
            eprintln!(
                "`{target}` matches no running salvor serve (not a discovered pid or listening port)"
            );
            Ok(1)
        }
    }
}

/// Multiple servers, `--kill` given no target: print the numbered table and
/// prompt on stdin. Piped/non-interactive stdin refuses rather than guessing.
async fn kill_interactive(servers: &[RunningServer]) -> Result<u8> {
    print!("{}", render::server_table(servers));

    if !io::stdin().is_terminal() {
        println!(
            "stdin is not a terminal, so salvor will not guess which to kill. \
             Re-run with an explicit target: salvor serve --kill <pid-or-port>"
        );
        return Ok(1);
    }

    print!("kill which? [1-{}, a for all, q to quit] ", servers.len());
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let choice = line.trim();

    match choice {
        "" | "q" | "Q" => {
            println!("nothing killed");
            Ok(0)
        }
        "a" | "A" => {
            let mut code = 0u8;
            for server in servers {
                let this = kill_one(server).await?;
                code = code.max(this);
            }
            Ok(code)
        }
        _ => match choice.parse::<usize>() {
            Ok(n) if n >= 1 && n <= servers.len() => kill_one(&servers[n - 1]).await,
            _ => {
                eprintln!(
                    "`{choice}` is not 1-{}, `a`, or `q`; nothing killed",
                    servers.len()
                );
                Ok(1)
            }
        },
    }
}

/// Kills exactly one server: SIGTERM, then up to ~2s waiting for it to exit.
/// Never escalates to SIGKILL on its own; a still-running process is reported
/// with its pid so a human can decide to force it.
async fn kill_one(server: &RunningServer) -> Result<u8> {
    match terminate(server.pid).await {
        TerminateOutcome::AlreadyGone => {
            println!(
                "pid {} (bind {}, store {}) had already exited",
                server.pid, server.bind, server.store
            );
            Ok(0)
        }
        TerminateOutcome::Exited => {
            println!(
                "killed pid {} (bind {}, store {})",
                server.pid, server.bind, server.store
            );
            Ok(0)
        }
        TerminateOutcome::StillRunning => {
            println!(
                "sent SIGTERM to pid {} (bind {}, store {}), but it had not exited after {secs}s. \
                 It may still be shutting down. To force it: kill -9 {pid}",
                server.pid,
                server.bind,
                server.store,
                secs = TERMINATE_GRACE.as_secs(),
                pid = server.pid,
            );
            Ok(1)
        }
        TerminateOutcome::NotSupported => {
            eprintln!(
                "cannot send a termination signal to pid {} on this platform",
                server.pid
            );
            Ok(1)
        }
        TerminateOutcome::Failed => {
            eprintln!(
                "failed to kill pid {} (bind {}, store {})",
                server.pid, server.bind, server.store
            );
            Ok(1)
        }
    }
}

/// Sends `Signal::Term` to `pid` and polls for up to ~2s for it to exit. A
/// fresh, narrowly-scoped [`System`] is built here (rather than reusing the
/// one from [`discover`]) so the process handle this kills is current even if
/// time passed between listing and this call (an interactive prompt, for
/// instance), and so a pid that has already been recycled by the OS is not
/// mistaken for the one just discovered.
async fn terminate(pid: u32) -> TerminateOutcome {
    let target = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let Some(process) = system.process(target) else {
        return TerminateOutcome::AlreadyGone;
    };

    match process.kill_with(Signal::Term) {
        None => return TerminateOutcome::NotSupported,
        Some(false) => return TerminateOutcome::Failed,
        Some(true) => {}
    }

    let deadline = Instant::now() + TERMINATE_GRACE;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[target]),
            true,
            ProcessRefreshKind::nothing(),
        );
        // Gone from the table, or still in it as a zombie: both mean the server itself has stopped.
        // A process that exits keeps its slot until its parent reaps it, so when the caller of
        // `--kill` is also the server's parent, the entry outlives the shutdown and only the status
        // distinguishes "finished, unreaped" from "still serving".
        match system.process(target) {
            None => return TerminateOutcome::Exited,
            Some(p) if matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead) => {
                return TerminateOutcome::Exited;
            }
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            return TerminateOutcome::StillRunning;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bind_and_store_when_both_given_as_two_tokens() {
        let argv = vec![
            "salvor".to_owned(),
            "--store".to_owned(),
            "/tmp/my.db".to_owned(),
            "serve".to_owned(),
            "--bind".to_owned(),
            "0.0.0.0:9000".to_owned(),
        ];
        assert_eq!(
            bind_and_store_from_argv(&argv),
            ("0.0.0.0:9000".to_owned(), "/tmp/my.db".to_owned())
        );
    }

    #[test]
    fn extracts_bind_and_store_given_as_equals_form() {
        let argv = vec![
            "salvor".to_owned(),
            "serve".to_owned(),
            "--bind=0.0.0.0:9000".to_owned(),
            "--store=/tmp/my.db".to_owned(),
        ];
        assert_eq!(
            bind_and_store_from_argv(&argv),
            ("0.0.0.0:9000".to_owned(), "/tmp/my.db".to_owned())
        );
    }

    #[test]
    fn defaults_bind_when_absent() {
        let argv = vec![
            "salvor".to_owned(),
            "--store".to_owned(),
            "/tmp/my.db".to_owned(),
            "serve".to_owned(),
        ];
        assert_eq!(
            bind_and_store_from_argv(&argv),
            (DEFAULT_BIND.to_owned(), "/tmp/my.db".to_owned())
        );
    }

    #[test]
    fn defaults_store_when_absent() {
        let argv = vec![
            "salvor".to_owned(),
            "serve".to_owned(),
            "--bind".to_owned(),
            "0.0.0.0:9000".to_owned(),
        ];
        assert_eq!(
            bind_and_store_from_argv(&argv),
            ("0.0.0.0:9000".to_owned(), DEFAULT_STORE.to_owned())
        );
    }

    #[test]
    fn defaults_both_when_neither_given() {
        let argv = vec!["salvor".to_owned(), "serve".to_owned()];
        assert_eq!(
            bind_and_store_from_argv(&argv),
            (DEFAULT_BIND.to_owned(), DEFAULT_STORE.to_owned())
        );
    }

    #[test]
    fn global_store_flag_recognized_after_the_verb_too() {
        // clap's `global = true` lets --store appear on either side of the
        // subcommand; discovery must not assume it always comes first.
        let argv = vec![
            "salvor".to_owned(),
            "serve".to_owned(),
            "--store".to_owned(),
            "/tmp/after.db".to_owned(),
        ];
        assert_eq!(
            bind_and_store_from_argv(&argv),
            (DEFAULT_BIND.to_owned(), "/tmp/after.db".to_owned())
        );
    }

    #[test]
    fn recognizes_serve_verb_regardless_of_flag_order() {
        assert!(argv_runs_serve(&[
            "salvor".to_owned(),
            "--store".to_owned(),
            "x.db".to_owned(),
            "serve".to_owned(),
        ]));
        assert!(argv_runs_serve(&["salvor".to_owned(), "serve".to_owned()]));
    }

    #[test]
    fn does_not_mistake_other_verbs_for_serve() {
        assert!(!argv_runs_serve(&[
            "salvor".to_owned(),
            "run".to_owned(),
            "--agent".to_owned(),
            "a.toml".to_owned(),
        ]));
        assert!(!argv_runs_serve(&["salvor".to_owned(), "list".to_owned()]));
    }

    fn sample_servers() -> Vec<RunningServer> {
        vec![
            RunningServer {
                pid: 111,
                bind: "127.0.0.1:8080".to_owned(),
                store: "./a.db".to_owned(),
            },
            RunningServer {
                pid: 222,
                bind: "0.0.0.0:9090".to_owned(),
                store: "./b.db".to_owned(),
            },
        ]
    }

    #[test]
    fn matches_target_by_pid() {
        let servers = sample_servers();
        assert_eq!(find_target("222", &servers), Some(&servers[1]));
    }

    #[test]
    fn matches_target_by_port_when_no_pid_matches() {
        let servers = sample_servers();
        assert_eq!(find_target("9090", &servers), Some(&servers[1]));
    }

    #[test]
    fn pid_match_wins_over_port_match() {
        // A server listening on the same numeric port as another's pid: the
        // pid match is checked first and wins.
        let servers = vec![RunningServer {
            pid: 9090,
            bind: "127.0.0.1:1".to_owned(),
            store: "./a.db".to_owned(),
        }];
        assert_eq!(find_target("9090", &servers), Some(&servers[0]));
    }

    #[test]
    fn no_match_returns_none() {
        let servers = sample_servers();
        assert_eq!(find_target("333", &servers), None);
    }

    #[test]
    fn non_numeric_target_returns_none() {
        let servers = sample_servers();
        assert_eq!(find_target("abc", &servers), None);
    }
}

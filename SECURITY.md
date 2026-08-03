# Security

## Reporting a vulnerability

Report privately through GitHub's [security advisory form](https://github.com/joseym/salvor/security/advisories/new), or by email to me@joseymorton.com. Please do not open a public issue for a vulnerability.

This is a single-maintainer project, so expect an acknowledgement within a week rather than within hours. If a report is confirmed, the fix and an advisory go out together, and you get credit unless you ask otherwise.

Salvor is pre-1.0 and has no long-term support branches. Fixes land on the latest release; there is no backporting to older tags.

## What Salvor assumes about its environment

Knowing where the trust boundaries are will tell you whether a finding is a vulnerability or the documented posture.

**The control plane is single-tenant.** `salvor serve` takes one optional shared-secret bearer token. There are no users, roles, or per-run access control: anyone who can reach the port and present the token can read and drive every run in the store. Do not expose it directly to the internet or to a group you would not give the whole database to.

**The store is not encrypted.** Events are SQLite rows. Anything a run recorded, including tool inputs and outputs, is readable by anything that can read the file.

**The event log is tamper-evident, not tamper-proof.** Every recorded row carries a SHA-256 hash over the exact stored envelope bytes chained to the hash of the row before it, per run, and `read_log` recomputes the whole chain before it returns a single event. Modifying, reordering, splicing into, or removing rows of a run that was already recorded is refused on read with a typed error naming the run and the position, including when the replacement is perfectly valid JSON. The chain definition is normative and written down in the `salvor_store::chain` rustdoc, so anyone with the database can recompute it independently. The `events` table also carries triggers that refuse `UPDATE` and `DELETE`; treat those as a locked door on a building with windows, since anyone who can write the file can drop a trigger. The chain is the evidence, not the triggers.

Three limits are worth stating plainly. First, the chain is unkeyed, so everything the verifier uses sits in the database next to the rows. An attacker who can write the database and knows the scheme can therefore rewrite an entire run from its first event forward, recomputing every hash and the recorded head, or extend a run with fabricated events chained correctly onto its current head, and nothing inside the store can tell either apart from the real thing. What the chain does guarantee unconditionally is that recorded history cannot be quietly revised: changing, reordering, splicing into, or dropping rows from a run that already exists is refused on read. Closing the other two cases needs an anchor the attacker does not control: a signature over a run's head hash under a key the store does not hold, or that head hash published somewhere append-only. Salvor does not ship either today; the head hash per run is the single value such an anchor would attach to. Second, a store created before this scheme has its chain backfilled the first time it is opened by a current binary. That backfill proves nothing changed after the migration and cannot say whether something had already been changed before it. Third, the chain proves the bytes are unchanged, never who wrote them; authorship is a question for whatever access control sits in front of the store.

A tamper-evident log is also only as good as what reads it. `read_log` is the enforcement point, so anything that reaches around it and reads the `events` table directly gets no verification and should not be treated as an audit path.

**Prompt recording writes prompts to disk.** It is off by default. Turning it on records the exact model request body into the durable log, so any secret or personal data in a prompt lands in the store and in anything that reads the store. That is the documented consequence, not a bug.

**Tools are as trusted as the process.** A native tool runs with the runtime's privileges. Tools reached over MCP run wherever you started that server. The sandboxed path is `salvor-wasm`, which runs WebAssembly component tools under wasmtime with WASI capabilities denied by default; that is the only place untrusted tool code belongs.

**An MCP server over stdio is a child process, and on macOS one can outlive a `kill -9`.** Salvor starts each stdio MCP server as the leader of its own process group with kill-on-drop set, so every shutdown it gets to run (a finished or failed run, a closed or dropped connection) kills the server and anything the server started, whether or not the server cooperates. On Linux the child also asks the kernel for `SIGKILL` on its parent's death, which covers the case where Salvor runs no code at all: `kill -9`, or any signal the process does not handle. macOS has no equivalent, so a Salvor process killed that way cannot reap its servers, and what happens next is up to the server. Most exit on their own, because their stdout pipe now has no reader and their next write to it raises `SIGPIPE`, or because their next read of stdin returns EOF. A server that does neither, one blocked writing somewhere else or looping without touching its stdio, keeps running, reparented to init, still holding whatever the run asked it to do. Clean it up with `kill -TERM -<pid>` against the server's process group. This is a documented limit of the platform, not a defect to report; on Linux the same `kill -9` leaves nothing behind, apart from processes the server itself started.

**Replay is pure by construction.** Replaying a log performs no IO and makes no network calls. A log that causes live calls during replay is a real bug, and an interesting one, so please report it.

## What is in scope

Anything that breaks a stated guarantee: a replay that executes side effects, a resume that repeats a recorded write, a client-driven append that the server accepts when the fold says it is not a legal next event, an auth bypass on the bearer token, a sandbox escape from a wasm tool, or a way to change a recorded event that `read_log` then serves as if it were history.

Findings that amount to "the documented posture is permissive" (no multi-tenancy, unencrypted store, trusted native tools, no external anchor over the hash chain, an MCP server surviving a `kill -9` on a platform with no parent-death signal) are not vulnerabilities, though a suggestion for how to tighten them is welcome as an issue.

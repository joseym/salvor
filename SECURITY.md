# Security

## Reporting a vulnerability

Report privately through GitHub's [security advisory form](https://github.com/joseym/salvor/security/advisories/new), or by email to me@joseymorton.com. Please do not open a public issue for a vulnerability.

This is a single-maintainer project, so expect an acknowledgement within a week rather than within hours. If a report is confirmed, the fix and an advisory go out together, and you get credit unless you ask otherwise.

Salvor is pre-1.0 and has no long-term support branches. Fixes land on the latest release; there is no backporting to older tags.

## What Salvor assumes about its environment

Knowing where the trust boundaries are will tell you whether a finding is a vulnerability or the documented posture.

**The control plane is single-tenant.** `salvor serve` takes one optional shared-secret bearer token. There are no users, roles, or per-run access control: anyone who can reach the port and present the token can read and drive every run in the store. Do not expose it directly to the internet or to a group you would not give the whole database to.

**The store is not encrypted.** Events are SQLite rows. Anything a run recorded, including tool inputs and outputs, is readable by anything that can read the file.

**Prompt recording writes prompts to disk.** It is off by default. Turning it on records the exact model request body into the durable log, so any secret or personal data in a prompt lands in the store and in anything that reads the store. That is the documented consequence, not a bug.

**Tools are as trusted as the process.** A native tool runs with the runtime's privileges. Tools reached over MCP run wherever you started that server. The sandboxed path is `salvor-wasm`, which runs WebAssembly component tools under wasmtime with WASI capabilities denied by default; that is the only place untrusted tool code belongs.

**Replay is pure by construction.** Replaying a log performs no IO and makes no network calls. A log that causes live calls during replay is a real bug, and an interesting one, so please report it.

## What is in scope

Anything that breaks a stated guarantee: a replay that executes side effects, a resume that repeats a recorded write, a client-driven append that the server accepts when the fold says it is not a legal next event, an auth bypass on the bearer token, or a sandbox escape from a wasm tool.

Findings that amount to "the documented posture is permissive" (no multi-tenancy, unencrypted store, trusted native tools) are not vulnerabilities, though a suggestion for how to tighten them is welcome as an issue.

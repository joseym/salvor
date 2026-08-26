# Operating salvor

The durable state is one SQLite file and the process that writes it.
This page covers the three operational questions the code answers only
indirectly: how to put TLS in front of the control plane, how to back
the store up and restore it, and what to do about a log that only ever
grows.

For the container image and its mandatory store volume, see
[CONTAINER.md](CONTAINER.md). For the trust boundaries all of this sits
inside, and for what the log records about your data, see
[SECURITY.md](../SECURITY.md).

## Serving over TLS

`salvor serve` speaks plain HTTP. It hands a raw
`tokio::net::TcpListener` to `axum::serve`; the server crate carries no
TLS dependency, and there is no flag that would turn one on. Terminating
TLS is a reverse proxy's job, and there is no supported way to do it
inside the salvor process.

The consequence is worth stating plainly: **with no proxy in front,
everything crosses the wire in cleartext, the shared secret included.**
Auth is a bearer token, so every `/v1` request
carries `Authorization: Bearer <token>` as a plaintext header; anything
on the path reads that token once and can then drive every run in the
store. The bodies are no better. Run inputs, full model responses, tool
arguments, and tool results all travel as plain JSON.

### Bind loopback, publish the proxy

`--bind` defaults to `127.0.0.1:8080` and has no environment variable
equivalent, so the address is set on the command line or not at all.
Leave it on loopback, or on a private interface only the proxy can
reach, and let the proxy own the public port.

The container image is the exception, and a deliberate one: its
entrypoint runs `salvor serve --bind 0.0.0.0:8080`, because a published
port cannot reach the container's own loopback. That moves the boundary
out to the container's network, so publish that port to the proxy, not
to the internet.

### A proxy that does not break SSE

Caddy, which also obtains and renews the certificate:

```
salvor.example.com {
	reverse_proxy 127.0.0.1:8080
}
```

nginx, with certificates you manage:

```nginx
server {
    listen 443 ssl;
    server_name salvor.example.com;

    ssl_certificate     /etc/ssl/salvor/fullchain.pem;
    ssl_certificate_key /etc/ssl/salvor/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;

        # /v1/runs/{id}/events and the client-driven streams are SSE.
        proxy_buffering off;
        proxy_read_timeout 1h;
    }
}
```

Three details are worth getting right.

Proxy the whole origin, not just `/v1`. The dashboard is served by the
same binary on the same origin (the published image is API-only and
answers `/` with a plain-text note instead), so a proxy that forwards
only `/v1` leaves the UI unreachable and buys nothing.

Do not buffer the event streams. `GET /v1/runs/{id}/events` is
Server-Sent Events, and the stream emits keep-alive comments while a
run sits between events. A buffering proxy holds those, so the client
sees nothing until the response ends, which for a long run means it
sees nothing at all; a short read timeout on top of that drops the
connection outright.

Pass `Authorization` through. nginx and Caddy both forward it by
default. A proxy configured to strip or replace request headers turns
every call into a `401`, and a proxy that injects the token on the
client's behalf hands the whole store to anyone who reaches the proxy.

### Auth is one shared secret, and it fails open

`--auth-token` takes the NAME of an environment variable holding the
token, never the token itself, matching how an agent file names its key
variable:

```sh
export SALVOR_TOKEN="$(openssl rand -hex 32)"
salvor serve --bind 127.0.0.1:8080 --auth-token SALVOR_TOKEN
```

Every `/v1` route then requires `Authorization: Bearer <that value>`.
The posture is single-tenant: no users, no roles, no per-run access
control. Whoever holds the token reads and drives every run in the
store.

If the named variable is unset or empty, **the server refuses to
start**, before it binds the port:

```
salvor: --auth-token names $SALVOR_TOKN, but it is unset or empty; export $SALVOR_TOKN with the bearer token before serving, or drop --auth-token to serve without auth
```

So a typo in the variable name is a failed start rather than an open
server, and serving unauthenticated takes omitting the flag, which is
a deliberate act. That leaves one case the refusal cannot catch: a
unit file or `docker run` that never passed `--auth-token` at all
looks equally healthy. Check the result from outside after every
start:

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://salvor.example.com/v1/runs
# 401: auth is on. 200: it is not, whatever the flags say.
```

Without a proxy in front, run the same check against the address
`--bind` actually opened:

```sh
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/v1/runs
# 401: auth is on. 200: it is not, whatever the flags say.
```

## Backup and restore

Everything durable is in the store: `--store` names it, else
`SALVOR_STORE`, else `./salvor.db`; the published image presets
`SALVOR_STORE=/data/salvor.db`. The same file serves `salvor run` and
`salvor serve`, so there is exactly one thing to copy.

A file-backed store opens with `journal_mode=WAL`, `synchronous=FULL`,
and `busy_timeout=5000`. `FULL` means a committed append is on disk
before the call returns, so an abrupt stop loses nothing that was
acknowledged. `WAL` means the store is really three files while a
writer holds it open: `salvor.db`, `salvor.db-wal`, and
`salvor.db-shm`. Both facts shape the procedure below.

### One `salvor serve` per store

Run one `salvor serve` per store file. A client-driven run's lease lives in
the server process's memory, not in the store, so two servers pointed at the
same file each think they are the only driver and each lets its own driver
into a thread the other server already believes it holds. The store itself
still refuses a second append at a taken position, so the log stays
consistent either way, but the one-driver-per-thread refusal only holds
behind one server: point a second `salvor serve` at the same file and that
guarantee is gone.

### With the writer stopped

The safest backup, and the one to prefer when a short pause is
acceptable. Stop the process first so the side files are quiescent,
then copy the whole set:

```sh
salvor serve --kill                       # or Ctrl-C, or docker stop
dest="/backups/salvor-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$dest" && cp salvor.db* "$dest/"
```

The glob matters. Copying `salvor.db` alone while a `-wal` sits beside
it takes the database without its most recent commits.

### Against a live store

`sqlite3` reads a consistent snapshot through SQLite's online backup
API, WAL contents included, and restarts itself if a writer commits
mid-copy:

```sh
sqlite3 /var/lib/salvor/salvor.db \
  ".backup '/backups/salvor-$(date -u +%Y%m%dT%H%M%SZ).db'"
```

The output is a single file with no side files of its own, but only at
the instant `.backup` finishes. The backup inherits the source's
`journal_mode=WAL`, so the next read against it, even a plain `sqlite3
SELECT`, recreates a `-wal` and `-shm` beside it; that is ordinary WAL
behavior on a clean file, not the backup coming apart. Prefer this
over `cp` on anything running: a plain copy of a live WAL database can
catch the main file and the log at different instants, and the result
may be torn or refuse to open.

### Restoring

Stop anything holding the destination store, put the file where the
path precedence will find it, and remove any stale `-wal` and `-shm`
left over from the store you are replacing. Side files from one
database next to the main file of another are a real way to corrupt a
restore:

```sh
salvor serve --kill
rm -f /var/lib/salvor/salvor.db /var/lib/salvor/salvor.db-wal \
      /var/lib/salvor/salvor.db-shm
cp /backups/salvor-20260731T090000Z.db /var/lib/salvor/salvor.db
```

One thing does not come back, and no backup ever held it.
`salvor serve` keeps submitted graph documents in a process-local,
in-memory registry, so a restart drops them and a restore does not
return them. Runs and their event logs survive; the document a run
referenced does not. The recovery path belongs to the client: whatever
submitted the graph resubmits it before a run or a fork references it
again. Get the order right: a `POST /v1/runs/{id}/resume` for a graph
run whose document is not back in the registry yet fails `404
unknown_graph`; the identical call succeeds once the document has been
resubmitted, so resubmit before you resume, not after.

### Verifying a restored store

Read it. Verification is not a separate command, because reading is
already the check:

```sh
salvor list --store /var/lib/salvor/salvor.db
```

`list` folds every run's log to derive its status, and every log read
goes through `read_log`, which recomputes the run's whole hash chain
before returning a single event. A listing that completes is therefore
an integrity pass over every run it walked, and only over every run it
walked: `no runs in <path>` is a pass over zero chains, not evidence
the restore worked. A verification worth trusting needs a store that
holds at least one run.
`salvor history <run-id> --store <path>` does the same for one run in
detail.

A failure names the run and the position it broke at. That is a
truncated copy, a torn copy of a live store, or an edit, and none of
them are worth a retry: treat it as an integrity incident and go back
to a backup that reads clean. Restoring a store whose chain does not
verify puts a log into service that `read_log` will keep refusing.

## Waking sleeping runs

A run parked on a durable timer (`sleeping`, with a `wake_at`) does not wake
itself. `salvor serve` sweeps for due timers every 60 seconds by default;
`--wake-interval SECS` changes that cadence, and `--wake-interval 0` turns
the sweep off, no task spawned.

A graph's `delay` node, a native Rust tool, or an MCP tool result carrying
`_meta.salvor.suspend` or `_meta.salvor.sleep_until` can each put a run to
sleep or suspend it; the runtime turns any of the three into the same
recorded pair, so an MCP-only setup is no longer limited to the `delay` node
for parking.

A sleeping run whose `wake_at` has passed and that nothing has woken reports
`overdue: true` and `overdue_seconds` alongside `sleeping` on
`GET /v1/runs/{id}`; the state word stays `sleeping`. The sweeper warns once
per unwakeable run and logs later passes at debug, so a quiet log does not
mean the run woke.

The sweeper only wakes what it can rebuild from what this server already
holds, by the hash the run recorded, regardless of what process started it:
an agent run wakes once the agent under that hash is registered with
`POST /v1/agents`, MCP tools and all, because the server rebuilds the agent
from that same definition. A graph run wakes once its document is submitted
with `POST /v1/graphs`, and, if it carries `tool` nodes, only once every one
of them names a tool this server's own registry holds (empty by default;
`--demo-tools` populates it with a fixed demo set, and an embedding host can
wire its own): over HTTP a `tool` node resolves only against that registry,
never against an agent's own MCP servers, submitted graph or not
(pre-existing; see
[`examples/graph-clients/README.md`](../examples/graph-clients/README.md#why-there-are-no-tool-nodes)).
So two things leave a run asleep: its agent or graph hash is not registered
here at all (typically a run started from the CLI against a store this
server never saw), or a graph run has a `tool` node this registry does not
hold. The sweeper warns why once per run, then logs the same fields at debug
on every later pass, until an operator wakes it the other way: `salvor wake`
with the same `--agent`/`--graph` files the run was
started with. A sleeping graph run outlives the server's memory of its
document (see [`README.md#graphs`](../README.md#graphs) on the in-memory
registry a restart drops), so keep the submitted document on disk; after a
restart either resubmit it with `POST /v1/graphs` or wake the run with
`salvor wake --graph <file>` directly.

For a store no running server is watching, cron does the sweeping instead.
`salvor wake` finds every run whose `wake_at` has passed, drives each one,
and exits, so it drops straight into a crontab line:

```
* * * * * salvor wake --store /var/lib/salvor/salvor.db --agent /etc/salvor/agents/reminder.toml
```

A store gets one waker, not both: the server's sweep, or cron running
`salvor wake` with the server started at `--wake-interval 0`, because the
sweep only skips runs it is already driving itself and has no way to know
about a second drive cron started on the same run. Running both against the
same due run still records it once: exactly-once holds, one write completes
the run, and the loser's drive fails on the store lock and reports the run
as taken by another driver; nothing is recorded twice. A client-driven run
that records a sleep is the client's to wake; both the server's sweeper and
`salvor wake` leave client-driven runs alone.

`wake` takes the same `--agent` (repeatable) and `--graph` a `resume` would
need, and for the same reason: the log records an agent run by its agent's
content hash and a graph run by the document's hash, never the definition
itself, so waking a run rebuilds it from the same files its author last ran
it with. `--dry-run` prints which runs are due and what waking each would
need, without driving anything.

A sleeping run holds no lock, no idempotency claim, and no process.
`SleepStarted` is recorded only after whatever came before it has already
settled, whether that is a tool's completion and its idempotency claim or a
graph node's own entry, so a sleeping run has nothing outstanding for a
restart or a backup to catch mid-flight. Restarting the server or backing up
the store while a run sleeps is exactly as safe as doing either while a run
sits idle between steps.

Sweeps select on the recorded deadline, not on when they happen to run: a
sweep only ever picks up a run whose `wake_at` has already passed, so firing
one early, from a short `--wake-interval` or an off-cadence cron line, finds
nothing and drives nothing. A run that has woken is no longer `sleeping`, so
an overlapping sweep, whether a second cron line or an overlapping server
sweep, does not see it as due either. A sweep can run early or run twice
without waking anything early or twice. The one path that can arrive before
the deadline is a person: `salvor resume` and `POST /v1/runs/{id}/resume`
both reach the run directly, and both are refused rather than silently
ignored, with the time remaining on the CLI and `409 still_sleeping` over
HTTP.

## Retention

Salvor has no retention. There is no pruning, rotation, expiry, or
garbage collection anywhere in the code. No CLI verb removes data:
`salvor abandon` retires a run by appending a `RunAbandoned` event,
which makes the log longer, not shorter. No route on the control plane
deletes anything either; there is no `DELETE` method on the API at all.
The store grows with every event, forever, and it grows roughly with
what runs record, which for a model-heavy workload means full model
responses and tool payloads.

### Why you cannot just delete rows

The `events` table carries triggers that abort any attempt:

```
salvor: events is append-only, UPDATE refused
salvor: events is append-only, DELETE refused
```

Dropping those triggers and reaching around them with `sqlite3` does
not get you anywhere useful either, because the hash chain notices. A
delete off the end of a run is caught when the recorded chain head
disagrees with what is left; a delete or an edit in the middle breaks
the `prev_hash` link at that position; and clearing the run's row from
`chain_heads` to hide the first case is itself refused, because a run
with events always has a head. Every one of those surfaces as a typed
tamper-evident error naming the run and the sequence number, and the
run stops being readable at all.

That is the designed outcome, not a limitation to work around. If you
meet one of those errors without having edited anything, treat it as an
integrity incident.

### The unit of retention is the whole store file

That leaves one supported strategy: rotate at the file level.

1. Point the writer at a fresh path: `--store <PATH>` on the command
   line, or `SALVOR_STORE` for the container. A path with no database
   at it gets a new store on first open.
2. Archive the file you rotated away. It stays independently
   verifiable and replayable: `salvor list --store <archive>` and
   `salvor history <run-id> --store <archive>` work against it forever,
   checking the chain as they read, and the chain definition is
   normative in the `salvor_store::chain` rustdoc, so a third party can
   recompute it without salvor at all.
3. Delete an archive when its retention period is up. Deleting the file
   is the deletion; there is no finer grain.

Two costs to weigh before picking a cadence. A run lives in exactly one
store, so rotating strands in-flight runs in the old file, and they can
only be resumed against that file. Cross-run idempotency is per store
too: the table that lets exactly one run execute a keyed call lives in
the database, so a call that would have been deduplicated against a run
in the archived file executes again in the fresh one. A quiet point is
one where `salvor list --store <path> --group waiting` and `salvor
list --store <path> --group progress` both print no runs, meaning
nothing is parked on a person or moving on its own. Rotate there, and
do not rotate away a run holding a keyed call that must never repeat.

Because there is no event-level deletion, the decision about what the
log holds has to be made before the run, not after it. What gets
recorded, what the one recording switch covers, and why erasure is
structurally out of reach are in
[SECURITY.md](../SECURITY.md#what-the-event-log-records).

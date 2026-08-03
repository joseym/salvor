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

If the named variable is unset or empty, **the server logs a warning
and serves without auth**. It still binds, still prints its listening
line, and still answers every route to anyone who asks:

```
WARN --auth-token names $SALVOR_TOKN, but it is unset or empty; serving without auth
```

A typo in the variable name therefore produces an unauthenticated
server that looks exactly like a healthy one, apart from that single
line. Do not trust the flag's presence in your unit file or your
`docker run`; check the result from outside after every start:

```sh
curl -s -o /dev/null -w '%{http_code}\n' https://salvor.example.com/v1/runs
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

The output is a single file with no side files of its own. Prefer this
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
again.

### Verifying a restored store

Read it. Verification is not a separate command, because reading is
already the check:

```sh
salvor list --store /var/lib/salvor/salvor.db
```

`list` folds every run's log to derive its status, and every log read
goes through `read_log`, which recomputes the run's whole hash chain
before returning a single event. A listing that completes is therefore
an integrity pass over every run it walked.
`salvor history <run-id> --store <path>` does the same for one run in
detail.

A failure names the run and the position it broke at. That is a
truncated copy, a torn copy of a live store, or an edit, and none of
them are worth a retry: treat it as an integrity incident and go back
to a backup that reads clean. Restoring a store whose chain does not
verify puts a log into service that `read_log` will keep refusing.

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
in the archived file executes again in the fresh one. Rotate at a quiet
point, and do not rotate away a run holding a keyed call that must
never repeat.

Because there is no event-level deletion, the decision about what the
log holds has to be made before the run, not after it. What gets
recorded, what the one recording switch covers, and why erasure is
structurally out of reach are in
[SECURITY.md](../SECURITY.md#what-the-event-log-records).

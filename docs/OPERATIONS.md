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
holds at least one run. A run whose rows are gone while the store still
records a head for it is read too, and refused, so a deletion is not
something a listing can walk past in silence.
`salvor history <run-id> --store <path>` does the same for one run in
detail.

`list`, `history` and `replay` read the store and never create one. A
`--store` path with no database at it is refused with exit 2 and the
same words `verify` uses, so a typo cannot come back as a store holding
no runs, which prints the same line and the same exit code as a store
that is genuinely empty. The verbs that create a store are the ones a
first run needs: `run`, `graph run`, and `serve`.

A failure names the run and the position it broke at. That is a
truncated copy, a torn copy of a live store, or an edit, and none of
them are worth a retry: treat it as an integrity incident and go back
to a backup that reads clean. Restoring a store whose chain does not
verify puts a log into service that `read_log` will keep refusing.

Reading proves the store still agrees with itself. What it cannot prove
is that the store still holds what it held before the loss, since a
writer who rewrites a run recomputes its hashes too. That is what an
anchor is for, next.

### Anchoring the chain

Reading is the check, and it has one blind spot. The chain is unkeyed:
every value the verification uses sits in the database beside the rows,
so somebody who can write the file can rewrite a run from its first
event and recompute every hash and the recorded head. The store then
reads clean and says nothing, because there is nothing left inside it
to compare against.

An anchor is a copy of what the heads were, kept where that writer
cannot reach it:

```sh
salvor anchor --store /var/lib/salvor/salvor.db --out /mnt/anchors/salvor-2026-08-28T02-00Z.json
```

```
anchored 2 run(s) (written to /mnt/anchors/salvor-2026-08-28T02-00Z.json). Keep it somewhere this store cannot reach.
```

The file is JSON, one entry per run, ordered by run id: how many events
the run held and the hash that commits to exactly those events.

```json
{
  "anchor": "salvor.anchor.v1",
  "chain": "salvor.chain.v1",
  "store": "/var/lib/salvor/salvor.db",
  "taken_at": "2026-08-28T16:40:11.482913Z",
  "runs": [
    {
      "run": "1f9c1f6d-0d3a-4a1c-9a0f-7f4a2d2b6c11",
      "len": 12,
      "hash": "4c26d7252c8cafb2842eae97494c28865b945a190bfb2518a77778b38af49e4c"
    }
  ]
}
```

With no `--out` the document goes to stdout and that one human line to
stderr, so `salvor anchor > anchor.json` gets the file and nothing else.

A store carried over from a binary older than the chain has its hashes
backfilled the first time a current binary opens it, and an anchor
taken afterwards attests the store from that migration onward. It
cannot say whether anything had already been changed before the
backfill, and it cannot mark which runs that covers: the entries for a
run recorded before the migration and a run recorded after it are the
same three fields. Note the migration date beside the anchors, because
the file will not carry it.

`anchor` reads the store and never creates one. A `--store` path with
no database at it is refused (exit 2) with nothing written, so a typo
cannot produce an anchor over an empty store the command just made. A
store that holds no runs is refused the same way, because an anchor
over zero runs commits to nothing and every later verify against it
passes having checked nothing; pass `--allow-empty` when the store is
empty on purpose and a file still has to appear on schedule. A write
that fails, such as a `--out` under a directory that is not mounted, is
exit 2 as well: no anchor was taken. Exit 1 is the two paragraphs
below: a run this store cannot read, or the file already at `--out`.

Every run's log is read back before anything is written, and a store
holding a run the store itself refuses is not anchored at all (exit 1),
whatever is or is not at `--out`:

```
salvor anchor: not anchoring /var/lib/salvor/salvor.db: run 1f9c1f6d-0d3a-4a1c-9a0f-7f4a2d2b6c11 fails its recorded hash chain at seq 4: expected 4c26..., found 1b27.... An anchor must not record a head for a run nobody can read, so nothing was written and /mnt/anchors/salvor-2026-08-29T02-00Z.json was left as it is. Go back to a backup that reads clean and read docs/OPERATIONS.md, Anchoring the chain. --force does not lift this.
```

An anchor over an unreadable run is a file that records a head nothing
can be checked against: it sits on the shelf looking like evidence, and
every later verify against it reports the same run broken. `--force`
does not lift this one, because `--force` is an answer about the file
at `--out`, and no answer about that file makes a run readable.

The file at `--out` is read before it is replaced. If it is an anchor
this store no longer verifies against, the write is refused (exit 1):
re-anchoring there would record the rewrite and destroy the only copy
of what the heads used to be, which is the failure mode a nightly
`--out /mnt/anchors/latest.json` walks straight into. One more shape
refuses the same way. If every run in that file is missing here while
this store holds runs it never names, the refusal says it may be the
wrong file and names both stores, because that reading and total loss
look identical and lead to opposite actions. Each refusal prints the
`salvor verify` line to run, with the `--store` it was given. A file
that is not an anchor at all is refused as well (exit 2).

`--force` overwrites the file at `--out` whatever it holds, and does
not silence what was found there. The comparison still runs and its
answer still prints, as a warning rather than a refusal:

```
warning: this store fails verification against /mnt/anchors/salvor-2026-08-29T02-00Z.json (1 of 3 anchored runs); overwriting anyway as asked.
anchored 3 run(s) (written to /mnt/anchors/salvor-2026-08-29T02-00Z.json). Keep it somewhere this store cannot reach.
```

Passing `--force` says "overwrite it", not "do not tell me what I am
overwriting", and that line is the last moment anything can say what
the old heads were. Capture stderr from a job that passes `--force`, or
the one sentence describing the evidence you just destroyed goes to a
terminal nobody is reading.

#### Custody

"Somewhere the store cannot reach" is a statement about credentials,
not about distance. Ask whether the identity that writes the database
could also write the anchor. If the same host, the same service
account, or the same deploy key reaches both, then whoever rewrites the
store rewrites the anchor in the same breath, and the file answers
nothing no matter which disk it sits on. Write anchors under a
credential domain the store's writer does not hold: pull them to a
workstation over SSH, or push them into a bucket with a role the
runtime has no way to assume.

Use immutable or append-only storage where you have it, such as S3
Object Lock, a WORM volume, or an append-only backup target. It answers
the second question, whether the copy itself was edited after it landed,
which custody alone does not. Where none of that is available, a hash of
the anchor recorded somewhere with its own retention, a ticket or a
chat channel, is still better than the file alone.

Give each anchor a name no later one reuses and keep the history:
`salvor-2026-08-29T02-00Z.json`, not `latest.json`, and not a name to
the day either if the job can run twice in one. One rolling file is one
file to overwrite, and overwriting it is the whole attack. A directory
of dated anchors is also what lets a restore be checked against the
anchor from before the loss rather than one taken after it. Keep an
anchor for as long as you keep the backup it was taken beside, since a
backup you can still restore and no longer have an anchor for is a
store you can bring back and cannot check.

#### Cadence

Two windows set the cadence, and both are measured in what you would be
unable to say afterwards.

An anchor says nothing about events recorded after it was taken, so a
run extended with fabricated events chained onto its current head is
invisible until the next anchor covers it. That window is counted in
events, not in hours: pick a cadence where the number of events you
cannot yet say anything about is a number you can live with. A store
recording a few hundred events a day and one recording a few hundred an
hour are not the same problem on the same schedule.

The second window is the restore. A store restored from a backup can
verify perfectly against a three-week-old anchor and still be missing
every run started since it was taken, and nothing in the store says so:
verify reports on the runs the anchor names, and a run that appears in
neither the anchor nor the restored file is a run nothing in the output
mentions. So take an anchor after every backup and keep the two
together, so that "restored from which backup" and "verified against
which anchor" have the same answer, and treat the gap between the last
anchor and the incident as runs you will have to account for by other
means.

The order within one nightly job matters as much as the interval. Check
the store against the newest anchor you already hold, and take tonight's
anchor only if that check passed. A job that anchors first and verifies
second has checked a store against a file it wrote a second earlier,
which passes by construction and says nothing; a job that anchors first
and stops has recorded whatever the store now holds as the truth. Never
verify against an anchor the same job just took.

Two things follow, and both have to be in the script rather than in the
operator's head. The first night there is nothing to check against, and
a job that treats that as a failure never gets off the ground; the
answer is to take the first anchor and say that is all that happened.
And the name has to be one no later run reuses, or the second run of
the day overwrites the file the first one wrote, which is the whole
attack performed by the cron job itself. A timestamp to the minute
gives every run its own name and sorts chronologically:

```sh
#!/bin/sh
# Nightly: check the newest anchor on hand, then record a new one.
set -eu
store=/var/lib/salvor/salvor.db
anchors=/mnt/anchors

# The newest anchor already on hand, chosen before anything is written,
# so this job can never verify against a file it wrote itself. Empty on
# the first night; `ls` failing on an empty directory is not an error
# here, which is why its status is not the pipeline's.
latest=$(ls -1 "$anchors"/salvor-*.json 2>/dev/null | tail -1)

# To the minute, and never reused: a rerun writes a new file beside the
# old one rather than over it.
out=$anchors/salvor-$(date -u +%Y-%m-%dT%H-%MZ).json
if [ -e "$out" ]; then
  echo "salvor: $out is already here; nothing checked and nothing written." >&2
  exit 0
fi

if [ -n "$latest" ]; then
  salvor verify --store "$store" --against "$latest"
  salvor anchor --store "$store" --out "$out"
else
  salvor anchor --store "$store" --out "$out"
  echo "salvor: no anchor was on hand, so nothing was checked; $out is the first one." >&2
fi
```

A second run inside the same minute stops there and checks nothing,
which is the honest answer: the newest anchor on hand would be the one
the previous run took seconds ago, and a store checked against an
anchor that fresh passes by construction. The next minute the job runs
normally.

`set -e` is what makes the order mean anything: a verify that exits 1 or
2 stops the job before the anchor is taken, so a store that no longer
matches its evidence does not get a fresh file recording what it now
holds. The exit code of the whole script is the exit code of whichever
command stopped it, so alert on 1 and on 2 separately, as below.

#### After a restore

Read the store (above), then check it against the anchor you took
before the loss:

```sh
salvor verify --store /var/lib/salvor/salvor.db --against /mnt/anchors/salvor-2026-08-28T02-00Z.json
```

```
run 1f9c1f6d-0d3a-4a1c-9a0f-7f4a2d2b6c11: intact: 15 event(s), anchored at 12, 3 recorded since
run 8a2b0c44-51e7-4f0a-b3d1-9c6e5f2a7d90: new since the anchor, 4 event(s). Not covered by this anchor; the next one covers it.
1 run(s) anchored, 1 intact, 0 failed, 1 new since the anchor
```

Every run is named, including the ones that are fine, because an answer
that lists only trouble cannot tell "nothing is wrong" from "nothing
was checked". A run that has grown since the anchor is intact: the
anchor commits to the prefix it recorded, and ordinary appending is not
a discrepancy. A run started after the anchor was taken is reported as
new and fails nothing; the next anchor covers it. The store's path is
never matched against the one recorded in the anchor, because a restore
to a new path is ordinary.

An intact run that has grown names both lengths. The anchored one is
not the current one, and a line that prints only the anchored length
reads as the size of a run that is in fact longer, which is the number
you would go on to compare against a backup. A run that has not grown
has one length worth naming and names it: `intact at 12 event(s)`.

Four findings are failures. Each names the run; `rewritten` and
`broken` name the position they failed at, and `missing` and
`shortened` name lengths, because a run that is not there and a run
that stops early have no position to point at.

* `missing`: the anchor recorded the run and this store does not hold
  it. Names the anchored length.
* `shortened`: this store holds fewer events than the anchor recorded.
  Names both lengths.
* `rewritten`: this store holds at least as many, and the hash at the
  anchored length is not the anchored one. The events the anchor
  covered are not the events this store now holds.
* `broken`: this store refuses the run's own log. That is the ordinary
  chain failure, found here because verifying reads every log. It names
  the sequence number when a row is what disagrees, and no position
  when the recorded head is: a head that commits to a different number
  of rows, or that outlived the rows under it, is not wrong at any one
  line.

```
run 8a2b0c44-51e7-4f0a-b3d1-9c6e5f2a7d90: broken. This store refuses its own log: the run's events are gone and only its recorded head remains (12 rows recorded).
```

That last line is what a deletion looks like: the rows were removed and
the head was left behind. `salvor list` and `salvor history` refuse the
same run in the same words.

```
run 1f9c1f6d-0d3a-4a1c-9a0f-7f4a2d2b6c11: rewritten at seq 11 (the anchored length is 12). The events this anchor covered are not the events this store now holds.
  the anchor recorded 4c26d7252c8cafb2842eae97494c28865b945a190bfb2518a77778b38af49e4c
  this store holds  1b27340ee0ce5419b995c63eedb0b937087b59a7033402802a6cca8c8a21a2d3
1 run(s) anchored, 0 intact, 1 failed, 0 new since the anchor

This store no longer holds what the anchor says it held. Do not re-anchor it: a fresh anchor
over a rewritten store records the rewrite. Go back to a backup that reads clean and verifies
against this anchor. See docs/OPERATIONS.md, Anchoring the chain.
```

Positions are sequence numbers, the same ones `salvor history` prints,
so a finding names a line you can go and read. Lengths are counts, and
the report says which is which: an anchored length of 12 is a rewrite
at seq 11.

`failed` counts anchored runs only, so the summary closes: intact plus
failed is the number the anchor covers. A run the anchor never names
whose log this store now refuses is a real finding and gets a clause of
its own, printed only when there is one:

```
1 run(s) anchored, 0 intact, 1 failed, 1 broken outside the anchor, 0 new since the anchor
```

Both exit 1. The anchor says nothing about that second run either way;
what found it is the store refusing its own log, which is the ordinary
chain failure and leads to the same backup.

If every anchored run comes back missing while the store holds runs the
anchor never names, the report says the anchor may simply be the wrong
one, and names the store it was taken over beside the store just
checked. It prints instead of the restore advice, not above it:

```
This may be the wrong anchor. Every run it records is missing here, and this store holds
1 run(s) it never names. The anchor was taken over /var/lib/salvor/other.db; this check read
/var/lib/salvor/salvor.db. Confirm the two belong together before doing anything else: if they do belong
together, treat this as a loss and see the restore advice in docs/OPERATIONS.md,
Anchoring the chain.
```

Being handed the wrong file looks exactly like total loss, and the two
are a minute apart in effort and hours apart in consequence; an
operator who reads "go back to a backup" first restores over a store
that was fine. An anchor that does not record a `store` at all says so
in that same sentence rather than naming an empty path. `salvor anchor`
says the same thing when it declines to overwrite such a file, rather
than the wording that sends you to a backup.

#### The three exit codes

```
0  the check ran and every anchored run is intact
1  the check ran and at least one run is missing, shortened, rewritten, or broken
2  the check did not run
```

Exit 0 is the only pass. Exit 1 is the page: this store is not the
store the anchor describes, and the next step is a backup, not another
anchor. Exit 2 says nothing was compared, so it is neither a pass nor a
finding: there was no store at the path, or the anchor file was
missing, unreadable, not JSON, written under a spec this binary does
not read, carrying a malformed entry, naming one run twice, or
committing to no runs at all (`--allow-empty` accepts that last one
deliberately). A check that quietly stops running looks identical to a
check that keeps passing, so alert on 2 as well, in its own words.

`salvor anchor` uses the same three codes, and 2 covers one more thing:
a write that failed. An anchor is not taken because `--out` names a
directory that is not there, and the job that reads exit 1 as "this
store no longer matches the file already at that path" must not read an
unmounted volume under that name.

```sh
#!/bin/sh
# Check this store against the newest anchor on hand, from cron.
anchor=$(ls -1 /mnt/anchors/salvor-*.json 2>/dev/null | tail -1)
if [ -z "$anchor" ]; then
  alert "salvor: no anchor on hand; nothing was checked"
  exit 2
fi
salvor verify --store /var/lib/salvor/salvor.db --against "$anchor" --json > /var/log/salvor-verify.json
case $? in
  0) exit 0 ;;
  1) alert "salvor: this store no longer matches its anchor" < /var/log/salvor-verify.json ;;
  *) alert "salvor: verify did not run" < /var/log/salvor-verify.json ;;
esac
```

`--json` prints a document for all three. A check that ran carries
`"checked": true`, `"ok"`, the counts (`anchored`, `intact`, `failed`,
`broken_unanchored`, `new`), `"maybe_wrong_anchor"` for the reading the
report prints in prose, and one entry per run with its `finding`; a
check that did not run carries `"checked": false` and an `error` saying
why. One parser reads every outcome, and an empty stdout is never one
of them.

#### Signing an anchor

Salvor ships no signing, and holds no key material. The anchor is a
plain JSON file, so on its own it proves what it says only as far as
you can vouch for where the file has been. A detached signature from
your own tooling closes the rest, under a key the store's writer does
not hold:

```sh
salvor anchor --store /var/lib/salvor/salvor.db --out salvor-2026-08-28T02-00Z.json
minisign -Sm salvor-2026-08-28T02-00Z.json           # writes salvor-2026-08-28T02-00Z.json.minisig
```

Verify the signature first, and only run `verify` if it passes, so a
substituted anchor is caught before salvor reads a word of it:

```sh
minisign -Vm salvor-2026-08-28T02-00Z.json -p /etc/salvor/anchors.pub \
  && salvor verify --store /var/lib/salvor/salvor.db --against salvor-2026-08-28T02-00Z.json
```

`gpg --detach-sign` and `gpg --verify` do the same job. Salvor does not
look for a signature file, does not check one, and will read an
unsigned anchor without comment; the ordering above is the whole
mechanism.

#### What an anchor does not close

It says nothing about events recorded after it was taken, which is the
extension window the cadence above is chosen against. It says nothing
about who wrote any of it. And it is only as good as its custody and,
if you signed it, its key.

The chain definition an anchor's hashes are built under is normative in
the `salvor_store::chain` rustdoc (`cargo doc -p salvor-store --open`,
module `chain`), and the physical layout is in `salvor_store::sqlite`:
the `events` table, whose `envelope` column holds the exact recorded
bytes that are hashed and whose `chain_idx` is the row's position in
its run's append order, and `chain_heads`, which holds one recorded
head per run as `chain_len` and `head_hash`. Between the two, anyone
with the database can recompute every hash in an anchor without salvor
at all. See SECURITY.md.

## Runs waiting on a person

A run waiting on a person, whether at a dangling write, a gate, or a budget ceiling, stays where it is until someone acts. Nothing times out and nothing escalates on its own. `salvor list --store <path> --group waiting` lists such runs; that is what to alert on.

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
not get you anywhere useful either, because the hash chain detects it.
A delete off the end of a run is caught when the recorded chain head
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

A rotation done for erasure is expected to fail `salvor verify` against
any anchor taken before it, because the fresh store holds none of the
runs that anchor names and every one of them is reported missing; that
is the rotation working rather than an incident, so anchor the fresh
store and keep the old anchor with the archive it describes.

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

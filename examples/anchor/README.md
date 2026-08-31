# anchor: the check the store cannot do for itself

Every recorded row carries a SHA-256 hash over the exact stored envelope bytes,
chained to the hash of the row before it, and `read_log` recomputes the whole
chain before it returns a single event. Changing a recorded row, reordering the
rows, splicing one into the middle, or dropping one out is refused on the next
read, with a typed error naming the run and the position. That much the store
does on its own, and `reconciliation/` and the rest of the examples lean on it.

It has one blind spot, and [`SECURITY.md`](../../SECURITY.md) states it plainly:
the chain is unkeyed. Every value the verification uses sits in the database
beside the rows, so somebody who can write the file can rewrite a run from its
first event forward, recompute every hash, move the recorded head along with
it, and put the append-only triggers back. The store then reads clean and says
nothing, because every value its own check compares was rewritten too.

An anchor is a copy of what the heads were, kept where that writer cannot reach.
This example takes one, then performs exactly that attack on a copy of the
store, using nothing but SQLite and SHA-256, and shows the store reading the
forgery back as history while `salvor verify` names it.

Everything here is offline: no API key, no network, no port, and no server.
Every command is `salvor` against a `--store` path.

## What an anchor is

One entry per run, ordered by run id: how many events the run held, and the
hash that commits to exactly those events. That is the whole file.

```
$ salvor --store /tmp/salvor-anchor.db anchor --out /tmp/salvor-anchor-heads.json
warning: /tmp/salvor-anchor-heads.json is in the same directory as /tmp/salvor-anchor.db. Whoever can rewrite the store can rewrite this file along with it, so an anchor kept here answers nothing. Copy it somewhere the store's writer cannot reach and keep it there.
anchored 2 run(s) (written to /tmp/salvor-anchor-heads.json). Keep it somewhere this store cannot reach.
```

```json
{
  "anchor": "salvor.anchor.v1",
  "chain": "salvor.chain.v1",
  "store": "/tmp/salvor-anchor.db",
  "taken_at": "2026-08-30T23:57:43.272427Z",
  "runs": [
    {
      "run": "c567b482-ff65-47d5-aad4-f698d10a6d13",
      "len": 3,
      "hash": "344d68e441e3f346305d56dfd72afe6bb376163421769a87b87d4abc35701a8e"
    },
    {
      "run": "e97bf2de-d89f-4707-b4ee-f613b4a5aba0",
      "len": 10,
      "hash": "d27bdb5db4db88f79a518fd16a46b65501a3d5de1ce801ff250e5a33ee1fd4cf"
    }
  ]
}
```

A head hash commits to the entire log before it, so an anchor holds 32 bytes per
run rather than a copy of the events. Nothing in it is a secret, and nothing in
it is signed. Its whole value is that it is somewhere the database cannot reach,
which is why `salvor anchor` says so when it is written next to the store it
describes. The script prints that warning honestly, because a shell script
cannot put a file on another machine. Custody is the operator's half of this,
and `docs/OPERATIONS.md` spends a section on it.

## The two runs

The claim run is the checked-in [`hero/`](../hero/) fixture, ten events, the
same offline run the terminal on salvor.run shows. The sign-off run is
[`sign-off.json`](sign-off.json), a graph of one `gate` node, which parks after
three events waiting for a person:

```
   0  2026-08-30 23:57:43Z  GraphRunStarted      graph sha256:924988f… input {"quarter":"2026-Q3"}
   1  2026-08-30 23:57:43Z  NodeEntered          enter sign_off
   2  2026-08-30 23:57:43Z  Suspended            reason: Sign off on the salvage claim ledger for this quarter?
```

It is here for one reason: it gives the store a run that is anchored while it is
still unfinished, so that proof 2 can let an anchored run grow honestly and ask
what the anchor then says about it.

## 1. Anchor two runs, and verify

```
run c567b482-ff65-47d5-aad4-f698d10a6d13: intact at 3 event(s)
run e97bf2de-d89f-4707-b4ee-f613b4a5aba0: intact at 10 event(s)
2 run(s) anchored, 2 intact, 0 failed, 0 new since the anchor
```

Exit 0, the only pass there is. Every run is named, including the ones that are
fine, because the question is "does this store still hold what it held", and an
answer listing only trouble cannot tell "nothing is wrong" from "nothing was
checked".

## 2. A run grows, and the anchor still holds

The gate is answered and the sign-off run runs to completion, three events
longer than when it was anchored. Nothing is re-anchored. The same check:

```
run c567b482-ff65-47d5-aad4-f698d10a6d13: intact: 6 event(s), anchored at 3, 3 recorded since
run e97bf2de-d89f-4707-b4ee-f613b4a5aba0: intact at 10 event(s)
2 run(s) anchored, 2 intact, 0 failed, 0 new since the anchor
```

Still exit 0. An anchor commits to the prefix it recorded, not to the size of a
run, and what has to still hold is that the run's chain passed through the
anchored hash at the anchored length. The report says how much has been added
rather than printing the anchored length as though it were the current one, and
those three events are exactly what this anchor says nothing about.

## 3. The forgery the store cannot detect

On a copy of the store, with the triggers taken off and put back afterwards,
every row of the claim run is rewritten so the recorded log says the claim was
for a different wreck, and the whole chain is rebuilt over it. The forgery uses
no salvor code: python's standard library gives SQLite and SHA-256, and the
chain definition is published, so this is what the preimage is built from and
nothing else.

```
preimage = "salvor.chain.v1" \n prev \n run_id \n seq \n envelope_json
row_hash = lowercase hex sha256(preimage)
```

`prev` is 64 zeros for a run's first row and the previous row's `row_hash`
after that, `envelope_json` is the exact bytes the `events` table recorded, and
`chain_heads` holds the run's `chain_len` and `head_hash`. The append-only
triggers on `events` are a locked door on a building with windows: whoever can
write the file can drop them, and the chain is the evidence, not the triggers.

The store reads the result back as history and says nothing:

```
$ salvor --store /tmp/salvor-anchor-rewritten.db history e97bf2de-d89f-4707-b4ee-f613b4a5aba0
   0  2026-08-30 23:57:43Z  RunStarted           agent sha256:b307d98… input {"item":"ss-cumberland"}
   1  2026-08-30 23:57:43Z  NowObserved          2026-08-30 23:57:43Z
   2  2026-08-30 23:57:43Z  ModelCallRequested   request sha256:e3de92d…
   3  2026-08-30 23:57:43Z  ModelCallCompleted   usage in 24 out 41
   4  2026-08-30 23:57:43Z  ToolCallRequested    save_claim [Write] input {"item":"ss-cumberland"}
   5  2026-08-30 23:57:43Z  ToolCallCompleted    output {"content":[{"text":"claim recorded: ss-cumberland","type":"text"}],"isError":fa…
   6  2026-08-30 23:57:43Z  NowObserved          2026-08-30 23:57:43Z
   7  2026-08-30 23:57:43Z  ModelCallRequested   request sha256:70d9a5b…
   8  2026-08-30 23:57:43Z  ModelCallCompleted   usage in 96 out 38
   9  2026-08-30 23:57:43Z  RunCompleted         output "1 claim recorded for ss-cumberland."
$ echo $?
0
```

The run really was recorded against `ss-waratah`, and that word now appears
nowhere in the log. `salvor list` reads the store just as happily. This is the
whole point: no reader inside the database can tell, because every input the
reader has is an input the writer had.

The anchor is outside the database, so it can:

```
$ salvor --store /tmp/salvor-anchor-rewritten.db verify --against /tmp/salvor-anchor-heads.json
run c567b482-ff65-47d5-aad4-f698d10a6d13: intact: 6 event(s), anchored at 3, 3 recorded since
run e97bf2de-d89f-4707-b4ee-f613b4a5aba0: rewritten at seq 9 (the anchored length is 10). The events this anchor covered are not the events this store now holds.
  the anchor recorded d27bdb5db4db88f79a518fd16a46b65501a3d5de1ce801ff250e5a33ee1fd4cf
  this store holds  1c6a9ad3132e2c4632f50ee072fc858e4e6070d77f3be16d42af4cfb8ba602f6
2 run(s) anchored, 1 intact, 1 failed, 0 new since the anchor

This store no longer holds what the anchor says it held. Do not re-anchor it: a fresh anchor
over a rewritten store records the rewrite. Go back to a backup that reads clean and verifies
against this anchor. See docs/OPERATIONS.md, Anchoring the chain.
$ echo $?
1
```

The finding names the run, the seq to go and read, and both hashes. It is also
one run wide: the other anchored run is still reported intact by name, so the
report says what was touched and what was not.

## 4. A run cut short

Same attack, different shape, on a fresh copy of the honest store: the claim
run's last row is deleted and `chain_heads` is walked back onto the row before
it. Every remaining link still holds, so the store reads this one clean too, and
shows a run that never finished. The anchor counted the events:

```
run c567b482-ff65-47d5-aad4-f698d10a6d13: intact: 6 event(s), anchored at 3, 3 recorded since
run e97bf2de-d89f-4707-b4ee-f613b4a5aba0: shortened. The anchor recorded 10 event(s); this store holds 9.
2 run(s) anchored, 1 intact, 1 failed, 0 new since the anchor
```

Exit 1 again. Lengths are counts and positions are sequence numbers, and the
report says which is which, so a finding names a line you can go and read.

## 5. The two checks that would mean nothing are refused

A pass over zero runs prints exactly like a pass over a store full of intact
ones, which is the one failure mode an anchor exists to rule out. So an anchor
over an empty store is refused, and so is a verify against an anchor that
commits to no runs:

```
$ salvor --store /tmp/salvor-anchor.db verify --against /tmp/salvor-anchor-empty.json
salvor verify: this anchor commits to nothing: /tmp/salvor-anchor-empty.json records no runs, so a pass against it would mean nothing was checked. Take an anchor over a store that holds runs, or pass --allow-empty to accept this one.
$ echo $?
2
```

Exit 2 is neither a pass nor a finding: the check did not run. A check that
quietly stops running looks identical to one that keeps passing, so alert on 2
in its own words. `--allow-empty` accepts an empty store deliberately, for the
case where a file still has to appear on schedule.

The other vacuous case is a typo. `anchor` reads a store and never creates one:

```
$ salvor --store /tmp/salvor-anchor-typo.db anchor --out /tmp/salvor-anchor-typo.json
salvor anchor: no store at /tmp/salvor-anchor-typo.db. Nothing was read and nothing was created: check the path, or point --store at the database.
$ echo $?
2
```

The script checks afterwards that neither file is there. An anchor over a store
a typo just created would be a file that commits to nothing and passes forever.

## 6. Re-anchoring over the evidence is refused

The nightly job that writes to a fixed `--out` walks into this: the store has
been rewritten, and the next anchor records the rewrite over the only copy of
what the heads used to be. The file at `--out` is read before it is replaced,
and this one is refused:

```
$ salvor --store /tmp/salvor-anchor-rewritten.db anchor --out /tmp/salvor-anchor-heads.json
salvor anchor: this store no longer verifies against the anchor already at /tmp/salvor-anchor-heads.json; not overwriting. 1 of 2 anchored run(s) failed. Run `salvor verify --store /tmp/salvor-anchor-rewritten.db --against /tmp/salvor-anchor-heads.json` to see which, and read docs/OPERATIONS.md, Anchoring the chain, before re-anchoring. Pass --force to overwrite anyway.
$ echo $?
1
```

The script then compares the anchor byte for byte against the copy it took when
the file was written, because "refused" and "left the file alone" are two
different claims.

`--force` is not demonstrated here, on purpose. It overwrites the file whatever
it holds, and over a store in this state that means recording the rewrite as the
new truth and destroying the evidence of it in the same second. It still runs
the comparison and still prints the answer, as a warning rather than a refusal,
and that warning is the last moment anything can say what the old heads were, so
a job that passes `--force` has to capture stderr. An example that ran it would
be demonstrating how to lose the only thing this example is about.

## The two limits, plainly

An anchor says nothing about events recorded after it was taken. Proof 2 shows
that as a feature, since a run that grew honestly still verifies, but it is the
same window an attacker uses: a run extended with fabricated events chained onto
its current head is invisible until the next anchor covers it. That window is
counted in events, not in hours, which is what sets the cadence.

The file is not signed. It proves what it says only to whoever can vouch for
where it has been. Salvor ships no signing and holds no key material; a detached
`minisign` or `gpg` signature from your own tooling, verified before `verify`
runs, closes that half, under a key the store's writer does not hold.

## Run it

```
cargo build
bash examples/anchor/run.sh
```

It prints a numbered `PROOF` line for every claim above, asserts each exit code
exactly (0, 1 and 2 are three different sentences here), and exits 0 only if
every proof holds. Anything that does not hold prints a `FAILED: expected ...`
line naming what it wanted and what it found, so a run that stopped early can
never be mistaken for one that passed.

`cargo build --release` works just as well: the script takes `target/debug` when
it is there and `target/release` otherwise. `SALVOR_BIN` overrides the binary
path outright, which is how an already-installed CLI drives this instead.
`SALVOR_EXAMPLE_SCRATCH` and `SALVOR_EXAMPLE_STORE` move the files it writes;
everything it produces lives under the scratch directory, and it removes only
the paths it owns. `python3` does the forgery and reads one hash back out of the
anchor; nothing else is needed. No key, no network, and no port is bound.

## Where the rest of this is written down

[`docs/OPERATIONS.md`](../../docs/OPERATIONS.md), "Anchoring the chain", is the
routine: custody, cadence, the order within a nightly job, a script to copy, the
findings `verify` reports, and the three exit codes.
[`SECURITY.md`](../../SECURITY.md) is the threat: what the chain guarantees
unconditionally, what it does not, and where an unsigned anchor sits between
them. The chain definition itself is normative in the `salvor_store::chain`
rustdoc (`cargo doc -p salvor-store --open`, module `chain`), and the physical
layout is in `salvor_store::sqlite`.

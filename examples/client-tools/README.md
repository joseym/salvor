# Example: a tool the client performs

This example is a refund desk where the money moves in the client's process.
Salvor never holds the payment code and never holds the payment credential. It
holds the log: an ordered record of every refund the desk asked for, with the
operator's effect class on it, the key the desk had to charge under, and the
provider's own identifier for what came back. A report carrying no such
identifier is refused and never becomes history.

Everything runs offline with no API key. `salvor-demo-model --script` stands in
for the model, and `provider.py` stands in for the payment provider.

## The seam

A team with working payment code will not rewrite it as an MCP server so salvor
can call it. Nor will they hand salvor the key that moves money. Client-performed
tools are the other direction: they keep the code, salvor keeps the log.

An operator declares the tool in a TOML file and starts the server with it:

```sh
salvor serve --client-tool examples/client-tools/refund-card.toml \
             --client-tool examples/client-tools/wire-payout.toml
```

The file says the tool's name, its effect class, the schema its input must
satisfy, the schema its reported result must satisfy, and whether the client may
close the call itself. There is no code behind it. The declaration format has no
command field, no path field, and no URL field, and an unknown key is a parse
error rather than an ignored line, so a declaration cannot name `provider.py`
even by accident.

Then, inside a client-driven run:

1. The desk fetches the declarations from `GET /v1/client-tools` and hands the
   model each `input_schema` as that tool's function definition. One schema, the
   one the server validates against.
2. The model asks to refund a card.
3. The desk opens an intent. Salvor checks the input against the declared
   schema, records `ToolCallRequested` before anything happens, and answers with
   an idempotency key it derived.
4. The desk runs `provider.py` as its own subprocess, under that key, with a
   credential from its own environment.
5. The desk reports what came back. Salvor checks it against the declared output
   schema and records `ToolCallCompleted`.

## What is here

- `refund-card.toml`: the first declaration. A `write`, self-completable, whose
  output schema requires `provider_refund_id`.
- `wire-payout.toml`: the second declaration. A `write` with
  `trust_completion = false`, so no client report can close it.
- `provider.py`: the stand-in payment provider, owned by this example and run by
  the desk. It mints a plausible charge identifier and keeps a ledger keyed by
  idempotency key, so a second call under a key already on file returns the same
  refund rather than making a new one. It refuses to run without
  `REFUND_PROVIDER_API_KEY` in its environment.
- `desk.py`: the application. It drives three client-driven runs through this
  checkout's Python SDK.
- `model-script.json`: three scripted conversations, selected by the case name in
  the system prompt.
- `run.sh`: the whole sequence, offline, with every port and path overridable.

## Running it

From anywhere:

```sh
bash examples/client-tools/run.sh
```

`run.sh` prints the exact command line it starts the server with, and then the
process as the operating system sees it, because what the server was not given
is the claim this example makes:

```
== starting salvor serve on 127.0.0.1:18964 (store /var/folders/_0/cwccwh193qg0tlv2fnbt67x00000gn/T//salvor-client-tools.db) ==
  salvor --store /var/folders/_0/cwccwh193qg0tlv2fnbt67x00000gn/T//salvor-client-tools.db serve --bind 127.0.0.1:18964 --client-tool examples/client-tools/refund-card.toml --client-tool examples/client-tools/wire-payout.toml
control plane ready on http://127.0.0.1:18964
  the running process, as the operating system sees it:
    /Users/joseymorton/Briefcase/salvor/target/debug/salvor --store /var/folders/_0/cwccwh193qg0tlv2fnbt67x00000gn/T//salvor-client-tools.db serve --bind 127.0.0.1:18964 --client-tool examples/client-tools/refund-card.toml --client-tool examples/client-tools/wire-payout.toml
```

Two declaration files. No tool registry, no MCP server, no `--demo-tools`, and no
`provider.py`. `REFUND_PROVIDER_API_KEY` is set on the desk's command line and
nowhere else, so it exists in the desk's process and in the subprocess the desk
spawns, and never in the server's.

The desk starts by reading what the operator declared:

```
[desk] what the operator declared (GET /v1/client-tools):
[desk]   refund_card: effect=write trust_completion=True input requires ['order_id', 'amount_cents', 'currency'] output requires ['provider_refund_id', 'status', 'amount_cents']
[desk]   wire_payout: effect=write trust_completion=False input requires ['payee_account', 'amount_cents', 'currency'] output requires ['provider_transfer_id', 'status', 'amount_cents']
[desk]   the payment credential is set in this process, and salvor was started without it
[desk]   the model gets 2 function definitions built from those input schemas: ['refund_card', 'wire_payout']
[desk]   the desk keeps no schema of its own, so it has none to let drift
```

### Case 1: the whole path

The model asks for a refund, the desk opens the intent, charges under the key it
was handed, and reports the result.

```
[desk] CASE 1: a refund the client performs and salvor records
[desk]   run 5691f3d0-d4aa-45b9-9875-2127c31e039e
[desk]   the model asked for refund_card: {"amount_cents": 4200, "currency": "USD", "order_id": "ORD-7781", "reason": "Item arrived damaged; customer kept the replacement."}
[desk]   intent recorded at seq 3, effect write (the operator's, not ours)
[desk]   the server derived the key: sha256:381c4e1686e70c110679285138a4010ac7493a03fea81135ba3c60950f9e7f53
[desk]   running in this process: provider.py refund --idempotency-key sha256:381c4e1686e7... ...
[desk]   the provider answered: {"provider_refund_id": "re_68838fbc1fd9", "status": "succeeded", "amount_cents": 4200, "replayed": false}
[desk]   completion recorded
[desk]   the model closed out: Refunded $42.00 to the card on ORD-7781. The provider has the refund on file.
[desk]   run state: completed
[desk]     seq 0  RunStarted
[desk]     seq 1  ModelCallRequested
[desk]     seq 2  ModelCallCompleted  usage in 260 out 52
[desk]     seq 3  ToolCallRequested  refund_card [write] performed_by=client key=sha256:381c4e1686e7...
[desk]     seq 4  ToolCallCompleted  {"amount_cents": 4200, "provider_refund_id": "re_68838fbc1fd9", "status": "succeeded"}
[desk]     seq 5  ModelCallRequested
[desk]     seq 6  ModelCallCompleted  usage in 330 out 26
[desk]     seq 7  RunCompleted
```

Sequence 3 carries `performed_by=client`. That is how a later reader tells a call
salvor witnessed from a call it was told about. A server-performed intent omits
the field.

### Case 2: a result with no evidence in it

The refund happens. Then the desk reports a summary of it that dropped the one
field it could not have invented:

```
[desk]   the provider answered: {"provider_refund_id": "re_6ba48815b1c2", "status": "succeeded", "amount_cents": 15900, "replayed": false}
[desk]   reporting a completion with no provider id: {"status": "succeeded", "amount_cents": 15900}
[desk]   REFUSED bad_request: the reported output does not match the declared output_schema for `refund_card`: $: missing required property `provider_refund_id`
[desk]   the log after the refusal (nothing was written):
[desk]     seq 0  RunStarted
[desk]     seq 1  ModelCallRequested
[desk]     seq 2  ModelCallCompleted  usage in 264 out 54
[desk]     seq 3  ToolCallRequested  refund_card [write] performed_by=client key=sha256:2857b485e07a...
[desk]   run state: needs_reconciliation
```

Four events. The intent stands, no completion follows it, and the run is where an
unsettled write always leaves a run. `needs_reconciliation` is a pure fold of that
log, computed by code that has never read a declaration file, which is why the
same log means the same thing replayed on a machine that has never seen this
server's `--client-tool` files.

The desk then does what a desk does after a dropped response. It re-posts the
same intent at the same position and charges again:

```
[desk]   re-posted the intent at seq 3; same key: True
[desk]   running in this process: provider.py refund --idempotency-key sha256:2857b485e07a... ...
[desk]   the provider answered: {"provider_refund_id": "re_6ba48815b1c2", "status": "succeeded", "amount_cents": 15900, "replayed": true}
[desk]   same refund as the first attempt: re_6ba48815b1c2
[desk]   completion recorded, now that it carries the provider's id
```

Two calls to the provider, one refund. The provider's ledger is the proof, and
`run.sh` prints it at the end:

```
== the provider's ledger, one line per call the desk made ==
   (created = money moved; replayed = the same key came back and it did not)
     1	{"n": 1, "action": "refund", "outcome": "created", "idempotency_key": "sha256:381c4e1686e70c110679285138a4010ac7493a03fea81135ba3c60950f9e7f53", "provider_refund_id": "re_68838fbc1fd9", "amount_cents": 4200, "order_id": "ORD-7781", "currency": "USD", "reason": "Item arrived damaged; customer kept the replacement."}
     2	{"n": 2, "action": "refund", "outcome": "created", "idempotency_key": "sha256:2857b485e07af0472996a0562d11a5f9811c1b0401ddd18ab13949009f092c51", "provider_refund_id": "re_6ba48815b1c2", "amount_cents": 15900, "order_id": "ORD-8120", "currency": "USD", "reason": "Duplicate charge on a single subscription renewal."}
     3	{"n": 3, "action": "refund", "outcome": "replayed", "idempotency_key": "sha256:2857b485e07af0472996a0562d11a5f9811c1b0401ddd18ab13949009f092c51", "provider_refund_id": "re_6ba48815b1c2", "amount_cents": 15900, "note": "same key already on file; no second movement of money", "order_id": "ORD-8120", "currency": "USD", "reason": "Duplicate charge on a single subscription renewal."}
     4	{"n": 4, "action": "payout", "outcome": "created", "idempotency_key": "sha256:72bb24715830eccdf8772fe9a3642d3b85cb918d7d6a2e22f4c97a462dafc056", "provider_transfer_id": "tr_fa0e00d6f752", "amount_cents": 240000, "payee": "****4417", "currency": "USD", "reference": "Card expired before the refund window; paying out by transfer."}
```

Line 3 is the one to read. Same key as line 2, same `provider_refund_id`, and
`"outcome": "replayed"`. The customer was refunded $159.00 once.

### Case 3: a tool the client may not close

`wire-payout.toml` carries `trust_completion = false`. The desk sends the
transfer and reports a result that satisfies the declared output schema. It is
still refused:

```
[desk]   the provider answered: {"provider_transfer_id": "tr_fa0e00d6f752", "status": "succeeded", "amount_cents": 240000, "replayed": false}
[desk]   reporting a completion that satisfies the output schema: {"provider_transfer_id": "tr_fa0e00d6f752", "status": "succeeded", "amount_cents": 240000}
[desk]   REFUSED client_completion_refused: tool `wire_payout` is declared with trust_completion = false, so a client may not record its own completion for it; verify the call externally, then settle it by hand with POST /v1/client-runs/d2f189cc-4569-4a14-92b6-93afc6b18995/resolve
[desk]   the log after the refusal:
[desk]     seq 0  RunStarted
[desk]     seq 1  ModelCallRequested
[desk]     seq 2  ModelCallCompleted  usage in 272 out 58
[desk]     seq 3  ToolCallRequested  wire_payout [write] performed_by=client key=sha256:72bb24715830...
[desk]   run state: needs_reconciliation
```

The refusal names the endpoint that settles it. Someone in payments looks the
transfer up at the provider and records what they saw. The example stands in for
that lookup by reading the provider's own ledger, because the point is that the
answer comes from outside the run:

```
[desk]   a person verifies the transfer at the provider, outside salvor:
[desk]     found on the provider's ledger: {"n": 4, "action": "payout", "outcome": "created", "idempotency_key": "sha256:72bb24715830eccdf8772fe9a3642d3b85cb918d7d6a2e22f4c97a462dafc056", "provider_transfer_id": "tr_fa0e00d6f752", "amount_cents": 240000, "payee": "****4417", "currency": "USD", "reference": "Card expired before the refund window; paying out by transfer."}
[desk]   settled by hand through resolve
[desk]   run state: running
[desk]   the model closed out: Sent $2,400.00 by bank transfer to ****4417, settled by the payments team.
[desk]   run state: completed
[desk]     seq 0  RunStarted
[desk]     seq 1  ModelCallRequested
[desk]     seq 2  ModelCallCompleted  usage in 272 out 58
[desk]     seq 3  ToolCallRequested  wire_payout [write] performed_by=client key=sha256:72bb24715830...
[desk]     seq 4  ToolCallCompleted  {"amount_cents": 240000, "provider_transfer_id": "tr_fa0e00d6f752", "status": "succeeded"}
[desk]     seq 5  ModelCallRequested
[desk]     seq 6  ModelCallCompleted  usage in 352 out 27
[desk]     seq 7  RunCompleted
```

After resolve the run drives again. What resolve recorded is the same
`ToolCallCompleted` the trusted tool wrote for itself in case 1, so a later
replay of this log behaves the same way that one does.

## Why the operator writes the declaration

The declaration carries the effect class, and the effect class decides whether an
unsettled call surfaces for a human. A client that could register its own
declaration would file its own refund as a `read`, and reads are freely retried:
the write-ahead rule that parks case 2 and case 3 would simply stop applying to
it. So declarations are loaded by `salvor serve --client-tool <FILE>`, or by an
embedding host through `AppState::with_client_tools`, and there is no endpoint
that accepts one. The server-performed `tool_step` already refuses to take an
effect from a request body for the same reason.

The output schema belongs to the operator for a related reason. It is the
operator saying which field a report has to carry before salvor will believe it,
and the useful choice is a field the client could not produce on its own. Here
that is `provider_refund_id`. A client writing its own schema would require
nothing, and every report would pass.

## Why the server derives the idempotency key

On the server-performed `tool_step` the client supplies the key. That is fine
there: salvor performs the call, so the party choosing the key is not the party
making the write, and a key chosen badly costs the caller nothing but a retry
that fails to collapse.

Here the client both chooses and performs. That is the one case where the party
choosing the key is the party who stands to gain from a duplicate landing. A
client that wants to be paid twice supplies a fresh key for its second attempt,
the provider sees two distinct calls and honors both, and salvor's log shows two
intents that each look honest. Deriving the key removes the choice. It is a
canonical hash of `(run, seq, tool)`, so the same position always derives the
same key, an honest retry presents the key the first attempt did, and a second
attempt cannot mint itself a new one. Any client can recompute it with a
canonical-JSON SHA-256 and check the server's work.

Case 2 is that mechanism doing its job. The desk retried because its report was
refused, and it retried under a key it did not pick.

## What `trust_completion = false` buys and costs

It buys this: no client report can close the call, whatever it says and whatever
schema it satisfies. The report in case 3 carried a real `provider_transfer_id`
and was refused anyway. What settles the call is a person going to the provider,
looking, and recording what they found. A schema check confirms the shape of a
claim, which for a transfer leaving the card rails is not enough.

It costs a person, every time. Every call for that tool parks its run at
`needs_reconciliation` and stays there until someone acts, which is throughput
traded for verification. It is worth setting when the call is rare, large, and
externally checkable. Setting it on a tool that fires a thousand times a day
produces a thousand parked runs and no more safety than a well-chosen output
schema would give.

The mechanism needed no new run state and no change to the fold. The completion
is refused at the boundary, the log ends at a write intent with nothing after it,
and `derive_state` already calls that `NeedsReconciliation`. Looking for
`trust_completion` in the fold will not find it. The fold is a pure function of
the log and has to stay that way, because a log has to mean the same thing on a
machine that has never seen this server's declaration files.

## What salvor never had

`provider.py` is never named in a declaration, never passed to `salvor serve`,
and never spawned by the control plane. `REFUND_PROVIDER_API_KEY` is set on the
desk's command line, and the provider exits with an error if it is missing from
its own environment. The result of a call reaches salvor when the desk reports
it, and only if the report passes the operator's schema.

The log holds three runs. Each records what was asked for, what class of thing it
was, which key it had to happen under, and what came back.

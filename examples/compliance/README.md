# Compliance gate: mandatory human sign-off, with the event log as the audit record

A Salvor library-tier example of a compliance control: an agent that may not take a consequential action until a human approves it in writing, where the append-only event log is the audit trail of every model decision, tool call, and human approval, and every row in it is chained so a later edit cannot pass as history.

The scenario is a refund. A customer asks for one; the model recommends issuing it; but issuing a refund is consequential, so the run **suspends and waits for a compliance officer's typed sign-off** before anything is written. Approve, and the refund is issued exactly once. Reject, and the run stops with no write at all. Every step (the proposal, the approval request, the officer's decision, and the executed-or-not action) is recorded in the durable log, which is the audit record.

## Why this is a real gate

The approval is implemented as a runtime primitive. When the run reaches the gate it calls `suspend`, which records a `Suspended` event with a typed input schema and parks the run durably; the process exits. The write that follows is only reachable by resuming the run with a decision, which is recorded as a `Resumed` event. So the code path to the consequential action cannot be taken except by way of a recorded human decision. A parked run is rows in a table rather than a blocked thread, so the wait survives the process exiting, a deploy, or a week of review.

The consequential action is an `Effect::Write` tool (`issue_refund`, appending to a ledger file). The runtime records the write's intent *before* the handler runs and the completion after, so on any rerun the recorded completion replays and the handler never executes again. This ensures the refund issues exactly once, even if the approval is resumed twice.

## The approval schema

The resume input a compliance officer submits must satisfy this schema, recorded on the `Suspended` event and validated on resume:

```json
{
  "type": "object",
  "required": ["approver", "decision"],
  "properties": {
    "approver": { "type": "string" },
    "decision": { "type": "string", "enum": ["approve", "reject"] },
    "note":     { "type": "string" }
  }
}
```

The `approver` and `note` are the evidence the audit record keeps: who signed off, and why.

## Runs offline, no API key

The model is a scripted stand-in served over a local mock HTTP server, so the run is deterministic and costs nothing. The single model call records a genuine `ModelCallCompleted` on the park invocation and replays from the log on every resume. The example teaches the durability and the approval control, not model quality, so a canned proposal is the honest choice.

## Run it

One run is driven across separate process invocations that share one SQLite file (`/tmp/salvor-compliance.db`) and one fixed run id, keyed by the `SALVOR_DECISION` environment variable.

```sh
# 1. propose the refund and park at the approval gate
cargo run -p salvor-runtime --example compliance_gate

# 2a. a compliance officer approves: the refund is issued exactly once
SALVOR_DECISION=approve cargo run -p salvor-runtime --example compliance_gate
```

To see the reject path, re-park a fresh run and reject it (a run can only be resumed once):

```sh
cargo run -p salvor-runtime --example compliance_gate
SALVOR_DECISION=reject cargo run -p salvor-runtime --example compliance_gate
```

Each invocation prints the run's event log as the audit trail, then the refund ledger and how many times the write actually executed.

## What to inspect in the audit log

- **The approval is on the record.** The `Suspended` event carries the reason and the approval schema; the `Resumed` event carries the officer's decision verbatim (approver, decision, note). Together they are proof that a human signed off, and what they chose.
- **The write is gated and exactly-once.** On approve, the trail shows a `ToolCallRequested` for `issue_refund` with `effect=Write` and its idempotency key, followed by `ToolCallCompleted`, and the ledger holds one line. Re-run the approve invocation: the whole log replays, the write executes zero more times, and the ledger is unchanged.
- **Reject leaves no write.** On reject, the trail goes straight from `Resumed` to `RunCompleted` with `status=refund_denied`. There is no `ToolCallRequested`, and the ledger stays empty.

The trail is a plain read of the same append-only log that drives replay, so the record and the execution can never disagree.

## What "tamper-evident" means here, exactly

The store hashes each recorded row over the exact envelope bytes it wrote, chained to the hash of the previous row in the same run, and re-walks that chain every time a log is read. So an edit to the approval note, a swapped decision, a `ToolCallCompleted` spliced in between recorded steps to make a write look approved, or a `Resumed` event deleted are all refused on the next read with an error naming the run and the position. That holds even when the replacement is well-formed JSON, which is the case a schema check or a JSON parse would happily wave through. The chain rule is written down in the `salvor_store::chain` rustdoc and needs no secret, so an auditor can recompute it from the database with any SHA-256 implementation.

What it does not claim, because an audit control that oversells itself is worse than none:

- The chain is unkeyed, so everything needed to check it also sits in the database. What it guarantees is that a run's recorded history cannot be quietly revised. It does not stop someone who can write the database file from rebuilding a whole run from its first event forward, or from extending a run with fabricated events chained correctly onto its current head, since both are arithmetically indistinguishable from the real thing. Catching those needs an anchor Salvor does not ship yet: a signature over each run's head hash under a key the store does not hold, or that head hash published somewhere append-only. The head hash per run is where such an anchor would attach.
- The `events` table refuses `UPDATE` and `DELETE` through SQLite triggers. That stops a fat-fingered `sqlite3` session, not an attacker, who can drop a trigger as easily as they can edit a row. The chain is what carries the guarantee.
- The chain proves the bytes have not changed. It says nothing about who wrote them. The `approver` field is as trustworthy as whoever was allowed to submit the resume, which is your access control, not this log's.
- A database written before this scheme existed has its chain computed on first open. From then on it is verified like any other, but the backfill cannot vouch for what happened before it ran.

## Related

- `examples/approval-loop` is the same library tier without the compliance framing: it shows the suspend / resume primitive on its own.
- `examples/todo-agent` is the batteries-included tier, where the built-in loop drives an agent described as data.

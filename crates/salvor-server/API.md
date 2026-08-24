# Salvor control-plane API

This is the HTTP and server-sent-events contract the Salvor control plane
serves. It is the surface the v0.3 SDKs and the dashboard build against, so it
is specified here rather than left implicit in the handlers.

The server is a thin network layer over the durable runtime. It owns one event
store and constructs a runtime per request; every guarantee the CLI has (exact
replay, crash-safe resume, the write-ahead reconciliation rule) holds over HTTP
unchanged, because the same runtime enforces it. Clients stay thin: they submit
data and read events, and hold none of the durability themselves.

All request and response bodies are JSON, except the agent-definition body on
registration (which may be TOML) and the event stream (which is
`text/event-stream`). Every path is versioned under `/v1`.

## Auth

One optional shared-secret bearer token, the single-tenant posture. Two modes:

- **Token set.** Every request must carry `Authorization: Bearer <token>`. A
  missing or wrong token is answered `401` with the standard error envelope.
- **No token.** The server trusts its caller; a reverse proxy is expected to
  own auth. This is the default.

There is no user model and no role system. The `salvor serve --auth-token
<ENV_VAR>` flag names an environment variable holding the token, never the
token itself.

## Error envelope

Every error, whatever its status, has one shape:

```json
{ "error": { "code": "unknown_run", "message": "no run ... in this store" } }
```

`code` is a stable machine token; `message` is a human sentence. A
`details` object is present only when there is structured evidence: the
reconciliation refusal (`details.intent`, below) and an invalid graph submission
(`details.errors`, the full node/edge-precise list). The status codes and their
codes:

| Status | `code` | When |
|---|---|---|
| 400 | `bad_request` | Malformed body, bad run id, or a resume input the recorded schema rejects |
| 401 | `unauthorized` | Missing or wrong bearer token |
| 404 | `unknown_run` | No run under that id |
| 404 | `unknown_agent` | No agent registered under that id (also: a graph run references an unregistered agent) |
| 400 | `invalid_graph` | A submitted graph document failed strict validation; `details.errors` carries the complete node/edge-precise list |
| 400 | `approval_schema_violation` | A graph run parked at a `gate` was resumed with an approval its `approval_schema` does not describe; `details.node` names the gate and `details.violations` lists every violation. Nothing is recorded; the run stays parked |
| 404 | `unknown_graph` | No graph stored under that hash |
| 409 | `not_a_graph_run` | The per-run graph projection (or a fork) was asked for an ordinary agent run |
| 409 | `invalid_fork_node` | A fork named a node the origin never entered |
| 409 | `origin_needs_reconciliation` | A fork of an origin parked at a dangling write; `details.intent` carries the origin's recorded write |
| 409 | `write_replay_hazard` | A fork would re-execute recorded writes the operator has not acknowledged; `details.writes` lists exactly the ones still needing acknowledgement |
| 409 | `run_exists` | Starting a run at an id that already has history |
| 409 | `wrong_state` | A verb applied to a run in the wrong state (resolving a run with no dangling write) |
| 409 | `needs_reconciliation` | Resuming a run whose log ends at a write intent with no completion; `details.intent` carries the recorded write |
| 409 | `still_sleeping` | Resuming a run parked on a durable timer before its instant; `details.wake_at` and `details.remaining_seconds` say when it can be driven. Nothing is recorded |
| 401 | `missing_drive_token` | A client-driven append with no drive token (see [Client-driven runs](#client-driven-runs)) |
| 403 | `invalid_drive_token` | A client-driven append whose drive token is not the run's current lease |
| 409 | `divergence` | A client-driven append that is not the legal next event, or different bytes at an already-recorded position |
| 422 | `unsupported_event_kind` | A client-driven append carrying a model or tool event, which this surface does not accept |
| 413 | `payload_too_large` | A client-driven append over the body-size or per-batch cap |
| 503 | `model_executor_unavailable` | A model step against a server with no model executor wired; no intent is written |
| 502 | `model_execution` | A model step's provider call failed; no completion is recorded, so the intent is left dangling |
| 404 | `unknown_tool` | A tool step naming a tool the server's registry does not hold; no intent is written |
| 503 | `tool_registry_unavailable` | A tool step against a server with no tool registry wired; no intent is written |
| 502 | `tool_execution` | A tool step's dispatch failed; no completion is recorded, so the intent is left dangling |
| 500 | `internal` | A store read or agent build failed unexpectedly |

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/agents` | Register and validate an agent definition |
| GET | `/v1/agents` | List registered agent ids |
| GET | `/v1/agents/{hash}` | Read one registered definition back |
| POST | `/v1/runs` | Start a run |
| GET | `/v1/runs` | List runs with folded status |
| GET | `/v1/runs/{id}` | Get one run's derived state |
| GET | `/v1/runs/{id}/replay` | Dry-run replay: the derived state, executing nothing |
| GET | `/v1/runs/{id}/events` | Stream a run's events (server-sent events) |
| POST | `/v1/runs/{id}/resume` | Continue a run (resume a parked one, recover a crashed one); works on a graph run unchanged |
| POST | `/v1/runs/{id}/resolve` | Record a dangling write's completion by hand |
| POST | `/v1/runs/{id}/abandon` | Retire a run by hand, appending a terminal `RunAbandoned` |
| GET | `/v1/runs/{id}/graph` | A graph run's per-node projection (for the canvas) |
| POST | `/v1/runs/{id}/fork` | Fork a graph run from a node boundary into a new run (refuse-then-record) |
| GET | `/v1/runs/{id}/forks` | The forks of a run, as a derived index |
| GET | `/v1/capabilities` | What this build of the control plane can do (a dashboard probes it) |
| POST | `/v1/graphs` | Submit and strictly validate a graph document |
| GET | `/v1/graphs` | List stored graphs with a shape summary |
| GET | `/v1/graphs/{hash}` | Read one stored graph document back |
| POST | `/v1/graphs/validate` | Validate a document without storing it |
| POST | `/v1/graph-runs` | Start a run of a stored graph |
| GET | `/v1/client-tools` | List the client-performed tool declarations this server holds |
| POST | `/v1/client-runs` | Open or resume a client-driven run |
| GET | `/v1/client-runs/{id}/log` | Read a client-driven run's recorded log |
| POST | `/v1/client-runs/{id}/events` | Append control and context events (the guarded append) |
| POST | `/v1/client-runs/{id}/model-step` | Perform and record a model call (server-performed) |
| POST | `/v1/client-runs/{id}/tool-step` | Perform and record a tool call (server-performed) |
| POST | `/v1/client-runs/{id}/client-tool-intent` | Open a tool call the CLIENT performs, and derive its idempotency key |
| POST | `/v1/client-runs/{id}/client-tool-completion` | Record what a client-performed tool call returned |
| POST | `/v1/client-runs/{id}/resolve` | Record a dangling write's completion by hand (client-driven) |

### POST /v1/agents

Register a definition. Under the single built-in loop an agent is pure data, so
it has a content hash (`agent_def_hash`, the same id every `RunStarted` event
records). A definition is submitted once and referenced by that hash from then
on, so a start payload carries only a hash and an input, and the same
definition drives every start, resume, and recover.

Registration also validates: the server builds the agent (which spawns and
immediately closes any MCP sessions) to confirm it is buildable and to compute
the hash. A definition that will not build is a `400`, which also covers an
out-of-bounds `name` (see below): the server re-validates it at this same
build step, the same as any other client-supplied config, rather than trusting
whatever a submitter's own tooling already checked.

- Request: the definition body. `Content-Type: application/toml` for the agent
  TOML the CLI reads, or `application/json` for the same fields as JSON. Both
  accept an optional top-level `name`: a short display label (at most 64
  characters, and not empty or all whitespace when set) shown by tooling that
  resolves `agent_def_hash` back to something readable. `name` plays no part
  in `agent_def_hash`, so renaming an agent (registering the same definition
  again under a different `name`) never mints a new identity.
- Response `201`:

```json
{ "agent": "sha256:34e0...", "created": true }
```

`created` is `false` when the identical definition was already registered.

The registry is process-local. After a restart, re-register definitions; the
hash is stable, so runs that recorded a reference to it still resolve.

### GET /v1/agents

```json
{ "agents": [ { "agent": "sha256:34e0..." }, { "agent": "sha256:9f2c...", "name": "support-triage" } ] }
```

### GET /v1/agents/{hash}

```json
{ "agent": "sha256:34e0...", "format": "toml", "definition": "model = ..." }
```

or, when the definition declared a `name`:

```json
{ "agent": "sha256:9f2c...", "format": "toml", "definition": "model = ...\nname = \"support-triage\"\n",
  "name": "support-triage" }
```

`name` is present only when the registered definition actually declared one;
there is no meaningful empty name to fall back to, so an agent registered with
none omits the field entirely from both this response and the list above,
rather than emitting `"name": null`. This is the same honest-absence rule
[`GET /v1/runs`](#get-v1runs) already applies to `agent_def_hash` and
`labels`.

`404 unknown_agent` when the hash is not registered.

### POST /v1/runs

Start a fresh run. Returns at once with the run id; the run then drives in the
background (see [Driving a run](#driving-a-run)).

- Request:

```json
{ "agent": "sha256:34e0...", "input": <any json>, "run_id": "<uuid, optional>",
  "labels": { "build": "42", "env": "prod" } }
```

`input` defaults to `null`. `run_id` is optional; when omitted the server mints
one. Passing one lets a client choose the id (a UUID).

`labels` is optional: free-form correlation tags recorded once on the run's
`RunStarted` event, so runs can be told apart in a list (which resume build
was this). At most 16 labels, each key at most 64 bytes and each value at
most 256 bytes; a label is a tag, not a payload. A run started with no
`labels` records none, and that is a different, honest thing from an empty
object (see [`GET /v1/runs`](#get-v1runs) below): omit the field rather than
send `{}` unless an explicit empty set is genuinely what is meant.

- Response `201`:

```json
{ "run": "6f...uuid", "status": "running" }
```

- `400 bad_request` when `labels` violates the bounds above. Checked before the
  run is spawned, so a rejected request creates no run at all.
- `404 unknown_agent` when the agent is not registered.
- `409 run_exists` when the chosen `run_id` already has history.

### GET /v1/runs

```json
{ "runs": [
  { "run": "6f...", "status": { "state": "running" },
    "event_count": 10, "first_recorded_at": "2026-...", "last_recorded_at": "2026-...",
    "usage": { "input_tokens": 250, "output_tokens": 50 },
    "step_count": 2,
    "agent_def_hash": "sha256:34e0...",
    "labels": { "build": "42", "env": "prod" },
    "driver": "attached" }
] }
```

Status is folded from each log, not stored, so it is always current.

`usage`, `step_count`, `agent_def_hash`, and `labels` are additive fields,
folded from the same per-run log read and fold `status` has always come from,
so listing does not read a run's log twice. `usage` is the same shape as
[`GET /v1/runs/{id}`](#get-v1runsid)'s `usage`. `step_count` is how many
`ModelCallRequested` events the run's log holds. `agent_def_hash` is the hash
recorded on the run's `RunStarted` event, the same value
[`POST /v1/agents`](#post-v1agents) returned when the definition was
registered. `labels` is whatever correlation tags were recorded on
`RunStarted` at [`POST /v1/runs`](#post-v1runs) time.

**Honest absence, not zero.** `usage`, `step_count`, and `agent_def_hash` are
present, and are real counts, whenever a run's log folds: a run with no model
calls yet reports a true `step_count: 0` and `usage` of all zeros, not a
missing field, because that zero is known. They are *absent* (omitted from the
object entirely, per `skip_serializing_if`, never `null` and never `0`) only
when a run's log cannot be read at all (a corrupt or unreadable stored
envelope). That failure is scoped to the one run whose row it is: the store's
per-run summary (`event_count`, `first_recorded_at`, `last_recorded_at`) is a
cheap aggregate that never touches the row's JSON payload, so it, and even
`status`, which also depends on the unreadable log, are the only fields such
a run's entry carries. Before this fold ran, a single unreadable log failed
the whole listing (`500`); now it degrades only that one entry, so this is
additive: old consumers reading only the pre-existing fields see the exact
same JSON for every run whose log reads cleanly.

`labels` follows the same absence rule, one step further: it is omitted both
when a run recorded no labels at all (an unlabeled run, or one from before
labels existed) *and* when it recorded an explicit empty set. The API never
emits `"labels": {}`, because an empty map is not a fact worth asserting any
more than an unknown count is. A run that recorded at least one label reports
exactly what was recorded.

**Liveness evidence: `driver`.** `driver` reports whether a driver is currently
running the run: `"attached"` or `"none"`. It reads no log. It consults only
the process's own driving-run set and client-run leases, the truth the server
already holds:

- A **server-driven** run is `"attached"` exactly when a driver task is still
  running it in this process, the same fact the event stream's `detached`
  end-frame reports (see [`GET /v1/runs/{id}/events`](#get-v1runsidevents)). The
  task is dropped the instant it ends (completes, parks, or errors), so this is
  exact, not a heuristic.
- A **client-driven** run is `"attached"` exactly when this process holds a
  current lease for it: the driver presented its drive token within the lease
  TTL (default 60s; `salvor serve` reads `SALVOR_CLIENT_LEASE_TTL_SECS`). A
  lapsed lease (the tab closed, the SDK exited) is `"none"`.

`driver` follows the same zero-vs-absent rule as the folded fields, one way: it
is **omitted entirely for a terminal run** (`completed` or `failed`), because
whether a driver is attached to a finished run is not a meaningful question, and
`"driver": "none"` would assert a placeholder where there is no fact. Every
non-terminal run reports a real `"attached"` or `"none"`; a run whose log could
not be read omits it along with `status` (there is no folded status to gate it).

`driver` is **evidence, not a verdict.** Paired with `last_recorded_at` (the
newest envelope's timestamp, the "when did anything last happen" fact), it is
what a dashboard reads to derive a *stalled* state: a run that folds to
`running` yet reports `"driver": "none"` and whose `last_recorded_at` has gone
stale is going nowhere: resolved but never re-driven, its driver crashed, or
its client abandoned. The server reports the two facts; the client derives the
stalled verdict, the same division of labor `status` itself has.

### GET /v1/runs/{id}

The run's derived state:

```json
{
  "run": "6f...",
  "status": { "state": "suspended", "reason": "...", "input_schema": { ... } },
  "event_count": 6,
  "usage": { "input_tokens": 250, "output_tokens": 50 },
  "pending": { "kind": "tool", "seq": 5, "tool": "record", "input": ...,
               "effect": "write", "idempotency_key": null },
  "first_recorded_at": "2026-...",
  "last_recorded_at": "2026-...",
  "driver": "attached"
}
```

`404 unknown_run` when the id has no history and no run is being driven under it.

`driver` is the same liveness evidence [`GET /v1/runs`](#get-v1runs) carries,
under the same rules: `"attached"` / `"none"` for a non-terminal run, omitted
for a terminal one. A run with no history yet but a driver task already spawned
reports `"driver": "attached"` alongside its `running` status.

#### The status object

Always `{ "state": "<name>", ... }`:

| `state` | Extra keys |
|---|---|
| `not_started`, `running`, `awaiting_model`, `awaiting_tool`, `needs_reconciliation` | none |
| `suspended` | `reason`, `input_schema`, `kind` (only `"signal"`, and only when present) |
| `sleeping` | `wake_at` (RFC 3339); once the server's clock is past it, also `overdue` (`true`) and `overdue_seconds` (whole seconds since `wake_at`) |
| `budget_exceeded` | `budget` (`{kind, limit}`), `observed` |
| `completed` | `output` |
| `failed` | `error` |
| `abandoned` | `reason` (when given), `unresolved_write` (`{seq, tool}`, only when a needs-reconciliation run was abandoned) |

On `suspended`, `kind` says what the run is waiting on when it is not a person:
`"signal"` means an external system (a webhook, a callback) will resume it, so
it is nobody's task and belongs nowhere near an approval inbox. The key is
absent for a human gate, which is what every suspension without it means.

It resumes exactly the way anything resumes a run: whatever holds the bearer
token calls `POST /v1/runs/{id}/resume` (the CLI equivalent is `salvor resume`)
with a payload the recorded `input_schema` accepts. There is no separate
webhook endpoint and no per-run secret, so nothing stops a resume that arrives
before the real signal; the token is the whole boundary (see the README's
"Operating it" section and `SECURITY.md`'s single shared secret). A signal
wait carries no deadline of its own and waits until something resumes it; an
operator can stand in for a signal that never comes with `salvor resume` or
the Bridge Inspector's "Awaiting a signal" band, the only place the Bridge
offers that action, since a signal wait never appears in the Inbox.

#### The pending object

`null`, or one of:

```json
{ "kind": "model", "seq": 3, "request_hash": "sha256:..." }
{ "kind": "tool", "seq": 5, "tool": "...", "input": <json>,
  "effect": "read|idempotent|write", "idempotency_key": "..."|null }
```

### GET /v1/runs/{id}/replay

The dry-run replay projection: the full derived state as a pure fold of the
recorded log, executing nothing. This is what the CLI's `replay --dry-run`
prints, as JSON.

```json
{
  "status": { "state": "completed", "output": ... },
  "usage": { "input_tokens": 250, "output_tokens": 50 },
  "next_seq": 10,
  "pending": null
}
```

`404 unknown_run` when the id has no history.

### GET /v1/runs/{id}/events

The event stream. `Content-Type: text/event-stream`.

#### Framing

Every recorded event is one frame:

```text
id: 4
data: {"run_id":"6f...","seq":4,"schema_version":1,"recorded_at":"...","event":{"kind":"ToolCallCompleted","payload":{...}}}

```

- `data` is exactly the pinned event-envelope wire JSON, the same bytes
  `GET /v1/runs/{id}` events come from and `salvor history --json` prints, so a
  client decodes stream frames and log rows with one parser.
- `id` is the event's sequence number.
- Envelope frames carry no `event:` field, so a browser `EventSource` receives
  them through `onmessage`.
- A `Suspended` event's payload carries the same optional `kind` the status
  object does: `"signal"` with the same meaning and absence rule described
  under [the status object](#the-status-object), so an SSE consumer can build
  the same filter the Bridge does. The full payload is `{"reason": "...", "input_schema": {...}}` for a human gate and `{"reason": "...", "input_schema": {...}, "kind": "signal"}` for a signal wait.
- When the run reaches a resting point (completed, failed, abandoned,
  suspended, sleeping, budget-exceeded, or needs-reconciliation) the stream
  sends one final frame with `event: end` carrying the status it rested at,
  then closes:

```text
event: end
data: {"status":{"state":"completed","output":...}}

```

  A run parked on a durable timer rests too, and its frame carries the deadline:

```text
event: end
data: {"status":{"state":"sleeping","wake_at":"2026-08-14T09:00:00Z"}}

```

  Nothing drives a sleeping run, and its nap is measured in hours or days, so
  the stream closes rather than polling for the duration. `wake_at` is when to
  open a fresh stream; the events the wake records are read by that one.

If the driving task was killed and no driver is running the run in this process,
the end frame also carries `"detached": true`; recovering the run opens a fresh
stream that tails the continuation.

#### Replay then live tail

On connect the server sends every recorded event at or after the cursor, then
polls the store and sends new events as they land, until the resting frame. A
log is append-only with contiguous ascending sequence numbers, so the stream is
gap-free and duplicate-free by construction.

#### Cursor: resuming a dropped stream

A dropped connection resumes without gaps or duplicates:

- **`Last-Event-ID` header.** A browser `EventSource` resends the last `id` it
  saw. The server resumes from that sequence plus one.
- **`?from_seq=<n>` query.** A non-browser client that tracks its own position
  asks for events from sequence `n` onward.

`Last-Event-ID` wins when both are present. With neither, the stream starts at
sequence 0 (a full replay).

`404 unknown_run` when the id has no history and no run is being driven under it.

### POST /v1/runs/{id}/resume

Continue a run. The server reads the run's derived state and dispatches on it,
the same mapping `salvor resume` uses:

- **Parked** (suspended or budget-exceeded): the request must carry an `input`,
  validated against the recorded suspension schema or the budget-extension
  shape before anything is recorded. The run then resumes in the background.
- **Crashed** (running, or interrupted mid model or tool step): the run
  recovers with no input. An `input` in the body is ignored.
- **Sleeping**: past its `wake_at`, it re-drives exactly as a crashed run does,
  which is what waking is. Before its `wake_at`, refused `409 still_sleeping`
  with the deadline as evidence (see below).
- **Needs reconciliation**: refused `409`, with the recorded write intent as
  evidence (see below). Use `resolve` to move past it.
- **Finished** (completed or failed): reported, `200`, left alone.

- Request (optional body):

```json
{ "input": <any json> }
```

- Response `202` for a run now driving:

```json
{ "run": "6f...", "status": "running", "outcome": "driving" }
```

- Response `200` for an already-finished run:

```json
{ "run": "6f...", "outcome": "completed", "status": { "state": "completed", "output": ... } }
```

- `400 bad_request` when a parked run is resumed with no input, or with an
  input the recorded schema rejects. `{"input": null}` is an input of `null`,
  not an absent one: it reaches validation like any other value.
- `400 approval_schema_violation` when a graph run parked at a `gate` is resumed
  with an approval the gate's `approval_schema` does not describe. The check
  runs before anything is spawned or appended, so the run is still parked at the
  gate and a corrected approval resumes it:

```json
{ "error": {
  "code": "approval_schema_violation",
  "message": "the approval does not satisfy gate `approve`'s approval_schema; the run is still parked at that gate, so a conforming approval resumes it",
  "details": {
    "node": "approve",
    "violations": [ { "path": "$.approved", "message": "\"yes\" is not of type \"boolean\"" } ]
  }
} }
```

  An `approval_schema` that names `required` or `properties` without a `type` is
  read as asking for an object, so a bare `null`, number, or string is refused
  even though plain JSON Schema semantics would let it pass vacuously.
- `409 needs_reconciliation`:

```json
{ "error": {
  "code": "needs_reconciliation",
  "message": "run ... needs reconciliation: a write was recorded but never completed ...",
  "details": { "intent": {
    "kind": "tool", "seq": 4, "tool": "charge", "input": { "amount": 10 },
    "effect": "write", "idempotency_key": null, "recorded_at": "2026-..."
  } }
} }
```

- `409 still_sleeping` when the run is parked on a durable timer whose instant
  has not arrived. Nothing is recorded and no driver is spawned, so the run is
  exactly as asleep as it was; retry at `wake_at` or later, or leave it to the
  wake sweeper if this server holds the agent or graph the run recorded, or run
  `salvor wake` with the run's files if it does not:

```json
{ "error": {
  "code": "still_sleeping",
  "message": "run ... is sleeping until 2026-08-14T09:00:00Z and cannot be resumed for another 1740s. It is not waiting on input: it continues when its deadline passes and something re-drives it: this server's wake sweeper, when it holds the agent or graph the run recorded, or salvor wake with the run's files",
  "details": { "wake_at": "2026-08-14T09:00:00Z", "remaining_seconds": 1740 }
} }
```

- `404 unknown_agent` when the agent the run started under is not registered on
  this server (re-register it, then resume).

To watch the continuation, open the event stream after a `202`.

### POST /v1/runs/{id}/resolve

Record the completion of a dangling write by hand, the operator side of
reconciliation. After a human has verified externally what a recorded-but-never
-completed write did, this records the completion they observed, so replay
treats the call as done and never re-runs it. It records exactly one event and
drives nothing.

- Request:

```json
{ "output": <the json the tool returned> }
```

- Response `200`:

```json
{ "run": "6f...", "resolved": true, "status": { "state": "running" } }
```

- `409 wrong_state` when the run does not need reconciliation (there is no
  dangling write to resolve).
- `404 unknown_run` when the id has no history.

### POST /v1/runs/{id}/abandon

Retire a run by hand. A deliberate sibling of `resolve`: the operator's "we do
not care about this run anymore" path, for a run that is dead forever or no
longer worth carrying in the inbox. It validates the run is non-terminal,
appends one terminal `RunAbandoned` event server-stamped through the append
guard, and returns the receipt: the appended seq and the re-derived status.
Abandonment is **not** failure: `RunFailed` is untouched, and the run derives to
its own terminal `abandoned` status, treated as terminal (never attention)
everywhere downstream.

**Why no lease.** Abandon is an operator action over the store, not a step in
driving the run, so it presents no drive token and needs no lease, unlike a
client-driven append. It works for any run in the store whatever drove it (a
server task, a client SDK, or nothing at all anymore); the very case it exists
for is a run no driver is coming back to. The append guard's terminal rule is
the only concurrency protection it needs: a run that reached a terminal first
refuses the abandonment.

- Request (optional body; an empty body abandons with no reason):

```json
{ "reason": "husk is dead forever" }
```

- Response `200`:

```json
{ "run": "6f...", "abandoned": true, "appended_seq": 7,
  "status": { "state": "abandoned", "reason": "husk is dead forever" } }
```

- **Needs reconciliation is allowed, not refused.** Abandoning a run parked at a
  dangling write is the case abandonment most needs to serve. The endpoint
  computes the outstanding write from the log's dangling intent and records it on
  the event, so the terminal status carries an `unresolved_write` and never
  claims the write question was answered:

```json
{ "run": "6f...", "abandoned": true, "appended_seq": 5,
  "status": { "state": "abandoned",
    "unresolved_write": { "seq": 4, "tool": "charge" } } }
```

- `409 wrong_state` when the run is already terminal (completed, failed, or
  previously abandoned); there is nothing left to abandon.
- `404 unknown_run` when the id has no history.

## Graphs and graph runs

A graph document is a control document: an acyclic set of nodes (`agent`,
`tool`, `gate`, `branch`, `map`, `fold`, `delay`) authored once, submitted,
hashed, and frozen for a run. Every node payload may carry an optional `name`: a short display
label (at most 64 characters, and, when set, not empty or all whitespace;
`400 invalid_graph` reports a violation node-precise as `node_name_too_long`
or `blank_node_name`). Unlike an agent definition's own `name` (excluded from
its `agent_def_hash` so a rename never mints a new agent identity), a node's
`name` is an ordinary field on the payload and hashes like any other: a graph
document IS its content hash, so renaming a node is authoring a new document
version, by design. A graph run is an ordinary run with a richer log: its head is
`GraphRunStarted` instead of `RunStarted`, and its nodes narrate the walk, so
[`GET /v1/runs/{id}`](#get-v1runsid), [`/replay`](#get-v1runsidreplay),
[`/events`](#get-v1runsidevents), the enriched [`GET /v1/runs`](#get-v1runs)
list, and [`POST /v1/runs/{id}/resume`](#post-v1runsidresume) all work on it
through their existing code. A graph run has no single `agent_def_hash` (it
coordinates many), so that field is simply absent from its run-list entry:
honest absence, exactly as `labels` is absent when a run recorded none.

### Resolution and the tool story

Starting a graph run resolves what the document references against the server's
live inventory, synchronously, before the run is spawned: every `agent` node's
hash must be a **registered agent** (built through the same factory an agent run
uses), and every `tool` node's tool must be present in the server's **tool
registry**, the SAME registry a client-driven [tool step](#post-v1client-runsidtool-step)
dispatches through. No separate tool-registration surface exists.
`salvor serve` wires that registry EMPTY, so on a stock server every `tool` node
is a precise `404 unknown_tool` until a host registers the tool it names.

### POST /v1/graphs

Submit a graph document (the body is the document JSON). The server validates it
strictly and all at once (collect-all, no short-circuit), and stores it
content-addressed by its reproducible hash.

- Response `201`:

```json
{ "graph": "sha256:...", "created": true }
```

`created` is `false` when the identical document was already stored
(re-submitting is idempotent: same document, same hash).

- `400 invalid_graph` on any validation failure. `details.errors` is the
  complete list; each entry has a `code`, a `message`, and the node or edge it
  names, for example:

```json
{ "error": { "code": "invalid_graph", "message": "the graph document has 1 validation error(s)",
  "details": { "errors": [
    { "code": "dangling_edge", "message": "edge `approve` -> `ghost` references unknown node id `ghost`",
      "edge": { "from": "approve", "to": "ghost" }, "missing": "ghost", "suggestion": null }
  ] } } }
```

A document that does not even parse strictly (an unknown field, a missing one)
is one `invalid_graph` error with code `malformed_document`.

### GET /v1/graphs

```json
{ "graphs": [ { "graph": "sha256:...", "node_count": 3, "edge_count": 2,
               "entry_nodes": ["research"], "terminal_nodes": ["publish"] } ] }
```

### GET /v1/graphs/{hash}

```json
{ "graph": "sha256:...", "document": { "schema_version": 1, "nodes": [ ... ], "edges": [ ... ] } }
```

`404 unknown_graph` when nothing is stored under the hash.

### POST /v1/graphs/validate

Validate a document without storing it: submit's dry run, the graph counterpart
of `/replay`. It always answers the question rather than treating an invalid
document as a bad request:

- Response `200`, valid:

```json
{ "valid": true, "graph": "sha256:...", "summary": { "node_count": 3, "edge_count": 2,
  "entry_nodes": ["research"], "terminal_nodes": ["publish"] } }
```

- Response `200`, invalid: `{ "valid": false, "errors": [ ... ] }`, the same
  node/edge-precise list `POST /v1/graphs` refuses with. Nothing is ever stored.

### POST /v1/graph-runs

Start a run of a stored graph and return its id at once (the same fire-and-return
shape [`POST /v1/runs`](#post-v1runs) uses).

- Request:

```json
{ "graph_hash": "sha256:...", "input": { ... }, "labels": { "build": "42" } }
```

`input` defaults to `null`; `labels` is optional (same bounds as an agent run's).

- Response `201`: `{ "run": "6f...", "status": "running" }`.
- `404 unknown_graph` when the hash names no stored graph.
- `400 bad_request` when `labels` violates the bounds.
- `404 unknown_agent` (naming the node) when an `agent` node references an
  unregistered agent; `404 unknown_tool` (naming the node) when a `tool` node
  names a tool the registry does not hold. Both are resolved up front, so a run
  is spawned only once everything it references resolves.

A parked graph run (a `gate`, a budget crossing) resumes through the ordinary
[`POST /v1/runs/{id}/resume`](#post-v1runsidresume): the server re-drives it over
the engine, looking the graph document back up by the hash the log records. A run
parked at a `gate` has its approval checked against that gate's `approval_schema`
first, and a non-conforming one is `400 approval_schema_violation` naming the node
and listing every violation, with nothing recorded and the run left parked.

### GET /v1/runs/{id}/graph

A graph run's per-node projection, for the canvas: which nodes the walk has
reached, which case each `branch` fired, and any `map` fan-out. Absent-vs-null
throughout: a node's `branch_case` and `map` appear only when recorded, and a
node the walk has not reached is simply absent (distinct from a `skipped` one).

```json
{ "graph_hash": "sha256:...", "current_node": "approve", "nodes": [
  { "node": "research", "state": "exited" },
  { "node": "approve", "state": "entered" },
  { "node": "reject", "state": "skipped", "reason": "no live inbound edge: an upstream branch routed to another case" }
] }
```

`state` is `entered`, `exited`, or `skipped` (with a `reason`). `current_node` is
present only while a node is entered and not yet exited. A forked run's
projection also carries a `forked_from` object (the `ForkOrigin` record:
`run_id`, `through_seq`, `from_node`, `graph_hash`, `acknowledged_writes`).

- `404 unknown_run` when the id has no history.
- `409 not_a_graph_run` when the run is an ordinary agent run (no
  `GraphRunStarted` head), mirroring the other `409` shapes.

### POST /v1/runs/{id}/fork

Fork a graph run from a node boundary into a NEW run, and (the differentiator)
refuse to re-execute a recorded write the operator has not acknowledged.

A fork is a new run whose log opens with the origin's prefix (every event below
the fork node's `NodeEntered`) rewritten under the fork's own id, its seq-0
`GraphRunStarted` carrying `forked_from`. The origin is never touched. The child
then continues from the fork node exactly as a recovered graph run does. A fork
reuses the origin's graph unchanged (it may not edit it): to change the graph,
submit a new document and start a fresh run.

- Request:

```json
{ "from_node": "publish", "acknowledge_writes": [4], "dry_run": false }
```

`from_node` is the node boundary to restart from. `acknowledge_writes` (default
`[]`) are the origin log positions of the `Effect::Write` intents in the
re-walked segment the operator accepts may re-fire; they must cover the full
hazard set. `dry_run` (default `false`) previews without creating a run.

- Response `201`: `{ "run": "<child>", "status": "running", "forked_from": {
  "run_id": "<origin>", "through_seq": 3, "from_node": "publish", "graph_hash":
  "sha256:...", "acknowledged_writes": [4] } }`. The `acknowledge_writes` seqs
  are recorded permanently into the child's `forked_from.acknowledged_writes`.
- `409 write_replay_hazard` when the re-walked segment holds unacknowledged
  writes (the refuse-then-record refusal):

```json
{ "error": {
  "code": "write_replay_hazard",
  "message": "forking run ... would re-execute 1 recorded write(s) ...",
  "details": { "writes": [
    { "seq": 4, "tool": "publish", "input": { ... },
      "idempotency_key": null, "recorded_at": "2026-..." }
  ] }
} }
```

  `details.writes` lists exactly the writes still needing acknowledgement (all of
  them on a first, bare fork; a partial acknowledgement narrows it to what is
  missing). Acknowledging every listed `seq` lets the fork proceed. Idempotent
  tools are NOT listed: a graph tool's idempotency key is derived from its
  position in the graph (graph hash, node id, call index), so a fork presents the
  same key its origin recorded and the provider collapses the duplicate, so
  `Write` is the only class needing acknowledgement.
- `409 origin_needs_reconciliation` (with the origin's `details.intent`) when the
  origin is parked at a dangling write; resolve the origin first.
- `409 invalid_fork_node` when the origin never entered `from_node`.
- `409 not_a_graph_run` when the run is an ordinary agent run.
- `404 unknown_graph` when the origin's graph is no longer stored (graphs are
  in-memory and do not survive a restart); resubmit the identical document, then
  fork.
- `dry_run: true` returns `200` with `{ "dry_run": true, "origin": "<id>",
  "from_node": "publish", "through_seq": 3, "graph_hash": "sha256:...",
  "prefix_event_count": 4, "writes": [ ... ], "unacknowledged_writes": [4],
  "would_proceed": false }` and creates nothing. The structural refusals above
  still apply under `dry_run` (a fork that could never proceed is reported, not
  faked).

### GET /v1/runs/{id}/forks

The forks of a run, as a DERIVED index. The origin is immutable and never points
forward at its children; this answer is a server-side scan of every run's
`forked_from`, labeled `"derived": true` to say so. It is not a fact the origin
recorded.

```json
{ "run": "<id>", "derived": true, "forks": [
  { "run": "<child>", "from_node": "publish", "through_seq": 3, "acknowledged_writes": [4] }
] }
```

- `404 unknown_run` when the id has no history.

### GET /v1/capabilities

What this build of the control plane can do, for a dashboard to probe before
offering a capability-gated action. Additive and honest: a capability is
advertised only when the feature genuinely exists on this server.

The sibling `server` object names the exact build serving the response.
`server.version` is the running binary's own `CARGO_PKG_VERSION`, so it always
agrees with `salvor --version` for the same build. `server.commit` is the
short git hash the build was compiled from, present only when that build had
a `.git` history to read at compile time (a source tarball or a checkout with
no `git` on `PATH` omits the key entirely rather than sending a placeholder
like `"unknown"`), and suffixed `-dirty` when the working tree carried
uncommitted changes at build time.

```json
{
  "capabilities": { "fork": true },
  "server": { "version": "0.1.0", "commit": "a1b2c3d" }
}
```

A build with no commit information available:

```json
{ "capabilities": { "fork": true }, "server": { "version": "0.1.0" } }
```

### Submitting a graph from the CLI

The `salvor` CLI drives graphs LOCALLY (`salvor graph run <graph.json> --input
<json> [--agent <file> ...]`), the same way `salvor run` drives an agent run; it
has no remote-verb convention, so it does not submit graphs to a server. Submit
and validate over HTTP with `curl` (or an SDK) against the endpoints above.

`salvor fork <run> --from-node <id> --graph <graph.json> [--agent <file> ...]
[--acknowledge-writes <seq,seq|all>] [--dry-run]` is the local flavor of the fork
endpoint: it re-supplies the origin's document (hash-checked against the recorded
one, since a fork reuses the graph unchanged), plans the fork, and drives the
child onward from the fork node, refusing any write the re-walked segment would
re-fire until `--acknowledge-writes` covers it. Same refuse-then-record contract
as the endpoint, exit 1 on an unacknowledged hazard.

## Client-performed tools

### GET /v1/client-tools

Every client-performed tool declaration this server was started with: the
tools an operator declared with `salvor serve --client-tool <FILE>`, which the
client runs itself, in its own process (see `POST
/v1/client-runs/{id}/client-tool-intent` below). This server holds no code for
them; the declaration is name, effect, and schemas, nothing else.

This is how a client-driven loop gets the model's function definitions. A
declaration's `input_schema` IS the tool's parameter schema, the exact one
this server checks a client-tool intent's input against, published here so a
client never keeps a second copy of it that can quietly drift from the one
the server validates against.

No drive token: this lists server configuration, not run state, so it sits
behind only the bearer-auth layer every other `/v1` route sits behind.

- Response `200`:

```json
{
  "client_tools": [
    {
      "name": "charge_card",
      "effect": "write",
      "input_schema": { "type": "object", "required": ["amount_cents"], "...": "..." },
      "output_schema": { "type": "object", "required": ["charge_id"], "...": "..." },
      "trust_completion": true
    }
  ]
}
```

`output_schema` is present only when the declaration carries one, following
the zero-vs-absent rule `GET /v1/agents` already applies to `name`: a tool
declared without an output schema cannot be self-completed by a client (see
`client-tool-completion` below), and there is no genuinely empty schema to
fall back to, so the key is omitted entirely rather than sent as `null`.

A server started with no `--client-tool` files answers `{ "client_tools": [] }`,
not an error: nothing declared is a complete, honest state, the same one every
client-tool intent already answers with a clean `unknown_tool`.

Declarations are loaded by the operator and are never registered over HTTP;
see `client-tool-intent` below for why that is not an omission.

## Client-driven runs

Everything above is the server-driven control plane: the server owns the loop
and drives it in a background task. The endpoints in this section are a second,
additive mode that moves ownership of the loop to the client while the server
keeps ownership of the log. The client (a browser folding a run's log in a wasm
`ReplayCursor`, or an SDK) owns the loop and streams the events it produces;
the server owns the durable log
and, on every append, re-folds the log with the pure `salvor-replay`
append-guard to confirm the incoming event is the one legal next event. The two
modes never collide: a client-driven run and a server-driven run cannot share an
id, and each surface serves only its own runs.

Timers and signals are out of scope for a client-driven run, for now (design
decision). A client-driven run that records a sleep is the client's to wake:
the server's wake sweep leaves every client-driven run alone, current lease
or lapsed, because re-driving one from this process would be a second writer
racing the client's own drive token for the same sequence numbers.

The generic append carries only the control and deterministic-context events the
client's cursor emits itself, which hold no secret and no side effect:
`RunStarted`, `NowObserved`, `RandomObserved`, `Suspended`, `Resumed`,
`SleepStarted`, `SleepCompleted`, `BudgetExceeded`, `RunCompleted`,
`RunFailed`. The side-effecting steps, which the server must perform because it
holds the key or the binary, have their own endpoints: the model call is the model-step endpoint and the tool call is the
tool-step endpoint below, and a model or tool event is still refused on the
generic append.

### The drive token

Opening a client-driven run mints a per-run `drive_token`: the single-writer
lease. Every append must present it in the `X-Drive-Token` header. It is the
per-run gate that layers on top of the process-wide bearer, so one authenticated
caller cannot drive another caller's run, and a second live driver without the
current lease is refused. Re-opening a run mints a fresh lease, so a resuming tab
always holds the current one; the superseded lease stops working.

### POST /v1/client-runs

Open a fresh client-driven run, or re-open (resume) one this server holds.

- Request:

```json
{ "agent": "sha256:34e0...", "input": <any json>, "run_id": "<uuid, optional>",
  "record_prompts": false }
```

`run_id` is optional; when omitted the server mints one. `agent` and `input` are
accepted for forward compatibility with the server-performed model step; this
endpoint records them nowhere, because the client appends its own
`RunStarted` (carrying the agent hash and input) as the run's first event.
`record_prompts` is stored against the run for the server-performed step.

- Response `201` for a fresh run:

```json
{ "run": "6f...", "drive_token": "dt_...", "log": [] }
```

The empty `log` is what the client builds its cursor from. The client then
appends its own `RunStarted` at seq 0 through the events endpoint.

- Response `200` for a re-open of a run this server already holds: the same
  shape, with `log` carrying every recorded envelope and a fresh `drive_token`.
- `409 run_exists` when the chosen `run_id` already has history and is not a
  client-driven run this server opened (a server-driven run, so the two modes
  cannot collide).

### GET /v1/client-runs/{id}/log

The recorded envelopes, for a refreshed tab to rebuild its cursor.

```json
{ "log": [ <envelope>, ... ] }
```

Each envelope is exactly the pinned event-envelope wire JSON the event stream
and `salvor history --json` use. `?from_seq=<n>` returns only envelopes at or
after `n`, so a client that already holds a prefix fetches just the tail. The
read needs no drive token, but it serves only client-driven runs this server
opened. `404 unknown_run` otherwise.

### POST /v1/client-runs/{id}/events

The generic guarded append. Requires the `X-Drive-Token` header.

- Request:

```json
{ "events": [ <EventEnvelope>, ... ] }
```

- Response `200`:

```json
{ "appended": [ <seq>, ... ] }
```

The server re-folds and appends the batch in order. The whole batch is validated
before anything is written, so a batch that turns illegal appends nothing.

A `RunStarted` event in the batch may carry `labels` (the client builds this
event itself; see [POST /v1/client-runs](#post-v1client-runs) above). The same
bounds `POST /v1/runs` enforces apply here, at the one point this server ever
sees them for a client-driven run: at most 16 labels, each key at most 64
bytes, each value at most 256 bytes. A `RunStarted` carrying labels over the
bounds is `400 bad_request`, and nothing in the batch is written.

Every envelope's `recorded_at` is **server-stamped**: the server overwrites it
with its own clock reading before folding or writing the event, regardless of
what the submitted envelope carries in that field. `recorded_at` is a required
field on the wire (the pinned `EventEnvelope` shape every event stream and
`salvor history --json` share), so a client must still send one, but its value
is ignored entirely: a client may send the current time, the Unix epoch, or
anything else, and the server's stamp always wins. This keeps `recorded_at`
meaning "when this store durably recorded the event" rather than "whatever a
browser's clock happened to read," uniformly with the model-step and tool-step
endpoints below, which have always stamped their own intents and completions
this way.

Semantics, keyed by sequence number:

- A byte-identical re-append at an already-recorded seq is a `200` no-op (the
  retry-safe case: a tab resends after a network blip). Its seq is still
  reported in `appended`, and the log does not grow.
- Different bytes at an already-recorded seq is `409 divergence`.
- An illegal next event (a wrong sequence number, a completion that does not
  correlate to its intent, a second pending intent, an event after a terminal
  event, or a malformed head) is `409 divergence`, with the append-guard's
  precise reason in `message`.
- A model or tool event is `422 unsupported_event_kind`: those are recorded
  through the model-step and tool-step endpoints, or, for a call the client
  performs in its own process, through the client-tool-intent and
  client-tool-completion endpoints. A client-performed tool call is possible; it
  is just not possible by hand-appending an event, because the effect class, the
  input check, and the idempotency key are the server's to decide from an
  operator's declaration.
- A missing drive token is `401 missing_drive_token`; a token that is not the
  run's current lease is `403 invalid_drive_token`.
- A body over the `8 MB` cap, or a batch over 1024 events, is `413
  payload_too_large`.

### POST /v1/client-runs/{id}/model-step

The server-performed model call. The client owns the loop and decides when to
call the model and with what; the server performs the call (it holds the key)
and records it. Requires the `X-Drive-Token` header.

- Request:

```json
{ "seq": 3, "request": <MessageRequest as JSON> }
```

`seq` is the log position the client's cursor reserved for the model intent.
`request` is the client's canonical model request value. The server recomputes
`request_hash` from `request` with the same canonical hash the runtime uses, so
the client cannot record a hash that does not match what was sent.

- Response `200`:

```json
{ "response": <MessageResponse as JSON>, "usage": { "input_tokens": 10, "output_tokens": 5 } }
```

The server appends `ModelCallRequested { seq, request_hash, request_body? }`
write-ahead (the body is recorded only when the run was opened with
`record_prompts: true`), performs the call through the injected model executor,
appends `ModelCallCompleted { seq, response, usage }`, and returns the
completion. The client feeds the response and the hash back to its cursor, which
advances over the two now-recorded events.

Retry identity is `(seq, request_hash)`, mirroring `ReplayCursor::model_call`:

- A step already completed at `seq` with the same hash returns the recorded
  completion; the provider is not called again and the log does not grow. This
  is the no-re-pay case.
- A dangling intent at `seq` with the same hash (the tab died mid-call) is
  re-executed: an unanswered model request has no external effect to double, so
  the fresh completion correlates to the recorded intent.
- A different hash at `seq`, or a non-model event there, is `409 divergence`. A
  `seq` beyond the log's end is `409 divergence` too.

The model executor is a general injection seam the embedding binary supplies
(the `AgentFactory` pattern): `salvor serve` wires a default from its own model
client out of the box, and another host injects its own. The default executor
reads `ANTHROPIC_API_KEY` for its credential and targets the public endpoint;
setting `SALVOR_MODEL_BASE_URL` points it at a local or offline endpoint
speaking the same Messages wire protocol instead. With no key set, no auth
header is sent at all, which is what local endpoints expect. A step against a
server with no executor wired is `503 model_executor_unavailable`, and no intent
is written for the call it cannot make, so the run stays drivable once one
exists. A provider failure is `502 model_execution`; no completion is recorded,
so the write-ahead intent is left dangling (the legal crash story) and a retry
re-issues the call safely.

#### Streaming variant

With `Accept: text/event-stream` (or `?stream=1`) the response is a server-sent
event stream for a live ticker:

```text
event: delta
data: { "type": "text_delta", "index": 0, "text": "the plan: " }

event: complete
data: { "response": <MessageResponse as JSON>, "usage": { ... } }

```

Each provider event that carries ticker text (text and thinking deltas, and the
final usage) rides a `delta` frame while the call runs; the assembled completion
is recorded once at the end and carried on the closing `complete` frame. The
recorded `ModelCallCompleted` is byte-identical to the non-streaming path for the
same underlying response. A tab that drops mid-stream leaves a dangling intent,
re-issued safely on resume; a mid-stream provider failure sends an `error` frame
and records no completion. A model step that resolves to a replay (already
recorded) streams a single `complete` frame carrying the recorded completion.

### POST /v1/client-runs/{id}/tool-step

The server-performed tool call. The client owns the loop and decides when to call
a tool and with what; the server performs the call (it holds the binary or the
credential the tool needs) and records it. Requires the `X-Drive-Token` header.

- Request:

```json
{ "seq": 5, "tool": "render", "input": <any json>, "idempotency_key": null }
```

`seq` is the log position the client's cursor reserved for the tool intent.
`tool` names a tool the server's registry holds. `input` is the tool's typed
input, recorded on the intent verbatim. `idempotency_key` is optional; for an
`Idempotent` tool the client draws it from a recorded `RandomObserved` so it
reproduces on replay. A client-declared `effect` field is accepted for shape
parity but ignored: the recorded effect is the tool's operator-declared one, so a
caller cannot up- or down-grade it.

- Response `200`:

```json
{ "output": <the json the tool returned> }
```

The server takes the effect from the registration, appends `ToolCallRequested {
seq, tool, input, effect, idempotency_key }` write-ahead, dispatches the tool,
appends `ToolCallCompleted { seq, output }`, and returns the output. The client
feeds the output back to its cursor, which advances over the two now-recorded
events.

Retry follows the effect table, mirroring `ReplayCursor::tool_call`:

- A step already completed at `seq` with the same `(tool, input, effect, key)`
  returns the recorded output; the tool is not dispatched again and the log does
  not grow. This is the no-re-execution case.
- A dangling `Read` or `Idempotent` intent at `seq` (the tab died mid-call) is
  re-executed under the RECORDED idempotency key, so an idempotent retry reuses
  the exact key the provider collapses duplicates on. The fresh completion
  correlates to the recorded intent.
- A dangling `Write` intent at `seq` is `409 needs_reconciliation` carrying the
  recorded intent in `details.intent`, and the tool is not dispatched: the write
  may have landed, and only the resolve endpoint below may record its completion.
- A different `(tool, input, effect, key)` at `seq`, or a non-tool event there,
  is `409 divergence`. A `seq` beyond the log's end is `409 divergence` too.

The tool registry is a general injection seam the embedding binary supplies (the
same pattern as the model executor and the `AgentFactory`): the binary registers
named tools whose effects it declares. `salvor serve` wires an empty registry, so
every tool-step is `404 unknown_tool` until a host registers a tool; another host
(for example a render server) registers its tools. A step naming an unregistered
tool is `404 unknown_tool`, and a step against a server with no registry at all
is `503 tool_registry_unavailable`; in both cases no intent is written, so the
step is retriable once the tool is present. A dispatch failure is `502
tool_execution`; no completion is recorded, so the write-ahead intent is left
dangling (the legal crash story), drivable-or-reconcilable per the tool's effect.

The CLI's `salvor serve --demo-tools` is the one built-in exception, off by
default: it registers three deterministic demo tools (`lookup_invoice` read,
`issue_refund` write, `send_email` idempotent, see
`salvor_cli::demo_tools`) so a demo or the served end-to-end suite can run a
tool-bearing graph with no embedding host at all. It changes nothing about the
seam above: the demo tools register through the exact same `ToolRegistry`
any other host would, and a plain `salvor serve` with no flag still wires it
empty, byte for byte.

### POST /v1/client-runs/{id}/client-tool-intent

Open a tool call the CLIENT performs, in its own process, with its own secrets.
This server holds no code for such a tool and never dispatches it; it records
that the call was asked for, before the effect happens, exactly as the
write-ahead rule demands of a call it performs itself. Requires the
`X-Drive-Token` header.

- Request:

```json
{ "seq": 5, "tool": "charge_card", "input": <any json> }
```

- Response `200`:

```json
{ "seq": 5, "idempotency_key": "sha256:...", "effect": "write", "settled": false }
```

Notice what the request does NOT carry, compared with `tool-step` above: no
`effect` and no `idempotency_key`.

`settled` is `true` when the intent at this position already has its
completion recorded, `false` otherwise. It matters most to a caller re-posting
an intent it believes it already opened: a payments caller retrying after a
dropped response gets back the same `200` and the same key either way, and
without `settled` it cannot tell "safe to perform the call" from "already
performed and completed, do not do it again" without separately reading the
log. A freshly-recorded intent is always `false`.

The effect is the operator's, from the declaration, for the same reason
`tool-step` refuses a client-declared effect: a caller must not be able to
up- or down-grade its own write into a freely retried read.

The idempotency key is DERIVED by the server, a canonical hash of `{ run, seq,
tool }`, and this deliberately differs from `tool-step`, where the client
supplies it. There, salvor performs the call, so the party choosing the key is
not the party making the write. Here the client both chooses and performs, which
is the one case where the party choosing the key is also the party who benefits
from a duplicate landing. Deriving it removes the choice: the same `(run, seq,
tool)` always derives the same key, so an honest retry after a dropped response
presents the identical key and the provider collapses the pair, and a second
attempt cannot mint itself a fresh one. A client can derive the key
independently, with any canonical-JSON SHA-256, to check the server's work.

The input is validated against the declaration's `input_schema` before anything
is written, so a malformed call never becomes history. The intent then goes
through the same append-guard every other write on this surface uses.

Refusals, each of which writes nothing:

- an undeclared tool is `404 unknown_tool`;
- an input failing `input_schema` is `400 bad_request`;
- a `seq` the log is not ready for, or a different event already recorded
  there, is `409 divergence`.

A byte-identical re-post at an already-recorded position is a `200` that
re-derives the same key and writes nothing: the safe retry a dropped response
leaves behind.

Declarations are loaded by the operator (`salvor serve --client-tool <FILE>`, or
`AppState::with_client_tools` for an embedding host) and are NEVER registered
over HTTP. That is not an omission. A declaration fixes the effect class, so a
client able to POST one would be deciding whether its own writes are subject to
the write-ahead rule at all.

### POST /v1/client-runs/{id}/client-tool-completion

Record what a client-performed tool call returned. The client ran the call;
salvor did not witness it, so everything this endpoint can check, it checks
before the report becomes history. Requires the `X-Drive-Token` header.

- Request:

```json
{ "seq": 5, "output": <the json the client says the call returned> }
```

- Response `200`:

```json
{ "seq": 5, "completed": true }
```

Refusals, each of which records nothing:

- the log does not end at a tool intent, or ends at one whose `seq` is not the
  one named: `409 divergence`;
- the pending intent was performed by the SERVER: `403
  client_completion_refused`. A client must not close a call salvor made, since
  salvor holds the real result;
- the declaration says `trust_completion = false`: `403
  client_completion_refused`;
- the declaration carries no `output_schema`: `403 client_completion_refused`.
  With nothing to check the report against, the completion is unfalsifiable,
  which is exactly what the schema exists to prevent;
- the output fails the declared `output_schema`: `400 bad_request`.

A refusal is not a dead end, and it needed no new run state. The log still ends
at the recorded `ToolCallRequested`, and for an `Effect::Write` the pure fold in
`salvor-replay` already reports that as `needs_reconciliation`, because an
uncompleted write intent as the log's last word is precisely what that status
means. The resolve endpoint below already exists to settle it by hand, once a
person has verified externally whether the call landed. So `trust_completion =
false` is enforced here, at the completion boundary, and deliberately not in
`derive_state`: that fold is a pure function of the log with no access to
declarations, and it must stay that way, because a log has to mean the same
thing to a replay on a machine that has never seen this server's declaration
files.

The recorded `ToolCallRequested` carries `performed_by: "client"`, which is how
a later reader tells a call salvor witnessed from a call it was told about. A
server-performed intent omits the field entirely.

### POST /v1/client-runs/{id}/resolve

Record the completion of a dangling write by hand for a client-driven run, the
drive-token-gated twin of the server-driven `POST /v1/runs/{id}/resolve`.
Requires the `X-Drive-Token` header.

- Request:

```json
{ "output": <the json the tool returned> }
```

- Response `200`:

```json
{ "run": "6f...", "resolved": true }
```

State-validated exactly like the server-driven resolve: it is legal only when the
run's log ends at a dangling `Write` intent, it correlates the caller-supplied
output to that intent, and it dispatches nothing. After it records the completion
the run is drivable again, so the client re-fetches the log and its cursor sails
past the once-dangling intent.

- `409 wrong_state` when the run does not need reconciliation (there is no
  dangling write to resolve).
- `401 missing_drive_token` / `403 invalid_drive_token` on a missing or superseded
  lease; `404 unknown_run` when the id is not a client-driven run this server
  opened.

## Driving a run

Starting or resuming a run means model and tool calls, which are long, so the
handlers do the fast synchronous part (validate, refuse a bad state, mint or
check the id) and hand the run to a background task that drives it to its next
resting point. The handler returns the run id immediately.

The run is designed to outlive its request. Every event is
persisted to the store inside the driving task, before the task moves on; the
task holds no state the store does not already have. So aborting the task or
dropping the whole server mid-run loses nothing: a fresh server over the same
store recovers the run from its log and continues it, re-executing no completed
model or tool call. That is the same durability the CLI has, over HTTP, and it
is exercised by the kill-safety test.

### Waking a sleeping run

A run parked on a durable timer (`{"state":"sleeping","wake_at":...}`) is
passive data. Nothing in the server holds it and nothing fires at its instant;
it continues only when something re-drives it, at which point the runtime reads
the clock and either records the wake or leaves the run asleep. Driving a run
early therefore cannot wake it: the deadline is enforced inside the run, not by
whoever asked.

`salvor serve` runs a **wake sweeper** for this. On an interval it lists the
runs whose recorded `wake_at` is at or before now and re-drives each one through
the same path `POST /v1/runs/{id}/resume` takes for a recoverable run, so a
woken run behaves identically whether a person or the clock woke it.

- **On by default**, every `--wake-interval` seconds (default `60`). A server
  that held a store and let its timers pass would be silently wrong, so this is
  not an opt-in.
- **`--wake-interval 0` turns it off**, for an operator who sweeps from cron
  with `salvor wake` instead and does not want two things reaching for the same
  run.
- **It never fights a driver already running.** A run a task in this process is
  still driving is skipped, and the sweep drives sequentially, so no run is
  driven twice at once.
- **What this server holds decides it, not what started the run.** By the
  hash a run recorded: an agent run wakes once that agent is registered with
  `POST /v1/agents`, MCP tools and all, because the server rebuilds the agent
  from that same definition; a graph run wakes once its document is
  submitted with `POST /v1/graphs` and every `tool` node it carries names a
  tool this server's own registry holds (empty by default). Over HTTP a
  `tool` node resolves only against that registry, never against tools an
  agent's own MCP declarations reach, so a graph run built that way cannot be
  woken here regardless of what is registered. Two things leave a run
  asleep: its recorded hash is not registered here at all (typically a run
  started from the CLI against a store this server never saw), or it is a
  graph run with an unmet `tool` node. Each case is logged and skipped, once
  per sweep; the run stays due, so an operator wakes it instead with `salvor
  wake`, passing the same `--agent`/`--graph` files the run needs.
- **One bad run does not stop the sweep.** Every failure is per-run, and the
  loop carries on to the next.

There is no wake endpoint. Waking is a re-drive, and `POST /v1/runs/{id}/resume`
already is one: sending it to a sleeping run whose deadline has passed wakes it.
Sending it before the deadline is refused `409 still_sleeping` carrying
`wake_at` and `remaining_seconds`, because the drive would record nothing and a
`202 driving` for a run that did not move is worse than a refusal that says
when to come back.

### A tool can start the timer

A tool parks its own run by returning the sleep outcome, which the runtime
records **inside** that call's `ToolCallCompleted` (as
`{"__salvor_sleep": {"wake_at": "..."}}`) before appending `SleepStarted`. The
recorded order is therefore intent, completion, `SleepStarted`.

That `{"__salvor_sleep": ...}` shape is what the runtime writes into the log
from a native tool's `ToolOutcome::Sleep`; it is not a value a tool server
sends, and an MCP tool cannot park a run this way no matter what its output
contains, because MCP has no concept of sleeping a call. From an MCP-only
setup, the graph `delay` node is how a run sleeps instead.

That order is the point. The completion settles the call, and for a call
carrying an idempotency key it settles the store's claim in the same atomic
append, so a run that sleeps for a week holds no claim while it sleeps and a
second run under the same key is never told `CallInFlight` by a sleeper. It
also means a process death during the sleep leaves no dangling write intent:
the write already completed.

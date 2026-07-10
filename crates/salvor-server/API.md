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
`details` object is present only when there is structured evidence, which today
is the reconciliation refusal (below). The status codes and their codes:

| Status | `code` | When |
|---|---|---|
| 400 | `bad_request` | Malformed body, bad run id, or a resume input the recorded schema rejects |
| 401 | `unauthorized` | Missing or wrong bearer token |
| 404 | `unknown_run` | No run under that id |
| 404 | `unknown_agent` | No agent registered under that id |
| 409 | `run_exists` | Starting a run at an id that already has history |
| 409 | `wrong_state` | A verb applied to a run in the wrong state (resolving a run with no dangling write) |
| 409 | `needs_reconciliation` | Resuming a run whose log ends at a write intent with no completion; `details.intent` carries the recorded write |
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
| POST | `/v1/runs/{id}/resume` | Continue a run (resume a parked one, recover a crashed one) |
| POST | `/v1/runs/{id}/resolve` | Record a dangling write's completion by hand |

### POST /v1/agents

Register a definition. Under the single built-in loop an agent is pure data, so
it has a content hash (`agent_def_hash`, the same id every `RunStarted` event
records). A definition is submitted once and referenced by that hash from then
on, so a start payload carries only a hash and an input, and the same
definition drives every start, resume, and recover.

Registration also validates: the server builds the agent (which spawns and
immediately closes any MCP sessions) to confirm it is buildable and to compute
the hash. A definition that will not build is a `400`.

- Request: the definition body. `Content-Type: application/toml` for the agent
  TOML the CLI reads, or `application/json` for the same fields as JSON.
- Response `201`:

```json
{ "agent": "sha256:34e0...", "created": true }
```

`created` is `false` when the identical definition was already registered.

The registry is process-local. After a restart, re-register definitions; the
hash is stable, so runs that recorded a reference to it still resolve.

### GET /v1/agents

```json
{ "agents": [ { "agent": "sha256:34e0..." } ] }
```

### GET /v1/agents/{hash}

```json
{ "agent": "sha256:34e0...", "format": "toml", "definition": "model = ..." }
```

`404 unknown_agent` when the hash is not registered.

### POST /v1/runs

Start a fresh run. Returns at once with the run id; the run then drives in the
background (see [Driving a run](#driving-a-run)).

- Request:

```json
{ "agent": "sha256:34e0...", "input": <any json>, "run_id": "<uuid, optional>" }
```

`input` defaults to `null`. `run_id` is optional; when omitted the server mints
one. Passing one lets a client choose the id (a UUID).

- Response `201`:

```json
{ "run": "6f...uuid", "status": "running" }
```

- `404 unknown_agent` when the agent is not registered.
- `409 run_exists` when the chosen `run_id` already has history.

### GET /v1/runs

```json
{ "runs": [
  { "run": "6f...", "status": { "state": "completed", "output": ... },
    "event_count": 10, "first_recorded_at": "2026-...", "last_recorded_at": "2026-..." }
] }
```

Status is folded from each log, not stored, so it is always current.

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
  "last_recorded_at": "2026-..."
}
```

`404 unknown_run` when the id has no history and no run is being driven under it.

#### The status object

Always `{ "state": "<name>", ... }`:

| `state` | Extra keys |
|---|---|
| `not_started`, `running`, `awaiting_model`, `awaiting_tool`, `needs_reconciliation` | none |
| `suspended` | `reason`, `input_schema` |
| `budget_exceeded` | `budget` (`{kind, limit}`), `observed` |
| `completed` | `output` |
| `failed` | `error` |

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
- When the run reaches a resting point (completed, failed, suspended,
  budget-exceeded, or needs-reconciliation) the stream sends one final frame
  with `event: end` carrying the final status, then closes:

```text
event: end
data: {"status":{"state":"completed","output":...}}

```

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
  input the recorded schema rejects.
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

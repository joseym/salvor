# browser-client-run: a client-driven run from the browser

Salvor has two modes. In the server-driven mode the server owns the agent loop
and drives it in a background task; the SDKs and `examples/polyglot-service`
show that one. This example is the other mode: the client owns the loop and
streams the events it produces, while the server owns the durable log and, on
every append, re-folds the log to confirm the incoming event is the one legal
next event. The client here is a browser page.

The page opens a client-driven run, appends its own control events, re-opens the
run and re-drives it from the fetched log with zero live calls, drives a
streaming model step whose tokens paint a live ticker, and attempts a tool step.
It imports the built `@salvor-run/client` SDK by relative path, the same pattern
`examples/polyglot-service` uses, so there is no bundler.

## Files

- `index.html`: the static page. A DOM sink renders the log and the ticker.
- `client-run-demo.js`: the demo logic, with the DOM held behind a `sink` seam
  so the exact same code runs in the browser and headless. It imports the SDK's
  built `dist` by relative path.
- `headless.mjs`: runs `client-run-demo.js` against the live stack with a
  console sink, so the logic is verifiable without a browser.

## Bring up the offline stack

Build the binaries and the SDK once, from the repository root:

```sh
# This example spawns the demo fixture binaries, which ship with the cargo install but not with
# the npm package:
cargo install salvor-cli            # or, from a checkout: cargo build

( cd sdks/typescript && npm install && npm run build )
```

Start the scripted demo model and the control plane. `SALVOR_MODEL_BASE_URL`
points the client-driven model step's executor at the scripted server, so the
model step runs offline with no key (the streaming note below explains the
unary fallback the demo takes here).

```sh
salvor-demo-model --port 8893 --delay-ms 50 &
SALVOR_MODEL_BASE_URL=http://127.0.0.1:8893 \
    salvor --store /tmp/salvor-browser.db serve --bind 127.0.0.1:8080 &
```

### Verify the logic headless

The demo logic runs end to end without a browser:

```sh
node examples/browser-client-run/headless.mjs http://127.0.0.1:8080
```

You see the control loop open a run, append RunStarted / NowObserved /
RunCompleted, re-open the run, and replay all three events from the log with
zero live calls. The model step then performs a real server-performed call
against the scripted model and records it (the stream falls back to a unary
retry offline, the note below), and the tool step reports its current limit.

## Open the page

A browser needs the page, the demo module, and the SDK's `dist` all reachable
over one origin, and it needs to reach the control plane's `/v1` on that same
origin (see the CORS note). Serve the repository root and proxy `/v1` to the
control plane, then open the page. Any static server plus a `/v1` proxy works;
the shape is the one the dashboard's Trunk dev server uses.

The page's relative import resolves to `/sdks/typescript/dist/index.js` when the
repository root is the served root, so open it at a URL like
`http://localhost:PORT/examples/browser-client-run/index.html`.

## The CORS reality

`salvor-server` sets no CORS headers. A browser page served from a different
origin than the control plane is refused by the browser before the request is
even seen server-side. There is no flag to turn CORS on, and this example does
not patch the server to add it.

So the page must be same-origin with the control plane's `/v1`: serve the static
files and reverse-proxy `/v1` to `salvor serve` under one origin (the Trunk
proxy the dashboard uses, or any dev proxy). Cross-origin from a bare
`file://` or a plain static server on another port will fail with a CORS error
in the console. Adding an opt-in CORS layer to `salvor-server` is a reasonable
follow-up; it is out of scope for this example, which does not change the
server.

## The model step and the tool step, honestly

- **The model step runs offline; live streaming needs an endpoint that
  streams.** `salvor serve`'s client-driven model executor reads
  `ANTHROPIC_API_KEY` for its credential and honors `SALVOR_MODEL_BASE_URL` to
  target a local or offline endpoint speaking the same Messages wire protocol
  (documented in `crates/salvor-server/API.md`). With the bring-up above the
  executor reaches the scripted demo model, which answers unary requests but
  does not implement the provider's streaming wire (it ignores `stream: true`
  and returns plain JSON), so the streaming attempt fails, leaves a dangling
  intent, and the demo retries with the unary step at the same position. That
  retry re-issues the dangling intent safely and records the completion, which
  is the crash story driven on purpose; the assembled response then paints at
  once. To watch the ticker fill token by token, give `salvor serve` a real
  `ANTHROPIC_API_KEY` (and drop `SALVOR_MODEL_BASE_URL`), or point it at any
  local endpoint that streams. Note `SALVOR_DEMO_BASE_URL` is a different knob:
  it configures the demo *agent's* own model for server-driven runs, not this
  executor.

- **The tool step needs a registered tool.** `salvor serve` wires an empty tool
  registry, so every tool step is `unknown_tool` until a host registers one. The
  demo reports that and moves on. This example is therefore scoped to the model
  step and control events; a host that registers a render tool would see the
  tool step dispatched and recorded.

## What was and was not automated

The demo logic is verified headless against the live offline stack by
`headless.mjs` (open, append, re-open, replay, the model step recorded through
the unary retry, and the tool step's clean refusal), and the page, the demo
module, and the SDK `dist` all load over static HTTP. A full in-browser click-through (loading the page in a
real browser behind a same-origin proxy and clicking Run) was not automated
here; the logic it would exercise is exactly what `headless.mjs` runs, since the
DOM is only a sink.

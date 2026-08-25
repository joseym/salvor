/**
 * `currentToolCall()`: what a tool body running under `wrapToolCall` was
 * recorded with.
 *
 * The middleware derives a call's idempotency key before the tool body ever
 * runs (`ClientRunDriver.clientToolIntent` returns it as part of opening the
 * intent), but nothing about `wrapToolCall` hands that key to the tool
 * itself: LangChain calls a tool with its arguments and nothing else. A tool
 * that talks to its own provider (a payments API, an email sender, anything
 * that takes its own idempotency token) needs that key to hand onward, so
 * this module makes it reachable from inside the tool body without changing
 * the tool's signature: `runWithToolCall` wraps the live call in an
 * `AsyncLocalStorage` context, and `currentToolCall()` reads it back.
 *
 * `key` is what salvor recorded for this call, not a suggestion: hand it to
 * the tool's own provider as the provider's idempotency token, the same way
 * the client-tool intent's key is meant to be used
 * (`ClientRunDriver.clientToolIntent`). A retried write then presents the key
 * the first attempt used, and the provider collapses the duplicate.
 *
 * `node:async_hooks` is a Node builtin, and this SDK's rule is that no
 * Node-only module is imported unconditionally under `src/`. So the module
 * is loaded through a dynamic `import()` of a specifier held in a variable
 * (not a string literal), which keeps TypeScript from resolving it at
 * compile time and keeps a bundler from pulling it into a browser build. The
 * import is attempted once, lazily, and any failure (no such module, no
 * such runtime) leaves `als` `undefined` forever: outside Node,
 * `currentToolCall()` always returns `undefined` and the middleware records
 * and replays exactly as it does with it.
 */

/** What `currentToolCall()` returns from inside a tool body. */
export interface CurrentToolCall {
  /** The idempotency key salvor derived for this call, from `(run, seq, tool)`. */
  key: string;
  /** The log position the call's intent landed at. */
  seq: number;
  /** The run this call belongs to. */
  runId: string;
  /** The tool's name, as the model invoked it. */
  tool: string;
}

interface AsyncLocalStorageLike<T> {
  getStore(): T | undefined;
  run<R>(store: T, callback: () => R): R;
}

const ASYNC_HOOKS_SPECIFIER = "node:async_hooks";

let als: AsyncLocalStorageLike<CurrentToolCall> | undefined;
let loaded: Promise<void> | undefined;

/** Load `node:async_hooks` at most once. Any failure leaves `als` unset. */
function ensureLoaded(): Promise<void> {
  if (!loaded) {
    loaded = import(ASYNC_HOOKS_SPECIFIER)
      .then((mod: { AsyncLocalStorage: new () => AsyncLocalStorageLike<CurrentToolCall> }) => {
        als = new mod.AsyncLocalStorage();
      })
      .catch(() => {
        als = undefined;
      });
  }
  return loaded;
}

/**
 * Run `fn` with `context` reachable from `currentToolCall()` anywhere `fn`
 * awaits into, including inside the tool body a `handler` eventually calls.
 *
 * Outside Node, or if `node:async_hooks` cannot be loaded, `fn` still runs;
 * `currentToolCall()` inside it just returns `undefined`, the same as it
 * would for a tool call outside `wrapToolCall` entirely.
 */
export async function runWithToolCall<T>(
  context: CurrentToolCall,
  fn: () => Promise<T>,
): Promise<T> {
  await ensureLoaded();
  if (!als) return fn();
  return als.run(context, fn);
}

/**
 * The idempotency key, seq, run id and tool name salvor recorded for the
 * call this tool body is running inside of, or `undefined` when called
 * outside a `wrapToolCall` invocation (or outside Node).
 */
export function currentToolCall(): CurrentToolCall | undefined {
  return als?.getStore();
}

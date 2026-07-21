import { Injectable, type Signal, inject, signal } from '@angular/core';

import { SALVOR_CLIENT } from './client';

/**
 * Resolves an `agent_def_hash` to a human name, when one is discoverable — for the Runs
 * ledger's agent column.
 *
 * `GET /v1/agents/{hash}` (`@salvor/client#getAgent`) answers `{ agent, format, definition }`,
 * where `definition` is the raw TOML/JSON body a client registered. The real agent schema
 * (`crates/salvor-cli/src/agent_config.rs`, `#[serde(deny_unknown_fields)]`) has no top-level
 * `name` field today, so a hash that resolves (200, registered) usually still carries no name to
 * show — that is a genuine, current absence, not a bug in this service. What it CAN safely read:
 * a JSON-format definition's own top-level `name` key, if a future definition ever adds one —
 * parsed structurally (`JSON.parse` + a root-property read), never by pattern-matching the TOML
 * text, which would risk surfacing a NESTED name (a `[[wasm_tools]]` entry's own `name`, which
 * `demo/agent.toml`-shaped files do carry) as if it were the agent's own identity. A wrong name is
 * worse than an honest hash, so TOML definitions are never scraped for one.
 *
 * Lookups are batched per hash and cached forever (the registry is content-hash-keyed, so a
 * resolved answer — including "no name" — never goes stale for that hash within a session). A
 * 404 (unregistered) or any other failure resolves silently: the hash stays unresolved and the
 * column falls back to displaying the hash itself, exactly the honest "unregistered" rendering —
 * an absent name is not an error worth surfacing to the operator.
 */
@Injectable({ providedIn: 'root' })
export class AgentRegistryService {
  private readonly client = inject(SALVOR_CLIENT);

  private readonly _names = signal<ReadonlyMap<string, string>>(new Map());
  /** hash → resolved name, only for hashes that actually resolved one. */
  readonly names: Signal<ReadonlyMap<string, string>> = this._names.asReadonly();

  private readonly attempted = new Set<string>();
  private readonly inFlight = new Map<string, Promise<void>>();

  /**
   * Kick off resolution for every hash not yet attempted. Production callers still treat this as
   * fire-and-forget — they read the result back off {@link names} as it fills in, never awaiting
   * this call — but it returns a promise (of just this call's newly-started lookups; an
   * already-attempted or already-in-flight hash contributes nothing to it) so a caller that DOES
   * need to know when the work is actually done (a test, principally) has a genuine signal to
   * await instead of guessing how many microtask ticks the fetch-mock's promise chain needs.
   */
  resolve(hashes: readonly string[]): Promise<void> {
    const started: Promise<void>[] = [];
    for (const hash of hashes) {
      if (this.attempted.has(hash) || this.inFlight.has(hash)) continue;
      const attempt = this.lookup(hash).finally(() => {
        this.attempted.add(hash);
        this.inFlight.delete(hash);
      });
      this.inFlight.set(hash, attempt);
      started.push(attempt);
    }
    return Promise.all(started).then(() => undefined);
  }

  private async lookup(hash: string): Promise<void> {
    try {
      const info = await this.client.getAgent(hash);
      const name = extractAgentName(info);
      if (name) {
        this._names.update((current) => {
          const next = new Map(current);
          next.set(hash, name);
          return next;
        });
      }
    } catch {
      /* unregistered (404) or unreachable — absence of a name is not an error */
    }
  }
}

/**
 * Read a top-level `name` string off a JSON-format agent definition. Returns `undefined` for
 * every other case (TOML format, unparsable body, missing/blank/non-string `name`) — all honest
 * "no name available" outcomes, never a guess.
 */
function extractAgentName(info: Record<string, unknown>): string | undefined {
  if (info['format'] !== 'json') return undefined;
  const definition = info['definition'];
  if (typeof definition !== 'string') return undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(definition);
  } catch {
    return undefined;
  }
  if (!parsed || typeof parsed !== 'object') return undefined;
  const name = (parsed as Record<string, unknown>)['name'];
  return typeof name === 'string' && name.trim() ? name.trim() : undefined;
}

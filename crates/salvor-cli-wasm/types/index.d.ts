// Hand-written type surface for the salvor-cli-wasm package.
//
// wasm-pack generates a .d.ts for the exported FUNCTIONS (their string
// signatures). It cannot describe the SHAPE of the JSON those functions carry,
// because every result crosses the boundary as a JSON string. This file pins
// that shape, the way salvor-replay-wasm's types/index.d.ts does for the fold.
//
// This surface is a contract. The Rust `surface_pin` tests in src/lib.rs byte-
// pin the parse envelope, and the same-render proof (tests/same_render.rs and
// js/same-render.mjs) exercises every function here against the native output
// of salvor-cli-core. If a DTO there changes, those fail and this file must be
// updated in lockstep.
//
// Consuming (any TS), against the --target web build in pkg/:
//   import init, { parseArgv, renderHistory, parseAgentToml } from "salvor-cli-wasm";
//   import type { ParseEnvelope, AgentEnvelope } from "salvor-cli-wasm/types";
//   await init();
//   const parsed: ParseEnvelope = JSON.parse(parseArgv(JSON.stringify(argv)));

/** The functions the wasm module exports. Mirrors the wasm-pack-generated
 *  signatures; kept here so `types/index.d.ts` is the one place a consumer
 *  reads. Every one of them throws only on input the caller malformed; a
 *  refusal by the CLI is always a value, never a throw. */
export function parseArgv(argvJson: string): string;
export function helpText(path: string): string;
export function helpTextAnsi(path: string): string;
export function renderList(rowsJson: string): string;
export function renderListPlain(rowsJson: string): string;
export function renderHistory(logJson: string): string;
export function renderHistoryPlain(logJson: string): string;
export function parseAgentToml(text: string): string;

/** What `parseArgv` returns, parsed. Exactly one of `command` and `message` is
 *  present, and `ok` says which. */
export type ParseEnvelope =
  | { ok: true; command: ParsedCli }
  | { ok: false; message: ClapMessage };

/** Text clap produced instead of a parse: a refusal, or the help or version it
 *  displays when asked for one. */
export interface ClapMessage {
  /** clap's own name for what happened, e.g. "InvalidValue", "DisplayHelp". */
  kind: string;
  /** True for a refusal, false when clap is displaying help or a version. */
  is_error: boolean;
  /** The exit code a shell would use: 0 for help or version, 2 for a refusal. */
  exit_code: number;
  /** clap's text, unstyled. */
  plain: string;
  /** The same text, with ANSI escape codes. */
  ansi: string;
}

/** The parsed command line: the one global option, and the verb. The verb is a
 *  `verb`-tagged union; the tag values are the CLI's own kebab-case names. */
export interface ParsedCli {
  store: string;
  command: { verb: string } & Record<string, unknown>;
}

/** What `parseAgentToml` returns, parsed. Exactly one of `config` and `error`
 *  is present, and `ok` says which. The `error` string is the message the CLI
 *  prints for that file, context chain included: it is the product, not a
 *  paraphrase of one. */
export type AgentEnvelope =
  | { ok: true; config: AgentConfigJson }
  | { ok: false; error: string };

/** A validated agent definition, under the key names the file itself uses.
 *  An optional the file left out comes back as `null`. Mirrors
 *  `salvor_cli_core::agent_config::AgentConfig`. */
export interface AgentConfigJson {
  /** The model id sent with each request. Required in the file. */
  model: string;
  /** A short human label. At most 64 characters when set. */
  name: string | null;
  system_prompt: string | null;
  system_prompt_path: string | null;
  llm: LlmConfigJson;
  budgets: BudgetsConfigJson;
  pricing: PricingConfigJson | null;
  max_response_tokens: number | null;
  mcp_servers: McpServerConfigJson[];
  wasm_tools: WasmToolConfigJson[];
  record_prompts: boolean | null;
}

/** Model transport settings. Every field names an environment variable or a
 *  URL; none of them ever holds a secret. */
export interface LlmConfigJson {
  base_url: string | null;
  base_url_env: string | null;
  api_key_env: string | null;
  api_key_kind: "api_key" | "oauth";
  max_retries: number | null;
  timeout_seconds: number | null;
}

/** Declared budget limits. This object, with `pricing` added, is exactly what
 *  salvor-replay-wasm's `checkBudgets` takes as its second argument. */
export interface BudgetsConfigJson {
  steps: number | null;
  tokens: number | null;
  cost_usd: number | null;
  wall_time_seconds: number | null;
}

/** Per-token pricing, dollars per million tokens. */
export interface PricingConfigJson {
  input_per_mtok: number;
  output_per_mtok: number;
}

/** Effect class of a tool call. Matches `salvor_replay::Effect`'s wire form. */
export type Effect = "read" | "idempotent" | "write";

/** One MCP server. Exactly one of `command` and `url` is set; the parse
 *  refuses a file where that is not true. */
export interface McpServerConfigJson {
  command: string | null;
  args: string[];
  env: Record<string, string>;
  url: string | null;
  bearer_token_env: string | null;
  /** The operator's per-tool trust decision, winning over the server's own
   *  annotations. */
  effect_overrides: Record<string, Effect>;
  /** Tool name to the input field path identifying the operation a call
   *  performs. A malformed path is refused at parse time. */
  idempotency_keys: Record<string, string>;
}

/** One sandboxed WebAssembly tool. Everything model-facing here is
 *  operator-authored; the binary is never asked to describe itself, and
 *  `effect` has no default for that reason. */
export interface WasmToolConfigJson {
  path: string;
  sha256: string | null;
  name: string;
  description: string;
  effect: Effect | null;
  input_schema: string | null;
  input_schema_path: string | null;
  idempotency_key: string | null;
  limits: WasmLimitsConfigJson;
  grants: WasmGrantsConfigJson;
}

/** Per-call resource caps. A null field takes the runtime's default. */
export interface WasmLimitsConfigJson {
  wall_time_ms: number | null;
  memory_bytes: number | null;
  fuel: number | null;
}

/** Capability grants. An empty `preopen` means the guest can open nothing. */
export interface WasmGrantsConfigJson {
  preopen: PreopenConfigJson[];
}

/** One preopened directory grant. */
export interface PreopenConfigJson {
  host: string;
  guest: string;
  perms: "read" | "read_write";
}

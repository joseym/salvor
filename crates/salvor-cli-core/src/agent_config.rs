//! The agent-definition file: the TOML document that declares an agent, and
//! the parse that turns it into a validated [`AgentConfig`].
//!
//! This is the schema and the checks, and nothing else. Reading the file off
//! disk, resolving an environment variable, and building a live
//! `salvor_runtime::Agent` out of the result are the IO edge's work and live in
//! `salvor_cli::agent_config`, one crate up. The split is the same one the rest
//! of this crate makes: what a surface is allowed to accept must not depend on
//! being able to spawn a process, so the browser, the control plane, and the
//! binary all reach the same parse.
//!
//! Under Salvor's single built-in loop an agent is pure data (model, prompt,
//! tools, budgets), which is exactly what makes a config file a legitimate
//! home for it. This module owns that file's schema, and nothing owns it
//! twice.
//!
//! # Schema
//!
//! ```toml
//! # Required. The model id sent with every request.
//! model = "claude-opus-4-8"
//!
//! # Optional. A short human label, shown by tooling that resolves
//! # `agent_def_hash` back to something readable (the control plane's
//! # `GET /v1/agents/{hash}` and its list). At most 64 characters, and not
//! # empty or all whitespace when set. Purely descriptive: it plays no part
//! # in `agent_def_hash`, so renaming an agent never mints a new identity or
//! # orphans its recorded runs (see `salvor_runtime::Agent::name`).
//! name = "support-triage"
//!
//! # Optional. Exactly one of these sets the system prompt; setting both is an
//! # error. A path is resolved relative to the directory of this file.
//! system_prompt = "You are a research agent."
//! # system_prompt_path = "prompt.txt"
//!
//! # Optional. How to reach the model. Defaults target the public Anthropic
//! # endpoint. The file names the ENV VAR the key is read from; it never holds
//! # the secret itself. The key is optional so local endpoints (LM Studio,
//! # Ollama) work with no key at all. `base_url_env` names an env var that,
//! # when set and non-empty, overrides `base_url`: one agent file can then
//! # target the real endpoint by default and a mock or local endpoint when
//! # the variable is exported (the demo/ agent uses this).
//! [llm]
//! base_url = "https://api.anthropic.com"
//! base_url_env = "SALVOR_DEMO_BASE_URL"
//! api_key_env = "ANTHROPIC_API_KEY"
//! # How the key authenticates. "api_key" (default) sends it as `x-api-key`,
//! # for standard API keys. "oauth" sends it as an `Authorization: Bearer`
//! # credential with the oauth beta header, for subscription OAuth tokens
//! # (`sk-ant-oat...`). Any other value is a loud parse error.
//! api_key_kind = "api_key"
//! max_retries = 2
//! timeout_seconds = 60
//!
//! # Optional. Every dimension is optional; an absent one is never enforced.
//! [budgets]
//! steps = 40
//! tokens = 100000
//! cost_usd = 2.0
//! wall_time_seconds = 600
//!
//! # Required only if budgets.cost_usd is set (a cost check with no rates
//! # cannot be computed; the build fails clearly if it is missing).
//! [pricing]
//! input_per_mtok = 3.0
//! output_per_mtok = 15.0
//!
//! # Optional. The shape of this agent's final answer, as a JSON Schema
//! # written in TOML. Declaring it puts every server-side run of the agent on
//! # the structured loop: the answer comes back as an object of this shape
//! # instead of prose. Use `output_schema_path = "answer.json"` instead to
//! # keep the schema in a JSON file beside the agent file; set one or the
//! # other, never both. See "Declared output shape" below, and note that this
//! # one IS part of `agent_def_hash`.
//! [output_schema]
//! type = "object"
//! required = ["score"]
//!
//! [output_schema.properties.score]
//! type = "number"
//!
//! # Optional, repeatable. Each MCP server contributes its tools to the agent.
//! # A server is reached over EXACTLY ONE of two transports:
//! #  - `command` (+ optional `args`/`env`) spawns a local child process and
//! #    speaks MCP over its stdio;
//! #  - `url` reaches a remote server over streamable HTTP.
//! # Setting both, or neither, is a loud parse error, as is pairing `args`/`env`
//! # with `url` or `bearer_token_env` with `command`.
//! #
//! # `effect_overrides` is valid with either transport: it is the operator's
//! # trust decision, since MCP effect annotations are hints a server
//! # may misstate, so an operator who knows a tool's true side-effect class pins
//! # it here and the runtime honors it over the wire hints.
//! #
//! # `idempotency_keys` is the operator's other per-tool declaration, valid with
//! # either transport: which input field's value identifies the operation a call
//! # performs. See "Idempotency keys" below.
//!
//! # A local stdio server:
//! [[mcp_servers]]
//! command = "python"
//! args = ["-m", "my_server"]
//! env = { API_TOKEN = "..." }
//! effect_overrides = { delete = "write", fetch = "read" }
//! idempotency_keys = { pay_claim = "claim_id" }
//!
//! # A remote HTTP server. `bearer_token_env` NAMES an env var holding a bearer
//! # token (never the token itself); when set and non-empty it is sent as
//! # `Authorization: Bearer <token>`. Omit it for a server that needs no auth.
//! [[mcp_servers]]
//! url = "https://mcp.example.com/mcp"
//! bearer_token_env = "MY_MCP_TOKEN"
//! effect_overrides = { delete = "write" }
//! idempotency_keys = { refund = "payment.charge_id" }
//!
//! # Optional, repeatable. Each entry is one sandboxed WebAssembly tool: an
//! # untrusted component (the `salvor:tool@0.1.0` world) run under wasmtime
//! # with no capabilities beyond what `grants` hands it. EVERY model-facing
//! # fact here is operator-authored; the binary is never asked to describe
//! # itself.
//! #
//! # `effect` is REQUIRED, with no default: one notch stricter than MCP.
//! # An MCP server legitimately self-describes, so its silence needs a safe
//! # reading (Write); a sandboxed binary gets no voice at all, so a missing
//! # `effect` is a missing operator decision and the parser refuses it.
//! [[wasm_tools]]
//! path = "tools/wordcount.wasm"       # resolved relative to this file
//! sha256 = "9f3a..."                  # optional integrity pin; mismatch = refuse to load
//! name = "wordcount"                  # the name the model calls
//! description = "Counts words in text"
//! effect = "read"                     # required: "read" | "idempotent" | "write"
//! # Exactly one of `input_schema` (inline JSON) or `input_schema_path`
//! # (a JSON file, resolved relative to this file).
//! input_schema = '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}'
//! # Optional. The input field identifying the operation a call performs, the
//! # singular of an MCP server's `idempotency_keys` (this table is one tool, and
//! # already names it). See "Idempotency keys" below.
//! # idempotency_key = "claim_id"
//!
//! [wasm_tools.limits]                 # optional; these are the defaults
//! wall_time_ms = 5000                 # per-call wall/CPU cap (epoch deadline)
//! memory_bytes = 134217728            # per-call linear-memory cap (128 MiB)
//! # fuel = 500000000                  # optional deterministic metering; unlimited when absent
//!
//! [wasm_tools.grants]                 # optional; absent = the guest can open nothing
//! # `host` is resolved relative to this file; `guest` is where the guest sees
//! # it; `perms` is "read" or "read_write".
//! preopen = [{ host = "./data", guest = "/data", perms = "read" }]
//!
//! # Optional. Records the FULL model request body (the exact prompt sent) into
//! # the durable event log, so the dashboard inspector can show it. OFF by
//! # default, on purpose: a request body can contain user data and secrets, so
//! # enabling this stores that verbatim in the log. Turn it on only when you
//! # accept that.
//! record_prompts = false
//! ```
//!
//! # Declared output shape (`output_schema`, `output_schema_path`)
//!
//! An agent with no declaration answers in prose, and a caller that wanted a
//! number out of it goes looking for one in a sentence. `output_schema` is the
//! agent file saying, once, what shape its answer takes. The built-in loop then
//! offers the model a `salvor_answer` tool whose input schema IS this schema,
//! requires a tool call, and ends the run only on an answer the schema accepts.
//! The run's output is that object, verbatim.
//!
//! Write it inline as a `[output_schema]` table, or keep it in a JSON file and
//! name it with `output_schema_path` (resolved relative to this file), the same
//! pair a `[[wasm_tools]]` entry offers for its `input_schema`. Setting both is
//! a parse error, as is an inline `output_schema` that is not a table: a JSON
//! Schema is an object, and a scalar there accepts everything, which is the one
//! thing a declared shape must not quietly do.
//!
//! **The enforced subset.** Validation is structural, not a full JSON Schema
//! implementation: `type`, `required`, `properties`, `items`, and `enum` are
//! checked, and every other keyword (`pattern`, `format`, numeric ranges,
//! `oneOf`, `$ref`) is carried to the model verbatim but never enforced by the
//! runtime. Write the richer keywords if they help the model; do not rely on
//! them to refuse an answer.
//!
//! **A graph node can override it.** An `agent` node in a graph document may
//! declare its own `output_schema`, and for that node it wins; this file's is
//! the fallback used wherever the agent runs without a node speaking for it
//! (`salvor run`, a `Runtime::start`, a node that declares nothing). The graph
//! author has the whole document in view, so the more specific declaration is
//! the more informed one.
//!
//! **It is part of the agent's identity.** Unlike `name`, `record_prompts`, and
//! labels, this key is hashed into `agent_def_hash`: it changes what the agent
//! produces, not how it is deployed or described. Adding it to an existing
//! agent file mints a new hash, so any graph document pinning the old one must
//! be repinned (`salvor agent hash` prints the new value). A file WITHOUT the
//! key hashes exactly as it always has. Which form carried the schema makes no
//! difference either: the hash covers the schema value, so inline and
//! `output_schema_path` produce the same hash for the same schema.
//!
//! **Server-side loops only.** This key drives the loops salvor runs: `salvor
//! run`, a resume, a graph node, a run driven through the control plane. A
//! client-driven run owns its own loop and its own model calls, so nothing here
//! reaches it; a client that wants a declared shape enforces one itself.
//!
//! # Idempotency keys (`idempotency_keys`, `idempotency_key`)
//!
//! Within one run, nothing happens twice because a recorded completion replays
//! instead of executing. Across two separate `salvor run` invocations there is
//! no shared log to replay, and what holds the line instead is an idempotency
//! key: a statement that a call *is* a particular operation in the world, so
//! the store can let exactly one run perform it.
//!
//! A hand-written Rust tool makes that statement in code. A tool that arrives
//! at runtime cannot: an MCP server is not this program and may not be this
//! machine, and a wasm component is untrusted by construction. What is
//! available is the call's input and the operator's knowledge of which field
//! of it names the thing being done. That is what these settings declare.
//!
//! On an `[[mcp_servers]]` entry, a map from tool name to field path:
//!
//! ```toml
//! idempotency_keys = { pay_claim = "claim_id", refund = "payment.charge_id" }
//! ```
//!
//! On a `[[wasm_tools]]` entry, the same thing in the singular, since the table
//! is already one named tool:
//!
//! ```toml
//! idempotency_key = "claim_id"
//! ```
//!
//! The path is a field name, or several joined by `.` to read a nested field.
//! There is no array indexing and no escaping, so a field name containing a dot
//! cannot be addressed. An empty path, or one with a leading, trailing, or
//! doubled dot, is a parse error on the file.
//!
//! **The key.** The runtime derives `<tool>:<field value>` at dispatch, so
//! `pay_claim` called with `{"claim_id": "wreck-9931", ...}` is the identity
//! `pay_claim:wreck-9931`. The format is fixed and worth relying on: it is
//! stable across processes and machines (nothing but the input feeds it), and
//! it stays legible where the key is printed on its own, in `salvor history`,
//! in a refusal naming the run that holds it, or in a grep of the store. The
//! tool prefix is redundant against the store's own `(tool, key)` index and
//! deliberately kept anyway, so the string a human reads says what was done as
//! well as to what.
//!
//! **A missing field is refused, never demoted.** The value must be a non-empty
//! string or a number. If the field is absent, if the path runs through a
//! non-object, or if the value is a boolean, a null, an empty string, an array,
//! or an object, the call fails before the tool is reached, with an error
//! naming the tool, the path, and the keys the input did carry. The tool does
//! not run keyless: an operator who declared an identity asked for exactly one
//! execution, and a call that quietly lost its key is how a claim gets paid
//! twice.
//!
//! **A name that binds to nothing fails the build.** Declaring a key for a tool
//! the server does not advertise is refused when the agent is built, listing
//! what the server does advertise. This is stricter than `effect_overrides`,
//! which ignores an unknown name; an unbound effect override leaves a tool at
//! the safe default, while an unbound key declaration leaves a tool the
//! operator meant to protect completely unprotected.
//!
//! Tools with no declaration are untouched by all of this.
//!
//! # Recording the prompt body (`record_prompts`)
//!
//! `record_prompts` opts one agent into storing the full model request body on
//! each `ModelCallRequested` event. It is off by default because the body can
//! carry user data and secrets, and enabling it writes that verbatim to the
//! durable log. The recorded body lands only in the log; it never reaches the
//! progress stream, stderr, or any console output, and it never affects replay
//! (the request hash, which correlation keys on, is computed the same either
//! way and the body is ignored on replay).
//!
//! What this module holds is the field: `true`, `false`, or unset. The
//! effective flag also depends on the `SALVOR_RECORD_PROMPTS` environment
//! variable, and resolving that is reading the environment, so the precedence
//! rule lives with the IO edge in `salvor_cli::agent_config`, which documents
//! it. In short: per-agent over environment over off, with an unset field
//! meaning "no opinion" rather than "off". There is deliberately no automatic
//! redaction.
//!
//! Native Rust tools are code, not config, so the CLI does not register them:
//! MCP servers and sandboxed wasm components are the config-reachable tool
//! boundary. Unknown fields are **rejected**, not ignored, so a typo like
//! `step` instead of `steps` is a loud parse error rather than a silently
//! dropped budget.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use salvor_replay::Effect;
use salvor_tools::IdempotencyPath;
use serde::{Deserialize, Serialize};

/// The longest an agent `name` may be, in characters. A name is a short
/// display label for the registry and dashboards, not a payload, so the
/// bound is generous for a title but rejects anything payload-shaped.
/// Checked in [`AgentConfig::validate`], which runs on every parse
/// (`salvor_cli::agent_config::load`, [`from_toml_str`](AgentConfig::from_toml_str),
/// [`from_json_str`](AgentConfig::from_json_str)), including the control
/// plane's `POST /v1/agents`, so a submitted name is bounded before it is
/// trusted, the same as any other client-supplied config.
pub const MAX_NAME_LEN: usize = 64;

/// The full agent definition, parsed from the TOML file. Every optional field
/// carries `#[serde(default)]` so a terse file is valid; `deny_unknown_fields`
/// turns a misspelled key into an error instead of a silent no-op.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// The model id sent with each request. Required.
    pub model: String,
    /// A short human label, shown by tooling that resolves `agent_def_hash`
    /// back to something readable. Optional; bounded to
    /// [`MAX_NAME_LEN`] characters and, when set, not empty or all
    /// whitespace (checked in [`validate`](Self::validate)). Excluded from
    /// `agent_def_hash`: see `salvor_runtime::Agent::name`.
    #[serde(default)]
    pub name: Option<String>,
    /// An inline system prompt. Mutually exclusive with `system_prompt_path`.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// A path to a file holding the system prompt, resolved relative to the
    /// agent file's directory. Mutually exclusive with `system_prompt`.
    #[serde(default)]
    pub system_prompt_path: Option<String>,
    /// Model transport settings.
    #[serde(default)]
    pub llm: LlmConfig,
    /// Declared budgets.
    #[serde(default)]
    pub budgets: BudgetsConfig,
    /// Per-token pricing, required when a cost budget is declared.
    #[serde(default)]
    pub pricing: Option<PricingConfig>,
    /// The `max_tokens` cap sent with each model request; defaults to the
    /// runtime's `DEFAULT_MAX_RESPONSE_TOKENS`.
    #[serde(default)]
    pub max_response_tokens: Option<u32>,
    /// The shape of the agent's final answer, inline: a JSON Schema written
    /// as a TOML table. Mutually exclusive with `output_schema_path`, and
    /// must be a table (an object), not a scalar. Setting either one puts
    /// every server-side run of this agent on the structured loop.
    ///
    /// Unlike `name`, this IS part of `agent_def_hash`: it changes what the
    /// agent produces. See the module docs (`output_schema`) and
    /// `salvor_runtime::Agent::output_schema`.
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
    /// A path to a JSON file holding that same schema, resolved relative to
    /// the agent file's directory. Mutually exclusive with `output_schema`.
    /// The two forms are interchangeable: the hash covers the schema VALUE,
    /// so moving a schema between them never changes `agent_def_hash`.
    #[serde(default)]
    pub output_schema_path: Option<String>,
    /// MCP servers whose tools the agent may call.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Sandboxed WebAssembly component tools the agent may call.
    #[serde(default)]
    pub wasm_tools: Vec<WasmToolConfig>,
    /// Whether to record the full model request body into the durable event
    /// log. Optional and off unless set. See the module docs (`record_prompts`)
    /// for the precedence against `SALVOR_RECORD_PROMPTS` and the PII warning.
    #[serde(default)]
    pub record_prompts: Option<bool>,
}

/// How the API key authenticates, as named in the `[llm]` section. Mirrors
/// `salvor_llm::AuthKind` but stays a config-layer type so the wire spelling
/// (`"api_key"` / `"oauth"`) lives with the schema. An unknown value is a loud
/// parse error, not a silent fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyKind {
    /// Send the key as `x-api-key`. The default, for standard API keys.
    #[default]
    ApiKey,
    /// Send the key as an `Authorization: Bearer` credential with the oauth
    /// beta header, for subscription OAuth tokens.
    Oauth,
}

/// The environment variable `[llm] api_key_env` reads from when the agent
/// file leaves it unset. Named once here so the resolution in
/// [`AgentConfig::client_config`], the name reported in
/// [`AgentConfig::api_key_env`], and a 401 error's own message can never
/// name three different defaults.
pub const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// Model transport settings. All optional; the defaults target the public
/// Anthropic endpoint.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    /// The base URL. Defaults to the public Anthropic endpoint via
    /// [`Config::new`].
    pub base_url: Option<String>,
    /// The name of an environment variable that, when set and non-empty,
    /// overrides `base_url`. Lets one agent file serve two modes: the real
    /// endpoint when the variable is unset, a mock or local endpoint when it
    /// is exported. Never the URL itself.
    pub base_url_env: Option<String>,
    /// The name of the environment variable the API key is read from
    /// (default `ANTHROPIC_API_KEY`). Never the key itself.
    pub api_key_env: Option<String>,
    /// How the key authenticates: `"api_key"` (default) for a standard API key
    /// on `x-api-key`, or `"oauth"` for a subscription OAuth token on the
    /// bearer scheme. An unknown value is rejected.
    #[serde(default)]
    pub api_key_kind: ApiKeyKind,
    /// Retry attempts for a retryable model-call failure.
    pub max_retries: Option<u32>,
    /// Per-request timeout, in seconds.
    pub timeout_seconds: Option<u64>,
}

/// Declared budget limits. Mirrors `salvor_replay::Budgets`, with wall time in
/// seconds for
/// a config-friendly shape.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetsConfig {
    /// Maximum completed model calls.
    pub steps: Option<u64>,
    /// Maximum total tokens.
    pub tokens: Option<u64>,
    /// Maximum cost in US dollars (needs `pricing`).
    pub cost_usd: Option<f64>,
    /// Maximum wall time, in seconds.
    pub wall_time_seconds: Option<f64>,
}

/// Per-token pricing, dollars per million tokens.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PricingConfig {
    /// Dollars per million input tokens.
    pub input_per_mtok: f64,
    /// Dollars per million output tokens.
    pub output_per_mtok: f64,
}

/// One MCP server, reached over one of two transports and carrying any per-tool
/// effect overrides.
///
/// Exactly one of `command` and `url` selects the transport: `command` (with
/// its `args`/`env`) spawns a local child process spoken to over stdio, `url`
/// reaches a remote server over streamable HTTP. Setting both, or neither, is a
/// loud parse error, as is pairing a field with the wrong transport (`args` or
/// `env` with `url`, `bearer_token_env` with `command`). `effect_overrides` is
/// valid with either. The exclusivity is enforced by
/// [`validate`](AgentConfig::validate), not by serde, so the error messages can
/// name the specific conflict.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// The program to spawn (stdio transport). Mutually exclusive with `url`.
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments passed to the program. Valid only with `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process. Valid only with
    /// `command`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The URL of a remote MCP server (streamable-HTTP transport). Mutually
    /// exclusive with `command`.
    #[serde(default)]
    pub url: Option<String>,
    /// The name of an environment variable holding a bearer token, sent as
    /// `Authorization: Bearer <token>` on every request to a `url` server.
    /// Never the token itself, mirroring `api_key_env`. Valid only with `url`;
    /// when the variable is unset or empty, the server is reached without auth.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Per-tool [`Effect`] overrides: the operator's trust decision, winning
    /// over the server's annotations. Valid with either transport.
    #[serde(default)]
    pub effect_overrides: BTreeMap<String, Effect>,
    /// Per-tool idempotency key declarations: tool name to the input field
    /// whose value identifies the operation the call performs, as
    /// `{ pay_claim = "claim_id" }`. A dotted path (`"payment.claim_id"`) reads
    /// a nested field. Valid with either transport.
    ///
    /// A tool named here declares an identity, and the store then lets exactly
    /// one run execute a given identity, across separate `salvor run`
    /// invocations. A tool left out is keyless and behaves as it always has.
    /// See the module docs for the derived key's format and what a call missing
    /// the field gets.
    #[serde(default)]
    pub idempotency_keys: BTreeMap<String, String>,
}

impl McpServerConfig {
    /// Checks the transport-exclusivity rules for one server entry.
    ///
    /// # Errors
    ///
    /// Fails when neither or both of `command`/`url` are set, when `args` or
    /// `env` accompany a `url`, or when `bearer_token_env` accompanies a
    /// `command`.
    fn validate(&self) -> Result<()> {
        match (self.command.is_some(), self.url.is_some()) {
            (false, false) => {
                bail!(
                    "an [[mcp_servers]] entry needs exactly one of `command` or `url`; neither is set"
                )
            }
            (true, true) => {
                bail!("an [[mcp_servers]] entry sets both `command` and `url`; use exactly one")
            }
            (true, false) => {
                if self.bearer_token_env.is_some() {
                    bail!("`bearer_token_env` applies only to a `url` server, not a `command` one");
                }
            }
            (false, true) => {
                if !self.args.is_empty() {
                    bail!("`args` applies only to a `command` server, not a `url` one");
                }
                if !self.env.is_empty() {
                    bail!("`env` applies only to a `command` server, not a `url` one");
                }
            }
        }
        self.parse_idempotency_keys()?;
        Ok(())
    }

    /// The declared key paths, parsed and thrown away. Run at load (through
    /// [`validate`](Self::validate)) so a malformed path fails when the file is
    /// read rather than when a payout is dispatched.
    ///
    /// It parses rather than pattern-matching the string, because the parse is
    /// the rule: `salvor_tools::IdempotencyPath` is the same type the runtime
    /// dispatches through, so what a file is allowed to declare and what a call
    /// is allowed to be keyed on cannot come apart. The parsed values are
    /// dropped here because the map the connection is handed is built at build
    /// time, one crate up, where the connection is.
    ///
    /// # Errors
    ///
    /// Fails on an empty path or one with an empty segment, naming the tool.
    pub(crate) fn parse_idempotency_keys(&self) -> Result<()> {
        for (tool, path) in &self.idempotency_keys {
            IdempotencyPath::parse(path)
                .with_context(|| format!("`idempotency_keys` entry for tool `{tool}`"))?;
        }
        Ok(())
    }
}

/// One sandboxed WebAssembly tool: an untrusted component file plus the
/// operator's complete declaration of what the model is told about it and
/// what the sandbox lets it do.
///
/// Everything model-facing (`name`, `description`, the input schema) and the
/// side-effect class (`effect`) is operator-authored, never read from the
/// binary: a hostile component's self-description would be a prompt-injection
/// surface, and its effect class is a trust decision the sandboxed code
/// cannot be allowed to make about itself. `effect` is therefore **required
/// with no default**, deliberately stricter than MCP's default-to-Write: an
/// MCP server legitimately self-describes, so silence needs a safe fallback;
/// a wasm binary has no channel to speak on, so silence can only mean the
/// operator has not decided yet, and [`validate`](Self::validate) refuses it
/// loudly.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WasmToolConfig {
    /// The component file, resolved relative to the agent file's directory.
    pub path: String,
    /// Optional sha256 integrity pin (lowercase or uppercase hex). When set,
    /// a file whose bytes hash differently is refused before it is compiled,
    /// let alone instantiated.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The name the model calls the tool by.
    pub name: String,
    /// The model-facing description. Operator-authored: the guest is never
    /// asked.
    pub description: String,
    /// The side-effect class. Required, no default; `None` here only survives
    /// until [`validate`](Self::validate).
    #[serde(default)]
    pub effect: Option<Effect>,
    /// The input JSON Schema, inline. Exactly one of this and
    /// `input_schema_path` must be set.
    #[serde(default)]
    pub input_schema: Option<String>,
    /// A path to a JSON Schema file, resolved relative to the agent file's
    /// directory. Exactly one of this and `input_schema` must be set.
    #[serde(default)]
    pub input_schema_path: Option<String>,
    /// The input field whose value identifies the operation this tool's calls
    /// perform, as `idempotency_key = "claim_id"`. A dotted path
    /// (`"payment.claim_id"`) reads a nested field. Absent means the tool's
    /// calls have no business identity, which is the default.
    ///
    /// This is the singular of an MCP server's `idempotency_keys` map, because
    /// a `[[wasm_tools]]` entry *is* one tool: it already carries the `name`,
    /// so a map keyed by that same name would only be a second place for it to
    /// be wrong.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Per-call resource caps; defaults apply to any left unset.
    #[serde(default)]
    pub limits: WasmLimitsConfig,
    /// Capability grants; absent means the guest can open nothing.
    #[serde(default)]
    pub grants: WasmGrantsConfig,
}

/// Per-call resource caps for one wasm tool. Mirrors
/// `salvor_wasm::ToolLimits`, with every field optional so a terse entry
/// gets the documented defaults.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WasmLimitsConfig {
    /// Wall-clock cap per call, in milliseconds (default 5000).
    pub wall_time_ms: Option<u64>,
    /// Linear-memory cap per call, in bytes (default 134217728 = 128 MiB).
    pub memory_bytes: Option<u64>,
    /// Optional deterministic fuel budget; unlimited when absent.
    pub fuel: Option<u64>,
}

/// Capability grants for one wasm tool. The only v0.2 grant is directory
/// preopens; network access is deliberately not offered (tools that need the
/// network use MCP).
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WasmGrantsConfig {
    /// Directories exposed to the guest.
    #[serde(default)]
    pub preopen: Vec<PreopenConfig>,
}

/// One preopened directory grant.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreopenConfig {
    /// The host directory, resolved relative to the agent file's directory.
    pub host: String,
    /// The path the guest sees it at (for example `/data`).
    pub guest: String,
    /// What the guest may do inside it.
    pub perms: PreopenPermsConfig,
}

/// The permission level of a preopen, as spelled in the file: `"read"` or
/// `"read_write"`. An unknown value is a loud parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreopenPermsConfig {
    /// List and read only.
    Read,
    /// List, read, create, write, and delete.
    ReadWrite,
}

impl WasmToolConfig {
    /// Checks the per-tool rules: a declared effect and exactly one schema
    /// source. Every message names the offending tool, because an agent file
    /// can carry many `[[wasm_tools]]` entries.
    ///
    /// # Errors
    ///
    /// Fails when `effect` is missing (it has no default on purpose), when
    /// neither or both of `input_schema`/`input_schema_path` are set, or when
    /// an inline `input_schema` is not valid JSON.
    fn validate(&self) -> Result<()> {
        if self.effect.is_none() {
            bail!(
                "wasm tool `{}`: `effect` is required (\"read\", \"idempotent\", or \"write\") \
                 and has no default. The sandboxed binary gets no say in its own side-effect \
                 class, so a missing effect is a missing operator decision, not something to \
                 guess",
                self.name
            );
        }
        match (
            self.input_schema.is_some(),
            self.input_schema_path.is_some(),
        ) {
            (false, false) => bail!(
                "wasm tool `{}`: set exactly one of `input_schema` or `input_schema_path`; \
                 neither is set",
                self.name
            ),
            (true, true) => bail!(
                "wasm tool `{}`: set exactly one of `input_schema` or `input_schema_path`, \
                 not both",
                self.name
            ),
            _ => {}
        }
        if let Some(inline) = &self.input_schema {
            serde_json::from_str::<serde_json::Value>(inline).with_context(|| {
                format!(
                    "wasm tool `{}`: `input_schema` is not valid JSON",
                    self.name
                )
            })?;
        }
        self.parsed_idempotency_key()?;
        Ok(())
    }

    /// The declared key path, parsed. Run at load through
    /// [`validate`](Self::validate), so a malformed path fails on the file
    /// rather than on a call.
    ///
    /// # Errors
    ///
    /// Fails on an empty path or one with an empty segment, naming the tool.
    fn parsed_idempotency_key(&self) -> Result<Option<IdempotencyPath>> {
        self.idempotency_key
            .as_deref()
            .map(|path| {
                IdempotencyPath::parse(path)
                    .with_context(|| format!("wasm tool `{}`: `idempotency_key`", self.name))
            })
            .transpose()
    }
}

impl AgentConfig {
    /// Parses an agent definition from a TOML string, then validates it.
    ///
    /// This is the parse: the same schema, the same unknown-field rejection,
    /// and the same cross-field checks a file read off disk goes through, for
    /// a definition that arrives as text. `salvor_cli::agent_config::load`
    /// reads a file and calls this; the control plane calls it on a submitted
    /// definition; a browser calls it through `salvor-cli-wasm`. One parse, so
    /// none of the three can accept what another refuses.
    ///
    /// # Errors
    ///
    /// Fails when the text is not valid TOML or breaks a cross-field rule.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let config: AgentConfig =
            toml::from_str(text).context("parsing agent definition as TOML")?;
        config.validate()?;
        Ok(config)
    }

    /// Parses an agent definition from a JSON string, then validates it. The
    /// JSON keys are the same as the TOML ones, so a thin SDK can submit the
    /// definition as JSON and get an identical agent.
    ///
    /// # Errors
    ///
    /// Fails when the text is not valid JSON or breaks a cross-field rule.
    pub fn from_json_str(text: &str) -> Result<Self> {
        let config: AgentConfig =
            serde_json::from_str(text).context("parsing agent definition as JSON")?;
        config.validate()?;
        Ok(config)
    }

    /// The cross-field checks every parse applies. Kept separate so a
    /// constructed config (as in unit tests) can be validated too.
    ///
    /// # Errors
    ///
    /// Fails when both prompt fields are set, when both output-schema fields
    /// are set or the inline one is not a table, when `name` is set but empty,
    /// all whitespace, or over [`MAX_NAME_LEN`] characters, when any
    /// `[[mcp_servers]]` entry breaks the `command`/`url`
    /// transport-exclusivity rules (see [`McpServerConfig`]), or when any
    /// `[[wasm_tools]]` entry is missing its required `effect` or breaks the
    /// schema-source rule (see [`WasmToolConfig`]).
    pub fn validate(&self) -> Result<()> {
        if self.system_prompt.is_some() && self.system_prompt_path.is_some() {
            bail!("set only one of `system_prompt` or `system_prompt_path`, not both");
        }
        if self.output_schema.is_some() && self.output_schema_path.is_some() {
            bail!("set only one of `output_schema` or `output_schema_path`, not both");
        }
        if let Some(schema) = &self.output_schema
            && !schema.is_object()
        {
            // A JSON Schema is an object. A scalar here is almost always an
            // author reaching for the path form and writing the value inline,
            // and a validator that silently accepts everything (which is what
            // a non-object schema means) would let that mistake through as a
            // structured run that checks nothing.
            bail!(
                "`output_schema` must be a schema table (for example `[output_schema]` with \
                 `type = \"object\"`), not a {}; use `output_schema_path` to name a JSON file",
                json_kind(schema)
            );
        }
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                bail!("`name`, if set, must not be empty or all whitespace");
            }
            let len = name.chars().count();
            if len > MAX_NAME_LEN {
                bail!("`name` is {len} characters, over the {MAX_NAME_LEN}-character cap");
            }
        }
        for server in &self.mcp_servers {
            server.validate()?;
        }
        for tool in &self.wasm_tools {
            tool.validate()?;
        }
        Ok(())
    }

    /// The environment variable the API key is read from: `[llm] api_key_env`
    /// when the file sets it, else [`DEFAULT_API_KEY_ENV`]. Public so a 401
    /// from the Messages API can be reported against the variable that was
    /// actually consulted, rather than assuming the default. Naming the
    /// variable is not reading it, so this stays on the pure side; the read
    /// itself lives at the IO edge.
    #[must_use]
    pub fn api_key_env(&self) -> &str {
        self.llm
            .api_key_env
            .as_deref()
            .unwrap_or(DEFAULT_API_KEY_ENV)
    }

    /// Every declared idempotency key across `mcp_servers` and `wasm_tools`,
    /// merged into one map from tool name to the raw field-path string as the
    /// file wrote it (not the parsed [`IdempotencyPath`]; this is for display,
    /// not dispatch).
    ///
    /// This reads only the parsed config, so it is available in `--no-connect`
    /// mode too, where nothing was spawned or dialed to ask a server what it
    /// advertises. `salvor agent validate` uses it to echo what a file
    /// declares back to the person checking it.
    #[must_use]
    pub fn declared_idempotency_keys(&self) -> BTreeMap<String, String> {
        let mut keys = BTreeMap::new();
        for server in &self.mcp_servers {
            for (tool, path) in &server.idempotency_keys {
                keys.insert(tool.clone(), path.clone());
            }
        }
        for tool in &self.wasm_tools {
            if let Some(path) = &tool.idempotency_key {
                keys.insert(tool.name.clone(), path.clone());
            }
        }
        keys
    }
}

/// What a JSON value is, in one word, for an error message that has to tell
/// an author what they wrote instead of a schema table.
fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "table",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `record_prompts` parses from the agent TOML: absent leaves it `None`, and
    /// an explicit `true`/`false` is read through.
    #[test]
    fn record_prompts_parses_from_toml() {
        let absent = AgentConfig::from_toml_str("model = \"m\"\n").expect("parses");
        assert_eq!(absent.record_prompts, None);

        let on =
            AgentConfig::from_toml_str("model = \"m\"\nrecord_prompts = true\n").expect("parses");
        assert_eq!(on.record_prompts, Some(true));

        let off =
            AgentConfig::from_toml_str("model = \"m\"\nrecord_prompts = false\n").expect("parses");
        assert_eq!(off.record_prompts, Some(false));
    }

    /// `name` parses from the agent TOML: absent leaves it `None`, and an
    /// explicit value is read through unchanged.
    #[test]
    fn name_parses_from_toml() {
        let absent = AgentConfig::from_toml_str("model = \"m\"\n").expect("parses");
        assert_eq!(absent.name, None);

        let named = AgentConfig::from_toml_str("model = \"m\"\nname = \"support-triage\"\n")
            .expect("parses");
        assert_eq!(named.name.as_deref(), Some("support-triage"));
    }

    /// An empty or all-whitespace `name` is rejected: it would render as
    /// nothing, so it is not a meaningful label. `from_toml_str` runs
    /// `validate` internally, so a blank name fails the whole call, not a
    /// later separate step.
    #[test]
    fn blank_name_is_rejected() {
        for blank in ["", "   ", "\t"] {
            let error = AgentConfig::from_toml_str(&format!("model = \"m\"\nname = \"{blank}\"\n"))
                .expect_err("blank name should be rejected");
            assert!(format!("{error:#}").contains("empty or all whitespace"));
        }
    }

    /// A name over the character cap is a loud, actionable parse error.
    #[test]
    fn oversized_name_is_rejected() {
        let long_name = "a".repeat(MAX_NAME_LEN + 1);
        let toml = format!("model = \"m\"\nname = \"{long_name}\"\n");
        let error = AgentConfig::from_toml_str(&toml).expect_err("oversized name rejected");
        let message = format!("{error:#}");
        assert!(message.contains("65 characters"), "{message}");
        assert!(
            message.contains(&format!("{MAX_NAME_LEN}-character cap")),
            "{message}"
        );
    }

    /// A name exactly at the cap is valid.
    #[test]
    fn name_exactly_at_the_cap_is_valid() {
        let name = "a".repeat(MAX_NAME_LEN);
        let toml = format!("model = \"m\"\nname = \"{name}\"\n");
        let config = AgentConfig::from_toml_str(&toml).expect("parses and validates");
        assert_eq!(config.name.as_deref(), Some(name.as_str()));
    }

    /// An unknown key is refused rather than ignored, which is the whole point
    /// of `deny_unknown_fields`: a misspelled budget that silently never fires
    /// is worse than a file that will not load.
    #[test]
    fn an_unknown_key_is_refused() {
        let error = AgentConfig::from_toml_str("model = \"m\"\nmodle = \"typo\"\n")
            .expect_err("an unknown key is refused");
        assert!(format!("{error:#}").contains("unknown field"), "{error:#}");
    }

    /// A malformed idempotency path fails the parse, not the payout. The rule
    /// is `salvor_tools::IdempotencyPath`'s own, reached from here so a file
    /// cannot declare a path a call could never be keyed on.
    #[test]
    fn a_malformed_idempotency_path_is_refused() {
        let error = AgentConfig::from_toml_str(
            "model = \"m\"\n\n\
             [[mcp_servers]]\n\
             command = \"x\"\n\
             idempotency_keys = { pay_claim = \"a..b\" }\n",
        )
        .expect_err("an empty path segment is refused");
        let message = format!("{error:#}");
        assert!(message.contains("pay_claim"), "{message}");
    }

    /// The inline `[output_schema]` table round-trips into the JSON value the
    /// runtime hashes and hands the model, nested tables and all. Absent, both
    /// schema fields stay `None`, which is what leaves an agent on the plain
    /// text loop.
    #[test]
    fn an_inline_output_schema_parses_from_toml() {
        let absent = AgentConfig::from_toml_str("model = \"m\"\n").expect("parses");
        assert_eq!(absent.output_schema, None);
        assert_eq!(absent.output_schema_path, None);

        let config = AgentConfig::from_toml_str(
            "model = \"m\"\n\n\
             [output_schema]\n\
             type = \"object\"\n\
             required = [\"score\"]\n\n\
             [output_schema.properties.score]\n\
             type = \"number\"\n",
        )
        .expect("parses");
        assert_eq!(
            config.output_schema,
            Some(serde_json::json!({
                "type": "object",
                "required": ["score"],
                "properties": {"score": {"type": "number"}}
            }))
        );
        assert_eq!(config.output_schema_path, None);
    }

    /// The path form parses as the string it is; reading the file it names is
    /// the IO edge's job (`salvor_cli::agent_config::build_agent`), so nothing
    /// here touches the filesystem.
    #[test]
    fn an_output_schema_path_parses_from_toml() {
        let config =
            AgentConfig::from_toml_str("model = \"m\"\noutput_schema_path = \"answer.json\"\n")
                .expect("parses");
        assert_eq!(config.output_schema_path.as_deref(), Some("answer.json"));
        assert_eq!(config.output_schema, None);
    }

    /// Both forms at once is a refusal naming both keys, exactly as the
    /// `system_prompt` pair and a wasm tool's `input_schema` pair are refused:
    /// two sources for one value is an unanswerable question, not a precedence
    /// puzzle to solve quietly.
    #[test]
    fn both_output_schema_forms_at_once_are_refused() {
        let error = AgentConfig::from_toml_str(
            "model = \"m\"\n\
             output_schema_path = \"answer.json\"\n\n\
             [output_schema]\n\
             type = \"object\"\n",
        )
        .expect_err("both schema sources are refused");
        let message = format!("{error:#}");
        assert!(message.contains("output_schema"), "{message}");
        assert!(message.contains("output_schema_path"), "{message}");
        assert!(message.contains("not both"), "{message}");
    }

    /// A non-table `output_schema` is refused. A scalar is not a JSON Schema,
    /// and the structural validator treats a non-object schema as accepting
    /// everything, so letting it through would produce a structured run that
    /// checks nothing.
    #[test]
    fn a_non_table_output_schema_is_refused() {
        for (literal, kind) in [
            ("\"answer.json\"", "string"),
            ("42", "number"),
            ("[\"a\"]", "array"),
            ("true", "boolean"),
        ] {
            let error =
                AgentConfig::from_toml_str(&format!("model = \"m\"\noutput_schema = {literal}\n"))
                    .expect_err("a non-table schema is refused");
            let message = format!("{error:#}");
            assert!(message.contains(kind), "should name the kind: {message}");
            assert!(message.contains("output_schema_path"), "{message}");
        }
    }
}

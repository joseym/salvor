//! The IO edge of the agent-definition file: reading one off disk, resolving
//! what it names in the environment, and building a live [`Agent`] out of it.
//!
//! The file's schema and its parse live one crate down, in
//! [`salvor_cli_core::agent_config`], and are re-exported below so
//! `salvor_cli::agent_config::AgentConfig` keeps naming the same type it
//! always did. Read that module for the field vocabulary, the cross-field
//! rules, and the idempotency-key contract. What is here is everything that
//! needs the host and therefore could not go with it:
//!
//! - [`AgentConfigExt::load`], which reads the file, hands the text to the
//!   parse, and names the path in the error;
//! - [`AgentConfigExt::client_config`] and
//!   [`AgentConfigExt::record_prompts_enabled`], which read the environment
//!   variables the file NAMES (the file never holds a secret, so resolving one
//!   is an IO act);
//! - [`build_agent`], which spawns or dials every declared MCP server, loads
//!   and hashes every declared wasm component, and hands the result to the
//!   runtime's agent builder.
//!
//! The split is what lets a browser and the control plane accept exactly the
//! agent files this binary accepts: the acceptance decision is the parse, and
//! the parse needs no process.
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
//! Two settings decide the effective flag, in this precedence:
//!
//! 1. the per-agent `record_prompts` key in the file, when set (`true` or
//!    `false`); it wins over everything below, so a file can force recording
//!    off even where the environment default is on;
//! 2. otherwise the `SALVOR_RECORD_PROMPTS` environment variable as the global
//!    default: `1`, `true`, or `yes` (case-insensitive) turn recording on;
//!    unset, empty, or any other value leave the default unset, so the env var
//!    can only raise the default, never force a per-agent opt-in back off;
//! 3. otherwise off.
//!
//! In short: per-agent over environment over off. There is deliberately no
//! automatic redaction. If a redaction pass is ever wanted, the recording edge
//! in the runtime is where it would go; today recording is all-or-nothing.
//!
//! The resolution reads the real environment, which is why it is here and not
//! with the schema. The precedence rule itself is a pure function
//! ([`resolve_record_prompts`]) so it stays unit-testable without touching the
//! process environment.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use salvor_llm::{AuthKind, Config};
use salvor_runtime::{Agent, AgentBuildError, Budgets, Pricing};
use salvor_tools::mcp::{EffectOverrides, IdempotencyKeys, McpServer};
use salvor_tools::{DynTool, IdempotencyPath};
use salvor_wasm::{DirGrant, WasmEngine, WasmTool, WasmToolSpec};

// The schema and its parse. Re-exported rather than wrapped, so every existing
// `salvor_cli::agent_config::` path keeps naming the same type, and a caller
// that only needs to parse can reach the pure crate directly instead.
pub use salvor_cli_core::agent_config::{
    AgentConfig, ApiKeyKind, BudgetsConfig, DEFAULT_API_KEY_ENV, LlmConfig, MAX_NAME_LEN,
    McpServerConfig, PreopenConfig, PreopenPermsConfig, PricingConfig, WasmGrantsConfig,
    WasmLimitsConfig, WasmToolConfig,
};

/// The environment variable naming the global default for prompt-body
/// recording. Set to `1`/`true`/`yes` (case-insensitive) to default recording
/// on; anything else leaves the default unset. Per-agent `record_prompts`
/// overrides it either way. See the module docs.
const RECORD_PROMPTS_ENV: &str = "SALVOR_RECORD_PROMPTS";

/// Resolves the effective prompt-recording flag from the per-agent setting and
/// the global env default. Per-agent wins over env, env over off:
/// `per_agent.or(env_default).unwrap_or(false)`. Kept pure (both inputs are
/// passed in) so the precedence is unit-testable without touching the real
/// environment.
fn resolve_record_prompts(per_agent: Option<bool>, env_default: Option<bool>) -> bool {
    per_agent.or(env_default).unwrap_or(false)
}

/// Parses the `SALVOR_RECORD_PROMPTS` spelling into a default. `1`, `true`, or
/// `yes` (case-insensitive, surrounding whitespace ignored) mean on; unset,
/// empty, or anything else yield `None`, so the env var never forces a
/// per-agent opt-in back off. It can only raise the default, never lower it.
fn parse_record_prompts_env(raw: Option<&str>) -> Option<bool> {
    match raw
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("1" | "true" | "yes") => Some(true),
        _ => None,
    }
}

/// Reads the global recording default from the real environment.
fn env_record_prompts_default() -> Option<bool> {
    parse_record_prompts_env(std::env::var(RECORD_PROMPTS_ENV).ok().as_deref())
}

/// The host-side half of [`AgentConfig`]: the operations that read a file or
/// an environment variable, and so could not live with the parse.
///
/// A trait rather than free functions so the call reads the way it always did
/// (`AgentConfig::load(path)`, `config.client_config()`); bring it into scope
/// and the method is there. The parse-side methods
/// ([`AgentConfig::from_toml_str`], [`AgentConfig::validate`],
/// [`AgentConfig::api_key_env`], [`AgentConfig::declared_idempotency_keys`])
/// are inherent on the type and need no import.
pub trait AgentConfigExt: Sized {
    /// Parses an agent file, rejecting unknown fields and mutually exclusive
    /// prompt settings.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, is not valid TOML, carries an
    /// unknown field, or breaks a cross-field rule. The path is named in the
    /// message either way.
    fn load(path: &Path) -> Result<Self>;

    /// The client [`Config`] the `[llm]` section resolves to. Reads the API
    /// key from the named environment variable, leaving it unset (fine for
    /// local endpoints) when the variable is unset or empty. When
    /// `base_url_env` names a set, non-empty variable, its value overrides
    /// `base_url`. Public so the resolution itself is testable.
    fn client_config(&self) -> Config;

    /// The effective prompt-recording flag: the per-agent `record_prompts`
    /// setting resolved against the `SALVOR_RECORD_PROMPTS` env default. Per
    /// agent wins over env, env over off (see the module docs). Reads the real
    /// environment, so both the CLI and the server factory get the same answer.
    fn record_prompts_enabled(&self) -> bool;
}

impl AgentConfigExt for AgentConfig {
    fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading agent file {}", path.display()))?;
        let config = AgentConfig::from_toml_str(&text)
            .with_context(|| format!("parsing agent file {}", path.display()))?;
        Ok(config)
    }

    fn client_config(&self) -> Config {
        let mut config = Config::new();
        let override_url = self
            .llm
            .base_url_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|url| !url.is_empty());
        if let Some(url) = override_url {
            config = config.with_base_url(url);
        } else if let Some(base_url) = &self.llm.base_url {
            config = config.with_base_url(base_url);
        }
        let key_env = self.api_key_env();
        if let Ok(key) = std::env::var(key_env)
            && !key.is_empty()
        {
            config = config.with_api_key(key);
        }
        config = config.with_auth_kind(auth_kind(self.llm.api_key_kind));
        if let Some(max_retries) = self.llm.max_retries {
            config = config.with_max_retries(max_retries);
        }
        if let Some(timeout) = self.llm.timeout_seconds {
            config = config.with_timeout(Duration::from_secs(timeout));
        }
        config
    }

    fn record_prompts_enabled(&self) -> bool {
        resolve_record_prompts(self.record_prompts, env_record_prompts_default())
    }
}

/// The [`AuthKind`] a config-file `api_key_kind` maps to. The file's spelling
/// lives with the schema; the transport type it selects lives here, with the
/// client this crate builds.
fn auth_kind(kind: ApiKeyKind) -> AuthKind {
    match kind {
        ApiKeyKind::ApiKey => AuthKind::ApiKey,
        ApiKeyKind::Oauth => AuthKind::Bearer,
    }
}

/// The declared budgets as the runtime type.
fn budgets(config: &AgentConfig) -> Budgets {
    Budgets {
        max_steps: config.budgets.steps,
        max_tokens: config.budgets.tokens,
        max_cost_usd: config.budgets.cost_usd,
        max_wall_time: config
            .budgets
            .wall_time_seconds
            .map(Duration::from_secs_f64),
    }
}

/// The system prompt text, reading the file when `system_prompt_path` is set
/// (relative to `agent_dir`).
fn system_prompt(config: &AgentConfig, agent_dir: &Path) -> Result<Option<String>> {
    if let Some(prompt) = &config.system_prompt {
        return Ok(Some(prompt.clone()));
    }
    if let Some(rel) = &config.system_prompt_path {
        let path = agent_dir.join(rel);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading system prompt file {}", path.display()))?;
        return Ok(Some(text));
    }
    Ok(None)
}

/// The declared key paths for one MCP server, parsed into the map the
/// connection is handed. The parse already ran at load (that is what refuses a
/// malformed path on the file); this is the same parse again, kept here because
/// the map it builds belongs to the connection, and `build_agent` is reachable
/// from a caller that assembled the config by hand.
///
/// # Errors
///
/// Fails on an empty path or one with an empty segment, naming the tool.
fn parsed_idempotency_keys(server: &McpServerConfig) -> Result<IdempotencyKeys> {
    let mut keys = IdempotencyKeys::new();
    for (tool, path) in &server.idempotency_keys {
        let parsed = IdempotencyPath::parse(path)
            .with_context(|| format!("`idempotency_keys` entry for tool `{tool}`"))?;
        keys.insert(tool.clone(), parsed);
    }
    Ok(keys)
}

/// The declared key path for one wasm tool, parsed, for the same reason.
///
/// # Errors
///
/// Fails on an empty path or one with an empty segment, naming the tool.
fn parsed_idempotency_key(tool: &WasmToolConfig) -> Result<Option<IdempotencyPath>> {
    tool.idempotency_key
        .as_deref()
        .map(|path| {
            IdempotencyPath::parse(path)
                .with_context(|| format!("wasm tool `{}`: `idempotency_key`", tool.name))
        })
        .transpose()
}

/// The runtime limits a `[wasm_tools.limits]` table resolves to, with defaults
/// filled in.
fn tool_limits(limits: &WasmLimitsConfig) -> salvor_wasm::ToolLimits {
    let defaults = salvor_wasm::ToolLimits::default();
    salvor_wasm::ToolLimits {
        wall_time_ms: limits.wall_time_ms.unwrap_or(defaults.wall_time_ms),
        memory_bytes: limits.memory_bytes.unwrap_or(defaults.memory_bytes),
        fuel: limits.fuel,
    }
}

/// The runtime grant level a preopen's spelling maps to.
fn grant_perms(perms: PreopenPermsConfig) -> salvor_wasm::GrantPerms {
    match perms {
        PreopenPermsConfig::Read => salvor_wasm::GrantPerms::Read,
        PreopenPermsConfig::ReadWrite => salvor_wasm::GrantPerms::ReadWrite,
    }
}

/// The input schema as a JSON value, reading the schema file when
/// `input_schema_path` is set (relative to `agent_dir`).
fn resolved_input_schema(tool: &WasmToolConfig, agent_dir: &Path) -> Result<serde_json::Value> {
    if let Some(inline) = &tool.input_schema {
        return serde_json::from_str(inline).with_context(|| {
            format!(
                "wasm tool `{}`: `input_schema` is not valid JSON",
                tool.name
            )
        });
    }
    let rel = tool
        .input_schema_path
        .as_ref()
        .expect("validate guarantees a schema source");
    let path = agent_dir.join(rel);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "wasm tool `{}`: reading input schema file {}",
            tool.name,
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| {
        format!(
            "wasm tool `{}`: input schema file {} is not valid JSON",
            tool.name,
            path.display()
        )
    })
}

/// The agent's declared output schema as a JSON value: the inline
/// `[output_schema]` table as parsed, or the JSON file `output_schema_path`
/// names (relative to `agent_dir`). `None` when the file declares neither,
/// which is what leaves the agent on the plain text loop.
///
/// The object check the inline form got at parse time is repeated on the file's
/// contents here, for the same reason it exists there: a schema that is not an
/// object accepts everything, so a run under it would be structured in name
/// only.
///
/// # Errors
///
/// Fails when the named file cannot be read, does not hold valid JSON, or holds
/// something other than a JSON object.
fn resolved_output_schema(
    config: &AgentConfig,
    agent_dir: &Path,
) -> Result<Option<serde_json::Value>> {
    if let Some(inline) = &config.output_schema {
        return Ok(Some(inline.clone()));
    }
    let Some(rel) = &config.output_schema_path else {
        return Ok(None);
    };
    let path = agent_dir.join(rel);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading output schema file {}", path.display()))?;
    let schema: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("output schema file {} is not valid JSON", path.display()))?;
    if !schema.is_object() {
        bail!(
            "output schema file {} must hold a JSON Schema object, not a bare value",
            path.display()
        );
    }
    Ok(Some(schema))
}

/// Builds a live [`Agent`] from a parsed config, spawning every declared MCP
/// server and registering its tools.
///
/// Returns the agent together with the connected [`McpServer`] handles. Those
/// handles **must stay alive for as long as the agent's tools are dispatched**:
/// each MCP tool holds a client-peer clone into its server's session, so
/// dropping the handles ends the sessions and the tools stop working. The
/// caller keeps them in scope across the run and drops (or closes) them
/// afterward.
///
/// `agent_path` is the path the config was loaded from; it fixes the base
/// directory for a relative `system_prompt_path`.
///
/// `no_connect` skips this step entirely: no MCP server is spawned (a `command`
/// transport) or dialed (a `url` transport), so a declared server whose command
/// does not exist, or whose endpoint is unreachable, does not fail the build.
/// The returned agent then carries no MCP tools and the returned server list
/// is always empty; the caller is the one that knows how many servers were
/// declared and skipped (`config.mcp_servers.len()`), since that count is not
/// otherwise recoverable from the return value. Because MCP tool schemas feed
/// [`Agent::def_hash`], a `no_connect` build's hash does not match the hash a
/// real run would record.
///
/// # Errors
///
/// Fails when the system prompt file or a named `output_schema_path` cannot be
/// read (or does not hold a JSON object), an MCP server cannot be spawned or
/// initialized (unless `no_connect` is set), or the builder rejects the
/// definition (a duplicate tool name, or a cost budget with no pricing).
pub async fn build_agent(
    config: &AgentConfig,
    agent_path: &Path,
    no_connect: bool,
) -> Result<(Agent, Vec<McpServer>)> {
    let agent_dir = agent_path.parent().unwrap_or_else(|| Path::new("."));

    let mut builder = Agent::builder().model(config.client_config(), &config.model);
    if let Some(name) = &config.name {
        builder = builder.name(name.clone());
    }
    if let Some(prompt) = system_prompt(config, agent_dir)? {
        builder = builder.system_prompt(prompt);
    }
    let budgets = budgets(config);
    if budgets.any_declared() {
        builder = builder.budgets(budgets);
    }
    if let Some(pricing) = &config.pricing {
        builder = builder.pricing(Pricing {
            input_per_mtok: pricing.input_per_mtok,
            output_per_mtok: pricing.output_per_mtok,
        });
    }
    if let Some(max_tokens) = config.max_response_tokens {
        builder = builder.max_response_tokens(max_tokens);
    }
    // Whichever form the file used, the builder sees the schema value, and the
    // value is what `agent_def_hash` covers: an agent that moves its schema
    // between the inline table and a JSON file keeps its identity.
    if let Some(schema) = resolved_output_schema(config, agent_dir)? {
        builder = builder.output_schema(schema);
    }
    // Resolve the prompt-recording flag once, here, so both the CLI and the
    // server factory (which both call this function) get the same precedence.
    builder = builder.record_prompts(config.record_prompts_enabled());

    let mut servers = Vec::new();
    if no_connect {
        // Field/shape validation only: `AgentConfig::load` already ran
        // `validate` over every declared server (exactly one transport set,
        // required fields present), so there is nothing left to check without
        // spawning a process or dialing a socket. Skip the loop below entirely.
    } else {
        for server_config in &config.mcp_servers {
            let mut overrides = EffectOverrides::new();
            for (name, effect) in &server_config.effect_overrides {
                overrides.insert(name.clone(), *effect);
            }
            // Parsed again here rather than carried from load, because
            // `build_agent` is reachable from a caller that built the config by
            // hand; the paths are a handful of strings and parsing is free.
            let keys = parsed_idempotency_keys(server_config)?;

            // `validate` (run at load) guarantees exactly one transport is set, so
            // the `url`-first branch is exhaustive: a config that reaches here with
            // neither would already have failed to load.
            let mut server = if let Some(url) = &server_config.url {
                // The bearer token, if any, is read from the named environment
                // variable, never the file. An unset or empty variable means no
                // auth, matching how `api_key_env` treats a missing key.
                let token = server_config
                    .bearer_token_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok())
                    .filter(|t| !t.is_empty());
                McpServer::connect_http(url, token.as_deref(), &overrides, &keys)
                    .await
                    .with_context(|| format!("connecting to MCP server at `{url}`"))?
            } else {
                let command_name = server_config
                    .command
                    .as_deref()
                    .expect("validate guarantees a command when there is no url");
                let mut command = tokio::process::Command::new(command_name);
                command.args(&server_config.args);
                for (key, value) in &server_config.env {
                    command.env(key, value);
                }
                McpServer::connect(command, &overrides, &keys)
                    .await
                    .with_context(|| format!("connecting to MCP server `{command_name}`"))?
            };

            check_declared_keys_exist(&keys, &server)?;

            for tool in server.take_tools() {
                builder = builder.tool_dyn(Box::new(tool));
            }
            servers.push(server);
        }
    }

    // Sandboxed wasm tools. One engine (compiler, WASI linker, epoch ticker)
    // is shared by every tool; each tool holds an Arc to it, so nothing extra
    // needs to stay alive after this function returns. Loading verifies any
    // sha256 pin against the file's bytes before compiling, so a tampered
    // component fails the build here, not mid-run.
    if !config.wasm_tools.is_empty() {
        let engine = WasmEngine::new().context("initializing the wasm sandbox engine")?;
        for tool_config in &config.wasm_tools {
            let component_path = agent_dir.join(&tool_config.path);
            let spec = WasmToolSpec {
                name: tool_config.name.clone(),
                description: tool_config.description.clone(),
                effect: tool_config
                    .effect
                    .expect("validate (run at load) guarantees an effect"),
                input_schema: resolved_input_schema(tool_config, agent_dir)?,
                idempotency_key: parsed_idempotency_key(tool_config)?,
                limits: tool_limits(&tool_config.limits),
                grants: tool_config
                    .grants
                    .preopen
                    .iter()
                    .map(|preopen| DirGrant {
                        host: agent_dir.join(&preopen.host),
                        guest: preopen.guest.clone(),
                        perms: grant_perms(preopen.perms),
                    })
                    .collect(),
            };
            let tool = WasmTool::load(
                Arc::clone(&engine),
                &component_path,
                tool_config.sha256.as_deref(),
                spec,
            )
            .with_context(|| {
                format!(
                    "loading wasm tool `{}` from {}",
                    tool_config.name,
                    component_path.display()
                )
            })?;
            builder = builder.tool_dyn(Box::new(tool));
        }
    }

    let agent = builder.build().map_err(build_error_context)?;
    Ok((agent, servers))
}

/// Fails the build when `idempotency_keys` names a tool the connected server
/// does not advertise.
///
/// `effect_overrides` ignores a name it does not find, and that is defensible:
/// an override that binds to nothing leaves the tool with the effect the
/// mapping already gave it, which is the safe class. An idempotency key that
/// binds to nothing is the opposite. The operator declared that a call has an
/// identity, and silence would leave the tool keyless, which is precisely the
/// state that lets a second run pay a claim twice. A misspelled tool name in
/// this table is a payments bug, so it stops the build.
///
/// The message lists what the server does advertise, since the usual cause is a
/// typo or a server whose tool was renamed.
///
/// # Errors
///
/// Fails naming the tool, the server's advertised tools, and what the entry was
/// asking for.
fn check_declared_keys_exist(keys: &IdempotencyKeys, server: &McpServer) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let advertised: Vec<&str> = server.tools().iter().map(DynTool::name).collect();
    for declared in keys.names() {
        if !advertised.contains(&declared) {
            let list = if advertised.is_empty() {
                "this server advertises no tools at all".to_owned()
            } else {
                format!("this server advertises: {}", advertised.join(", "))
            };
            bail!(
                "`idempotency_keys` declares a key for tool `{declared}`, but {list}. A key that \
                 binds to no tool would leave the tool it was meant for keyless, which is the \
                 state cross-run deduplication exists to prevent, so this is refused rather than \
                 ignored. Check the spelling, or drop the entry"
            );
        }
    }
    Ok(())
}

/// Turns an [`AgentBuildError`] into an actionable message. The cost-budget
/// case is the one worth spelling out, since the fix (add `[pricing]`) is not
/// obvious from the bare error.
fn build_error_context(error: AgentBuildError) -> anyhow::Error {
    match error {
        AgentBuildError::CostBudgetWithoutPricing => anyhow::anyhow!(
            "budgets.cost_usd is set but there is no [pricing] table; add pricing with input_per_mtok and output_per_mtok, or remove the cost budget"
        ),
        other => anyhow::Error::new(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The precedence rule, exercised over the four cases that matter. Per
    /// agent wins over the env default, which wins over off. The env default is
    /// always `Some(true)` or `None` (see [`parse_record_prompts_env`]), so
    /// these cover every reachable combination.
    #[test]
    fn record_prompts_precedence() {
        // Per-agent true with env unset: on.
        assert!(resolve_record_prompts(Some(true), None));
        // Per-agent unset with env default true: on.
        assert!(resolve_record_prompts(None, Some(true)));
        // Both unset: off.
        assert!(!resolve_record_prompts(None, None));
        // Per-agent false overrides an env default of true: off.
        assert!(!resolve_record_prompts(Some(false), Some(true)));
    }

    /// The env spellings that turn recording on, and everything that leaves the
    /// default unset (so it can never force a per-agent opt-in back off).
    #[test]
    fn record_prompts_env_parsing() {
        for on in ["1", "true", "TRUE", "Yes", "  yes  "] {
            assert_eq!(parse_record_prompts_env(Some(on)), Some(true), "{on:?}");
        }
        for unset in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("x"),
        ] {
            assert_eq!(parse_record_prompts_env(unset), None, "{unset:?}");
        }
    }

    /// The `salvor agent validate --no-connect` contract, exercised directly
    /// against [`build_agent`] rather than the binary: a declared `command`
    /// that does not exist on this machine fails an ordinary build (the
    /// process spawn fails), but passes with `no_connect: true`, since that
    /// mode never spawns anything and checks fields and shape only.
    #[tokio::test]
    async fn no_connect_skips_a_command_that_does_not_exist() {
        let config = AgentConfig::from_toml_str(
            "model = \"m\"\n\n\
             [[mcp_servers]]\n\
             command = \"this-command-does-not-exist-anywhere-on-path\"\n",
        )
        .expect("parses: field/shape validation happens at `load`, not `build_agent`");
        let pseudo_path = Path::new("agent.toml");

        let connected = build_agent(&config, pseudo_path, false).await;
        assert!(
            connected.is_err(),
            "default mode spawns the declared command and must fail when it does not exist"
        );

        let (agent, servers) = build_agent(&config, pseudo_path, true)
            .await
            .expect("--no-connect must not spawn the command, so a missing one still passes");
        assert!(
            servers.is_empty(),
            "no-connect mode connects to nothing, so there are no sessions to keep alive"
        );
        assert_eq!(
            agent.tools().len(),
            0,
            "no-connect mode collects no tools, since collecting them needs a connection"
        );
    }

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
}

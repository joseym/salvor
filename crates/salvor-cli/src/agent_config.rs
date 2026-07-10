//! The agent-definition file: a TOML document the CLI reads into a live
//! [`Agent`].
//!
//! Under Salvor's single built-in loop an agent is pure data (model, prompt,
//! tools, budgets), which is exactly what makes a config file a legitimate
//! home for it. This module owns that file's schema and the mapping
//! from it into the runtime types.
//!
//! # Schema
//!
//! ```toml
//! # Required. The model id sent with every request.
//! model = "claude-opus-4-8"
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
//!
//! # A local stdio server:
//! [[mcp_servers]]
//! command = "python"
//! args = ["-m", "my_server"]
//! env = { API_TOKEN = "..." }
//! effect_overrides = { delete = "write", fetch = "read" }
//!
//! # A remote HTTP server. `bearer_token_env` NAMES an env var holding a bearer
//! # token (never the token itself); when set and non-empty it is sent as
//! # `Authorization: Bearer <token>`. Omit it for a server that needs no auth.
//! [[mcp_servers]]
//! url = "https://mcp.example.com/mcp"
//! bearer_token_env = "MY_MCP_TOKEN"
//! effect_overrides = { delete = "write" }
//! ```
//!
//! Native Rust tools are code, not config, so the CLI does not register them:
//! MCP is the config-reachable tool boundary and covers the demo. Unknown
//! fields are **rejected**, not ignored, so a typo like `step` instead of
//! `steps` is a loud parse error rather than a silently dropped budget.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use salvor_core::Effect;
use salvor_llm::{AuthKind, Config};
use salvor_runtime::{Agent, AgentBuildError, Budgets, Pricing};
use salvor_tools::mcp::{EffectOverrides, McpServer};
use serde::Deserialize;

/// The full agent definition, parsed from the TOML file. Every optional field
/// carries `#[serde(default)]` so a terse file is valid; `deny_unknown_fields`
/// turns a misspelled key into an error instead of a silent no-op.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// The model id sent with each request. Required.
    pub model: String,
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
    /// runtime's [`DEFAULT_MAX_RESPONSE_TOKENS`](salvor_runtime::DEFAULT_MAX_RESPONSE_TOKENS).
    #[serde(default)]
    pub max_response_tokens: Option<u32>,
    /// MCP servers whose tools the agent may call.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

/// How the API key authenticates, as named in the `[llm]` section. Mirrors
/// [`salvor_llm::AuthKind`] but stays a config-layer type so the wire spelling
/// (`"api_key"` / `"oauth"`) lives with the schema. An unknown value is a loud
/// parse error, not a silent fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyKind {
    /// Send the key as `x-api-key`. The default, for standard API keys.
    #[default]
    ApiKey,
    /// Send the key as an `Authorization: Bearer` credential with the oauth
    /// beta header, for subscription OAuth tokens.
    Oauth,
}

impl ApiKeyKind {
    /// The [`AuthKind`] this maps to in the client config.
    fn auth_kind(self) -> AuthKind {
        match self {
            ApiKeyKind::ApiKey => AuthKind::ApiKey,
            ApiKeyKind::Oauth => AuthKind::Bearer,
        }
    }
}

/// Model transport settings. All optional; the defaults target the public
/// Anthropic endpoint.
#[derive(Debug, Default, Deserialize)]
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

/// Declared budget limits. Mirrors [`Budgets`], with wall time in seconds for
/// a config-friendly shape.
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
        Ok(())
    }
}

impl AgentConfig {
    /// Parses an agent file, rejecting unknown fields and mutually exclusive
    /// prompt settings.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, is not valid TOML, carries an
    /// unknown field, or sets both `system_prompt` and `system_prompt_path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading agent file {}", path.display()))?;
        let config: AgentConfig = toml::from_str(&text)
            .with_context(|| format!("parsing agent file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// The cross-field checks `load` applies. Kept separate so a constructed
    /// config (as in unit tests) can be validated too.
    ///
    /// # Errors
    ///
    /// Fails when both prompt fields are set, or when any `[[mcp_servers]]`
    /// entry breaks the `command`/`url` transport-exclusivity rules (see
    /// [`McpServerConfig`]).
    pub fn validate(&self) -> Result<()> {
        if self.system_prompt.is_some() && self.system_prompt_path.is_some() {
            bail!("set only one of `system_prompt` or `system_prompt_path`, not both");
        }
        for server in &self.mcp_servers {
            server.validate()?;
        }
        Ok(())
    }

    /// The declared budgets as the runtime type.
    fn budgets(&self) -> Budgets {
        Budgets {
            max_steps: self.budgets.steps,
            max_tokens: self.budgets.tokens,
            max_cost_usd: self.budgets.cost_usd,
            max_wall_time: self.budgets.wall_time_seconds.map(Duration::from_secs_f64),
        }
    }

    /// The client [`Config`] the `[llm]` section resolves to. Reads the API
    /// key from the named environment variable, leaving it unset (fine for
    /// local endpoints) when the variable is unset or empty. When
    /// `base_url_env` names a set, non-empty variable, its value overrides
    /// `base_url`. Public so the resolution itself is testable.
    #[must_use]
    pub fn client_config(&self) -> Config {
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
        let key_env = self
            .llm
            .api_key_env
            .as_deref()
            .unwrap_or("ANTHROPIC_API_KEY");
        if let Ok(key) = std::env::var(key_env)
            && !key.is_empty()
        {
            config = config.with_api_key(key);
        }
        config = config.with_auth_kind(self.llm.api_key_kind.auth_kind());
        if let Some(max_retries) = self.llm.max_retries {
            config = config.with_max_retries(max_retries);
        }
        if let Some(timeout) = self.llm.timeout_seconds {
            config = config.with_timeout(Duration::from_secs(timeout));
        }
        config
    }

    /// The system prompt text, reading the file when `system_prompt_path` is
    /// set (relative to `agent_dir`).
    fn system_prompt(&self, agent_dir: &Path) -> Result<Option<String>> {
        if let Some(prompt) = &self.system_prompt {
            return Ok(Some(prompt.clone()));
        }
        if let Some(rel) = &self.system_prompt_path {
            let path = agent_dir.join(rel);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading system prompt file {}", path.display()))?;
            return Ok(Some(text));
        }
        Ok(None)
    }
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
/// # Errors
///
/// Fails when the system prompt file cannot be read, an MCP server cannot be
/// spawned or initialized, or the builder rejects the definition (a duplicate
/// tool name, or a cost budget with no pricing).
pub async fn build_agent(
    config: &AgentConfig,
    agent_path: &Path,
) -> Result<(Agent, Vec<McpServer>)> {
    let agent_dir = agent_path.parent().unwrap_or_else(|| Path::new("."));

    let mut builder = Agent::builder().model(config.client_config(), &config.model);
    if let Some(prompt) = config.system_prompt(agent_dir)? {
        builder = builder.system_prompt(prompt);
    }
    let budgets = config.budgets();
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

    let mut servers = Vec::new();
    for server_config in &config.mcp_servers {
        let mut overrides = EffectOverrides::new();
        for (name, effect) in &server_config.effect_overrides {
            overrides.insert(name.clone(), *effect);
        }

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
            McpServer::connect_http(url, token.as_deref(), &overrides)
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
            McpServer::connect(command, &overrides)
                .await
                .with_context(|| format!("connecting to MCP server `{command_name}`"))?
        };

        for tool in server.take_tools() {
            builder = builder.tool_dyn(Box::new(tool));
        }
        servers.push(server);
    }

    let agent = builder.build().map_err(build_error_context)?;
    Ok((agent, servers))
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

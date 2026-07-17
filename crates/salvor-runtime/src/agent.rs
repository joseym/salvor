//! [`Agent`] and [`Agent::builder`]: the batteries-included agent
//! definition.
//!
//! An agent is model + system prompt + tools + budgets (+ pricing when a
//! cost budget is declared). Under the single built-in loop, that makes an
//! agent definition pure data, and pure data can be content-hashed:
//!
//! # The definition hash
//!
//! `agent_def_hash` (recorded in `RunStarted` and re-checked on every
//! resume) is `sha256:` over the canonical serialization (see
//! [`crate::hash`]) of this JSON value:
//!
//! ```json
//! {
//!     "budgets": {"max_cost_usd": null, "max_steps": 40, "max_tokens": null,
//!                  "max_wall_time_seconds": null},
//!     "model": "<model id>",
//!     "pricing": {"input_per_mtok": 3.0, "output_per_mtok": 15.0},
//!     "system_prompt": "...",
//!     "tools": [{"description": "...", "effect": "read",
//!                "input_schema": { ... }, "name": "..."} , ...]
//! }
//! ```
//!
//! Tools appear sorted by name (the `ToolSet` enumerates them that way), so
//! registration order never changes the hash; any change to the model id,
//! prompt, a tool contract, a budget, or pricing does. The client
//! configuration (base URL, API key, retries) is deliberately *not* hashed:
//! it is transport, not definition, and pointing the same agent at a local
//! endpoint must not orphan its recorded runs. MCP-backed tools participate
//! exactly like native ones, through their `DynTool` descriptors.
//!
//! # Build-time checks
//!
//! [`AgentBuilder::build`] fails (rather than letting a run fail later)
//! when no model is configured, when a duplicate tool name is registered,
//! or when a cost budget is declared without [`Pricing`], since a cost
//! check without rates cannot be computed at all.

use std::collections::BTreeMap;

use salvor_llm::{Client, Config};
use salvor_tools::{DynTool, ToolHandler, ToolSet};
use serde_json::{Value, json};
use thiserror::Error;

use crate::budgets::{Budgets, Pricing};
use crate::hash::hash_value;

/// The default `max_tokens` sent with each model request when the builder
/// is not told otherwise.
pub const DEFAULT_MAX_RESPONSE_TOKENS: u32 = 4096;

/// A built agent definition plus the client that executes its model calls.
/// Construct with [`Agent::builder`].
pub struct Agent {
    client: Client,
    model: String,
    system_prompt: Option<String>,
    tools: ToolSet,
    budgets: Budgets,
    pricing: Option<Pricing>,
    max_response_tokens: u32,
    def_hash: String,
    record_prompts: bool,
    labels: Option<BTreeMap<String, String>>,
    name: Option<String>,
}

impl Agent {
    /// Starts building an agent.
    #[must_use]
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    /// The content hash of this definition, as recorded in `RunStarted`.
    /// Computed once at build time; see the module docs for what it covers.
    #[must_use]
    pub fn def_hash(&self) -> &str {
        &self.def_hash
    }

    /// The client model calls go through.
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The model id sent with every request.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The system prompt, when one is set.
    #[must_use]
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// The tools the model may call.
    #[must_use]
    pub fn tools(&self) -> &ToolSet {
        &self.tools
    }

    /// The declared budgets.
    #[must_use]
    pub fn budgets(&self) -> &Budgets {
        &self.budgets
    }

    /// The pricing table, when one is set.
    #[must_use]
    pub fn pricing(&self) -> Option<&Pricing> {
        self.pricing.as_ref()
    }

    /// The `max_tokens` cap sent with each model request.
    #[must_use]
    pub fn max_response_tokens(&self) -> u32 {
        self.max_response_tokens
    }

    /// Whether runs of this agent record the full model request body into the
    /// durable log. Off by default; see [`AgentBuilder::record_prompts`]. This
    /// is operator/transport policy, not part of the definition, so it is
    /// deliberately excluded from [`def_hash`](Self::def_hash): flipping it
    /// must not orphan an agent's recorded runs.
    #[must_use]
    pub fn record_prompts(&self) -> bool {
        self.record_prompts
    }

    /// Correlation tags to stamp on every fresh run of this agent, when set
    /// with [`AgentBuilder::labels`]. Like [`record_prompts`](Self::record_prompts),
    /// this is operator/deployment metadata, not part of the definition, so
    /// it is deliberately excluded from [`def_hash`](Self::def_hash):
    /// relabeling an agent must not orphan its recorded runs.
    #[must_use]
    pub fn labels(&self) -> Option<&BTreeMap<String, String>> {
        self.labels.as_ref()
    }

    /// A short human label for this agent, when set with
    /// [`AgentBuilder::name`] — a display name the control plane's agent
    /// registry (`GET /v1/agents/{hash}`) can hand back to a caller that only
    /// has the hash. Like [`labels`](Self::labels) and
    /// [`record_prompts`](Self::record_prompts), this is descriptive
    /// metadata, not part of the definition, so it is deliberately excluded
    /// from [`def_hash`](Self::def_hash): renaming an agent must not mint a
    /// new identity or orphan its recorded runs.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Builds an [`Agent`]:
///
/// ```no_run
/// use salvor_llm::Config;
/// use salvor_runtime::{Agent, Budgets};
///
/// # fn demo() -> Result<(), salvor_runtime::AgentBuildError> {
/// let agent = Agent::builder()
///     .model(Config::from_env(), "claude-opus-4-8")
///     .system_prompt("You are a research agent.")
///     .budgets(Budgets {
///         max_steps: Some(40),
///         ..Budgets::default()
///     })
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct AgentBuilder {
    client: Option<Client>,
    config: Option<Config>,
    model: Option<String>,
    system_prompt: Option<String>,
    tools: Vec<Box<dyn DynTool>>,
    budgets: Budgets,
    pricing: Option<Pricing>,
    max_response_tokens: Option<u32>,
    record_prompts: bool,
    labels: Option<BTreeMap<String, String>>,
    name: Option<String>,
}

impl AgentBuilder {
    /// An empty builder; [`Agent::builder`] is the usual entry point.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the model by client configuration plus model id. The client is
    /// constructed at [`build`](Self::build) time.
    #[must_use]
    pub fn model(mut self, config: Config, model: impl Into<String>) -> Self {
        self.config = Some(config);
        self.client = None;
        self.model = Some(model.into());
        self
    }

    /// Sets the model by an already-built client plus model id.
    #[must_use]
    pub fn client(mut self, client: Client, model: impl Into<String>) -> Self {
        self.client = Some(client);
        self.config = None;
        self.model = Some(model.into());
        self
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Adds a typed tool handler. Duplicate names are reported at
    /// [`build`](Self::build) time.
    #[must_use]
    pub fn tool<H: ToolHandler + 'static>(mut self, handler: H) -> Self {
        self.tools
            .push(Box::new(salvor_tools::TypedTool::new(handler)));
        self
    }

    /// Adds an already type-erased tool. This is how MCP-backed tools (and
    /// any other runtime-defined `DynTool`) join the agent, on equal footing
    /// with native handlers.
    #[must_use]
    pub fn tool_dyn(mut self, tool: Box<dyn DynTool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Sets the declared budgets.
    #[must_use]
    pub fn budgets(mut self, budgets: Budgets) -> Self {
        self.budgets = budgets;
        self
    }

    /// Sets the pricing table cost budgets are computed against.
    #[must_use]
    pub fn pricing(mut self, pricing: Pricing) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// Sets the `max_tokens` cap sent with each model request (default
    /// [`DEFAULT_MAX_RESPONSE_TOKENS`]).
    #[must_use]
    pub fn max_response_tokens(mut self, max_tokens: u32) -> Self {
        self.max_response_tokens = Some(max_tokens);
        self
    }

    /// Turns on recording of the full model request body for runs of this
    /// agent (default off). This is the resolved effective setting; the CLI
    /// and server compute it from the per-agent `record_prompts` config and the
    /// `SALVOR_RECORD_PROMPTS` default before calling this. It is PII-sensitive
    /// (the body may hold user data or secrets) and is deliberately kept out of
    /// the definition hash. See [`Agent::record_prompts`].
    #[must_use]
    pub fn record_prompts(mut self, record_prompts: bool) -> Self {
        self.record_prompts = record_prompts;
        self
    }

    /// Sets correlation tags to stamp on every fresh run of this agent (a
    /// build id, an environment name). Unset by default. Like
    /// [`record_prompts`](Self::record_prompts), this is operator/deployment
    /// metadata rather than part of what the agent runs, so it is excluded
    /// from [`Agent::def_hash`] (see that method's docs). Sanity bounds on
    /// the labels themselves are not checked here; they are enforced where a
    /// run is actually created (see [`crate::validate_labels`]), so this
    /// setter is infallible.
    #[must_use]
    pub fn labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = Some(labels);
        self
    }

    /// Sets a short human label for this agent (unset by default). Like
    /// [`record_prompts`](Self::record_prompts) and [`labels`](Self::labels),
    /// this is descriptive metadata rather than part of what the agent runs,
    /// so it is excluded from [`Agent::def_hash`] (see that method's docs).
    /// Sanity bounds on the name itself are not checked here; a config-file
    /// caller enforces them where the config is parsed (see
    /// `salvor_cli::agent_config::MAX_NAME_LEN`), so this setter is
    /// infallible, mirroring [`labels`](Self::labels).
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Builds the agent, computing its definition hash.
    ///
    /// # Errors
    ///
    /// [`AgentBuildError::MissingModel`] when neither
    /// [`model`](Self::model) nor [`client`](Self::client) was called;
    /// [`AgentBuildError::DuplicateTool`] when two tools share a name;
    /// [`AgentBuildError::CostBudgetWithoutPricing`] when `max_cost_usd` is
    /// declared with no [`Pricing`]; [`AgentBuildError::Client`] when the
    /// client cannot be constructed from the given configuration.
    pub fn build(self) -> Result<Agent, AgentBuildError> {
        let model = self.model.ok_or(AgentBuildError::MissingModel)?;
        let client = match (self.client, self.config) {
            (Some(client), _) => client,
            (None, Some(config)) => Client::new(config).map_err(AgentBuildError::Client)?,
            (None, None) => return Err(AgentBuildError::MissingModel),
        };
        if self.budgets.max_cost_usd.is_some() && self.pricing.is_none() {
            return Err(AgentBuildError::CostBudgetWithoutPricing);
        }

        let mut tools = ToolSet::new();
        for tool in self.tools {
            let name = tool.name().to_owned();
            tools
                .register_dyn(tool)
                .map_err(|_| AgentBuildError::DuplicateTool { name })?;
        }

        let def_hash = compute_def_hash(
            &model,
            self.system_prompt.as_deref(),
            &tools,
            &self.budgets,
            self.pricing.as_ref(),
        );

        Ok(Agent {
            client,
            model,
            system_prompt: self.system_prompt,
            tools,
            budgets: self.budgets,
            pricing: self.pricing,
            max_response_tokens: self
                .max_response_tokens
                .unwrap_or(DEFAULT_MAX_RESPONSE_TOKENS),
            def_hash,
            record_prompts: self.record_prompts,
            labels: self.labels,
            name: self.name,
        })
    }
}

/// Why an agent could not be built.
#[derive(Debug, Error)]
pub enum AgentBuildError {
    /// No model was configured.
    #[error("an agent needs a model: call .model(config, id) or .client(client, id)")]
    MissingModel,

    /// A cost budget was declared with no pricing to compute cost from.
    #[error("max_cost_usd is declared but no pricing is set; call .pricing(Pricing {{ .. }})")]
    CostBudgetWithoutPricing,

    /// Two registered tools share a name.
    #[error("a tool named `{name}` is registered twice")]
    DuplicateTool {
        /// The name that collided.
        name: String,
    },

    /// The client could not be constructed from the given configuration.
    #[error("client construction failed: {0}")]
    Client(salvor_llm::Error),
}

/// Builds the canonical definition value documented at module level and
/// hashes it.
fn compute_def_hash(
    model: &str,
    system_prompt: Option<&str>,
    tools: &ToolSet,
    budgets: &Budgets,
    pricing: Option<&Pricing>,
) -> String {
    let tool_values: Vec<Value> = tools
        .descriptors()
        .into_iter()
        .map(|descriptor| {
            json!({
                "description": descriptor.description,
                "effect": descriptor.effect,
                "input_schema": descriptor.input_schema,
                "name": descriptor.name,
            })
        })
        .collect();
    let value = json!({
        "budgets": {
            "max_cost_usd": budgets.max_cost_usd,
            "max_steps": budgets.max_steps,
            "max_tokens": budgets.max_tokens,
            "max_wall_time_seconds": budgets.max_wall_time.map(|d| d.as_secs_f64()),
        },
        "model": model,
        "pricing": pricing.map(|p| {
            json!({"input_per_mtok": p.input_per_mtok, "output_per_mtok": p.output_per_mtok})
        }),
        "system_prompt": system_prompt,
        "tools": tool_values,
    });
    hash_value(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use salvor_core::Effect;
    use salvor_tools::{ToolCtx, ToolError, ToolOutcome};
    use serde_json::json;

    /// A minimal `DynTool` for definition-hash tests.
    struct StubTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl DynTool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn effect(&self) -> Effect {
            Effect::Read
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call_json(
            &self,
            _ctx: &ToolCtx,
            input: Value,
        ) -> Result<ToolOutcome<Value>, ToolError> {
            Ok(ToolOutcome::Output(input))
        }
    }

    fn base_builder() -> AgentBuilder {
        Agent::builder()
            .model(Config::new(), "test-model")
            .system_prompt("prompt")
            .tool_dyn(Box::new(StubTool {
                name: "alpha",
                description: "first",
            }))
    }

    /// The same definition hashes to the same value, whatever the tool
    /// registration order.
    #[test]
    fn identical_definitions_share_a_hash() {
        let a = Agent::builder()
            .model(Config::new(), "test-model")
            .tool_dyn(Box::new(StubTool {
                name: "alpha",
                description: "first",
            }))
            .tool_dyn(Box::new(StubTool {
                name: "beta",
                description: "second",
            }))
            .build()
            .unwrap();
        let b = Agent::builder()
            .model(Config::new(), "test-model")
            .tool_dyn(Box::new(StubTool {
                name: "beta",
                description: "second",
            }))
            .tool_dyn(Box::new(StubTool {
                name: "alpha",
                description: "first",
            }))
            .build()
            .unwrap();
        assert_eq!(a.def_hash(), b.def_hash());
        assert!(a.def_hash().starts_with("sha256:"));
    }

    /// Changing any hashed component changes the hash.
    #[test]
    fn any_definition_change_changes_the_hash() {
        let base = base_builder().build().unwrap();

        let model_changed = base_builder();
        let model_changed = AgentBuilder {
            model: Some("other-model".to_owned()),
            ..model_changed
        }
        .build()
        .unwrap();
        assert_ne!(base.def_hash(), model_changed.def_hash());

        let prompt_changed = base_builder().system_prompt("different").build().unwrap();
        assert_ne!(base.def_hash(), prompt_changed.def_hash());

        let tool_changed = Agent::builder()
            .model(Config::new(), "test-model")
            .system_prompt("prompt")
            .tool_dyn(Box::new(StubTool {
                name: "alpha",
                description: "changed description",
            }))
            .build()
            .unwrap();
        assert_ne!(base.def_hash(), tool_changed.def_hash());

        let budget_changed = base_builder()
            .budgets(Budgets {
                max_steps: Some(10),
                ..Budgets::default()
            })
            .build()
            .unwrap();
        assert_ne!(base.def_hash(), budget_changed.def_hash());

        let pricing_changed = base_builder()
            .pricing(Pricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
            })
            .build()
            .unwrap();
        assert_ne!(base.def_hash(), pricing_changed.def_hash());
    }

    /// Labels are operator/deployment metadata, not part of the definition:
    /// setting them, changing them, or leaving them unset never changes
    /// `def_hash`, mirroring how `record_prompts` is excluded. This is the
    /// def-hash half of the "hashing is unaffected by labels" guarantee; the
    /// request-hash half is proven in `salvor-runtime`'s `happy_path.rs`
    /// integration test, alongside `record_prompts`'s identical proof.
    #[test]
    fn labels_never_affect_the_definition_hash() {
        let unlabeled = base_builder().build().unwrap();
        let labeled_a = base_builder()
            .labels(BTreeMap::from([("build".to_owned(), "42".to_owned())]))
            .build()
            .unwrap();
        let labeled_b = base_builder()
            .labels(BTreeMap::from([
                ("build".to_owned(), "43".to_owned()),
                ("env".to_owned(), "staging".to_owned()),
            ]))
            .build()
            .unwrap();

        assert_eq!(unlabeled.def_hash(), labeled_a.def_hash());
        assert_eq!(unlabeled.def_hash(), labeled_b.def_hash());
        assert_eq!(unlabeled.labels(), None);
        assert_eq!(
            labeled_a.labels(),
            Some(&BTreeMap::from([("build".to_owned(), "42".to_owned())]))
        );
    }

    /// `name` is descriptive metadata, not part of the definition: setting
    /// it, changing it, or leaving it unset never changes `def_hash`,
    /// mirroring how `record_prompts` and `labels` are excluded. This is the
    /// def-hash half of the "a rename must not mint a new agent identity"
    /// guarantee the CLI's TOML `name` field relies on (see
    /// `salvor_cli::agent_config`'s own same-TOML-plus-or-minus-`name` test).
    #[test]
    fn name_never_affects_the_definition_hash() {
        let unnamed = base_builder().build().unwrap();
        let named_a = base_builder().name("triage-agent").build().unwrap();
        let named_b = base_builder().name("a-different-name").build().unwrap();

        assert_eq!(unnamed.def_hash(), named_a.def_hash());
        assert_eq!(unnamed.def_hash(), named_b.def_hash());
        assert_eq!(unnamed.name(), None);
        assert_eq!(named_a.name(), Some("triage-agent"));
    }

    /// A cost budget without pricing is a build-time error, not a run-time
    /// surprise.
    #[test]
    fn cost_budget_without_pricing_fails_to_build() {
        let result = base_builder()
            .budgets(Budgets {
                max_cost_usd: Some(2.0),
                ..Budgets::default()
            })
            .build();
        assert!(matches!(
            result,
            Err(AgentBuildError::CostBudgetWithoutPricing)
        ));
    }

    /// Duplicate tool names and a missing model fail at build time.
    #[test]
    fn duplicate_tools_and_missing_model_fail_to_build() {
        let duplicate = base_builder()
            .tool_dyn(Box::new(StubTool {
                name: "alpha",
                description: "again",
            }))
            .build();
        assert!(matches!(
            duplicate,
            Err(AgentBuildError::DuplicateTool { name }) if name == "alpha"
        ));

        assert!(matches!(
            Agent::builder().build(),
            Err(AgentBuildError::MissingModel)
        ));
    }
}

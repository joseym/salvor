//! Unit tests for the agent-definition TOML schema, driven through the
//! library API rather than the binary. They pin the round-trip, the clear
//! failure when a cost budget lacks pricing, and that an unknown field is
//! rejected (not silently ignored). One case loads the committed
//! `examples/web-research/agent.toml` through the real loader, so that
//! documented example cannot drift out of a shape the parser accepts.

use std::io::Write;
use std::path::PathBuf;

use salvor_cli::agent_config::{AgentConfig, build_agent};
use salvor_core::Effect;
use tempfile::NamedTempFile;

/// Writes `toml` to a temp file and loads it, returning both so the caller can
/// pass the path to `build_agent` (which resolves relative paths against it).
fn load_from_str(toml: &str) -> (AgentConfig, NamedTempFile) {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write toml");
    let config = AgentConfig::load(file.path()).expect("config parses");
    (config, file)
}

/// A full config round-trips into the expected typed values.
#[test]
fn full_config_parses() {
    let toml = r#"
model = "claude-opus-4-8"
system_prompt = "You are a research agent."
max_response_tokens = 2048

[llm]
base_url = "http://localhost:1234"
api_key_env = "MY_KEY"
max_retries = 3
timeout_seconds = 30

[budgets]
steps = 40
tokens = 100000
cost_usd = 2.0
wall_time_seconds = 600

[pricing]
input_per_mtok = 3.0
output_per_mtok = 15.0

[[mcp_servers]]
command = "python"
args = ["-m", "server"]
env = { TOKEN = "abc" }
effect_overrides = { delete = "write", fetch = "read" }
"#;
    let (config, _file) = load_from_str(toml);
    assert_eq!(config.model, "claude-opus-4-8");
    assert_eq!(
        config.system_prompt.as_deref(),
        Some("You are a research agent.")
    );
    assert_eq!(config.max_response_tokens, Some(2048));
    assert_eq!(
        config.llm.base_url.as_deref(),
        Some("http://localhost:1234")
    );
    assert_eq!(config.llm.api_key_env.as_deref(), Some("MY_KEY"));
    assert_eq!(config.llm.max_retries, Some(3));
    assert_eq!(config.budgets.steps, Some(40));
    assert_eq!(config.budgets.cost_usd, Some(2.0));
    let pricing = config.pricing.as_ref().expect("pricing present");
    assert_eq!(pricing.input_per_mtok, 3.0);
    assert_eq!(config.mcp_servers.len(), 1);
    let server = &config.mcp_servers[0];
    assert_eq!(server.command, "python");
    assert_eq!(server.args, vec!["-m", "server"]);
    assert_eq!(server.env.get("TOKEN").map(String::as_str), Some("abc"));
    assert_eq!(server.effect_overrides.len(), 2);
}

/// The committed `examples/web-research/agent.toml` loads through the real
/// config loader and has the shape its README documents: both official MCP
/// servers, the two grounded effect overrides, and budgets with the pricing a
/// cost budget requires. This is the only CI-facing piece of that example (the
/// live run needs a real API key and network), so it guards the parse contract
/// the walkthrough depends on, not the run itself.
#[test]
fn web_research_example_parses() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/web-research/agent.toml");
    let config = AgentConfig::load(&path).expect("example agent.toml parses");

    assert_eq!(config.model, "claude-opus-4-8");
    assert_eq!(config.llm.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));

    // Budgets and the pricing that a cost budget requires are all present.
    assert_eq!(config.budgets.steps, Some(30));
    assert_eq!(config.budgets.tokens, Some(500_000));
    assert_eq!(config.budgets.cost_usd, Some(3.00));
    assert_eq!(config.budgets.wall_time_seconds, Some(600.0));
    let pricing = config.pricing.as_ref().expect("pricing present");
    assert_eq!(pricing.input_per_mtok, 5.0);
    assert_eq!(pricing.output_per_mtok, 25.0);

    // Both servers, keyed by command, with the effect overrides the README
    // explains: fetch pinned to Read, the report write pinned to Write.
    assert_eq!(config.mcp_servers.len(), 2);
    let fetch = config
        .mcp_servers
        .iter()
        .find(|s| s.command == "uvx")
        .expect("fetch server present");
    assert_eq!(fetch.effect_overrides.get("fetch"), Some(&Effect::Read));
    let filesystem = config
        .mcp_servers
        .iter()
        .find(|s| s.command == "npx")
        .expect("filesystem server present");
    assert_eq!(
        filesystem.effect_overrides.get("write_file"),
        Some(&Effect::Write)
    );
}

/// A terse config (only the required `model`) parses, with everything else at
/// its default.
#[test]
fn minimal_config_parses() {
    let (config, _file) = load_from_str("model = \"test-model\"\n");
    assert_eq!(config.model, "test-model");
    assert!(config.system_prompt.is_none());
    assert!(config.mcp_servers.is_empty());
    assert!(config.budgets.steps.is_none());
}

/// A cost budget with no pricing is a clear, actionable build error, and it
/// names the fix.
#[tokio::test]
async fn cost_budget_without_pricing_is_a_clear_error() {
    let toml = "model = \"test-model\"\n\n[budgets]\ncost_usd = 2.0\n";
    let (config, file) = load_from_str(toml);
    let error = match build_agent(&config, file.path()).await {
        Ok(_) => panic!("cost budget without pricing should fail to build"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("pricing"),
        "error should name pricing: {message}"
    );
    assert!(
        message.contains("cost_usd") || message.contains("cost budget"),
        "error should name the cost budget: {message}"
    );
}

/// `base_url_env` resolution: the named variable overrides `base_url` when
/// set and non-empty, and falls back to `base_url` otherwise. One test body
/// covers all cases so the process-global environment mutation cannot race
/// a parallel test over the same variable.
#[test]
fn base_url_env_overrides_when_set_and_falls_back_when_not() {
    let toml = "model = \"m\"\n\n[llm]\nbase_url = \"http://from-file:1\"\nbase_url_env = \"SALVOR_TEST_BASE_URL_OVERRIDE\"\n";
    let (config, _file) = load_from_str(toml);

    // Unset: the file's base_url wins.
    // SAFETY: this test is the only reader and writer of this uniquely
    // named variable, and all uses are within this single test body.
    unsafe { std::env::remove_var("SALVOR_TEST_BASE_URL_OVERRIDE") };
    assert_eq!(config.client_config().base_url, "http://from-file:1");

    // Empty: treated as unset.
    unsafe { std::env::set_var("SALVOR_TEST_BASE_URL_OVERRIDE", "") };
    assert_eq!(config.client_config().base_url, "http://from-file:1");

    // Set and non-empty: the variable wins.
    unsafe { std::env::set_var("SALVOR_TEST_BASE_URL_OVERRIDE", "http://from-env:2") };
    assert_eq!(config.client_config().base_url, "http://from-env:2");

    unsafe { std::env::remove_var("SALVOR_TEST_BASE_URL_OVERRIDE") };
}

/// An unknown field is rejected, so a typo cannot silently drop a setting.
#[test]
fn unknown_field_is_rejected() {
    let mut file = NamedTempFile::new().expect("temp file");
    // `step` instead of `steps`: a plausible typo.
    file.write_all(b"model = \"m\"\n\n[budgets]\nstep = 5\n")
        .expect("write");
    let error = AgentConfig::load(file.path()).expect_err("unknown field rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("step") || message.contains("unknown"),
        "error should point at the unknown field: {message}"
    );
}

/// Setting both prompt sources is rejected as ambiguous.
#[test]
fn both_prompt_sources_is_rejected() {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(b"model = \"m\"\nsystem_prompt = \"a\"\nsystem_prompt_path = \"p.txt\"\n")
        .expect("write");
    let error = AgentConfig::load(file.path()).expect_err("ambiguous prompt rejected");
    assert!(format!("{error:#}").contains("system_prompt"));
}

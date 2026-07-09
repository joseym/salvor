//! Unit tests for the agent-definition TOML schema, driven through the
//! library API rather than the binary. They pin the round-trip, the clear
//! failure when a cost budget lacks pricing, and that an unknown field is
//! rejected (not silently ignored).

use std::io::Write;

use salvor_cli::agent_config::{AgentConfig, build_agent};
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

//! Unit tests for the agent-definition TOML schema, driven through the
//! library API rather than the binary. They pin the round-trip, the clear
//! failure when a cost budget lacks pricing, and that an unknown field is
//! rejected (not silently ignored). Three cases load committed example
//! `agent.toml` files through the real loader, so those documented examples
//! cannot drift out of a shape the parser accepts: `web-research` (two official
//! MCP servers) and the two polyglot examples, `python-tools` and
//! `typescript-tools`, each an agent whose whole tool layer is a Python or
//! TypeScript MCP server. Every one of the three pins the effect override its
//! README explains, so these tests guard that the documented trust decision
//! survives a schema change; the live runs (real key, real network) stay out of
//! CI.

use std::io::Write;
use std::path::PathBuf;

use salvor_cli::agent_config::{AgentConfig, ApiKeyKind, PreopenPermsConfig, build_agent};
use salvor_core::Effect;
use salvor_llm::AuthKind;
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
    assert_eq!(server.command.as_deref(), Some("python"));
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
    // The example names a dedicated demo variable so a walkthrough run cannot
    // accidentally spend the primary ANTHROPIC_API_KEY.
    assert_eq!(
        config.llm.api_key_env.as_deref(),
        Some("DEMO_ANTHROPIC_API_KEY")
    );

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
        .find(|s| s.command.as_deref() == Some("uvx"))
        .expect("fetch server present");
    assert_eq!(fetch.effect_overrides.get("fetch"), Some(&Effect::Read));
    let filesystem = config
        .mcp_servers
        .iter()
        .find(|s| s.command.as_deref() == Some("npx"))
        .expect("filesystem server present");
    assert_eq!(
        filesystem.effect_overrides.get("write_file"),
        Some(&Effect::Write)
    );
}

/// The committed `examples/python-tools/agent.toml` loads through the real
/// config loader and has the shape its README documents: one MCP server (the
/// venv Python running `server.py`), budgets with pricing, and the single
/// grounded effect override pinning `add_expense` to Write. The server
/// advertises `add_expense` with `idempotentHint: true`, which the default
/// mapping would read as Idempotent; the pin corrects that, because the tool
/// appends a line and a retry would duplicate it.
#[test]
fn python_tools_example_parses() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/python-tools/agent.toml");
    let config = AgentConfig::load(&path).expect("example agent.toml parses");

    assert_eq!(config.model, "claude-opus-4-8");
    assert_eq!(
        config.llm.api_key_env.as_deref(),
        Some("DEMO_ANTHROPIC_API_KEY")
    );

    // A cost budget with the pricing it requires: the validation constraint the
    // walkthrough runs stay under.
    assert_eq!(config.budgets.cost_usd, Some(0.50));
    let pricing = config.pricing.as_ref().expect("pricing present");
    assert_eq!(pricing.input_per_mtok, 5.0);
    assert_eq!(pricing.output_per_mtok, 25.0);

    // One server: the venv Python interpreter running server.py, with the
    // ledger path handed to it via the environment.
    assert_eq!(config.mcp_servers.len(), 1);
    let server = &config.mcp_servers[0];
    assert_eq!(
        server.command.as_deref(),
        Some("examples/python-tools/.venv/bin/python")
    );
    assert_eq!(server.args, vec!["examples/python-tools/server.py"]);
    assert_eq!(
        server.env.get("EXPENSE_LEDGER").map(String::as_str),
        Some("examples/python-tools/ledger.jsonl")
    );
    // The append tool pinned to Write over its optimistic idempotent hint.
    assert_eq!(
        server.effect_overrides.get("add_expense"),
        Some(&Effect::Write)
    );
}

/// The committed `examples/typescript-tools/agent.toml` loads through the real
/// config loader and has the shape its README documents: one MCP server (Node
/// running `server.mjs`), budgets with pricing, and the single grounded effect
/// override pinning `save_bookmark` to Write, for the same append-duplicates
/// reason as the Python example.
#[test]
fn typescript_tools_example_parses() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/typescript-tools/agent.toml");
    let config = AgentConfig::load(&path).expect("example agent.toml parses");

    assert_eq!(config.model, "claude-opus-4-8");
    assert_eq!(
        config.llm.api_key_env.as_deref(),
        Some("DEMO_ANTHROPIC_API_KEY")
    );

    assert_eq!(config.budgets.cost_usd, Some(0.50));
    let pricing = config.pricing.as_ref().expect("pricing present");
    assert_eq!(pricing.input_per_mtok, 5.0);
    assert_eq!(pricing.output_per_mtok, 25.0);

    // One server: Node running server.mjs, with the store path via environment.
    assert_eq!(config.mcp_servers.len(), 1);
    let server = &config.mcp_servers[0];
    assert_eq!(server.command.as_deref(), Some("node"));
    assert_eq!(server.args, vec!["examples/typescript-tools/server.mjs"]);
    assert_eq!(
        server.env.get("BOOKMARKS_FILE").map(String::as_str),
        Some("examples/typescript-tools/bookmarks.jsonl")
    );
    assert_eq!(
        server.effect_overrides.get("save_bookmark"),
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

/// `api_key_kind` parses both accepted spellings, defaults to the api-key
/// scheme when absent, and resolves into the matching client `AuthKind`.
#[test]
fn api_key_kind_parses_both_values_and_default() {
    // Absent: defaults to the api-key scheme.
    let (config, _f) = load_from_str("model = \"m\"\n");
    assert_eq!(config.llm.api_key_kind, ApiKeyKind::ApiKey);
    assert_eq!(config.client_config().auth_kind, AuthKind::ApiKey);

    // Explicit "api_key".
    let (config, _f) = load_from_str("model = \"m\"\n\n[llm]\napi_key_kind = \"api_key\"\n");
    assert_eq!(config.llm.api_key_kind, ApiKeyKind::ApiKey);
    assert_eq!(config.client_config().auth_kind, AuthKind::ApiKey);

    // "oauth" maps to the bearer scheme.
    let (config, _f) = load_from_str("model = \"m\"\n\n[llm]\napi_key_kind = \"oauth\"\n");
    assert_eq!(config.llm.api_key_kind, ApiKeyKind::Oauth);
    assert_eq!(config.client_config().auth_kind, AuthKind::Bearer);
}

/// An unrecognized `api_key_kind` is a loud parse error, never a silent
/// fallback to the default scheme.
#[test]
fn unknown_api_key_kind_is_rejected() {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(b"model = \"m\"\n\n[llm]\napi_key_kind = \"bearer\"\n")
        .expect("write");
    let error = AgentConfig::load(file.path()).expect_err("unknown api_key_kind rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("api_key_kind") || message.contains("bearer"),
        "error should point at the bad value: {message}"
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

/// A `url` MCP server parses into the URL and bearer-token-env fields, with the
/// stdio-only fields at their empty defaults.
#[test]
fn url_mcp_server_parses() {
    let toml = "model = \"m\"\n\n[[mcp_servers]]\nurl = \"https://mcp.example.com/mcp\"\nbearer_token_env = \"MY_MCP_TOKEN\"\neffect_overrides = { delete = \"write\" }\n";
    let (config, _f) = load_from_str(toml);
    assert_eq!(config.mcp_servers.len(), 1);
    let server = &config.mcp_servers[0];
    assert_eq!(server.command, None);
    assert_eq!(server.url.as_deref(), Some("https://mcp.example.com/mcp"));
    assert_eq!(server.bearer_token_env.as_deref(), Some("MY_MCP_TOKEN"));
    assert_eq!(server.effect_overrides.get("delete"), Some(&Effect::Write));
    assert!(server.args.is_empty());
    assert!(server.env.is_empty());
}

/// An entry that sets neither `command` nor `url` is a loud error: there is no
/// transport to reach the server over.
#[test]
fn mcp_server_with_neither_command_nor_url_is_rejected() {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(b"model = \"m\"\n\n[[mcp_servers]]\neffect_overrides = { x = \"read\" }\n")
        .expect("write");
    let error = AgentConfig::load(file.path()).expect_err("no transport rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("command") && message.contains("url"),
        "error should name both transport keys: {message}"
    );
}

/// An entry that sets both `command` and `url` is a loud error: the transport
/// would be ambiguous.
#[test]
fn mcp_server_with_both_command_and_url_is_rejected() {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(
        b"model = \"m\"\n\n[[mcp_servers]]\ncommand = \"python\"\nurl = \"https://x/mcp\"\n",
    )
    .expect("write");
    let error = AgentConfig::load(file.path()).expect_err("both transports rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("both") || (message.contains("command") && message.contains("url")),
        "error should flag the command/url conflict: {message}"
    );
}

/// `args` alongside a `url` server is rejected: arguments belong to a spawned
/// command, not an HTTP endpoint.
#[test]
fn args_with_a_url_server_is_rejected() {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(b"model = \"m\"\n\n[[mcp_servers]]\nurl = \"https://x/mcp\"\nargs = [\"-x\"]\n")
        .expect("write");
    let error = AgentConfig::load(file.path()).expect_err("args with url rejected");
    assert!(format!("{error:#}").contains("args"));
}

/// `bearer_token_env` alongside a `command` server is rejected: stdio has no
/// authorization header to carry a token.
#[test]
fn bearer_token_env_with_a_command_server_is_rejected() {
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(
        b"model = \"m\"\n\n[[mcp_servers]]\ncommand = \"python\"\nbearer_token_env = \"T\"\n",
    )
    .expect("write");
    let error = AgentConfig::load(file.path()).expect_err("bearer with command rejected");
    assert!(format!("{error:#}").contains("bearer_token_env"));
}

/// A full `[[wasm_tools]]` entry parses: identity, pin, required effect,
/// inline schema, limits, and a preopen grant.
#[test]
fn wasm_tool_config_parses() {
    let toml = r#"
model = "m"

[[wasm_tools]]
path = "tools/wordcount.wasm"
sha256 = "9f3a"
name = "wordcount"
description = "Counts words in text"
effect = "read"
input_schema = '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}'

[wasm_tools.limits]
wall_time_ms = 2000
memory_bytes = 33554432
fuel = 500000000

[wasm_tools.grants]
preopen = [{ host = "./data", guest = "/data", perms = "read" }]
"#;
    let (config, _file) = load_from_str(toml);
    assert_eq!(config.wasm_tools.len(), 1);
    let tool = &config.wasm_tools[0];
    assert_eq!(tool.path, "tools/wordcount.wasm");
    assert_eq!(tool.sha256.as_deref(), Some("9f3a"));
    assert_eq!(tool.name, "wordcount");
    assert_eq!(tool.description, "Counts words in text");
    assert_eq!(tool.effect, Some(Effect::Read));
    assert!(tool.input_schema.as_deref().unwrap().contains("\"text\""));
    assert_eq!(tool.limits.wall_time_ms, Some(2000));
    assert_eq!(tool.limits.memory_bytes, Some(33_554_432));
    assert_eq!(tool.limits.fuel, Some(500_000_000));
    assert_eq!(tool.grants.preopen.len(), 1);
    let preopen = &tool.grants.preopen[0];
    assert_eq!(preopen.host, "./data");
    assert_eq!(preopen.guest, "/data");
    assert_eq!(preopen.perms, PreopenPermsConfig::Read);
}

/// `effect` has no default for a sandboxed binary. Omitting it is refused
/// loudly, the error names the offending tool, and the message says why
/// there is nothing to fall back on.
#[test]
fn wasm_tool_missing_effect_is_rejected_naming_the_tool() {
    let toml = r#"
model = "m"

[[wasm_tools]]
path = "t.wasm"
name = "wordcount"
description = "d"
input_schema = '{"type":"object"}'
"#;
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write toml");
    let error = AgentConfig::load(file.path()).expect_err("missing effect rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("wasm tool `wordcount`"),
        "error should name the tool: {message}"
    );
    assert!(
        message.contains("`effect` is required"),
        "error should name the missing key: {message}"
    );
}

/// Exactly one schema source: neither is a loud error naming the tool.
#[test]
fn wasm_tool_with_no_schema_source_is_rejected() {
    let toml = "model = \"m\"\n\n[[wasm_tools]]\npath = \"t.wasm\"\nname = \"w\"\ndescription = \"d\"\neffect = \"read\"\n";
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write toml");
    let error = AgentConfig::load(file.path()).expect_err("no schema source rejected");
    let message = format!("{error:#}");
    assert!(message.contains("wasm tool `w`"), "{message}");
    assert!(message.contains("neither is set"), "{message}");
}

/// Exactly one schema source: both is a loud error naming the tool.
#[test]
fn wasm_tool_with_both_schema_sources_is_rejected() {
    let toml = "model = \"m\"\n\n[[wasm_tools]]\npath = \"t.wasm\"\nname = \"w\"\ndescription = \"d\"\neffect = \"read\"\ninput_schema = '{}'\ninput_schema_path = \"s.json\"\n";
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write toml");
    let error = AgentConfig::load(file.path()).expect_err("both schema sources rejected");
    let message = format!("{error:#}");
    assert!(message.contains("wasm tool `w`"), "{message}");
    assert!(message.contains("not both"), "{message}");
}

/// An unknown field inside a `[[wasm_tools]]` entry is rejected like any
/// other typo, not silently dropped.
#[test]
fn wasm_tool_unknown_field_is_rejected() {
    let toml = "model = \"m\"\n\n[[wasm_tools]]\npath = \"t.wasm\"\nname = \"w\"\ndescription = \"d\"\neffect = \"read\"\ninput_schema = '{}'\nnetwork = true\n";
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write toml");
    let error = AgentConfig::load(file.path()).expect_err("unknown field rejected");
    assert!(format!("{error:#}").contains("network"));
}

/// A preopen permission outside the two spellings is a loud parse error.
#[test]
fn wasm_tool_unknown_perms_value_is_rejected() {
    let toml = "model = \"m\"\n\n[[wasm_tools]]\npath = \"t.wasm\"\nname = \"w\"\ndescription = \"d\"\neffect = \"read\"\ninput_schema = '{}'\n\n[wasm_tools.grants]\npreopen = [{ host = \".\", guest = \"/d\", perms = \"write\" }]\n";
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write toml");
    let error = AgentConfig::load(file.path()).expect_err("unknown perms rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("read_write") || message.contains("unknown variant"),
        "{message}"
    );
}

/// The committed `examples/wasm-tools/agent.toml` loads through the real
/// config loader with the shape its README documents: one sandboxed tool
/// with a required effect, per-call limits, and no grants.
#[test]
fn wasm_tools_example_parses() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/wasm-tools/agent.toml");
    let config = AgentConfig::load(&path).expect("example agent.toml parses");

    assert_eq!(config.model, "claude-opus-4-8");
    assert_eq!(
        config.llm.api_key_env.as_deref(),
        Some("DEMO_ANTHROPIC_API_KEY")
    );
    let pricing = config.pricing.as_ref().expect("pricing present");
    assert_eq!(pricing.input_per_mtok, 5.0);

    assert_eq!(config.wasm_tools.len(), 1);
    let tool = &config.wasm_tools[0];
    assert_eq!(tool.name, "wordcount");
    // The operator's trust decision the README explains: pure computation,
    // no grants, so Read is honest and lets an interrupted call retry.
    assert_eq!(tool.effect, Some(Effect::Read));
    assert!(tool.grants.preopen.is_empty());
    assert_eq!(tool.limits.wall_time_ms, Some(2000));
}

/// Builds the salvor-wasm fixture guest and returns the component path, so
/// the build test below exercises a real component end to end.
fn wasm_fixture_component() -> PathBuf {
    let guest_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../salvor-wasm/tests/fixture-guest");
    // Same shared build location salvor-wasm's own tests use (the workspace
    // root's target/, not a nested target/ inside crates/); cargo's directory
    // locking makes the two suites building concurrently safe, and the second
    // build is a cache hit.
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm-guests");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--target", "wasm32-wasip2", "--release", "--quiet"])
        .arg("--target-dir")
        .arg(&target_dir)
        .current_dir(&guest_dir)
        .status()
        .expect("spawning cargo to build the fixture guest");
    assert!(
        status.success(),
        "building the fixture guest failed; if the target is missing, run \
         `rustup target add wasm32-wasip2`"
    );
    target_dir.join("wasm32-wasip2/release/fixture_guest.wasm")
}

/// `build_agent` registers a sandboxed wasm tool beside an MCP server's
/// tools in the same agent: the two config-reachable tool boundaries share
/// one registry, and the wasm tool actually executes through it.
#[tokio::test]
async fn wasm_tool_builds_beside_mcp_tools() {
    let component = wasm_fixture_component();
    let count_file = NamedTempFile::new().expect("count file");
    let toml = format!(
        r#"
model = "m"

[[mcp_servers]]
command = "{fixture}"
args = ["{count_file}"]
effect_overrides = {{ record = "write" }}

[[wasm_tools]]
path = "{component}"
name = "fixture_wasm"
description = "the salvor-wasm test guest"
effect = "read"
input_schema = '{{"type":"object"}}'
"#,
        fixture = env!("CARGO_BIN_EXE_salvor-mcp-count-fixture"),
        count_file = count_file.path().display(),
        component = component.display(),
    );
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(toml.as_bytes()).expect("write toml");
    let config = AgentConfig::load(file.path()).expect("config parses");

    let (agent, servers) = build_agent(&config, file.path())
        .await
        .expect("agent builds with both tool kinds");

    let names: Vec<&str> = agent.tools().tools().map(|tool| tool.name()).collect();
    assert!(names.contains(&"record"), "mcp tool registered: {names:?}");
    assert!(
        names.contains(&"fixture_wasm"),
        "wasm tool registered: {names:?}"
    );

    // The wasm tool is not just listed; it dispatches through the shared
    // registry seam.
    let tool = agent.tools().get("fixture_wasm").expect("wasm tool");
    let outcome = tool
        .call_json(
            &salvor_tools::ToolCtx::default(),
            serde_json::json!({ "mode": "wordcount", "text": "a b c" }),
        )
        .await
        .expect("wasm call succeeds");
    match outcome {
        salvor_tools::ToolOutcome::Output(value) => {
            assert_eq!(value, serde_json::json!({ "words": 3, "chars": 5 }));
        }
        salvor_tools::ToolOutcome::Suspend(_) => panic!("wasm tools cannot suspend"),
    }

    for server in servers {
        server.close().await.expect("server closes");
    }
}

/// The committed example does not just parse: its guest builds and its
/// `wordcount` tool executes through the real `build_agent` path, called with
/// the JSON shape the example's schema documents. This is the example's
/// compile-and-run gate; the live walkthrough (real key, real model) stays
/// out of CI.
#[tokio::test]
async fn wasm_tools_example_guest_runs() {
    // Build the example guest exactly as its README instructs, into the
    // workspace target/ the committed agent.toml points at.
    let guest_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/wasm-tools/guest");
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm-guests");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--target", "wasm32-wasip2", "--release", "--quiet"])
        .arg("--target-dir")
        .arg(&target_dir)
        .current_dir(&guest_dir)
        .status()
        .expect("spawning cargo to build the example guest");
    assert!(
        status.success(),
        "building the example guest failed; if the target is missing, run \
         `rustup target add wasm32-wasip2`"
    );

    let agent_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/wasm-tools/agent.toml");
    let config = AgentConfig::load(&agent_path).expect("example agent.toml parses");
    let (agent, servers) = build_agent(&config, &agent_path)
        .await
        .expect("example agent builds");
    assert!(servers.is_empty(), "the example declares no MCP servers");

    let tool = agent
        .tools()
        .get("wordcount")
        .expect("wordcount registered");
    let outcome = tool
        .call_json(
            &salvor_tools::ToolCtx::default(),
            serde_json::json!({ "text": "counting words is honest work" }),
        )
        .await
        .expect("wordcount call succeeds");
    match outcome {
        salvor_tools::ToolOutcome::Output(value) => {
            assert_eq!(value["words"], 5);
            assert_eq!(value["lines"], 1);
            assert_eq!(value["longest_word"], "counting");
        }
        salvor_tools::ToolOutcome::Suspend(_) => panic!("wasm tools cannot suspend"),
    }
}

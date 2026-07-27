//! The salvage-claim MCP server behind the `examples/hero` fixture.
//!
//! A smaller sibling of `demo_research.rs`: the same shape (a real rmcp
//! server over stdio, behind the crate's default-on `fixture` feature, never
//! part of the `salvor` product binary), cut down to the one tool the hero
//! demo on salvor.run needs. It is spawned by `salvor run --fixture
//! examples/hero`, so the demo exercises the true spawn/initialize/list/call
//! path, including the fresh respawn a resume performs after a `kill -9`.
//!
//! # The tool
//!
//! - `save_claim` (no read-only and no idempotent hint, so a client following
//!   the conservative annotation mapping lands on `Effect::Write`): appends
//!   one line to the claims file, naming the item claimed.
//!
//! There is deliberately nothing else. The hero story is "one write, recorded
//! once, never replayed blind", and every extra tool is another thing to
//! explain before that lands.
//!
//! # The claims file
//!
//! The side-effect ledger must live outside the process because a resume
//! respawns this server; an in-memory counter would reset and prove nothing.
//! The path is the `SALVOR_HERO_CLAIMS` environment variable when it is set
//! and non-empty, else `claims.txt` under the working directory. That
//! environment hook is what lets the fixture test point the unmodified
//! `examples/hero/agent.toml` at a temporary file: the variable travels from
//! the test through `salvor` to this child process by ordinary environment
//! inheritance, so the repository stays clean.
//!
//! One line per real execution is the zero-duplicate-write proof: `wc -l` on
//! the claims file before a `kill -9` and after the `resume` must be
//! identical, because the recorded completion replays and the write never
//! fires twice.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// The argument `save_claim` takes.
#[derive(Debug, Deserialize, JsonSchema)]
struct SaveClaimArgs {
    /// The wreck or item the claim is recorded against.
    item: String,
}

/// The hero tools server: the generated tool router plus the claims file path
/// resolved at startup.
#[derive(Clone)]
struct HeroTools {
    tool_router: ToolRouter<Self>,
    claims_file: Arc<PathBuf>,
}

#[tool_router]
impl HeroTools {
    fn new(claims_file: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            claims_file: Arc::new(claims_file),
        }
    }

    /// The write: appends one line to the claims file. Deliberately carries
    /// no read-only and no idempotent hint, so a client following the
    /// conservative annotation mapping treats it as a Write and records its
    /// intent before executing it. (`examples/hero/agent.toml` also pins it
    /// with an `effect_overrides` entry, so the operator's decision stands
    /// independently of what this server claims about itself.)
    #[tool(description = "Record one salvage claim by appending it durably to the claims file.")]
    async fn save_claim(
        &self,
        Parameters(SaveClaimArgs { item }): Parameters<SaveClaimArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.claims_file.as_ref())
            .and_then(|mut file| writeln!(file, "{item}"));
        match appended {
            Ok(()) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "claim recorded: {item}"
            ))])),
            // A failed write comes back as a tool error, not a transport
            // error: the model gets to see what went wrong, and the runtime
            // still records a completion for the intent it wrote first.
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to record claim: {error}"
            ))])),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HeroTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Salvor hero tools: one durable salvage-claim ledger.")
    }
}

/// Resolves the claims file path: the `SALVOR_HERO_CLAIMS` environment
/// variable when set and non-empty, else `claims.txt` in the working
/// directory. An empty value is treated as unset, so `SALVOR_HERO_CLAIMS=`
/// in a shell profile cannot silently redirect the ledger to a path with no
/// name.
fn claims_path() -> PathBuf {
    if let Ok(from_env) = std::env::var("SALVOR_HERO_CLAIMS")
        && !from_env.is_empty()
    {
        return PathBuf::from(from_env);
    }
    PathBuf::from("claims.txt")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Serve over stdio: the salvor process owns this child's stdin/stdout as
    // the JSON-RPC stream and closes the session when the run finishes.
    let service = HeroTools::new(claims_path()).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

//! A hermetic payout MCP server for the CLI integration tests: the in-repo
//! rebuild of the rig a field tester wrote by hand.
//!
//! This is test-support code, gated behind the crate's `fixture` feature (on by
//! default), not part of the `salvor` product binary. It is a real MCP server
//! spoken to over stdio by the product's own client path, so the tests that use
//! it exercise the true spawn/initialize/list/call sequence.
//!
//! # What it is for
//!
//! It exposes one tool, `pay_claim`, that "wires" a payout and appends one line
//! to a ledger file. **It has no deduplication of its own**, on purpose: no
//! idempotency check, no ledger read-back, nothing. That is the shape of a
//! first-draft payments integration, and it is what makes the ledger an honest
//! witness. Every line in it is one execution that really happened, so a test
//! that counts lines is measuring salvor's promise rather than the tool's own
//! carefulness.
//!
//! The tool's calls are identified by `claim_id`, but nothing here says so:
//! the MCP wire has no field for it, and a server's account of itself would not
//! be the operator's decision anyway. The agent file says it, with
//! `idempotency_keys = { pay_claim = "claim_id" }`, and the client derives the
//! key from the call's input.
//!
//! # The crash window
//!
//! `SALVOR_PAYOUT_SLOW_MS` (default 0) delays the call *before* the ledger is
//! written, and `SALVOR_PAYOUT_SLOW_AFTER_MS` (default 0) delays it after. A
//! test that wants to kill a run while the money is committed but the outcome
//! is not yet recorded sets the second one and kills during it: the ledger has
//! the line, salvor's log has only the intent, and the identity is held by a
//! run that will never finish on its own. That is the state a human resolves.
//!
//! # Reachability from the tests
//!
//! Building under the default `fixture` feature makes Cargo set
//! `CARGO_BIN_EXE_salvor-mcp-payout-fixture` for the crate's integration tests,
//! so they name this binary with no path guessing, exactly as they do the
//! counting fixture.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// The arguments `pay_claim` takes: the claim being paid, the amount, and the
/// currency. The claim id is what identifies the payout, but that is the
/// operator's declaration to make, not this server's.
#[derive(Debug, Deserialize, JsonSchema)]
struct PayClaimArgs {
    /// The claim being paid out.
    claim_id: String,
    /// Payout amount, in cents.
    amount_cents: i64,
    /// ISO 4217 currency code.
    currency: String,
}

/// The fixture server: the generated tool router, the ledger path, and the two
/// delays that let a test choose where a kill lands.
#[derive(Clone)]
struct Fixture {
    tool_router: ToolRouter<Self>,
    ledger: Arc<PathBuf>,
    before: Duration,
    after: Duration,
}

#[tool_router]
impl Fixture {
    fn new(ledger: PathBuf, before: Duration, after: Duration) -> Self {
        Self {
            tool_router: Self::tool_router(),
            ledger: Arc::new(ledger),
            before,
            after,
        }
    }

    /// Pays one claim by appending a line to the ledger and minting a fresh
    /// charge id. A second execution mints a second charge id, so a duplicate
    /// is visible as a duplicate rather than hiding behind an equal-looking
    /// line.
    #[tool(description = "Wire a payout to the claimant on file for a claim.")]
    async fn pay_claim(
        &self,
        Parameters(PayClaimArgs {
            claim_id,
            amount_cents,
            currency,
        }): Parameters<PayClaimArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        tokio::time::sleep(self.before).await;

        let charge_id = format!("po_{}", uuid::Uuid::new_v4());
        let line = serde_json::json!({
            "claim_id": claim_id,
            "amount_cents": amount_cents,
            "currency": currency,
            "charge_id": charge_id,
        });
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.ledger.as_ref())
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = appended {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to record the payout: {error}"
            ))]));
        }

        // The window a kill lands in: the money is committed and the ledger
        // knows it, and salvor's log still holds only the intent.
        tokio::time::sleep(self.after).await;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Paid {amount_cents} {currency} to claim {claim_id}. charge_id={charge_id}"
        ))]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Salvor CLI payout fixture server.")
    }
}

/// Reads a millisecond delay from an environment variable, treating anything
/// unset or unparseable as no delay.
fn delay(var: &str) -> Duration {
    Duration::from_millis(
        std::env::var(var)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ledger = std::env::args()
        .nth(1)
        .expect("the ledger path is required as the first argument");
    let service = Fixture::new(
        PathBuf::from(ledger),
        delay("SALVOR_PAYOUT_SLOW_MS"),
        delay("SALVOR_PAYOUT_SLOW_AFTER_MS"),
    )
    .serve(stdio())
    .await?;
    service.waiting().await?;
    Ok(())
}

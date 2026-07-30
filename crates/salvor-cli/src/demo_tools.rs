//! The demo tool set behind `salvor serve --demo-tools`.
//!
//! This is demo- and test-support code, compiled only under the crate's
//! default-on `fixture` feature (the same gate `demo_script` and the in-repo
//! MCP servers use), never part of a `--no-default-features` product build.
//!
//! # Why this exists
//!
//! [`salvor_server::ToolRegistry`] is deliberately EMPTY in stock `salvor
//! serve`: the mechanism is wired, but salvor-server owns no policy about
//! which tools exist (see that type's own module docs). A tool-bearing graph
//! (a `tool` node, or a client-driven tool step) is therefore a clean
//! `unknown_tool` until a host registers something. That is correct for a
//! library the way an embedding host (aarg's own `ServeRenderTool`, or
//! anyone else's) uses it. But it also means a stock `salvor serve` cannot
//! run the shape of graph the product demos and the served end-to-end suite
//! want to show: an entry agent, a branch, a gate, a tool node that reads,
//! and a tool node that writes.
//!
//! `--demo-tools` is the opt-in seam that closes that gap without touching
//! the stock default: passing it builds a small registry ([`registry`]) of
//! DETERMINISTIC, hermetic tools and hands it to [`AppState::with_tool_registry`](salvor_server::AppState::with_tool_registry)
//! exactly the way a real embedding host would. Nothing here runs unless the
//! flag is passed; the default `ToolRegistry::new()` path in
//! [`crate::commands::serve`] is untouched.
//!
//! # The three tools, and why these three
//!
//! One tool per [`Effect`] class, so the hazard and fork stories (a write
//! that dangles, a read that replays freely, an idempotent call safe to
//! retry) are all genuinely demonstrable against a live server, not just
//! described:
//!
//! - [`LookupInvoice`] (`lookup_invoice`, [`Effect::Read`]): a canned,
//!   deterministic lookup. Safe to re-run freely; the engine and the
//!   client-driven tool step both treat a `Read` as freely retryable.
//! - [`IssueRefund`] (`issue_refund`, [`Effect::Write`]): appends one durable
//!   line to a ledger file, mirroring `demo_research`'s `save_finding` (see
//!   that module's doc comment) so the same kill-mid-write, dangling-intent
//!   story that agent tool calls already demo is demonstrable through a
//!   graph `tool` node too. The ledger path resolves from
//!   `SALVOR_DEMO_TOOL_LEDGER` when set and non-empty, else
//!   `demo-tool-ledger.txt` under the working directory.
//! - [`SendEmail`] (`send_email`, [`Effect::Idempotent`]): the output's
//!   `message_id` is derived from the call's idempotency key when the
//!   runtime supplies one, so a retried attempt under the same key
//!   reproduces the identical id: the concrete, observable proof of "safe
//!   to repeat under the same key."
//!
//! Every tool is pure computation over its typed input (plus, for
//! `issue_refund`, one append to a local file): no network, no clock, no
//! randomness, so a demo run is byte-for-byte repeatable.

use std::io::Write as _;
use std::path::PathBuf;

use async_trait::async_trait;
use salvor_server::ToolRegistry;
use salvor_tools::{HandlerError, Tool, ToolCtx, ToolHandler, ToolOutcome, TypedTool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Builds the `--demo-tools` registry: [`LookupInvoice`], [`IssueRefund`],
/// and [`SendEmail`], under their derived names (`lookup_invoice`,
/// `issue_refund`, `send_email`). Called once, from
/// [`crate::commands::serve`], only when `--demo-tools` is passed.
#[must_use]
pub fn registry() -> ToolRegistry {
    ToolRegistry::new()
        .with_tool(std::sync::Arc::new(TypedTool::new(LookupInvoice)))
        .with_tool(std::sync::Arc::new(TypedTool::new(IssueRefund)))
        .with_tool(std::sync::Arc::new(TypedTool::new(SendEmail)))
}

// --- lookup_invoice (Read) --------------------------------------------------

/// `lookup_invoice`'s input: the invoice id to resolve.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupInvoiceInput {
    /// The invoice id to look up, e.g. `"inv_1001"`.
    pub invoice_id: String,
}

/// `lookup_invoice`'s output: the canned invoice record.
#[derive(Debug, Serialize, JsonSchema, PartialEq)]
pub struct LookupInvoiceOutput {
    /// Echoes the requested id.
    pub invoice_id: String,
    /// The invoice amount, in US dollars.
    pub amount_usd: f64,
    /// The watchers to notify about this invoice, for a downstream `map`
    /// node fanning out over them.
    pub watchers: Vec<String>,
}

/// A read-only, canned invoice lookup: no network, no store, the same input
/// always produces the same output, so it is always safe to re-run.
#[derive(Tool)]
#[tool(
    effect = "read",
    description = "Look up a canned invoice record by id (deterministic demo data)."
)]
pub struct LookupInvoice;

/// The small canned table a few known demo invoice ids resolve against.
/// Anything outside this table still resolves (see
/// [`fallback_invoice`]), so the tool never refuses on an unrecognized id.
const CANNED_INVOICES: &[(&str, f64, &[&str])] = &[
    (
        "inv_1001",
        128.50,
        &["alice@example.com", "bob@example.com"],
    ),
    ("inv_2002", 512.00, &["carol@example.com"]),
    ("inv_3003", 47.25, &["dave@example.com", "erin@example.com"]),
];

/// A deterministic fallback for an invoice id outside [`CANNED_INVOICES`]:
/// the amount and watcher roster are derived from the id's bytes, so the
/// same unrecognized id always resolves to the same record.
fn fallback_invoice(invoice_id: &str) -> (f64, Vec<String>) {
    let sum: u32 = invoice_id.bytes().map(u32::from).sum();
    let amount_usd = f64::from(sum % 10_000) / 100.0;
    let roster = [
        "alice@example.com",
        "bob@example.com",
        "carol@example.com",
        "dave@example.com",
    ];
    let watcher = roster[(sum as usize) % roster.len()].to_owned();
    (amount_usd, vec![watcher])
}

#[async_trait]
impl ToolHandler for LookupInvoice {
    type Input = LookupInvoiceInput;
    type Output = LookupInvoiceOutput;

    async fn call(
        &self,
        _ctx: &ToolCtx,
        input: Self::Input,
    ) -> Result<ToolOutcome<Self::Output>, HandlerError> {
        let (amount_usd, watchers) = CANNED_INVOICES
            .iter()
            .find(|(id, ..)| *id == input.invoice_id)
            .map(|(_, amount, watchers)| {
                (*amount, watchers.iter().map(|w| (*w).to_owned()).collect())
            })
            .unwrap_or_else(|| fallback_invoice(&input.invoice_id));
        Ok(ToolOutcome::Output(LookupInvoiceOutput {
            invoice_id: input.invoice_id,
            amount_usd,
            watchers,
        }))
    }
}

// --- issue_refund (Write) ---------------------------------------------------

/// `issue_refund`'s input: which invoice to refund, how much, and (for a
/// downstream `map`/notify step in the same graph) who to credit as the
/// watcher of record.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IssueRefundInput {
    /// The invoice id being refunded.
    pub invoice_id: String,
    /// The amount to refund, in US dollars.
    pub amount_usd: f64,
    /// The watcher to carry through to the output, unchanged, so a graph's
    /// next node (e.g. a `send_email` notification) can reach it without a
    /// second lookup. Optional: absent input falls back to a fixed demo
    /// address rather than failing the call.
    #[serde(default)]
    pub watcher: Option<String>,
}

/// `issue_refund`'s output: the minted refund id and what it covered.
#[derive(Debug, Serialize, JsonSchema, PartialEq)]
pub struct IssueRefundOutput {
    /// A deterministic id for this refund, derived from its input so a
    /// duplicate call (same invoice, same amount) is recognizable as a
    /// repeat rather than a new refund.
    pub refund_id: String,
    /// Echoes the refunded invoice id.
    pub invoice_id: String,
    /// Echoes the refunded amount.
    pub amount_usd: f64,
    /// Echoes the input's watcher, defaulting to a fixed demo address when
    /// none was supplied (see [`IssueRefundInput::watcher`]).
    pub watcher: String,
}

/// The fallback watcher address used when a call to `issue_refund` supplies
/// none.
const DEFAULT_WATCHER: &str = "watchers@example.com";

/// A write: not safe to repeat blindly. Appends one durable line to a ledger
/// file, mirroring `demo_research::Research::save_finding`: this is the
/// tool the hazard/fork stories point at, and the ledger is what a
/// kill-mid-write walkthrough counts to prove zero duplicate writes.
#[derive(Tool)]
#[tool(
    effect = "write",
    description = "Issue a refund for an invoice, durably recording it (demo data; no real payment provider)."
)]
pub struct IssueRefund;

/// Resolves the ledger file path: `SALVOR_DEMO_TOOL_LEDGER` when set and
/// non-empty, else `demo-tool-ledger.txt` under the working directory.
/// Mirrors `demo_research::findings_path`'s precedence (env var, then a
/// fixed relative default) minus the argv override that binary alone needs.
fn ledger_path() -> PathBuf {
    if let Ok(from_env) = std::env::var("SALVOR_DEMO_TOOL_LEDGER")
        && !from_env.is_empty()
    {
        return PathBuf::from(from_env);
    }
    PathBuf::from("demo-tool-ledger.txt")
}

#[async_trait]
impl ToolHandler for IssueRefund {
    type Input = IssueRefundInput;
    type Output = IssueRefundOutput;

    async fn call(
        &self,
        ctx: &ToolCtx,
        input: Self::Input,
    ) -> Result<ToolOutcome<Self::Output>, HandlerError> {
        let refund_id = format!(
            "rfnd_{:08x}",
            simple_hash(&format!("{}:{:.2}", input.invoice_id, input.amount_usd))
        );
        let watcher = input.watcher.unwrap_or_else(|| DEFAULT_WATCHER.to_owned());
        let key = ctx.idempotency_key().unwrap_or("none");
        let line = format!(
            "issue_refund invoice_id={} amount_usd={:.2} refund_id={refund_id} watcher={watcher} key={key}",
            input.invoice_id, input.amount_usd
        );
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ledger_path())
            .and_then(|mut file| writeln!(file, "{line}"))
            .map_err(HandlerError::new)?;
        Ok(ToolOutcome::Output(IssueRefundOutput {
            refund_id,
            invoice_id: input.invoice_id,
            amount_usd: input.amount_usd,
            watcher,
        }))
    }
}

// --- send_email (Idempotent) ------------------------------------------------

/// `send_email`'s input: who to notify, about which invoice.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendEmailInput {
    /// The watcher's address.
    pub watcher: String,
    /// The invoice this notification concerns.
    pub invoice_id: String,
}

/// `send_email`'s output: the deterministic message id sent.
#[derive(Debug, Serialize, JsonSchema, PartialEq)]
pub struct SendEmailOutput {
    /// Echoes the watcher notified.
    pub watcher: String,
    /// A deterministic id for the sent message. Derived from the call's
    /// idempotency key when the runtime supplies one, else from the input
    /// alone; either way, a retried attempt reproduces the SAME id, the
    /// observable proof that this tool is safe to repeat under a key.
    pub message_id: String,
}

/// An idempotent notification: safe to retry under the same key, because the
/// output it produces is a pure function of that key (or, absent one, of the
/// input). No file is written; the determinism itself is the demo.
#[derive(Tool)]
#[tool(
    effect = "idempotent",
    description = "Notify a watcher about an invoice by email (demo data; no real mail is sent)."
)]
pub struct SendEmail;

#[async_trait]
impl ToolHandler for SendEmail {
    type Input = SendEmailInput;
    type Output = SendEmailOutput;

    async fn call(
        &self,
        ctx: &ToolCtx,
        input: Self::Input,
    ) -> Result<ToolOutcome<Self::Output>, HandlerError> {
        let seed = ctx
            .idempotency_key()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}:{}", input.watcher, input.invoice_id));
        let message_id = format!("msg_{:08x}", simple_hash(&seed));
        Ok(ToolOutcome::Output(SendEmailOutput {
            watcher: input.watcher,
            message_id,
        }))
    }
}

/// A small, stable, non-cryptographic string hash (FNV-1a), used only to
/// mint short deterministic ids from demo input. Not a security boundary and
/// not meant to be one.
fn simple_hash(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use salvor_tools::{DynTool, Effect, ToolMeta};
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn lookup_invoice_is_read_and_resolves_the_canned_table() {
        assert_eq!(LookupInvoice::NAME, "lookup_invoice");
        assert_eq!(LookupInvoice::EFFECT, Effect::Read);
        let tool = TypedTool::new(LookupInvoice);
        let outcome = tool
            .call_json(&ToolCtx::new(None), json!({ "invoice_id": "inv_1001" }))
            .await
            .expect("dispatch succeeds");
        match outcome {
            ToolOutcome::Output(value) => assert_eq!(
                value,
                json!({
                    "invoice_id": "inv_1001",
                    "amount_usd": 128.5,
                    "watchers": ["alice@example.com", "bob@example.com"],
                })
            ),
            ToolOutcome::Suspend(_) => panic!("expected an output"),
        }
    }

    #[tokio::test]
    async fn lookup_invoice_falls_back_deterministically_for_an_unknown_id() {
        let tool = TypedTool::new(LookupInvoice);
        let a = tool
            .call_json(&ToolCtx::new(None), json!({ "invoice_id": "inv_unknown" }))
            .await
            .expect("dispatch succeeds");
        let b = tool
            .call_json(&ToolCtx::new(None), json!({ "invoice_id": "inv_unknown" }))
            .await
            .expect("dispatch succeeds");
        assert_eq!(a, b, "the same unrecognized id must resolve identically");
    }

    #[tokio::test]
    async fn issue_refund_is_write_and_appends_one_durable_line() {
        assert_eq!(IssueRefund::NAME, "issue_refund");
        assert_eq!(IssueRefund::EFFECT, Effect::Write);
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = dir.path().join("ledger.txt");
        // SAFETY (test-only, single-threaded within this test): sets the
        // process env var this handler reads via `ledger_path`.
        unsafe {
            std::env::set_var("SALVOR_DEMO_TOOL_LEDGER", &ledger);
        }
        let tool = TypedTool::new(IssueRefund);
        let outcome = tool
            .call_json(
                &ToolCtx::new(Some("key-1".to_owned())),
                json!({ "invoice_id": "inv_1001", "amount_usd": 128.5 }),
            )
            .await
            .expect("dispatch succeeds");
        match outcome {
            ToolOutcome::Output(value) => {
                assert_eq!(value["invoice_id"], "inv_1001");
                assert_eq!(value["amount_usd"], 128.5);
                assert!(value["refund_id"].as_str().unwrap().starts_with("rfnd_"));
            }
            ToolOutcome::Suspend(_) => panic!("expected an output"),
        }
        let written = std::fs::read_to_string(&ledger).expect("ledger written");
        assert_eq!(
            written.lines().count(),
            1,
            "exactly one durable line: {written}"
        );
        assert!(written.contains("invoice_id=inv_1001"));
        assert!(written.contains("key=key-1"));
        unsafe {
            std::env::remove_var("SALVOR_DEMO_TOOL_LEDGER");
        }
    }

    #[tokio::test]
    async fn send_email_is_idempotent_and_a_retried_key_reproduces_the_same_message_id() {
        assert_eq!(SendEmail::NAME, "send_email");
        assert_eq!(SendEmail::EFFECT, Effect::Idempotent);
        let tool = TypedTool::new(SendEmail);
        let input = json!({ "watcher": "alice@example.com", "invoice_id": "inv_1001" });
        let first = tool
            .call_json(&ToolCtx::new(Some("retry-key".to_owned())), input.clone())
            .await
            .expect("dispatch succeeds");
        let second = tool
            .call_json(&ToolCtx::new(Some("retry-key".to_owned())), input)
            .await
            .expect("dispatch succeeds");
        assert_eq!(
            first, second,
            "a retried call under the same idempotency key must reproduce the identical output"
        );
    }

    #[test]
    fn registry_holds_exactly_the_three_demo_tools() {
        let registry = registry();
        assert_eq!(registry.len(), 3);
        assert!(registry.resolve("lookup_invoice").is_some());
        assert!(registry.resolve("issue_refund").is_some());
        assert!(registry.resolve("send_email").is_some());
        assert!(registry.resolve("not_a_demo_tool").is_none());
    }
}

//! The built-in agent loop: model call, tool dispatch, repeat until a final
//! answer or a budget crossing.
//!
//! This module is deliberately unprivileged. It consumes only this crate's
//! **public** API ([`RunCtx`] and the other exported types), so it doubles
//! as the reference example for the library-first tier: everything it does,
//! an outside crate can do (the `custom_loop` integration test proves it).
//! If this loop ever needed a private hook, the API design would be wrong.
//!
//! # Shape
//!
//! ```text
//! begin
//! loop {
//!     observe ctx.now()             (one recorded observation per iteration)
//!     budget checks                 (steps, tokens, cost, wall time; park on crossing)
//!     build MessageRequest          (system prompt, tools, conversation so far)
//!     ctx.model_call
//!     if the response has tool_use blocks {
//!         dispatch each through ctx.tool_call
//!         (suspension parks; failure compacts into the tool_result)
//!         append tool results to the conversation
//!     } else {
//!         ctx.complete_run(final text)
//!     }
//! }
//! ```
//!
//! # Determinism inventory
//!
//! Everything the loop feeds forward is a pure function of recorded data:
//! the conversation is rebuilt from replayed model responses, tool outputs,
//! and resume inputs; budget observations come from recorded usage and
//! recorded `now` observations; idempotency keys derive from recorded
//! random bits; error compaction is a pure function of recorded failures.
//! So a replayed drive makes bit-identical requests (hashes match) and
//! takes identical branches.
//!
//! Two deliberately unrecorded edges, both derived from recorded data and
//! therefore still deterministic: an unknown tool name (the model asked for
//! a tool the agent does not have) becomes an error `tool_result` without
//! any event, and the count-based repeat summary replaces repeated error
//! text without changing the log.

use salvor_llm::{ContentBlock, Message, MessageRequest, Tool};
use serde_json::Value;

use crate::agent::Agent;
use crate::budgets::{BudgetExtensions, BudgetObservations};
use crate::compact::FailureTracker;
use crate::ctx::{Resumption, RunCtx, ToolCallResult};
use crate::error::RuntimeError;
use crate::runtime::ParkReason;
use crate::wire::content_string;
use salvor_core::Effect;
use time::OffsetDateTime;

/// How one drive of the loop ended: the run completed, or it parked and the
/// process should stop driving it.
#[derive(Debug, Clone)]
pub(crate) enum LoopOutcome {
    /// The run completed with this output.
    Completed(Value),
    /// The run is parked durably; resume it later with input.
    Parked(ParkReason),
}

/// Drives one run (fresh, recovering, or resuming; the `ctx` knows which)
/// to completion or a park.
pub(crate) async fn drive(
    ctx: &mut RunCtx,
    agent: &Agent,
    initial_input: &Value,
) -> Result<LoopOutcome, RuntimeError> {
    let input = ctx.begin(agent.def_hash(), initial_input).await?;

    let mut conversation: Vec<Message> = vec![Message::user(content_string(&input))];
    let llm_tools: Vec<Tool> = agent
        .tools()
        .descriptors()
        .into_iter()
        .map(|descriptor| Tool {
            name: descriptor.name,
            description: Some(descriptor.description),
            input_schema: descriptor.input_schema,
        })
        .collect();

    let mut steps: u64 = 0;
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut started_at: Option<OffsetDateTime> = None;
    let mut extensions = BudgetExtensions::default();
    let mut failures = FailureTracker::new();

    loop {
        // One recorded clock observation per iteration; the first doubles as
        // the wall-time baseline. Never the ambient clock: the identical
        // elapsed value must be observable on replay.
        let now = ctx.now().await?;
        let start = *started_at.get_or_insert(now);

        // Budget checks run between events, before the model call, over
        // replayed data only. A crossing parks exactly like a suspension; a
        // recorded resume may extend the budget and the check re-runs.
        loop {
            let observations = BudgetObservations {
                steps,
                input_tokens,
                output_tokens,
                elapsed_seconds: (now - start).as_seconds_f64(),
            };
            let Some((budget, observed)) =
                agent
                    .budgets()
                    .first_crossing(&extensions, agent.pricing(), &observations)
            else {
                break;
            };
            ctx.budget_exceeded(budget, observed).await?;
            match ctx.await_resume().await? {
                Resumption::Parked => {
                    return Ok(LoopOutcome::Parked(ParkReason::BudgetExceeded {
                        budget,
                        observed,
                    }));
                }
                Resumption::Resumed(resume_input) => extensions.absorb(&resume_input),
            }
        }

        let mut request = MessageRequest::new(agent.model(), agent.max_response_tokens())
            .with_messages(conversation.clone());
        if let Some(system) = agent.system_prompt() {
            request = request.with_system(system);
        }
        if !llm_tools.is_empty() {
            request = request.with_tools(llm_tools.clone());
        }

        let turn = ctx.model_call(agent.client(), &request).await?;
        steps += 1;
        input_tokens = input_tokens.saturating_add(u64::from(turn.usage.input_tokens));
        output_tokens = output_tokens.saturating_add(u64::from(turn.usage.output_tokens));

        let tool_uses: Vec<(String, String, Value)> = turn
            .response
            .tool_uses()
            .into_iter()
            .map(|(id, name, tool_input)| (id.to_owned(), name.to_owned(), tool_input.clone()))
            .collect();

        conversation.push(Message::assistant_blocks(turn.response.content.clone()));

        // No tool calls: the text is the final answer.
        if tool_uses.is_empty() {
            let output = Value::String(turn.response.text());
            ctx.complete_run(&output).await?;
            return Ok(LoopOutcome::Completed(output));
        }

        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        for (tool_use_id, name, tool_input) in tool_uses {
            let Some(tool) = agent.tools().get(&name) else {
                // The model named a tool the agent does not have. This is
                // derived purely from the recorded response, so it needs no
                // event of its own; the error content is deterministic.
                result_blocks.push(ContentBlock::tool_error(
                    tool_use_id,
                    format!("unknown tool `{name}`"),
                ));
                continue;
            };

            // Idempotent calls get a key derived from recorded randomness,
            // so the same key reappears on replay and on a post-crash retry.
            let idempotency_key = match tool.effect() {
                Effect::Idempotent => Some(format!("{:016x}", ctx.random().await?)),
                Effect::Read | Effect::Write => None,
            };

            match ctx
                .tool_call(tool, &tool_input, idempotency_key.as_deref())
                .await?
            {
                ToolCallResult::Output(output) => {
                    failures.record_success();
                    result_blocks.push(ContentBlock::tool_result(
                        tool_use_id,
                        content_string(&output),
                    ));
                }
                ToolCallResult::Failed(failure) => {
                    // The full error is already in the event log; the model
                    // sees the compacted or collapsed form only.
                    let content = failures.content_for_failure(&name, &failure.message);
                    result_blocks.push(ContentBlock::tool_error(tool_use_id, content));
                }
                ToolCallResult::Suspended(suspension) => {
                    ctx.suspend(&suspension.reason, &suspension.input_schema)
                        .await?;
                    match ctx.await_resume().await? {
                        Resumption::Parked => {
                            return Ok(LoopOutcome::Parked(ParkReason::Suspended {
                                reason: suspension.reason,
                                input_schema: suspension.input_schema,
                            }));
                        }
                        Resumption::Resumed(resume_input) => {
                            // The recorded resume input is the tool's answer.
                            failures.record_success();
                            result_blocks.push(ContentBlock::tool_result(
                                tool_use_id,
                                content_string(&resume_input),
                            ));
                        }
                    }
                }
            }
        }
        conversation.push(Message::user_blocks(result_blocks));
    }
}

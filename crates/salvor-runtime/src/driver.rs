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

/// How one drive of the loop ended: it produced a final output, or it parked
/// and the process should stop driving it.
///
/// [`Completed`](LoopOutcome::Completed) carries the loop's final output but
/// does **not** mean the run's terminal `RunCompleted` has been recorded:
/// [`drive_loop`] deliberately leaves that to its caller. [`drive`] records it
/// straight away, preserving the built-in loop's log byte for byte; the graph
/// engine records it once, after its last node, so an agent loop can run as one
/// node among many inside a single graph log without each node closing the run.
#[derive(Debug, Clone)]
pub enum LoopOutcome {
    /// The loop produced this final output. The caller records the terminal
    /// `RunCompleted`.
    Completed(Value),
    /// The run is parked durably; resume it later with input.
    Parked(ParkReason),
}

/// Drives one run (fresh, recovering, or resuming; the `ctx` knows which)
/// to a final output or a park.
///
/// This is exactly [`begin`] followed by [`drive_loop`], with the terminal
/// `RunCompleted` recorded here on completion. Splitting those two halves out
/// is what lets the graph engine run [`drive_loop`] against a `RunCtx` whose
/// log it already opened with `GraphRunStarted`: the agent node contributes its
/// model and tool events without a second run head and without closing the run.
pub(crate) async fn drive(
    ctx: &mut RunCtx,
    agent: &Agent,
    initial_input: &Value,
) -> Result<LoopOutcome, RuntimeError> {
    let input = begin(ctx, agent, initial_input).await?;
    let outcome = drive_loop(ctx, agent, &input).await?;
    // The built-in path records the terminal itself, in the same position and
    // with the same output the loop used to record inline. Moving the call here
    // changes no bytes: `begin`, the loop's events, then `RunCompleted`, in that
    // order, exactly as before the split.
    if let LoopOutcome::Completed(output) = &outcome {
        ctx.complete_run(output).await?;
    }
    Ok(outcome)
}

/// Records (or replays) the run's head and returns the input the loop drives
/// on. The first half of [`drive`], split out so [`drive_loop`] can be driven
/// against a run whose head was opened some other way.
pub(crate) async fn begin(
    ctx: &mut RunCtx,
    agent: &Agent,
    initial_input: &Value,
) -> Result<Value, RuntimeError> {
    ctx.begin(agent.def_hash(), initial_input).await
}

/// Runs the built-in agent loop over an already-begun run, returning the final
/// output (a [`LoopOutcome::Completed`]) or a park — but **not** recording the
/// terminal `RunCompleted`.
///
/// The second half of [`drive`], made public so an external driver can run an
/// agent loop inside a run it opened itself. The graph engine uses exactly
/// this: it opens the log with `GraphRunStarted`, records `NodeEntered`, calls
/// `drive_loop` (whose model and tool events land in the same log), records
/// `NodeExited`, and moves to the next node — recording the single terminal
/// `RunCompleted` only after its last node. Leaving the terminal to the caller
/// is the whole reason the completion moved out of the loop and into [`drive`].
///
/// `input` is the already-begun run's input (what [`begin`] returned).
///
/// # Errors
///
/// Whatever the `RunCtx` operations surface: [`RuntimeError::Replay`] on
/// divergence, [`RuntimeError::Model`] on a live provider failure,
/// [`RuntimeError::Store`] on a persistence failure.
pub async fn drive_loop(
    ctx: &mut RunCtx,
    agent: &Agent,
    input: &Value,
) -> Result<LoopOutcome, RuntimeError> {
    let mut conversation: Vec<Message> = vec![Message::user(content_string(input))];
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

        // No tool calls: the text is the final answer. The loop returns it
        // without recording the terminal; the caller records `RunCompleted`
        // (`drive` straight away, the graph engine once after its last node).
        if tool_uses.is_empty() {
            let output = Value::String(turn.response.text());
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

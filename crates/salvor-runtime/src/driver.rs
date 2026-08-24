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
//!         (suspension parks; sleep parks on a timer; failure compacts into
//!          the tool_result)
//!         append tool results to the conversation
//!     } else {
//!         ctx.complete_run(final text)
//!     }
//! }
//! ```
//!
//! # Structured output
//!
//! [`drive_loop_structured`] runs the same loop under a declared output
//! schema. The request then carries one synthetic tool beyond the agent's
//! own, [`ANSWER_TOOL`], whose `input_schema` IS the declared schema, and a
//! `tool_choice` of `any`, so a bare-text terminal turn cannot happen by API
//! contract. The loop ends when that tool is called alone in a turn and its
//! input passes [`crate::validate_against_schema`]; the input, verbatim, is
//! the loop's output. Anything else feeds back and the loop asks again: an
//! answer called beside real tool work gets a `tool_error` saying so (the
//! real calls run normally), a violating answer gets a `tool_error` naming
//! the violation, and a turn with no tool call at all (providers vary) gets
//! a re-ask. There is no retry counter: the steps budget is the bound, and a
//! crossing mid-re-ask parks like any other.
//!
//! Nothing has to ask for that path by name. An [`Agent`] that declares an
//! [`output_schema`](Agent::output_schema) drives it automatically through
//! [`drive`], so a schema written into an `agent.toml` reaches every
//! server-side loop that runs the agent; [`drive_loop_structured`] is the
//! same thing for a caller driving a schema of its own, which is how a
//! graph's `agent` node overrides its agent's default.
//!
//! # Determinism inventory
//!
//! Everything the loop feeds forward is a pure function of recorded data:
//! the conversation is rebuilt from replayed model responses, tool outputs,
//! and resume inputs; budget observations come from recorded usage and
//! recorded `now` observations; idempotency keys derive from recorded
//! random bits; error compaction is a pure function of recorded failures; a
//! woken tool call's result is a fixed shape over the wake instant its own
//! recorded completion holds, never a clock read.
//! So a replayed drive makes bit-identical requests (hashes match) and
//! takes identical branches.
//!
//! Two deliberately unrecorded edges, both derived from recorded data and
//! therefore still deterministic: an unknown tool name (the model asked for
//! a tool the agent does not have) becomes an error `tool_result` without
//! any event, and the count-based repeat summary replaces repeated error
//! text without changing the log.
//!
//! Structured output adds no third edge: every string it feeds back is one of
//! this module's fixed templates plus, for a violation, the verdict string
//! [`crate::validate_against_schema`] returned. That verdict decides whether
//! another hashed model call happens, which is exactly why it comes from this
//! repo's own validator and not a library whose message text is free to
//! change under a version bump.

use salvor_llm::{ContentBlock, Message, MessageRequest, Tool, ToolChoice};
use serde_json::Value;

use crate::agent::Agent;
use crate::compact::FailureTracker;
use crate::ctx::{Resumption, RunCtx, ToolCallResult, Waking};
use crate::error::RuntimeError;
use crate::runtime::ParkReason;
use crate::validate::validate_against_schema;
use crate::wire::{content_string, slept_output};
use salvor_core::Effect;
use salvor_core::{BudgetExtensions, BudgetObservations};
use time::OffsetDateTime;

/// The name of the synthetic tool a structured-output loop answers through.
///
/// An agent that offers a real tool under this name cannot run under a
/// declared output schema: the two calls would be indistinguishable in the
/// response, so [`drive_loop_structured`] refuses with
/// [`RuntimeError::AnswerToolNameTaken`] before any model call.
pub const ANSWER_TOOL: &str = "salvor_answer";

/// The answer tool's description. Fixed text, like every other string this
/// loop puts in a request: it is hashed into `ModelCallRequested`.
const ANSWER_TOOL_DESCRIPTION: &str = "Deliver your final reply by calling this tool; its input is \
     the reply itself, and nothing else you write is read as the answer.";

/// Fed back when the answer call shared a turn with real tool calls.
const ANSWER_NOT_ALONE: &str = "`salvor_answer` was called alongside other tools; it ends the turn, \
     so call it alone once the tool work it depends on has come back.";

/// Fed back when a turn carried no tool call at all, which `tool_choice` was
/// supposed to make impossible. Providers vary; the loop asks again rather
/// than reading a bare-text turn as an answer it never validated.
const NO_TOOL_CALL_REASK: &str = "That turn called no tool. Call a tool, or deliver your final \
     reply by calling `salvor_answer`.";

/// The content a schema violation feeds back: our validator's own verdict,
/// wrapped in a fixed template naming what to do about it.
fn violation_content(violation: &str) -> String {
    format!(
        "`salvor_answer` was called with input that does not match its schema: {violation}. Call \
         it again with input in the declared shape."
    )
}

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
/// This is exactly [`begin`] followed by the loop, with the terminal
/// `RunCompleted` recorded here on completion. Splitting those two halves out
/// is what lets the graph engine run [`drive_loop`] against a `RunCtx` whose
/// log it already opened with `GraphRunStarted`: the agent node contributes its
/// model and tool events without a second run head and without closing the run.
///
/// Which loop runs is the agent's own decision: an agent that declares an
/// [`output_schema`](Agent::output_schema) drives the structured path, and one
/// that declares none drives the plain one. So the declaration in an
/// `agent.toml` reaches `salvor run`, `Runtime::start`, `recover`, and
/// `resume` without any of them being told about it a second time. (A
/// structured drive whose agent already owns a real `salvor_answer` tool still
/// fails with [`RuntimeError::AnswerToolNameTaken`], but here the head is
/// already recorded when it does, exactly as for any other failure on the
/// first step.)
pub(crate) async fn drive(
    ctx: &mut RunCtx,
    agent: &Agent,
    initial_input: &Value,
) -> Result<LoopOutcome, RuntimeError> {
    let input = begin(ctx, agent, initial_input).await?;
    let outcome = drive_loop_inner(ctx, agent, &input, agent.output_schema()).await?;
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
/// output (a [`LoopOutcome::Completed`]) or a park, but **not** recording the
/// terminal `RunCompleted`.
///
/// The second half of [`drive`], made public so an external driver can run an
/// agent loop inside a run it opened itself. The graph engine uses exactly
/// this: it opens the log with `GraphRunStarted`, records `NodeEntered`, calls
/// `drive_loop` (whose model and tool events land in the same log), records
/// `NodeExited`, and moves to the next node, recording the single terminal
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
    drive_loop_inner(ctx, agent, input, None).await
}

/// Runs the built-in agent loop under a declared output schema, returning a
/// [`LoopOutcome::Completed`] whose value is the model's structured answer
/// (never a string of prose) or a park.
///
/// Same loop, same events, same caller contract as [`drive_loop`]: the
/// difference is how the loop is allowed to end. The request offers
/// [`ANSWER_TOOL`] beside the agent's own tools with `schema` as its input
/// schema and forces some tool call, and the loop ends only when that tool is
/// called alone and its input satisfies `schema` under
/// [`crate::validate_against_schema`]. See the module docs for what each other
/// shape of turn feeds back.
///
/// # Errors
///
/// Everything [`drive_loop`] surfaces, plus
/// [`RuntimeError::AnswerToolNameTaken`] when the agent already offers a real
/// tool named [`ANSWER_TOOL`]. That one is checked before the first model
/// call, so a refused drive records nothing.
pub async fn drive_loop_structured(
    ctx: &mut RunCtx,
    agent: &Agent,
    input: &Value,
    schema: &Value,
) -> Result<LoopOutcome, RuntimeError> {
    drive_loop_inner(ctx, agent, input, Some(schema)).await
}

/// The one implementation behind [`drive_loop`] and
/// [`drive_loop_structured`]; `output_schema` is what separates them.
async fn drive_loop_inner(
    ctx: &mut RunCtx,
    agent: &Agent,
    input: &Value,
    output_schema: Option<&Value>,
) -> Result<LoopOutcome, RuntimeError> {
    let mut conversation: Vec<Message> = vec![Message::user(content_string(input))];
    let mut llm_tools: Vec<Tool> = agent
        .tools()
        .descriptors()
        .into_iter()
        .map(|descriptor| Tool {
            name: descriptor.name,
            description: Some(descriptor.description),
            input_schema: descriptor.input_schema,
        })
        .collect();

    if let Some(schema) = output_schema {
        // Before anything is recorded: two tools under one name would make the
        // answer call unreadable in the response.
        if llm_tools.iter().any(|tool| tool.name == ANSWER_TOOL) {
            return Err(RuntimeError::AnswerToolNameTaken);
        }
        llm_tools.push(Tool {
            name: ANSWER_TOOL.to_owned(),
            description: Some(ANSWER_TOOL_DESCRIPTION.to_owned()),
            input_schema: schema.clone(),
        });
    }

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
        if output_schema.is_some() {
            // Some tool, the model's pick: the answer tool is one of them, so
            // the turn that ends the loop is a tool call like any other and a
            // bare-text terminal turn is off the table by API contract.
            request = request.with_tool_choice(ToolChoice::any());
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

        // No tool calls. Unstructured, the text is the final answer: the loop
        // returns it without recording the terminal; the caller records
        // `RunCompleted` (`drive` straight away, the graph engine once after
        // its last node). Structured, `tool_choice` asked for a call and none
        // came, so the loop asks again rather than reading prose as an answer
        // it never validated.
        if tool_uses.is_empty() {
            if output_schema.is_none() {
                let output = Value::String(turn.response.text());
                return Ok(LoopOutcome::Completed(output));
            }
            conversation.push(Message::user(NO_TOOL_CALL_REASK));
            continue;
        }

        // The one way a structured loop ends: the answer tool alone in its
        // turn, carrying input the declared schema accepts. The input is the
        // output, verbatim.
        if let Some(schema) = output_schema
            && let [(tool_use_id, name, answer)] = tool_uses.as_slice()
            && name == ANSWER_TOOL
        {
            match validate_against_schema(answer, schema) {
                Ok(()) => return Ok(LoopOutcome::Completed(answer.clone())),
                Err(violation) => {
                    // A violation is a failed call like any other, streak
                    // collapse included: an answer that keeps missing the
                    // shape the same way stops re-sending the same wall of
                    // text back.
                    let content =
                        failures.content_for_failure(ANSWER_TOOL, &violation_content(&violation));
                    conversation.push(Message::user_blocks(vec![ContentBlock::tool_error(
                        tool_use_id.clone(),
                        content,
                    )]));
                    continue;
                }
            }
        }

        let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        for (tool_use_id, name, tool_input) in tool_uses {
            // An answer call that got here shared its turn with other calls
            // (or repeated itself). The real calls run; this one is told to
            // come back alone, so the answer is always the whole turn.
            if output_schema.is_some() && name == ANSWER_TOOL {
                result_blocks.push(ContentBlock::tool_error(tool_use_id, ANSWER_NOT_ALONE));
                continue;
            }
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
                    // The tool's discriminator is recorded and then carried
                    // out to the caller unchanged. A tool that parked the run
                    // on a webhook has said so, and a park report that turned
                    // that back into a human gate would send an operator
                    // looking for an approval nobody is asking them for.
                    ctx.suspend_with_kind(
                        &suspension.reason,
                        &suspension.input_schema,
                        suspension.kind,
                    )
                    .await?;
                    match ctx.await_resume().await? {
                        Resumption::Parked => {
                            return Ok(LoopOutcome::Parked(ParkReason::Suspended {
                                reason: suspension.reason,
                                input_schema: suspension.input_schema,
                                kind: suspension.kind,
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
                // The timer counterpart of the arm above, and deliberately its
                // twin: park, and on a later drive carry on from the recorded
                // events. The call is already settled when this arm runs (its
                // completion carried the request), so the sleep that starts
                // here holds no idempotency claim, however long it lasts.
                ToolCallResult::Sleeping(sleep) => {
                    ctx.sleep_until(sleep.wake_at).await?;
                    match ctx.await_wake().await? {
                        Waking::Asleep { wake_at } => {
                            return Ok(LoopOutcome::Parked(ParkReason::Sleeping { wake_at }));
                        }
                        Waking::Woken => {
                            // The tool returned a deadline rather than a value,
                            // so the result is derived from the recorded wake
                            // instant. Deterministic: the instant comes from
                            // the completion, not from a clock read here.
                            failures.record_success();
                            result_blocks.push(ContentBlock::tool_result(
                                tool_use_id,
                                content_string(&slept_output(sleep.wake_at)),
                            ));
                        }
                    }
                }
            }
        }
        conversation.push(Message::user_blocks(result_blocks));
    }
}

//! The four views the router targets.
//!
//! Three of them are built here: the run list (`/`), the approval and
//! reconciliation inbox (`/inbox`), and the spend overview (`/spend`). The
//! fourth, `Inspector`, stays a thin delegation to
//! [`RunInspector`](crate::inspector::RunInspector) (its own module, untouched).
//!
//! # What each view reads, and how it reuses the foundation
//!
//! - **Run list** reads [`api::list_runs`] once and renders a table. Status is
//!   the shared [`StatusBadge`], the id is the shared [`RunRef`], and attention
//!   states sort to the top. The list endpoint carries no agent, no usage, and
//!   no step count, so those columns are marked unavailable rather than faked
//!   (see [`RunTableRow`]).
//! - **Inbox** reads [`api::list_runs`], keeps only the waiting-on-human runs,
//!   and renders one card per parked run. The suspension form is generated from
//!   the recorded `input_schema` the folded status already carries; the budget
//!   card builds the `extend` payload; the reconciliation card fetches the
//!   dangling write intent and records its outcome. Actions call the real
//!   [`api::resume_run`] / [`api::resolve_run`].
//! - **Spend** enumerates runs with [`api::list_runs`], then folds each run's
//!   log through the same [`connect_run_stream`](crate::sse::connect_run_stream)
//!   and [`pricing`] the inspector uses, because the dollar estimate needs the
//!   per-call model id that only the log carries. Tokens are exact; dollars are
//!   an estimate; a declared ceiling is shown only when a crossing recorded it.
//!
//! # The one shared async shape
//!
//! Every data-backed view holds a [`Load`] signal (loading / ready / failed) and
//! fills it from a [`spawn_local`] future, re-runnable through a reload trigger.
//! That covers the loading, empty, and error states section 03 of the spec asks
//! every data-backed component to have.

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use crate::api::{self, ApiPending, ApiStatus, RunSummary};
use crate::config::Config;
use crate::inspector::RunInspector;
use crate::pricing::{self, CostSummary};
use crate::replay::{Budget, BudgetKind, Event, EventEnvelope, RunStatus, TokenTotals, head_state};
use crate::sse::{RunStream, connect_run_stream};
use crate::status::{RunRef, StatusBadge, StatusGroup, truncate_id};

/// A data-backed view's three states, shared by every fetch here.
#[derive(Clone)]
enum Load<T> {
    /// The request is in flight.
    Loading,
    /// The request returned this value.
    Ready(T),
    /// The request failed with this message.
    Failed(String),
}

// -------------------------------------------------------------------------
// Run list (route `/`)
// -------------------------------------------------------------------------

/// Run list (landing): every run at a glance, attention states first.
#[component]
pub fn RunList() -> impl IntoView {
    let config = expect_context::<Config>();
    let runs = RwSignal::new(Load::<Vec<RunSummary>>::Loading);
    let reload = RwSignal::new(0u32);

    // Fetch on mount and whenever the reload trigger bumps. The effect reads
    // only `reload`, so setting `runs` inside it cannot loop.
    Effect::new(move |_| {
        reload.get();
        let config = config.clone();
        runs.set(Load::Loading);
        spawn_local(async move {
            match api::list_runs(&config).await {
                Ok(mut list) => {
                    sort_for_attention(&mut list);
                    runs.set(Load::Ready(list));
                }
                Err(err) => runs.set(Load::Failed(err.to_string())),
            }
        });
    });

    view! {
        <section class="run-list">
            <header class="view-header">
                <h1>"Runs"</h1>
                <button class="reload" on:click=move |_| reload.update(|n| *n += 1)>"Reload"</button>
            </header>
            {move || match runs.get() {
                Load::Loading => view! {
                    <p class="view-state" data-state="loading">"Loading runs\u{2026}"</p>
                }.into_any(),
                Load::Failed(err) => view! {
                    <p class="view-state" data-state="error">{format!("Could not load runs: {err}")}</p>
                }.into_any(),
                Load::Ready(list) if list.is_empty() => view! {
                    <p class="view-state" data-state="empty">
                        "No runs yet. Start one with the CLI or the API and it appears here."
                    </p>
                }.into_any(),
                Load::Ready(list) => view! { <RunTable runs=list /> }.into_any(),
            }}
        </section>
    }
}

/// The run table: the spec's columns, one [`RunTableRow`] per run.
#[component]
fn RunTable(runs: Vec<RunSummary>) -> impl IntoView {
    let rows = runs
        .into_iter()
        .map(|run| view! { <RunTableRow run=run /> })
        .collect::<Vec<_>>();
    view! {
        <table class="run-table">
            <thead>
                <tr>
                    <th>"Status"</th>
                    <th>"Run"</th>
                    <th>"Agent"</th>
                    <th>"Cost so far"</th>
                    <th>"Steps"</th>
                    <th>"Events"</th>
                    <th>"Age"</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
        <p class="table-note">
            "Agent, cost, and step count are not in the run-list endpoint; open a run to see them."
        </p>
    }
}

/// One table row. A row opens the run in the inspector via [`RunRef`].
///
/// `GET /v1/runs` gives status, id, event count, and timestamps and nothing
/// more, so three columns are honestly blank: the agent id lives in a
/// `RunStarted` event, cost needs per-call usage the list omits, and step count
/// needs the log. Each blank cell says why on hover rather than inventing a value.
#[component]
fn RunTableRow(run: RunSummary) -> impl IntoView {
    let status = run.status.to_run_status();
    let group_slug = run.group().slug();
    let age = age_from_rfc3339(&run.last_recorded_at);
    view! {
        <tr class="run-table-row" data-group=group_slug>
            <td><StatusBadge status=status /></td>
            <td><RunRef id=run.run.clone() /></td>
            <td class="cell-unavailable" title="The run list does not carry the agent; open the run to see it">"\u{2014}"</td>
            <td class="cell-unavailable" title="Cost needs per-call token usage the list does not include">"\u{2014}"</td>
            <td class="cell-unavailable" title="Step count needs the run's event log, not fetched for the list">"\u{2014}"</td>
            <td>{run.event_count.to_string()}</td>
            <td>{age}</td>
        </tr>
    }
}

/// The attention rank of a group: waiting-on-human first, then in-progress,
/// then terminal. Lower sorts earlier.
const fn group_rank(group: StatusGroup) -> u8 {
    match group {
        StatusGroup::WaitingOnHuman => 0,
        StatusGroup::InProgress => 1,
        StatusGroup::Terminal => 2,
    }
}

/// Sorts attention states to the top, then most-recent activity first inside
/// each group. RFC 3339 timestamps in the same `Z` offset compare in time order
/// as plain strings, which is enough for this ordering.
fn sort_for_attention(runs: &mut [RunSummary]) {
    runs.sort_by(|a, b| {
        group_rank(a.group())
            .cmp(&group_rank(b.group()))
            .then_with(|| b.last_recorded_at.cmp(&a.last_recorded_at))
    });
}

// -------------------------------------------------------------------------
// Inbox (route `/inbox`)
// -------------------------------------------------------------------------

/// Approval and reconciliation inbox: one card per parked run, or "all clear".
#[component]
pub fn Inbox() -> impl IntoView {
    let config = expect_context::<Config>();
    let queue = RwSignal::new(Load::<Vec<RunSummary>>::Loading);
    // Cards bump this after a successful action; the queue re-fetches and the
    // acted-on card leaves because the run is no longer waiting on a human.
    let reload = RwSignal::new(0u32);

    Effect::new(move |_| {
        reload.get();
        let config = config.clone();
        queue.set(Load::Loading);
        spawn_local(async move {
            match api::list_runs(&config).await {
                Ok(list) => {
                    let parked = list
                        .into_iter()
                        .filter(|run| run.group() == StatusGroup::WaitingOnHuman)
                        .collect::<Vec<_>>();
                    queue.set(Load::Ready(parked));
                }
                Err(err) => queue.set(Load::Failed(err.to_string())),
            }
        });
    });

    view! {
        <section class="inbox">
            <header class="view-header">
                <h1>"Inbox"</h1>
                <button class="reload" on:click=move |_| reload.update(|n| *n += 1)>"Reload"</button>
            </header>
            {move || match queue.get() {
                Load::Loading => view! {
                    <p class="view-state" data-state="loading">"Loading the inbox\u{2026}"</p>
                }.into_any(),
                Load::Failed(err) => view! {
                    <p class="view-state" data-state="error">{format!("Could not load the inbox: {err}")}</p>
                }.into_any(),
                Load::Ready(list) if list.is_empty() => view! {
                    <p class="inbox-empty view-state" data-state="all-clear">
                        "All clear. No runs are waiting on a human."
                    </p>
                }.into_any(),
                Load::Ready(list) => list
                    .into_iter()
                    .map(|run| view! { <InboxCard run=run reload=reload /> })
                    .collect::<Vec<_>>()
                    .into_any(),
            }}
        </section>
    }
}

/// Dispatches a parked run to the card for why it parked. Non-parked statuses
/// never reach here (the queue filters them), so they render nothing.
#[component]
fn InboxCard(run: RunSummary, reload: RwSignal<u32>) -> impl IntoView {
    match run.status.clone() {
        ApiStatus::Suspended {
            reason,
            input_schema,
        } => view! {
            <SuspensionCard run_id=run.run reason=reason input_schema=input_schema reload=reload />
        }
        .into_any(),
        ApiStatus::BudgetExceeded { budget, observed } => view! {
            <BudgetCard run_id=run.run budget=budget observed=observed reload=reload />
        }
        .into_any(),
        ApiStatus::NeedsReconciliation => view! {
            <ReconciliationCard run_id=run.run reload=reload />
        }
        .into_any(),
        _ => ().into_any(),
    }
}

/// A suspended run: the reason and a form generated from the recorded
/// `input_schema`, submitting a resume with the form's JSON as `input`.
///
/// The schema comes from the folded status the list already returned (the fold
/// carries the `Suspended` event's schema), so no extra fetch is needed. A
/// schema this build can turn into fields becomes one input per property;
/// anything else falls back to a raw-JSON textarea so the action still works.
#[component]
fn SuspensionCard(
    run_id: String,
    reason: String,
    input_schema: Value,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let config = expect_context::<Config>();
    let fields = StoredValue::new(schema_fields(&input_schema));
    let values = RwSignal::new(BTreeMap::<String, String>::new());
    let raw = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let submitting = RwSignal::new(false);

    let run_id = StoredValue::new(run_id);
    let config = StoredValue::new(config);

    // The submit handler: build the input the schema implies, then resume.
    let submit = move |_| {
        let built = match fields.get_value() {
            Some(spec) => build_input(&spec, &values.get_untracked()),
            None => serde_json::from_str::<Value>(&raw.get_untracked())
                .map_err(|err| format!("not valid JSON: {err}")),
        };
        let input = match built {
            Ok(input) => input,
            Err(message) => {
                error.set(Some(message));
                return;
            }
        };
        error.set(None);
        submitting.set(true);
        let config = config.get_value();
        let id = run_id.get_value();
        spawn_local(async move {
            match api::resume_run(&config, &id, Some(input)).await {
                Ok(_) => reload.update(|n| *n += 1),
                Err(err) => {
                    error.set(Some(err.to_string()));
                    submitting.set(false);
                }
            }
        });
    };

    let form_body = match fields.get_value() {
        Some(spec) => spec
            .into_iter()
            .map(|field| schema_field_input(field, values))
            .collect::<Vec<_>>()
            .into_any(),
        None => view! {
            <label class="form-field">
                <span>"Resume input (raw JSON)"</span>
                <textarea
                    class="raw-json"
                    prop:value=move || raw.get()
                    on:input=move |ev| raw.set(event_target_value(&ev))
                ></textarea>
            </label>
        }
        .into_any(),
    };

    view! {
        <article class="inbox-card" data-kind="suspension" data-group="waiting-on-human">
            <header class="card-header">
                <span class="card-kind">"Suspended"</span>
                <RunRef id=run_id.get_value() />
            </header>
            <p class="card-reason">{reason}</p>
            <div class="card-form">{form_body}</div>
            {move || error.get().map(|message| view! {
                <p class="card-error" data-state="error">{message}</p>
            })}
            <button class="card-action" prop:disabled=move || submitting.get() on:click=submit>
                {move || if submitting.get() { "Resuming\u{2026}" } else { "Resume run" }}
            </button>
        </article>
    }
}

/// A budget-crossed run: the crossed limit and observed value, an input for the
/// extra headroom to grant, and a resume carrying the `extend` payload.
///
/// The extension is additive: the effective limit is the declared limit plus the
/// granted amount (confirmed against the runtime's budget module). The input
/// therefore asks for how much more to allow, keyed by the crossed dimension.
#[component]
fn BudgetCard(
    run_id: String,
    budget: Budget,
    observed: f64,
    reload: RwSignal<u32>,
) -> impl IntoView {
    let config = expect_context::<Config>();
    let amount = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let submitting = RwSignal::new(false);

    let run_id = StoredValue::new(run_id);
    let config = StoredValue::new(config);
    let kind = budget.kind;

    let submit = move |_| {
        let input = match build_extension(kind, &amount.get_untracked()) {
            Ok(input) => input,
            Err(message) => {
                error.set(Some(message));
                return;
            }
        };
        error.set(None);
        submitting.set(true);
        let config = config.get_value();
        let id = run_id.get_value();
        spawn_local(async move {
            match api::resume_run(&config, &id, Some(input)).await {
                Ok(_) => reload.update(|n| *n += 1),
                Err(err) => {
                    error.set(Some(err.to_string()));
                    submitting.set(false);
                }
            }
        });
    };

    let kind_label = budget_kind_label(kind);
    view! {
        <article class="inbox-card" data-kind="budget" data-group="waiting-on-human">
            <header class="card-header">
                <span class="card-kind">"Budget exceeded"</span>
                <RunRef id=run_id.get_value() />
            </header>
            <dl class="card-detail">
                <dt>"Dimension"</dt>
                <dd>{kind_label}</dd>
                <dt>"Declared limit"</dt>
                <dd>{format!("{}", budget.limit)}</dd>
                <dt>"Observed"</dt>
                <dd>{format!("{observed}")}</dd>
            </dl>
            <label class="form-field">
                <span>{format!("Extra {kind_label} to grant")}</span>
                <input
                    type="number"
                    prop:value=move || amount.get()
                    on:input=move |ev| amount.set(event_target_value(&ev))
                />
            </label>
            {move || error.get().map(|message| view! {
                <p class="card-error" data-state="error">{message}</p>
            })}
            <button class="card-action" prop:disabled=move || submitting.get() on:click=submit>
                {move || if submitting.get() { "Resuming\u{2026}" } else { "Raise limit and resume" }}
            </button>
        </article>
    }
}

/// A run needing reconciliation: the recorded write intent as evidence and one
/// question, whether the call reached the provider. Submitting records the
/// observed outcome via resolve; it never retries the write.
///
/// The intent is not in the folded status the list returns (that is only
/// `{ "state": "needs_reconciliation" }`), so this card fetches
/// `GET /v1/runs/{id}` for the dangling `pending` write when it mounts.
#[component]
fn ReconciliationCard(run_id: String, reload: RwSignal<u32>) -> impl IntoView {
    let config = expect_context::<Config>();
    let intent = RwSignal::new(Load::<Option<ApiPending>>::Loading);
    let output = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let submitting = RwSignal::new(false);

    let run_id = StoredValue::new(run_id);
    let config = StoredValue::new(config);

    // Fetch the recorded write intent once on mount.
    {
        let config = config.get_value();
        let id = run_id.get_value();
        spawn_local(async move {
            match api::get_run(&config, &id).await {
                Ok(detail) => intent.set(Load::Ready(detail.pending)),
                Err(err) => intent.set(Load::Failed(err.to_string())),
            }
        });
    }

    let submit = move |_| {
        let observed = match serde_json::from_str::<Value>(&output.get_untracked()) {
            Ok(value) => value,
            Err(err) => {
                error.set(Some(format!(
                    "the observed output must be valid JSON: {err}"
                )));
                return;
            }
        };
        error.set(None);
        submitting.set(true);
        let config = config.get_value();
        let id = run_id.get_value();
        spawn_local(async move {
            match api::resolve_run(&config, &id, observed).await {
                Ok(_) => reload.update(|n| *n += 1),
                Err(err) => {
                    error.set(Some(err.to_string()));
                    submitting.set(false);
                }
            }
        });
    };

    view! {
        <article class="inbox-card" data-kind="reconciliation" data-group="waiting-on-human">
            <header class="card-header">
                <span class="card-kind">"Needs reconciliation"</span>
                <RunRef id=run_id.get_value() />
            </header>
            {move || match intent.get() {
                Load::Loading => view! {
                    <p class="view-state" data-state="loading">"Loading the recorded write\u{2026}"</p>
                }.into_any(),
                Load::Failed(err) => view! {
                    <p class="view-state" data-state="error">{format!("Could not load the write intent: {err}")}</p>
                }.into_any(),
                Load::Ready(pending) => reconciliation_evidence(pending).into_any(),
            }}
            <p class="card-question">
                "Did this write reach the provider? Verify externally, then record the outcome the tool returned. This records a completion; it never retries the write."
            </p>
            <label class="form-field">
                <span>"Observed output (JSON)"</span>
                <textarea
                    class="raw-json"
                    prop:value=move || output.get()
                    on:input=move |ev| output.set(event_target_value(&ev))
                ></textarea>
            </label>
            {move || error.get().map(|message| view! {
                <p class="card-error" data-state="error">{message}</p>
            })}
            <button class="card-action" prop:disabled=move || submitting.get() on:click=submit>
                {move || if submitting.get() { "Recording\u{2026}" } else { "Record outcome" }}
            </button>
        </article>
    }
}

/// Renders the recorded write intent as evidence, or a note when the pending
/// call is unexpectedly absent or not a tool write.
fn reconciliation_evidence(pending: Option<ApiPending>) -> AnyView {
    match pending {
        Some(ApiPending::Tool {
            tool,
            input,
            idempotency_key,
            seq,
            ..
        }) => {
            let key = idempotency_key.unwrap_or_else(|| "none".to_string());
            view! {
                <dl class="card-detail">
                    <dt>"Tool"</dt>
                    <dd>{tool}</dd>
                    <dt>"Recorded at seq"</dt>
                    <dd>{seq.to_string()}</dd>
                    <dt>"Idempotency key"</dt>
                    <dd>{key}</dd>
                    <dt>"Recorded input"</dt>
                    <dd><pre>{pretty(&input)}</pre></dd>
                </dl>
            }
            .into_any()
        }
        _ => view! {
            <p class="view-state" data-state="empty">
                "No dangling write intent is recorded for this run."
            </p>
        }
        .into_any(),
    }
}

// -------------------------------------------------------------------------
// Schema-to-form (used by the suspension card)
// -------------------------------------------------------------------------

/// A form field a JSON Schema property maps to.
#[derive(Clone, Debug, PartialEq)]
struct SchemaField {
    /// The property name.
    name: String,
    /// Which scalar type it is.
    ty: FieldType,
    /// Whether the schema lists it in `required`.
    required: bool,
}

/// The scalar JSON types the generated form supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldType {
    /// A JSON string.
    Text,
    /// A JSON number.
    Number,
    /// A JSON integer.
    Integer,
    /// A JSON boolean.
    Boolean,
}

/// Turns an object JSON Schema into a flat list of scalar fields, or `None` when
/// the schema is not a simple object of scalars (the caller then falls back to a
/// raw-JSON textarea so a resume still works).
///
/// Deliberately small: it handles `type: object` with scalar `properties`
/// (string, number, integer, boolean) and the `required` list. Nested objects,
/// arrays, enums, and anything else return `None` rather than a lossy form.
fn schema_fields(schema: &Value) -> Option<Vec<SchemaField>> {
    let object = schema.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    let properties = object.get("properties")?.as_object()?;
    let required: Vec<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut fields = Vec::with_capacity(properties.len());
    for (name, spec) in properties {
        let ty = match spec.get("type").and_then(Value::as_str) {
            Some("string") => FieldType::Text,
            Some("number") => FieldType::Number,
            Some("integer") => FieldType::Integer,
            Some("boolean") => FieldType::Boolean,
            // Any unsupported property makes the whole form fall back to raw JSON.
            _ => return None,
        };
        fields.push(SchemaField {
            name: name.clone(),
            ty,
            required: required.contains(&name.as_str()),
        });
    }
    Some(fields)
}

/// Coerces one field's raw text into the JSON value its type calls for.
fn coerce_field(ty: FieldType, raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    match ty {
        FieldType::Text => Ok(Value::String(raw.to_string())),
        FieldType::Number => raw
            .parse::<f64>()
            .map(|number| json!(number))
            .map_err(|_| format!("expected a number, got '{raw}'")),
        FieldType::Integer => raw
            .parse::<i64>()
            .map(|number| json!(number))
            .map_err(|_| format!("expected a whole number, got '{raw}'")),
        FieldType::Boolean => match raw {
            "true" => Ok(json!(true)),
            "false" | "" => Ok(json!(false)),
            other => Err(format!("expected true or false, got '{other}'")),
        },
    }
}

/// Builds the resume input object from the field specs and the collected raw
/// values. An empty required field is an error; an empty optional field is left
/// out; a boolean defaults to `false` when never toggled.
fn build_input(fields: &[SchemaField], values: &BTreeMap<String, String>) -> Result<Value, String> {
    let mut object = serde_json::Map::new();
    for field in fields {
        let raw = values.get(&field.name).map(String::as_str).unwrap_or("");
        let value = match field.ty {
            FieldType::Boolean => coerce_field(FieldType::Boolean, raw)?,
            _ if raw.trim().is_empty() => {
                if field.required {
                    return Err(format!("'{}' is required", field.name));
                }
                continue;
            }
            _ => coerce_field(field.ty, raw)?,
        };
        object.insert(field.name.clone(), value);
    }
    Ok(Value::Object(object))
}

/// Renders one input for a schema field, writing its raw text into `values`.
fn schema_field_input(field: SchemaField, values: RwSignal<BTreeMap<String, String>>) -> AnyView {
    let name = field.name.clone();
    let label = if field.required {
        format!("{name} *")
    } else {
        name.clone()
    };
    match field.ty {
        FieldType::Boolean => {
            let key = name.clone();
            view! {
                <label class="form-field form-field-bool">
                    <input
                        type="checkbox"
                        on:change=move |ev| {
                            let checked = event_target_checked(&ev);
                            values.update(|map| {
                                map.insert(key.clone(), checked.to_string());
                            });
                        }
                    />
                    <span>{label}</span>
                </label>
            }
            .into_any()
        }
        other => {
            let key = name.clone();
            let input_type = if other == FieldType::Text {
                "text"
            } else {
                "number"
            };
            view! {
                <label class="form-field">
                    <span>{label}</span>
                    <input
                        type=input_type
                        on:input=move |ev| {
                            let text = event_target_value(&ev);
                            values.update(|map| {
                                map.insert(key.clone(), text);
                            });
                        }
                    />
                </label>
            }
            .into_any()
        }
    }
}

/// The `extend` key one budget dimension maps to in the resume payload. These
/// four keys are the only ones the runtime's extension parser accepts.
const fn extend_key_for(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Steps => "steps",
        BudgetKind::Tokens => "tokens",
        BudgetKind::CostUsd => "cost_usd",
        BudgetKind::WallTime => "wall_time_seconds",
    }
}

/// Builds the budget-extension resume input, `{"extend": {<key>: <amount>}}`.
/// Steps and tokens take a whole number; cost and wall time take any number.
fn build_extension(kind: BudgetKind, raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    let label = budget_kind_label(kind);
    let amount = match kind {
        BudgetKind::Steps | BudgetKind::Tokens => raw
            .parse::<u64>()
            .map(|number| json!(number))
            .map_err(|_| format!("enter a whole number of extra {label} to grant"))?,
        BudgetKind::CostUsd | BudgetKind::WallTime => raw
            .parse::<f64>()
            .map(|number| json!(number))
            .map_err(|_| format!("enter a number of extra {label} to grant"))?,
    };
    Ok(json!({ "extend": { extend_key_for(kind): amount } }))
}

/// A human label for a budget dimension.
const fn budget_kind_label(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Tokens => "tokens",
        BudgetKind::CostUsd => "cost (USD)",
        BudgetKind::WallTime => "wall-time seconds",
        BudgetKind::Steps => "steps",
    }
}

// -------------------------------------------------------------------------
// Spend (route `/spend`)
// -------------------------------------------------------------------------

/// Spend overview: per-run burn-down and aggregates by agent and by day.
#[component]
pub fn Spend() -> impl IntoView {
    let config = expect_context::<Config>();
    let runs = RwSignal::new(Load::<Vec<RunSummary>>::Loading);
    let reload = RwSignal::new(0u32);

    Effect::new(move |_| {
        reload.get();
        let config = config.clone();
        runs.set(Load::Loading);
        spawn_local(async move {
            match api::list_runs(&config).await {
                Ok(list) => runs.set(Load::Ready(list)),
                Err(err) => runs.set(Load::Failed(err.to_string())),
            }
        });
    });

    view! {
        <section class="spend">
            <header class="view-header">
                <h1>"Spend"</h1>
                <button class="reload" on:click=move |_| reload.update(|n| *n += 1)>"Reload"</button>
            </header>
            <p class="estimate-note">
                "Tokens are exact, read from each run's recorded log. Dollars are an estimate from a static price table; a run that used a model not in the table shows tokens only. A declared budget ceiling is shown only when the run actually crossed one, because ceilings live in the agent definition, not the log."
            </p>
            {move || match runs.get() {
                Load::Loading => view! {
                    <p class="view-state" data-state="loading">"Loading runs\u{2026}"</p>
                }.into_any(),
                Load::Failed(err) => view! {
                    <p class="view-state" data-state="error">{format!("Could not load runs: {err}")}</p>
                }.into_any(),
                Load::Ready(list) if list.is_empty() => view! {
                    <p class="view-state" data-state="empty">"No runs yet, so there is nothing to total."</p>
                }.into_any(),
                Load::Ready(list) => view! { <SpendBody runs=list /> }.into_any(),
            }}
        </section>
    }
}

/// Opens one folding stream per run, then renders the burn-down and the
/// aggregates over them.
///
/// Cost needs the per-call model id, which only the event log carries, so this
/// reuses [`connect_run_stream`] per run exactly as the inspector does and folds
/// each log with [`pricing::estimate_cost`]. That is a stream per run; it is the
/// honest price of a real dollar estimate, and terminal runs close their stream
/// after the replay. This is an overview, not an analytics product.
#[component]
fn SpendBody(runs: Vec<RunSummary>) -> impl IntoView {
    let config = expect_context::<Config>();
    let streams = runs
        .into_iter()
        .map(|run| {
            let stream = connect_run_stream(config.clone(), run.run.clone());
            (run, stream)
        })
        .collect::<Vec<_>>();
    let streams = StoredValue::new(streams);
    view! {
        <BurnDown streams=streams />
        <Aggregate streams=streams />
    }
}

/// Per-run cost folded from one run's log. Tokens exact, dollars an estimate,
/// ceiling known only when a crossing recorded it.
#[derive(Clone, Debug, PartialEq)]
struct RunCost {
    /// The run id.
    run_id: String,
    /// The agent-definition hash from `RunStarted`, when present.
    agent: Option<String>,
    /// The calendar day of the first recorded event (`YYYY-MM-DD`).
    day: Option<String>,
    /// Exact accumulated token totals from the real fold.
    usage: TokenTotals,
    /// The dollar estimate and its priced/unpriced call counts.
    cost: CostSummary,
    /// The crossed budget, when a `BudgetExceeded` event recorded one.
    ceiling: Option<Budget>,
}

/// Folds one run's log into its [`RunCost`]. Pure over the log, so it is unit
/// tested directly from wire frames.
fn run_cost(run_id: &str, log: &[EventEnvelope]) -> RunCost {
    let usage = head_state(log).usage;
    let cost = pricing::estimate_cost(log);
    let agent = log.iter().find_map(|env| match &env.event {
        Event::RunStarted { agent_def_hash, .. } => Some(agent_def_hash.clone()),
        _ => None,
    });
    let day = log.first().map(|env| {
        let dt = env.recorded_at;
        format!(
            "{:04}-{:02}-{:02}",
            dt.year(),
            u8::from(dt.month()),
            dt.day()
        )
    });
    let ceiling = log.iter().find_map(|env| match &env.event {
        Event::BudgetExceeded { budget, .. } => Some(*budget),
        _ => None,
    });
    RunCost {
        run_id: run_id.to_string(),
        agent,
        day,
        usage,
        cost,
        ceiling,
    }
}

/// Per-run spend against a known ceiling.
#[component]
fn BurnDown(streams: StoredValue<Vec<(RunSummary, RunStream)>>) -> impl IntoView {
    let rows = streams.with_value(|list| {
        list.iter()
            .map(|(run, stream)| {
                view! {
                    <BurnRow
                        run_id=run.run.clone()
                        status=run.status.to_run_status()
                        stream=*stream
                    />
                }
            })
            .collect::<Vec<_>>()
    });
    view! {
        <section class="burn-down">
            <h2>"Per-run spend"</h2>
            <table class="burn-table">
                <thead>
                    <tr>
                        <th>"Run"</th>
                        <th>"Status"</th>
                        <th>"Tokens (exact)"</th>
                        <th>"Cost (estimate)"</th>
                        <th>"Ceiling"</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
        </section>
    }
}

/// One burn-down row, folding its own stream reactively as events arrive.
#[component]
fn BurnRow(run_id: String, status: RunStatus, stream: RunStream) -> impl IntoView {
    let id_for_fold = run_id.clone();
    // A memo (Copy) so every cell can read the same folded cost; it recomputes
    // as the run's stream appends events.
    let cost = Memo::new(move |_| stream.events.with(|log| run_cost(&id_for_fold, log)));
    view! {
        <tr class="burn-row">
            <td><RunRef id=run_id /></td>
            <td><StatusBadge status=status /></td>
            <td>{move || {
                let c = cost.get();
                format!("{} in / {} out", c.usage.input_tokens, c.usage.output_tokens)
            }}</td>
            <td>{move || {
                let c = cost.get();
                if c.cost.is_available() {
                    format!("~${:.4}", c.cost.usd)
                } else {
                    "tokens only (unknown model)".to_string()
                }
            }}</td>
            <td>{move || match cost.get().ceiling {
                Some(budget) => format!("{} {} (crossed)", budget.limit, budget_kind_label(budget.kind)),
                None => "unknown (not in log)".to_string(),
            }}</td>
        </tr>
    }
}

/// Spend totalled by agent and by day.
#[component]
fn Aggregate(streams: StoredValue<Vec<(RunSummary, RunStream)>>) -> impl IntoView {
    // Reading every stream's events subscribes to all of them, so the totals
    // recompute as any run's log grows.
    let costs = move || {
        streams.with_value(|list| {
            list.iter()
                .map(|(run, stream)| stream.events.with(|log| run_cost(&run.run, log)))
                .collect::<Vec<_>>()
        })
    };
    view! {
        <section class="aggregate">
            <h2>"Totals"</h2>
            <div class="aggregate-tables">
                <div class="aggregate-block">
                    <h3>"By agent"</h3>
                    <AggregateTable
                        first_column="Agent"
                        rows=Signal::derive(move || aggregate_by_agent(&costs()))
                    />
                </div>
                <div class="aggregate-block">
                    <h3>"By day"</h3>
                    <AggregateTable
                        first_column="Day"
                        rows=Signal::derive(move || aggregate_by_day(&costs()))
                    />
                </div>
            </div>
        </section>
    }
}

/// One aggregate table, shared by the by-agent and by-day breakdowns.
#[component]
fn AggregateTable(first_column: &'static str, rows: Signal<Vec<AggRow>>) -> impl IntoView {
    view! {
        <table class="aggregate-table">
            <thead>
                <tr>
                    <th>{first_column}</th>
                    <th>"Runs"</th>
                    <th>"Tokens (exact)"</th>
                    <th>"Cost (estimate)"</th>
                </tr>
            </thead>
            <tbody>
                {move || rows.get().into_iter().map(|row| view! {
                    <tr class="aggregate-row">
                        <td>{row.key}</td>
                        <td>{row.runs.to_string()}</td>
                        <td>{format!("{} in / {} out", row.input_tokens, row.output_tokens)}</td>
                        <td>{
                            if row.usd_complete {
                                format!("~${:.4}", row.usd)
                            } else {
                                format!("~${:.4} (partial; some runs unpriced)", row.usd)
                            }
                        }</td>
                    </tr>
                }).collect::<Vec<_>>()}
            </tbody>
        </table>
    }
}

/// One aggregated line: a key (agent or day) and its summed spend.
#[derive(Clone, Debug, PartialEq)]
struct AggRow {
    /// The group key (agent hash, truncated, or a day).
    key: String,
    /// How many runs fell in this group.
    runs: u32,
    /// Summed exact input tokens.
    input_tokens: u64,
    /// Summed exact output tokens.
    output_tokens: u64,
    /// Summed estimated dollars over the runs that could be priced.
    usd: f64,
    /// True when every run in the group was fully priced (else `usd` is partial).
    usd_complete: bool,
}

/// Totals spend by agent (unknown agents grouped under one key), sorted by key.
fn aggregate_by_agent(costs: &[RunCost]) -> Vec<AggRow> {
    aggregate_by(costs, |cost| {
        cost.agent
            .as_deref()
            .map(|hash| truncate_id(hash))
            .unwrap_or_else(|| "unknown agent".to_string())
    })
}

/// Totals spend by calendar day (undated runs grouped under one key), sorted by
/// key so days read in order.
fn aggregate_by_day(costs: &[RunCost]) -> Vec<AggRow> {
    aggregate_by(costs, |cost| {
        cost.day.clone().unwrap_or_else(|| "undated".to_string())
    })
}

/// Groups run costs by a key function and sums each group. Shared by the two
/// breakdowns; pure, so both are unit tested.
fn aggregate_by(costs: &[RunCost], key_of: impl Fn(&RunCost) -> String) -> Vec<AggRow> {
    let mut groups: BTreeMap<String, AggRow> = BTreeMap::new();
    for cost in costs {
        let key = key_of(cost);
        let row = groups.entry(key.clone()).or_insert(AggRow {
            key,
            runs: 0,
            input_tokens: 0,
            output_tokens: 0,
            usd: 0.0,
            usd_complete: true,
        });
        row.runs += 1;
        row.input_tokens = row.input_tokens.saturating_add(cost.usage.input_tokens);
        row.output_tokens = row.output_tokens.saturating_add(cost.usage.output_tokens);
        row.usd += cost.cost.usd;
        // One unpriced call in any run makes the group's dollar figure partial.
        row.usd_complete = row.usd_complete && cost.cost.is_available();
    }
    groups.into_values().collect()
}

// -------------------------------------------------------------------------
// Inspector delegation and fallback (unchanged)
// -------------------------------------------------------------------------

/// Run inspector, route `/runs/:id`. Delegates to
/// [`RunInspector`](crate::inspector::RunInspector), which reads the `:id`
/// param, opens the run's stream, and renders the header, timeline, and scrubber.
#[component]
pub fn Inspector() -> impl IntoView {
    view! { <RunInspector /> }
}

/// Fallback for an unmatched route.
#[component]
pub fn NotFound() -> impl IntoView {
    view! { <p class="view-placeholder">"Not found"</p> }
}

/// Parses an RFC 3339 timestamp and renders a compact age against the browser
/// clock. Browser-only (it reads `Date.now()`); the pure formatting it defers to
/// [`format_age_secs`] is what the tests cover.
fn age_from_rfc3339(recorded_at: &str) -> String {
    let then_ms = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(recorded_at)).get_time();
    if then_ms.is_nan() {
        return "\u{2014}".to_string();
    }
    let now_ms = js_sys::Date::now();
    format_age_secs((((now_ms - then_ms) / 1000.0).max(0.0)) as i64)
}

/// A compact human age from a whole number of seconds. Pure, so it is tested off
/// the wall clock.
fn format_age_secs(mut secs: i64) -> String {
    secs = secs.max(0);
    let days = secs / 86_400;
    secs %= 86_400;
    let hours = secs / 3_600;
    secs %= 3_600;
    let minutes = secs / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Pretty-prints a JSON payload for a `<pre>` block.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(frames: &[&str]) -> Vec<EventEnvelope> {
        frames
            .iter()
            .map(|frame| serde_json::from_str(frame).expect("frame is valid envelope JSON"))
            .collect()
    }

    const STARTED: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":0,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"RunStarted","payload":{"agent_def_hash":"sha256:agentone","input":{}}}}"#;
    const MODEL_DONE: &str = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":1,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ModelCallCompleted","payload":{"seq":1,"response":{"model":"claude-opus-4-8"},"usage":{"input_tokens":1000000,"output_tokens":1000000}}}}"#;

    fn summary(run: &str, status: ApiStatus, last: &str) -> RunSummary {
        RunSummary {
            run: run.to_string(),
            status,
            event_count: 1,
            first_recorded_at: last.to_string(),
            last_recorded_at: last.to_string(),
        }
    }

    #[test]
    fn attention_states_sort_to_the_top() {
        let mut runs = vec![
            summary(
                "done",
                ApiStatus::Completed {
                    output: json!(null),
                },
                "2026-07-09T10:00:00Z",
            ),
            summary(
                "parked",
                ApiStatus::Suspended {
                    reason: "hold".into(),
                    input_schema: json!({}),
                },
                "2026-07-09T09:00:00Z",
            ),
            summary("busy", ApiStatus::Running, "2026-07-09T11:00:00Z"),
        ];
        sort_for_attention(&mut runs);
        assert_eq!(runs[0].run, "parked", "waiting-on-human sorts first");
        assert_eq!(runs[1].run, "busy", "in-progress next");
        assert_eq!(runs[2].run, "done", "terminal last");
    }

    #[test]
    fn scalar_object_schema_becomes_fields() {
        let schema = json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer" },
                "ready": { "type": "boolean" }
            }
        });
        let fields = schema_fields(&schema).expect("a scalar object is renderable");
        assert_eq!(fields.len(), 3);
        let name = fields.iter().find(|f| f.name == "name").unwrap();
        assert_eq!(name.ty, FieldType::Text);
        assert!(name.required);
        assert!(!fields.iter().find(|f| f.name == "count").unwrap().required);
    }

    #[test]
    fn nested_or_unsupported_schema_falls_back_to_raw() {
        // A nested object is not a scalar field, so the whole form falls back.
        let nested = json!({
            "type": "object",
            "properties": { "child": { "type": "object" } }
        });
        assert!(schema_fields(&nested).is_none());
        // A non-object schema also falls back.
        assert!(schema_fields(&json!({ "type": "array" })).is_none());
    }

    #[test]
    fn build_input_coerces_and_enforces_required() {
        let fields = vec![
            SchemaField {
                name: "name".into(),
                ty: FieldType::Text,
                required: true,
            },
            SchemaField {
                name: "count".into(),
                ty: FieldType::Integer,
                required: false,
            },
            SchemaField {
                name: "ready".into(),
                ty: FieldType::Boolean,
                required: false,
            },
        ];
        let mut values = BTreeMap::new();
        values.insert("name".to_string(), "otter".to_string());
        values.insert("count".to_string(), "3".to_string());
        // `ready` never toggled -> defaults to false.
        let input = build_input(&fields, &values).expect("valid input");
        assert_eq!(
            input,
            json!({ "name": "otter", "count": 3, "ready": false })
        );

        // Missing the required `name` is an error.
        let empty = BTreeMap::new();
        assert!(build_input(&fields, &empty).is_err());
    }

    #[test]
    fn budget_extension_uses_the_right_key_and_type() {
        // Steps takes a whole number under the "steps" key.
        assert_eq!(
            build_extension(BudgetKind::Steps, "100").unwrap(),
            json!({ "extend": { "steps": 100 } })
        );
        // Wall time takes a number under "wall_time_seconds".
        assert_eq!(
            build_extension(BudgetKind::WallTime, "600").unwrap(),
            json!({ "extend": { "wall_time_seconds": 600.0 } })
        );
        // Cost takes a number under "cost_usd".
        assert_eq!(
            build_extension(BudgetKind::CostUsd, "1.5").unwrap(),
            json!({ "extend": { "cost_usd": 1.5 } })
        );
        // A non-numeric amount is rejected.
        assert!(build_extension(BudgetKind::Tokens, "lots").is_err());
    }

    #[test]
    fn run_cost_folds_exact_tokens_and_estimated_dollars() {
        let cost = run_cost("r1", &log(&[STARTED, MODEL_DONE]));
        assert_eq!(cost.usage.input_tokens, 1_000_000);
        assert_eq!(cost.usage.output_tokens, 1_000_000);
        // 1M in * $5 + 1M out * $25 = $30 for opus-4-8.
        assert!(cost.cost.is_available());
        assert_eq!(cost.cost.usd, 30.0);
        assert_eq!(cost.agent.as_deref(), Some("sha256:agentone"));
        assert_eq!(cost.day.as_deref(), Some("2026-07-09"));
        assert!(
            cost.ceiling.is_none(),
            "no crossing recorded, so no ceiling"
        );
    }

    #[test]
    fn aggregate_groups_and_flags_partial_estimates() {
        let priced = run_cost("r1", &log(&[STARTED, MODEL_DONE]));
        // A second run under the same agent with an unpriced call.
        let unpriced_frame = r#"{"run_id":"00000000-0000-4000-8000-000000000009","seq":1,"schema_version":1,"recorded_at":"2026-07-09T12:00:00Z","event":{"kind":"ModelCallCompleted","payload":{"seq":1,"response":{"text":"hi"},"usage":{"input_tokens":10,"output_tokens":10}}}}"#;
        let partial = run_cost("r2", &log(&[STARTED, unpriced_frame]));

        let rows = aggregate_by_agent(&[priced, partial]);
        assert_eq!(rows.len(), 1, "both runs share one agent");
        let row = &rows[0];
        assert_eq!(row.runs, 2);
        assert_eq!(row.input_tokens, 1_000_010);
        assert!(
            !row.usd_complete,
            "the unpriced call makes the total partial"
        );
    }

    #[test]
    fn age_formats_coarsely() {
        assert_eq!(format_age_secs(45), "45s");
        assert_eq!(format_age_secs(3 * 60 + 20), "3m 20s");
        assert_eq!(format_age_secs(2 * 3600 + 5 * 60), "2h 5m");
        assert_eq!(format_age_secs(3 * 86_400 + 4 * 3600), "3d 4h");
    }
}

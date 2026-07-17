//! The agent registry's `name` plumbing: `POST /v1/agents` carries a built
//! agent's `Agent::name()` into the stored [`RegisteredAgent`], and
//! `GET /v1/agents`/`GET /v1/agents/{hash}` hand it back — present only when
//! the definition actually declared one, omitted (never `null`) otherwise.
//! The bound on `name` itself (at most 64 characters, not empty/all
//! whitespace) is enforced where the definition is parsed
//! (`salvor_cli::agent_config::AgentConfig::validate`, pinned in that crate's
//! own tests); `salvor serve`'s `AgentFactory` runs exactly that parse-and-
//! validate path on every submitted definition, which is what makes the
//! bound a server-enforced one and not merely a client-side courtesy. What
//! this file pins is the registry/response plumbing downstream of a build
//! that already carries (or does not carry) a name — a fixed in-process
//! factory stands in for the real parser, the same stand-in pattern
//! `common::agent_factory` already uses for every other test in this suite.

mod common;

use common::{TestServer, app_state, get_json, memory_store, register_agent, sample_toml};
use reqwest::StatusCode;
use salvor_llm::Config;
use salvor_runtime::Agent;
use salvor_server::{AgentFactory, BuiltAgent};
use std::sync::Arc;

/// A factory that always builds the same named agent, regardless of the
/// submitted definition — the same "submission is opaque to the factory"
/// pattern `common::agent_factory` uses, extended to carry a name.
fn named_agent_factory(name: &'static str) -> AgentFactory {
    Arc::new(move |_definition| {
        Box::pin(async move {
            let agent = Agent::builder()
                .model(Config::new(), "test-model")
                .name(name)
                .build()
                .map_err(|error| error.to_string())?;
            Ok(BuiltAgent {
                agent,
                servers: vec![],
            })
        })
    })
}

/// A factory that always builds the same UNNAMED agent.
fn unnamed_agent_factory() -> AgentFactory {
    Arc::new(move |_definition| {
        Box::pin(async move {
            let agent = Agent::builder()
                .model(Config::new(), "test-model")
                .build()
                .map_err(|error| error.to_string())?;
            Ok(BuiltAgent {
                agent,
                servers: vec![],
            })
        })
    })
}

/// A registered agent whose build carried a name: `GET /v1/agents/{hash}`
/// and `GET /v1/agents` both hand it back.
#[tokio::test]
async fn get_and_list_return_the_name_when_the_build_carries_one() {
    let server = TestServer::spawn(app_state(
        memory_store(),
        named_agent_factory("support-triage"),
    ))
    .await;
    let client = reqwest::Client::new();
    let hash = register_agent(&client, &server.base, sample_toml(), None).await;

    let (status, body) =
        get_json(&client, &format!("{}/v1/agents/{hash}", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["agent"], hash);
    assert_eq!(body["name"], "support-triage");

    let (status, body) = get_json(&client, &format!("{}/v1/agents", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    let agents = body["agents"].as_array().expect("agents array");
    let entry = agents
        .iter()
        .find(|entry| entry["agent"] == hash)
        .expect("the registered agent is listed");
    assert_eq!(entry["name"], "support-triage");
}

/// A registered agent whose build carried no name omits the field entirely
/// from both endpoints — never `"name": null`, the same honest-absence rule
/// `GET /v1/runs` already applies to `agent_def_hash` and `labels`.
#[tokio::test]
async fn get_and_list_omit_name_when_the_build_carries_none() {
    let server = TestServer::spawn(app_state(memory_store(), unnamed_agent_factory())).await;
    let client = reqwest::Client::new();
    let hash = register_agent(&client, &server.base, sample_toml(), None).await;

    let (status, body) =
        get_json(&client, &format!("{}/v1/agents/{hash}", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["agent"], hash);
    assert!(
        body.as_object().expect("object").get("name").is_none(),
        "no name key at all, not null: {body}"
    );

    let (status, body) = get_json(&client, &format!("{}/v1/agents", server.base), None).await;
    assert_eq!(status, StatusCode::OK);
    let agents = body["agents"].as_array().expect("agents array");
    let entry = agents
        .iter()
        .find(|entry| entry["agent"] == hash)
        .expect("the registered agent is listed");
    assert!(
        entry.as_object().expect("object").get("name").is_none(),
        "no name key at all, not null: {entry}"
    );
}

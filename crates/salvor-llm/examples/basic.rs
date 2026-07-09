//! A minimal end-to-end use of the client.
//!
//! This example reads `ANTHROPIC_API_KEY` from the environment, asks the model
//! one question, and prints the answer. It is not run in CI or the test suite
//! because it makes a real network call; run it by hand with:
//!
//! ```text
//! ANTHROPIC_API_KEY=sk-... cargo run --example basic
//! ```
//!
//! To talk to a local model instead (LM Studio, Ollama), build the client from
//! a [`salvor_llm::Config`] with `with_base_url` and no API key.

use salvor_llm::{Client, Message, MessageRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Reads ANTHROPIC_API_KEY from the environment (the explicit opt-in).
    let client = Client::from_env()?;

    let request = MessageRequest::new("claude-opus-4-8", 1024)
        .with_system("You are a concise Rust tutor.")
        .push_message(Message::user("What does the `?` operator do in Rust?"));

    let response = client.send_message(&request).await?;

    println!("{}", response.text());
    println!(
        "\n[tokens: {} in, {} out]",
        response.usage.input_tokens, response.usage.output_tokens
    );

    Ok(())
}

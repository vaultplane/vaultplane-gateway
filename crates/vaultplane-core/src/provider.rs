//! Provider connector contract.
//!
//! Every upstream model provider family (OpenAI and OpenAI-compatible self-hosted,
//! Anthropic, Azure OpenAI, AWS Bedrock) is reached through a single trait with one
//! implementation per family. Adding a provider should touch only this trait, its
//! implementation, and the model registry.

use crate::error::Result;

/// A normalized chat request flowing through the gateway. Placeholder shape.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// The virtual model name the caller requested.
    pub model: String,
    /// The raw request body, in the OpenAI Chat Completions schema.
    pub body: Vec<u8>,
}

/// A normalized chat response. Placeholder shape.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// HTTP status returned to the client.
    pub status: u16,
    /// The raw response body, in the OpenAI Chat Completions schema.
    pub body: Vec<u8>,
}

/// The contract every provider connector implements.
///
/// This will become an async trait once the runtime is wired in. It is defined
/// synchronously for now so the shape is visible without committing runtime details.
pub trait Connector: Send + Sync {
    /// A stable identifier for the provider family, for example `openai`.
    fn name(&self) -> &str;

    /// Forward a chat completion request to the upstream provider.
    fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
}

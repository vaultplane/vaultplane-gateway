//! Provider connector contract.
//!
//! Every upstream model provider family (OpenAI and OpenAI-compatible self-hosted,
//! Anthropic, Azure OpenAI, AWS Bedrock) is reached through a single trait, with one
//! implementation per family. Adding a provider should touch only this trait, its
//! implementation, and the model registry. Responses pass through as byte streams so
//! the gateway never buffers a full upstream response.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::error::Result;

pub mod openai;

/// A streaming response body: a sequence of byte chunks.
pub type BodyStream = BoxStream<'static, std::result::Result<Bytes, std::io::Error>>;

/// A chat completion request flowing through the gateway.
pub struct ChatRequest {
    /// The model the caller requested.
    pub model: String,
    /// Whether the caller asked for a streamed (SSE) response.
    pub stream: bool,
    /// The raw request body, in the OpenAI Chat Completions schema.
    pub body: Bytes,
}

/// A chat completion response from an upstream provider.
pub struct ChatResponse {
    /// HTTP status returned by the upstream provider.
    pub status: u16,
    /// The upstream `Content-Type`, forwarded to the client.
    pub content_type: Option<String>,
    /// The response body, streamed without buffering.
    pub body: BodyStream,
}

/// The contract every provider connector implements.
#[async_trait]
pub trait Connector: Send + Sync {
    /// A stable identifier for the provider family, for example `openai`.
    fn name(&self) -> &str;

    /// Forward a chat completion request to the upstream provider and return its
    /// response as a stream, suitable for both buffered and streamed replies.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
}

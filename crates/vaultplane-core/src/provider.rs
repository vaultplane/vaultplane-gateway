// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Provider connector contract.
//!
//! Every upstream model provider family (OpenAI and OpenAI-compatible self-hosted,
//! Anthropic, Azure OpenAI, AWS Bedrock) is reached through a single trait, with one
//! implementation per family. Adding a provider should touch only this trait, its
//! implementation, and the model registry. Responses pass through as byte streams so
//! the gateway never buffers a full upstream response.

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{self, BoxStream};

use crate::error::{Error, Result};

pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod openai;
pub mod registry;
pub(crate) mod sigv4;

/// A streaming response body: a sequence of byte chunks.
pub type BodyStream = BoxStream<'static, std::result::Result<Bytes, std::io::Error>>;

/// Wrap a complete, in-memory body as a single-chunk stream.
pub(crate) fn single_chunk(bytes: Vec<u8>) -> BodyStream {
    stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(bytes)) }).boxed()
}

/// Extract token usage from an OpenAI-shaped response body.
///
/// Returns `None` if the body is not JSON or does not contain a `usage` object with
/// at least a `prompt_tokens` number. Embeddings responses omit `completion_tokens`,
/// so it defaults to zero when absent. Shared by the OpenAI and Azure connectors,
/// which speak the same response schema for both chat and embeddings.
pub(crate) fn parse_openai_usage(body: &[u8]) -> Option<Usage> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = value.get("usage")?;
    let prompt_tokens = usage.get("prompt_tokens")?.as_u64()? as u32;
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens,
        completion_tokens,
    })
}

/// A chat completion request flowing through the gateway.
pub struct ChatRequest {
    /// The upstream model to send. The registry rewrites this per route.
    pub model: String,
    /// Whether the caller asked for a streamed (SSE) response.
    pub stream: bool,
    /// The raw request body, in the OpenAI Chat Completions schema.
    pub body: Bytes,
}

/// An embeddings request flowing through the gateway.
///
/// Embeddings are never streamed (the OpenAI API has no streaming variant), so
/// there is no `stream` field. The body is forwarded verbatim to the upstream
/// after the registry rewrites the `model` field.
pub struct EmbeddingsRequest {
    /// The upstream model to send. The registry rewrites this per route.
    pub model: String,
    /// The raw request body, in the OpenAI Embeddings schema.
    pub body: Bytes,
}

/// Token usage reported by an upstream provider.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    /// Tokens consumed by the prompt (input).
    pub prompt_tokens: u32,
    /// Tokens generated in the completion (output).
    pub completion_tokens: u32,
}

/// A chat completion response from an upstream provider.
pub struct ChatResponse {
    /// HTTP status returned by the upstream provider.
    pub status: u16,
    /// The upstream `Content-Type`, forwarded to the client.
    pub content_type: Option<String>,
    /// The response body, streamed without buffering.
    pub body: BodyStream,
    /// The provider family that served the request (`openai`, `anthropic`,
    /// `azure`, `bedrock`).
    pub provider: String,
    /// The upstream model the request was sent to.
    pub model: String,
    /// Token usage, when the connector parses the response body.
    pub usage: Option<Usage>,
    /// Number of provider attempts made to produce this response (1 if no failover).
    pub attempts: u32,
}

/// The contract every provider connector implements.
///
/// The [`embeddings`](Connector::embeddings) method defaults to "not supported"
/// so providers without an embeddings endpoint (Anthropic Messages, AWS
/// Bedrock today) inherit a clean error without writing a stub. Providers
/// that do support embeddings (OpenAI, Azure OpenAI) override the default.
#[async_trait]
pub trait Connector: Send + Sync {
    /// A stable identifier for the provider family, for example `openai`.
    fn name(&self) -> &str;

    /// Forward a chat completion request to the upstream provider and return its
    /// response as a stream, suitable for both buffered and streamed replies.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Forward an embeddings request to the upstream provider. Defaults to a
    /// "not supported" error for providers that do not offer embeddings.
    async fn embeddings(&self, _request: EmbeddingsRequest) -> Result<ChatResponse> {
        Err(Error::Provider(format!(
            "provider '{}' does not support embeddings",
            self.name()
        )))
    }
}

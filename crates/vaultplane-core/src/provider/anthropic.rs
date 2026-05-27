//! Anthropic provider connector.
//!
//! Translates between the OpenAI Chat Completions schema (the gateway's wire format)
//! and the Anthropic Messages schema. System messages are hoisted to Anthropic's
//! top-level `system` field, `max_tokens` is supplied (Anthropic requires it), and
//! the response, finish reason, and token usage are mapped back to the OpenAI shape.
//!
//! Streaming requests are not yet supported (they return 501). Multimodal (array)
//! message content is not yet supported.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::provider::{BodyStream, ChatRequest, ChatResponse, Connector};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Connector for the Anthropic Messages API.
pub struct AnthropicConnector {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicConnector {
    /// Build a connector for the given base URL and API key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::Provider(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            client,
        })
    }
}

// Incoming OpenAI Chat Completions request (the subset we read).
#[derive(Deserialize)]
struct OpenAiRequest {
    model: String,
    #[serde(default)]
    messages: Vec<OpenAiMessage>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    role: String,
    #[serde(default)]
    content: String,
}

// Outgoing Anthropic Messages request.
#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

// Incoming Anthropic Messages response (the subset we map).
#[derive(Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Translate an OpenAI Chat Completions request into an Anthropic Messages request.
fn to_anthropic_request(body: &[u8]) -> Result<AnthropicRequest> {
    let request: OpenAiRequest = serde_json::from_slice(body)
        .map_err(|e| Error::Provider(format!("invalid chat request: {e}")))?;

    let mut system = String::new();
    let mut messages = Vec::new();
    for message in request.messages {
        if message.role == "system" {
            if !system.is_empty() {
                system.push('\n');
            }
            system.push_str(&message.content);
        } else {
            messages.push(AnthropicMessage {
                role: message.role,
                content: message.content,
            });
        }
    }

    Ok(AnthropicRequest {
        model: request.model,
        messages,
        max_tokens: request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system: (!system.is_empty()).then_some(system),
        temperature: request.temperature,
    })
}

/// Map an Anthropic stop reason to an OpenAI finish reason.
fn finish_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("max_tokens") => "length",
        _ => "stop",
    }
}

/// Translate an Anthropic Messages response into an OpenAI Chat Completions response.
fn to_openai_response(body: &[u8]) -> Result<Vec<u8>> {
    let response: AnthropicResponse = serde_json::from_slice(body)
        .map_err(|e| Error::Provider(format!("invalid Anthropic response: {e}")))?;

    let content: String = response
        .content
        .iter()
        .filter(|block| block.kind == "text")
        .map(|block| block.text.as_str())
        .collect();

    let usage = response.usage.unwrap_or_default();
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let openai = json!({
        "id": response.id,
        "object": "chat.completion",
        "created": created,
        "model": response.model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish_reason(response.stop_reason.as_deref()),
        }],
        "usage": {
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens + usage.output_tokens,
        },
    });

    serde_json::to_vec(&openai)
        .map_err(|e| Error::Provider(format!("failed to encode response: {e}")))
}

/// Wrap a complete body as a single-chunk stream.
fn single_chunk(bytes: Vec<u8>) -> BodyStream {
    stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(bytes)) }).boxed()
}

#[async_trait]
impl Connector for AnthropicConnector {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        if request.stream {
            let body = json!({
                "error": { "message": "streaming is not yet supported for the Anthropic provider" }
            });
            return Ok(ChatResponse {
                status: 501,
                content_type: Some("application/json".to_string()),
                body: single_chunk(serde_json::to_vec(&body).unwrap_or_default()),
            });
        }

        let payload = serde_json::to_vec(&to_anthropic_request(&request.body)?)
            .map_err(|e| Error::Provider(format!("failed to encode request: {e}")))?;

        let url = format!("{}/v1/messages", self.base_url);
        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request to Anthropic failed: {e}")))?;

        let status = response.status().as_u16();
        let upstream_body = response
            .bytes()
            .await
            .map_err(|e| Error::Provider(format!("reading Anthropic response failed: {e}")))?;

        // Forward upstream errors as-is; only the success body needs translating.
        let body = if status == 200 {
            to_openai_response(&upstream_body)?
        } else {
            upstream_body.to_vec()
        };

        Ok(ChatResponse {
            status,
            content_type: Some("application/json".to_string()),
            body: single_chunk(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ANTHROPIC_VERSION, AnthropicConnector};
    use crate::provider::{BodyStream, ChatRequest, Connector};
    use bytes::Bytes;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn collect(mut body: BodyStream) -> Vec<u8> {
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }

    #[tokio::test]
    async fn transforms_request_and_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .and(body_partial_json(
                json!({ "system": "be brief", "max_tokens": 4096 }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_1",
                "model": "claude-3-7-sonnet",
                "content": [{ "type": "text", "text": "hi there" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 10, "output_tokens": 5 }
            })))
            .mount(&server)
            .await;

        let connector = AnthropicConnector::new(server.uri(), "test-key").unwrap();
        let body = serde_json::to_vec(&json!({
            "model": "claude-3-7-sonnet",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hello" }
            ]
        }))
        .unwrap();

        let response = connector
            .chat(ChatRequest {
                model: "claude-3-7-sonnet".to_string(),
                stream: false,
                body: Bytes::from(body),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        let out = collect(response.body).await;
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["choices"][0]["message"]["content"], "hi there");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(value["usage"]["prompt_tokens"], 10);
        assert_eq!(value["usage"]["completion_tokens"], 5);
        assert_eq!(value["usage"]["total_tokens"], 15);
    }

    #[tokio::test]
    async fn streaming_is_not_yet_supported() {
        let connector = AnthropicConnector::new("http://127.0.0.1:1", "test-key").unwrap();
        let response = connector
            .chat(ChatRequest {
                model: "claude-3-7-sonnet".to_string(),
                stream: true,
                body: Bytes::from_static(b"{}"),
            })
            .await
            .unwrap();
        assert_eq!(response.status, 501);
    }
}

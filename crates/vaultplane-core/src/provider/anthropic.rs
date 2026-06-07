// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Anthropic provider connector.
//!
//! Translates between the OpenAI Chat Completions schema (the gateway's wire format)
//! and the Anthropic Messages schema. System messages are hoisted to Anthropic's
//! top-level `system` field, `max_tokens` is supplied (Anthropic requires it), and
//! the response, finish reason, and token usage are mapped back to the OpenAI shape.
//! The upstream model comes from the route, not the request body.
//!
//! Streaming is supported: when the client requests `stream: true`, Anthropic's
//! native SSE event stream is converted on the fly into OpenAI Chat Completions
//! chunks (`chat.completion.chunk` events, terminated by `data: [DONE]`). Token
//! usage on streaming responses is not yet recorded against the span and spend
//! tracker; that is a follow-up.
//!
//! The schema transform (`parse_openai_messages`, `to_openai_response`) is shared
//! with the Bedrock connector, which speaks the same Anthropic Messages schema.
//! Multimodal (array) message content is not yet supported.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::provider::{BodyStream, ChatRequest, ChatResponse, Connector, Usage, single_chunk};
use crate::sse::{SseEvent, SseParser};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DONE_MARKER: &[u8] = b"data: [DONE]\n\n";

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

/// A message in the Anthropic Messages schema. Shared with the Bedrock connector.
#[derive(Serialize)]
pub(crate) struct Message {
    pub role: String,
    pub content: String,
}

/// An OpenAI Chat Completions request normalized for the Anthropic Messages schema.
pub(crate) struct ChatMessages {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub temperature: Option<f64>,
}

// Incoming OpenAI Chat Completions request (the subset we read).
#[derive(Deserialize)]
struct OpenAiRequest {
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

/// Parse an OpenAI Chat Completions body, hoisting system messages and applying the
/// default `max_tokens`.
pub(crate) fn parse_openai_messages(body: &[u8]) -> Result<ChatMessages> {
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
            messages.push(Message {
                role: message.role,
                content: message.content,
            });
        }
    }

    Ok(ChatMessages {
        system: (!system.is_empty()).then_some(system),
        messages,
        max_tokens: request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        temperature: request.temperature,
    })
}

// Outgoing Anthropic Messages request.
#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
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

/// Map an Anthropic stop reason to an OpenAI finish reason.
fn map_finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "max_tokens" => "length",
        _ => "stop",
    }
}

/// Translate an Anthropic Messages response into an OpenAI Chat Completions response,
/// returning the encoded body together with the extracted token usage. Shared with
/// the Bedrock connector.
pub(crate) fn to_openai_response(body: &[u8]) -> Result<(Vec<u8>, Usage)> {
    let response: AnthropicResponse = serde_json::from_slice(body)
        .map_err(|e| Error::Provider(format!("invalid Anthropic response: {e}")))?;

    let content: String = response
        .content
        .iter()
        .filter(|block| block.kind == "text")
        .map(|block| block.text.as_str())
        .collect();

    let upstream_usage = response.usage.unwrap_or_default();
    let usage = Usage {
        prompt_tokens: upstream_usage.input_tokens,
        completion_tokens: upstream_usage.output_tokens,
    };
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
            "finish_reason": map_finish_reason(response.stop_reason.as_deref().unwrap_or("")),
        }],
        "usage": {
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.prompt_tokens + usage.completion_tokens,
        },
    });

    let encoded = serde_json::to_vec(&openai)
        .map_err(|e| Error::Provider(format!("failed to encode response: {e}")))?;
    Ok((encoded, usage))
}

/// Stateful conversion from Anthropic event stream to OpenAI Chat Completions
/// chunk stream.
struct AnthropicTransform {
    chat_id: String,
    created: u64,
    model: String,
    input_tokens: u32,
    output_tokens: u32,
}

impl AnthropicTransform {
    fn new(model: String) -> Self {
        let mut id_bytes = [0u8; 8];
        let _ = getrandom::getrandom(&mut id_bytes);
        let chat_id = format!("chatcmpl-{}", hex::encode(id_bytes));
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            chat_id,
            created,
            model,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn process(&mut self, event: &SseEvent) -> Vec<Bytes> {
        let mut out = Vec::new();
        let event_type = event.event.as_deref().unwrap_or("");
        let data: serde_json::Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(_) => return out,
        };

        match event_type {
            "message_start" => {
                if let Some(model) = data
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                {
                    self.model = model.to_string();
                }
                if let Some(tokens) = data
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                {
                    self.input_tokens = tokens as u32;
                }
                out.push(self.chunk(json!({ "role": "assistant" }), None));
            }
            "content_block_delta" => {
                if data
                    .get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("text_delta")
                    && let Some(text) = data
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                {
                    out.push(self.chunk(json!({ "content": text }), None));
                }
            }
            "message_delta" => {
                let stop_reason = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str())
                    .map(map_finish_reason);
                if let Some(tokens) = data
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                {
                    self.output_tokens = tokens as u32;
                }
                out.push(self.chunk(json!({}), stop_reason));
                out.push(self.usage_chunk());
            }
            // `message_stop`, `content_block_start`, `content_block_stop`, `ping`,
            // and any unknown events have no OpenAI equivalent and are dropped.
            // The terminal `data: [DONE]` is emitted by the adapter on stream end.
            _ => {}
        }
        out
    }

    fn chunk(&self, delta: serde_json::Value, finish_reason: Option<&'static str>) -> Bytes {
        let chunk = json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        });
        Bytes::from(format!("data: {chunk}\n\n").into_bytes())
    }

    /// Emit a final OpenAI-shape usage chunk (empty `choices`, top-level `usage`)
    /// after the finish chunk so a downstream observer can pick up token totals.
    fn usage_chunk(&self) -> Bytes {
        let total = self.input_tokens + self.output_tokens;
        let chunk = json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": {
                "prompt_tokens": self.input_tokens,
                "completion_tokens": self.output_tokens,
                "total_tokens": total,
            }
        });
        Bytes::from(format!("data: {chunk}\n\n").into_bytes())
    }
}

/// Stream adapter that consumes Anthropic SSE bytes and emits OpenAI Chat
/// Completions SSE bytes. Terminates the output with `data: [DONE]\n\n`.
struct AnthropicStreamAdapter {
    inner: BodyStream,
    parser: SseParser,
    transform: AnthropicTransform,
    pending: VecDeque<Bytes>,
    upstream_done: bool,
    emitted_done: bool,
}

impl AnthropicStreamAdapter {
    fn new(inner: BodyStream, initial_model: String) -> Self {
        Self {
            inner,
            parser: SseParser::new(),
            transform: AnthropicTransform::new(initial_model),
            pending: VecDeque::new(),
            upstream_done: false,
            emitted_done: false,
        }
    }
}

impl Stream for AnthropicStreamAdapter {
    type Item = std::io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(out) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(out)));
            }
            if this.upstream_done {
                if !this.emitted_done {
                    this.emitted_done = true;
                    return Poll::Ready(Some(Ok(Bytes::from_static(DONE_MARKER))));
                }
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(chunk))) => {
                    this.parser.feed(&chunk);
                    while let Some(event) = this.parser.next_event() {
                        for out in this.transform.process(&event) {
                            this.pending.push_back(out);
                        }
                    }
                }
                Poll::Ready(None) => {
                    this.upstream_done = true;
                }
            }
        }
    }
}

#[async_trait]
impl Connector for AnthropicConnector {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn reachable(&self) -> bool {
        crate::provider::http_reachable(&self.client, &self.base_url).await
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let parts = parse_openai_messages(&request.body)?;
        let stream_requested = request.stream;
        let model_for_response = request.model.clone();
        let payload = serde_json::to_vec(&AnthropicRequest {
            model: request.model,
            messages: parts.messages,
            max_tokens: parts.max_tokens,
            system: parts.system,
            temperature: parts.temperature,
            stream: stream_requested.then_some(true),
        })
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

        if stream_requested && status == 200 {
            let upstream: BodyStream = response
                .bytes_stream()
                .map(|chunk| chunk.map_err(std::io::Error::other))
                .boxed();
            let adapter = AnthropicStreamAdapter::new(upstream, model_for_response.clone());
            return Ok(ChatResponse {
                status: 200,
                content_type: Some("text/event-stream".to_string()),
                body: adapter.boxed(),
                provider: "anthropic".to_string(),
                model: model_for_response,
                // Streaming usage extraction is a follow-up; it requires observing
                // the SSE events past the proxy handler return point.
                usage: None,
                attempts: 1,
            });
        }

        // Non-streaming path (or error response: pass through the JSON body).
        let upstream_body = response
            .bytes()
            .await
            .map_err(|e| Error::Provider(format!("reading Anthropic response failed: {e}")))?;

        let (body, usage) = if status == 200 {
            let (encoded, usage) = to_openai_response(&upstream_body)?;
            (encoded, Some(usage))
        } else {
            // Non-2xx: Anthropic returns its own error envelope
            // (`{"type":"error","error":{...}}`). Rewrite it into the OpenAI
            // error shape so clients see a consistent format regardless of the
            // upstream that handled the request.
            (to_openai_error(&upstream_body), None)
        };

        Ok(ChatResponse {
            status,
            content_type: Some("application/json".to_string()),
            body: single_chunk(body),
            provider: "anthropic".to_string(),
            model: model_for_response,
            usage,
            attempts: 1,
        })
    }
}

/// Convert an Anthropic error response body into the OpenAI error envelope.
///
/// Anthropic returns `{"type":"error","error":{"type":"...","message":"..."}}`;
/// OpenAI clients expect `{"error":{"message":"...","type":"..."}}`. If the
/// body is not parseable as the Anthropic shape, the raw body text is wrapped
/// in a synthetic OpenAI envelope so the client always gets a useful payload.
fn to_openai_error(body: &[u8]) -> Vec<u8> {
    let parsed: Option<(String, Option<String>)> =
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                let error = value.get("error")?;
                let message = error.get("message")?.as_str()?.to_string();
                let kind = error
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                Some((message, kind))
            });

    let (message, kind) = match parsed {
        Some((m, k)) => (m, k),
        None => {
            // Fall back to the raw body text, trimmed to avoid huge payloads.
            let text = std::str::from_utf8(body).unwrap_or("upstream returned a non-JSON error");
            let trimmed = if text.len() > 512 { &text[..512] } else { text };
            (trimmed.to_string(), None)
        }
    };

    let envelope = match kind {
        Some(k) => json!({ "error": { "message": message, "type": k } }),
        None => json!({ "error": { "message": message } }),
    };
    serde_json::to_vec(&envelope).unwrap_or_else(|_| {
        // Should never happen for a value built from json!() above.
        br#"{"error":{"message":"upstream returned an error"}}"#.to_vec()
    })
}

#[cfg(test)]
mod tests {
    use super::{ANTHROPIC_VERSION, AnthropicConnector};
    use crate::provider::{BodyStream, ChatRequest, Connector, EmbeddingsRequest};
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
            .and(body_partial_json(json!({
                "model": "claude-3-7-sonnet",
                "system": "be brief",
                "max_tokens": 4096
            })))
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
            "model": "ignored-the-route-model-wins",
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
        let usage = response.usage.expect("usage is populated for Anthropic");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        let out = collect(response.body).await;
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["choices"][0]["message"]["content"], "hi there");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert_eq!(value["usage"]["total_tokens"], 15);
    }

    #[tokio::test]
    async fn streams_anthropic_events_as_openai_sse() {
        let server = MockServer::start().await;
        let upstream_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-3-7-sonnet\",\"content\":[],\"usage\":{\"input_tokens\":10}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({ "stream": true })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(upstream_body),
            )
            .mount(&server)
            .await;

        let connector = AnthropicConnector::new(server.uri(), "test-key").unwrap();
        let body = serde_json::to_vec(&json!({
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .unwrap();

        let response = connector
            .chat(ChatRequest {
                model: "claude-3-7-sonnet".to_string(),
                stream: true,
                body: Bytes::from(body),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type.as_deref(), Some("text/event-stream"));
        let bytes = collect(response.body).await;
        let text = String::from_utf8(bytes).unwrap();

        assert!(
            text.contains("\"role\":\"assistant\""),
            "missing role chunk: {text}"
        );
        assert!(
            text.contains("\"content\":\"Hello\""),
            "missing 'Hello' chunk: {text}"
        );
        assert!(
            text.contains("\"content\":\" world\""),
            "missing ' world' chunk: {text}"
        );
        assert!(
            text.contains("\"finish_reason\":\"stop\""),
            "missing finish chunk: {text}"
        );
        assert!(text.contains("data: [DONE]"), "missing DONE: {text}");
        assert!(
            text.contains("chat.completion.chunk"),
            "missing chunk object: {text}"
        );
        assert!(
            text.contains("\"prompt_tokens\":10"),
            "missing usage chunk prompt_tokens: {text}"
        );
        assert!(
            text.contains("\"completion_tokens\":5"),
            "missing usage chunk completion_tokens: {text}"
        );
    }

    /// On a non-2xx response the Anthropic connector rewrites the body into
    /// the OpenAI error envelope so clients see a consistent shape regardless
    /// of which upstream actually served the request.
    #[tokio::test]
    async fn non_2xx_response_is_rewritten_into_the_openai_error_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
                    ),
            )
            .mount(&server)
            .await;

        let connector = AnthropicConnector::new(server.uri(), "bad-key").unwrap();
        let body = json!({
            "model": "claude-3-7-sonnet",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let response = connector
            .chat(ChatRequest {
                model: "claude-3-7-sonnet".to_string(),
                stream: false,
                body: Bytes::from(serde_json::to_vec(&body).unwrap()),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 401);
        let bytes = collect(response.body).await;
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["error"]["message"], "invalid x-api-key",
            "message should come from anthropic body, got: {value}"
        );
        assert_eq!(value["error"]["type"], "authentication_error");
        assert!(
            value.get("type").is_none(),
            "OpenAI envelope should not have a top-level 'type' field"
        );
    }

    /// Anthropic's Messages API has no embeddings endpoint, so the connector
    /// inherits the trait's default "not supported" error.
    #[tokio::test]
    async fn embeddings_are_unsupported_for_anthropic() {
        let connector =
            AnthropicConnector::new("http://127.0.0.1:1".to_string(), "test-key".to_string())
                .unwrap();
        match connector
            .embeddings(EmbeddingsRequest {
                model: "claude-3-7-sonnet".to_string(),
                body: Bytes::from_static(b"{}"),
            })
            .await
        {
            Err(err) => {
                let message = format!("{err}");
                assert!(
                    message.contains("anthropic")
                        && message.contains("does not support embeddings"),
                    "expected 'does not support embeddings' error for anthropic, got: {message}"
                );
            }
            Ok(_) => panic!("anthropic should reject embeddings as unsupported"),
        }
    }
}

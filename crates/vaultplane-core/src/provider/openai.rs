//! OpenAI and OpenAI-compatible provider connector.
//!
//! Forwards a request to the configured base URL with the provider API key swapped
//! in and the request body's `model` rewritten to the route's upstream model.
//!
//! For non-streaming responses the connector buffers the body (bounded, kilobytes)
//! and extracts the `usage` field, so cost and token counts are reported. Streaming
//! responses pass through unchanged without buffering; usage is not extracted on the
//! streaming path (that needs a tee-style observer on the SSE events).

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;

use crate::error::{Error, Result};
use crate::provider::{
    BodyStream, ChatRequest, ChatResponse, Connector, EmbeddingsRequest, parse_openai_usage,
    single_chunk,
};

/// Connector for the OpenAI REST API and OpenAI-compatible self-hosted servers.
pub struct OpenAiConnector {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiConnector {
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

/// Rewrite the `model` field of a JSON request body. Returns the body unchanged if
/// it is not a JSON object.
fn rewrite_model(body: &Bytes, model: &str) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "model".to_string(),
                    serde_json::Value::String(model.to_string()),
                );
                return serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec());
            }
            body.to_vec()
        }
        Err(_) => body.to_vec(),
    }
}

#[async_trait]
impl Connector for OpenAiConnector {
    fn name(&self) -> &str {
        "openai"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(rewrite_model(&request.body, &request.model))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request to OpenAI failed: {e}")))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        if !request.stream {
            // Non-streaming: buffer the bounded JSON body so we can extract usage.
            let upstream_body = response
                .bytes()
                .await
                .map_err(|e| Error::Provider(format!("reading OpenAI response failed: {e}")))?;
            let usage = parse_openai_usage(&upstream_body);
            return Ok(ChatResponse {
                status,
                content_type,
                body: single_chunk(upstream_body.to_vec()),
                provider: "openai".to_string(),
                model: request.model,
                usage,
                attempts: 1,
            });
        }

        // Streaming: pass chunks straight through; usage is not extracted today.
        let body: BodyStream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other))
            .boxed();

        Ok(ChatResponse {
            status,
            content_type,
            body,
            provider: "openai".to_string(),
            model: request.model,
            usage: None,
            attempts: 1,
        })
    }

    async fn embeddings(&self, request: EmbeddingsRequest) -> Result<ChatResponse> {
        let url = format!("{}/v1/embeddings", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(rewrite_model(&request.body, &request.model))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request to OpenAI failed: {e}")))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        let upstream_body = response
            .bytes()
            .await
            .map_err(|e| Error::Provider(format!("reading OpenAI response failed: {e}")))?;
        let usage = parse_openai_usage(&upstream_body);
        Ok(ChatResponse {
            status,
            content_type,
            body: single_chunk(upstream_body.to_vec()),
            provider: "openai".to_string(),
            model: request.model,
            usage,
            attempts: 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAiConnector;
    use crate::provider::{ChatRequest, Connector, EmbeddingsRequest};
    use bytes::Bytes;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn unreachable_upstream_is_an_error() {
        // Port 1 is not listening; the request should fail rather than hang or panic.
        let connector = OpenAiConnector::new("http://127.0.0.1:1", "test-key").unwrap();
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            stream: false,
            body: Bytes::from_static(b"{}"),
        };
        assert!(connector.chat(request).await.is_err());
    }

    #[tokio::test]
    async fn non_streaming_response_includes_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"object":"chat.completion","usage":{"prompt_tokens":7,"completion_tokens":11,"total_tokens":18}}"#,
                    ),
            )
            .mount(&server)
            .await;

        let connector = OpenAiConnector::new(server.uri(), "test-key").unwrap();
        let response = connector
            .chat(ChatRequest {
                model: "gpt-4o".to_string(),
                stream: false,
                body: Bytes::from_static(b"{}"),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        let usage = response.usage.expect("usage on a non-streaming response");
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 11);
    }

    #[tokio::test]
    async fn embeddings_response_includes_prompt_token_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"object":"list","data":[{"object":"embedding","embedding":[0.1]}],"usage":{"prompt_tokens":4,"total_tokens":4}}"#,
                    ),
            )
            .mount(&server)
            .await;

        let connector = OpenAiConnector::new(server.uri(), "test-key").unwrap();
        let response = connector
            .embeddings(EmbeddingsRequest {
                model: "text-embedding-3-small".to_string(),
                body: Bytes::from_static(b"{\"input\":\"hi\"}"),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        let usage = response.usage.expect("usage on an embeddings response");
        assert_eq!(usage.prompt_tokens, 4);
        // Embeddings have no output tokens; the parser defaults to zero.
        assert_eq!(usage.completion_tokens, 0);
    }

    #[tokio::test]
    async fn streaming_response_has_no_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: {}\n\ndata: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let connector = OpenAiConnector::new(server.uri(), "test-key").unwrap();
        let response = connector
            .chat(ChatRequest {
                model: "gpt-4o".to_string(),
                stream: true,
                body: Bytes::from_static(b"{}"),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        assert!(response.usage.is_none());
    }
}

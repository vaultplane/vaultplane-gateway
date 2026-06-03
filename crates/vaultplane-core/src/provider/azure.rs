//! Azure OpenAI provider connector.
//!
//! Azure OpenAI uses the OpenAI Chat Completions schema but a different endpoint
//! shape: the model is an Azure *deployment* named in the URL path, the API key is
//! sent in the `api-key` header, and the API version is a query parameter. Request
//! and response bodies are the OpenAI schema, so they pass through unchanged.
//!
//! For non-streaming responses the connector buffers the body and extracts the
//! `usage` field. Streaming responses pass through unchanged without buffering;
//! usage is not extracted on the streaming path.

use async_trait::async_trait;
use futures::StreamExt;

use crate::error::{Error, Result};
use crate::provider::{
    BodyStream, ChatRequest, ChatResponse, Connector, EmbeddingsRequest, parse_openai_usage,
    single_chunk,
};

/// Connector for Azure OpenAI deployments.
pub struct AzureConnector {
    base_url: String,
    api_key: String,
    api_version: String,
    client: reqwest::Client,
}

impl AzureConnector {
    /// Build a connector for the given resource base URL, API key, and API version.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_version: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::Provider(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            api_version: api_version.into(),
            client,
        })
    }
}

#[async_trait]
impl Connector for AzureConnector {
    fn name(&self) -> &str {
        "azure"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        // The route model is the Azure deployment name, which goes in the URL path.
        let url = format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.base_url, request.model, self.api_version
        );
        let response = self
            .client
            .post(&url)
            .header("api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request.body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request to Azure OpenAI failed: {e}")))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        if !request.stream {
            let upstream_body = response.bytes().await.map_err(|e| {
                Error::Provider(format!("reading Azure OpenAI response failed: {e}"))
            })?;
            let usage = parse_openai_usage(&upstream_body);
            return Ok(ChatResponse {
                status,
                content_type,
                body: single_chunk(upstream_body.to_vec()),
                provider: "azure".to_string(),
                model: request.model,
                usage,
                attempts: 1,
            });
        }

        let body: BodyStream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other))
            .boxed();

        Ok(ChatResponse {
            status,
            content_type,
            body,
            provider: "azure".to_string(),
            model: request.model,
            usage: None,
            attempts: 1,
        })
    }

    async fn embeddings(&self, request: EmbeddingsRequest) -> Result<ChatResponse> {
        // The route model is the Azure deployment name, which goes in the URL path.
        let url = format!(
            "{}/openai/deployments/{}/embeddings?api-version={}",
            self.base_url, request.model, self.api_version
        );
        let response = self
            .client
            .post(&url)
            .header("api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request.body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request to Azure OpenAI failed: {e}")))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        let upstream_body = response
            .bytes()
            .await
            .map_err(|e| Error::Provider(format!("reading Azure OpenAI response failed: {e}")))?;
        let usage = parse_openai_usage(&upstream_body);
        Ok(ChatResponse {
            status,
            content_type,
            body: single_chunk(upstream_body.to_vec()),
            provider: "azure".to_string(),
            model: request.model,
            usage,
            attempts: 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::AzureConnector;
    use crate::provider::{ChatRequest, Connector};
    use bytes::Bytes;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn forwards_to_the_deployment_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/prod-gpt4o/chat/completions"))
            .and(query_param("api-version", "2024-10-21"))
            .and(header("api-key", "test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"object":"chat.completion"}"#),
            )
            .mount(&server)
            .await;

        let connector = AzureConnector::new(server.uri(), "test-key", "2024-10-21").unwrap();
        let response = connector
            .chat(ChatRequest {
                model: "prod-gpt4o".to_string(),
                stream: false,
                body: Bytes::from_static(b"{}"),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn non_streaming_response_includes_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/openai/deployments/prod-gpt4o/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(
                        r#"{"object":"chat.completion","usage":{"prompt_tokens":4,"completion_tokens":6,"total_tokens":10}}"#,
                    ),
            )
            .mount(&server)
            .await;

        let connector = AzureConnector::new(server.uri(), "test-key", "2024-10-21").unwrap();
        let response = connector
            .chat(ChatRequest {
                model: "prod-gpt4o".to_string(),
                stream: false,
                body: Bytes::from_static(b"{}"),
            })
            .await
            .unwrap();

        let usage = response
            .usage
            .expect("usage on a non-streaming Azure response");
        assert_eq!(usage.prompt_tokens, 4);
        assert_eq!(usage.completion_tokens, 6);
    }
}

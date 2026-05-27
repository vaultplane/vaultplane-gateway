//! OpenAI and OpenAI-compatible provider connector.
//!
//! Forwards a request to the configured base URL with the provider API key swapped
//! in and the request body's `model` rewritten to the route's upstream model, then
//! streams the response back without buffering.

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;

use crate::error::{Error, Result};
use crate::provider::{BodyStream, ChatRequest, ChatResponse, Connector};

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

        let body: BodyStream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other))
            .boxed();

        Ok(ChatResponse {
            status,
            content_type,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAiConnector;
    use crate::provider::{ChatRequest, Connector};
    use bytes::Bytes;

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
}

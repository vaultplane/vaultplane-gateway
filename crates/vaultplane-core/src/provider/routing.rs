//! Model-based provider routing.
//!
//! Dispatches a request to a provider connector based on the requested model. For
//! now this is a fixed prefix rule (models beginning with `claude` go to Anthropic,
//! everything else to OpenAI), pending the configuration-driven model registry.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::provider::{ChatRequest, ChatResponse, Connector};

/// Routes requests to one of several provider connectors by model name.
pub struct RoutingConnector {
    openai: Arc<dyn Connector>,
    anthropic: Arc<dyn Connector>,
}

impl RoutingConnector {
    /// Build a router over the OpenAI and Anthropic connectors.
    pub fn new(openai: Arc<dyn Connector>, anthropic: Arc<dyn Connector>) -> Self {
        Self { openai, anthropic }
    }

    fn select(&self, model: &str) -> &Arc<dyn Connector> {
        if model.starts_with("claude") {
            &self.anthropic
        } else {
            &self.openai
        }
    }
}

#[async_trait]
impl Connector for RoutingConnector {
    fn name(&self) -> &str {
        "router"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        self.select(&request.model).chat(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::RoutingConnector;
    use crate::provider::anthropic::AnthropicConnector;
    use crate::provider::openai::OpenAiConnector;
    use crate::provider::{BodyStream, ChatRequest, Connector};
    use bytes::Bytes;
    use futures::StreamExt;
    use serde_json::json;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn collect(mut body: BodyStream) -> Vec<u8> {
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }

    fn chat(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            stream: false,
            body: Bytes::from(format!(r#"{{"model":"{model}"}}"#)),
        }
    }

    #[tokio::test]
    async fn routes_by_model_prefix() {
        let openai = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"who":"openai"}"#))
            .mount(&openai)
            .await;

        let anthropic = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "m",
                "model": "claude",
                "content": [{ "type": "text", "text": "ok" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&anthropic)
            .await;

        let router = RoutingConnector::new(
            Arc::new(OpenAiConnector::new(openai.uri(), "k").unwrap()),
            Arc::new(AnthropicConnector::new(anthropic.uri(), "k").unwrap()),
        );

        let openai_body = collect(router.chat(chat("gpt-4o")).await.unwrap().body).await;
        assert!(String::from_utf8_lossy(&openai_body).contains("openai"));

        let anthropic_body =
            collect(router.chat(chat("claude-3-7-sonnet")).await.unwrap().body).await;
        assert!(String::from_utf8_lossy(&anthropic_body).contains("chat.completion"));
    }
}

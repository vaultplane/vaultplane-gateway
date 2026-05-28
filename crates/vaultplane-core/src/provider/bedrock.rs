//! AWS Bedrock provider connector (Anthropic models).
//!
//! Bedrock invokes a model by id in the URL path and requires SigV4-signed requests.
//! For Anthropic models the body is the Anthropic Messages schema with an
//! `anthropic_version` field (the model is in the URL, not the body). Requests and
//! non-streaming responses are translated to and from the OpenAI schema, reusing the
//! Anthropic transform. Region and credentials come from configuration (typically the
//! standard AWS environment variables).
//!
//! This connector is unit-tested (request transform, SigV4 signing, routing) but has
//! not been verified against a live Bedrock endpoint. Streaming is not yet supported.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::provider::anthropic::{Message, parse_openai_messages, to_openai_response};
use crate::provider::sigv4;
use crate::provider::{ChatRequest, ChatResponse, Connector, single_chunk};

const SERVICE: &str = "bedrock";
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Connector for AWS Bedrock (Anthropic models), using SigV4 request signing.
pub struct BedrockConnector {
    endpoint: String,
    host: String,
    region: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    client: reqwest::Client,
}

impl BedrockConnector {
    /// Build a connector for the given region and AWS credentials.
    pub fn new(
        region: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        session_token: Option<String>,
    ) -> Result<Self> {
        let region = region.into();
        let endpoint = format!("https://bedrock-runtime.{region}.amazonaws.com");
        Self::build(
            endpoint,
            region,
            access_key.into(),
            secret_key.into(),
            session_token,
        )
    }

    fn build(
        endpoint: String,
        region: String,
        access_key: String,
        secret_key: String,
        session_token: Option<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::Provider(format!("failed to build HTTP client: {e}")))?;
        let host = endpoint
            .strip_prefix("https://")
            .or_else(|| endpoint.strip_prefix("http://"))
            .unwrap_or(&endpoint)
            .to_string();
        Ok(Self {
            endpoint,
            host,
            region,
            access_key,
            secret_key,
            session_token,
            client,
        })
    }
}

// Outgoing Bedrock InvokeModel body for Anthropic models.
#[derive(Serialize)]
struct BedrockRequest {
    anthropic_version: &'static str,
    messages: Vec<Message>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

#[async_trait]
impl Connector for BedrockConnector {
    fn name(&self) -> &str {
        "bedrock"
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        if request.stream {
            let body = json!({
                "error": { "message": "streaming is not yet supported for the Bedrock provider" }
            });
            return Ok(ChatResponse {
                status: 501,
                content_type: Some("application/json".to_string()),
                body: single_chunk(serde_json::to_vec(&body).unwrap_or_default()),
                provider: "bedrock".to_string(),
                model: request.model,
                usage: None,
                attempts: 1,
            });
        }

        let parts = parse_openai_messages(&request.body)?;
        let payload = serde_json::to_vec(&BedrockRequest {
            anthropic_version: BEDROCK_ANTHROPIC_VERSION,
            messages: parts.messages,
            max_tokens: parts.max_tokens,
            system: parts.system,
            temperature: parts.temperature,
        })
        .map_err(|e| Error::Provider(format!("failed to encode request: {e}")))?;

        // The route model is the Bedrock model id, encoded into the URL path.
        let path = format!(
            "/model/{}/invoke",
            sigv4::uri_encode_segment(&request.model)
        );
        let url = format!("{}{}", self.endpoint, path);
        let (amz_date, date_stamp) = sigv4::now_amz_date();
        let authorization = sigv4::sign_post(
            &self.access_key,
            &self.secret_key,
            self.session_token.as_deref(),
            &self.region,
            SERVICE,
            &self.host,
            &path,
            "application/json",
            &payload,
            &amz_date,
            &date_stamp,
        );

        let mut builder = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("x-amz-date", &amz_date)
            .header(reqwest::header::AUTHORIZATION, authorization);
        if let Some(token) = &self.session_token {
            builder = builder.header("x-amz-security-token", token);
        }

        let response = builder
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request to Bedrock failed: {e}")))?;

        let status = response.status().as_u16();
        let upstream_body = response
            .bytes()
            .await
            .map_err(|e| Error::Provider(format!("reading Bedrock response failed: {e}")))?;

        let (body, usage) = if status == 200 {
            let (encoded, usage) = to_openai_response(&upstream_body)?;
            (encoded, Some(usage))
        } else {
            (upstream_body.to_vec(), None)
        };

        Ok(ChatResponse {
            status,
            content_type: Some("application/json".to_string()),
            body: single_chunk(body),
            provider: "bedrock".to_string(),
            model: request.model,
            usage,
            attempts: 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::BedrockConnector;
    use crate::provider::{ChatRequest, Connector};
    use bytes::Bytes;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn signs_and_transforms_an_anthropic_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/model/anthropic.claude-3-7-sonnet/invoke"))
            .and(header_exists("authorization"))
            .and(header_exists("x-amz-date"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_1",
                "model": "claude",
                "content": [{ "type": "text", "text": "ok" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 3, "output_tokens": 2 }
            })))
            .mount(&server)
            .await;

        let connector = BedrockConnector::build(
            server.uri(),
            "us-east-1".to_string(),
            "AKIDEXAMPLE".to_string(),
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            None,
        )
        .unwrap();

        let body = serde_json::to_vec(&serde_json::json!({
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .unwrap();

        let response = connector
            .chat(ChatRequest {
                model: "anthropic.claude-3-7-sonnet".to_string(),
                stream: false,
                body: Bytes::from(body),
            })
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        let usage = response.usage.expect("usage is populated for Bedrock");
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 2);
    }

    #[tokio::test]
    async fn streaming_is_not_yet_supported() {
        let connector = BedrockConnector::new("us-east-1", "AKIDEXAMPLE", "secret", None).unwrap();
        let response = connector
            .chat(ChatRequest {
                model: "anthropic.claude-3-7-sonnet".to_string(),
                stream: true,
                body: Bytes::from_static(b"{}"),
            })
            .await
            .unwrap();
        assert_eq!(response.status, 501);
    }
}

//! Streaming usage observer.
//!
//! Wraps a [`BodyStream`] of OpenAI Chat Completions SSE chunks, parses the events
//! as they flow past, and on encountering an OpenAI-style `usage` field it records
//! `gen_ai.usage.*` and `vaultplane.cost_usd` on the supplied tracing span and (if a
//! spend limit is configured) accumulates the cost against the [`SpendTracker`].
//!
//! The bytes themselves are forwarded unchanged: the observer is a pass-through tee.
//! All four streaming connector outputs (OpenAI, Azure, Anthropic, Bedrock once
//! streaming is added) speak the same OpenAI SSE shape, so a single observer
//! suffices.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::stream::Stream;

use crate::auth::{SpendLimit, SpendTracker};
use crate::config::Pricing;
use crate::cost;
use crate::provider::{BodyStream, Usage};
use crate::sse::SseParser;

/// A stream adapter that forwards chunks unchanged while observing OpenAI SSE
/// chunks for a `usage` payload.
pub struct UsageObservingStream {
    inner: BodyStream,
    parser: SseParser,
    span: tracing::Span,
    pricing: Arc<Pricing>,
    spend_tracker: Arc<SpendTracker>,
    key_id: String,
    spend_limit: Option<SpendLimit>,
    provider: String,
    model: String,
    found_usage: bool,
}

impl UsageObservingStream {
    /// Build a new observer.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inner: BodyStream,
        span: tracing::Span,
        pricing: Arc<Pricing>,
        spend_tracker: Arc<SpendTracker>,
        key_id: String,
        spend_limit: Option<SpendLimit>,
        provider: String,
        model: String,
    ) -> Self {
        Self {
            inner,
            parser: SseParser::new(),
            span,
            pricing,
            spend_tracker,
            key_id,
            spend_limit,
            provider,
            model,
            found_usage: false,
        }
    }
}

impl Stream for UsageObservingStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let polled = this.inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(Ok(chunk))) = &polled
            && !this.found_usage
        {
            this.parser.feed(chunk);
            while let Some(event) = this.parser.next_event() {
                if let Some(usage) = parse_chunk_usage(&event.data) {
                    this.span
                        .record("gen_ai.usage.input_tokens", u64::from(usage.prompt_tokens));
                    this.span.record(
                        "gen_ai.usage.output_tokens",
                        u64::from(usage.completion_tokens),
                    );
                    if let Some(c) =
                        cost::compute(&this.pricing, &this.provider, &this.model, &usage)
                    {
                        this.span.record("vaultplane.cost_usd", c);
                        if let Some(limit) = &this.spend_limit {
                            this.spend_tracker.record(&this.key_id, limit.period, c);
                        }
                    }
                    this.found_usage = true;
                    break;
                }
            }
        }
        polled
    }
}

/// Try to extract `usage` from an OpenAI Chat Completions SSE chunk payload.
fn parse_chunk_usage(data: &str) -> Option<Usage> {
    if data.trim() == "[DONE]" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let usage = value.get("usage")?;
    let prompt_tokens = usage.get("prompt_tokens")?.as_u64()? as u32;
    let completion_tokens = usage.get("completion_tokens")?.as_u64()? as u32;
    Some(Usage {
        prompt_tokens,
        completion_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_chunk_usage;

    #[test]
    fn extracts_usage_from_an_openai_chunk_payload() {
        let usage = parse_chunk_usage(
            r#"{"id":"x","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn ignores_chunks_without_usage_and_the_done_marker() {
        assert!(parse_chunk_usage("[DONE]").is_none());
        assert!(parse_chunk_usage(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).is_none());
    }
}

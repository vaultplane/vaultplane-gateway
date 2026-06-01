//! Per-request cost computation from token usage and the pricing table.
//!
//! Shared by the proxy's request handler and the streaming usage observer so a
//! request reports the same cost regardless of whether usage arrives in the
//! buffered response body or in a streaming SSE chunk.

use crate::config::Pricing;
use crate::provider::Usage;

/// Compute the request cost in USD from upstream usage and the pricing table.
/// Returns `None` when the pricing table has no entry for the provider/model pair.
pub fn compute(pricing: &Pricing, provider: &str, model: &str, usage: &Usage) -> Option<f64> {
    let model_pricing = pricing.providers.get(provider)?.get(model)?;
    let input = (usage.prompt_tokens as f64) / 1000.0 * model_pricing.input_per_1k_tokens_usd;
    let output = (usage.completion_tokens as f64) / 1000.0 * model_pricing.output_per_1k_tokens_usd;
    Some(input + output)
}

#[cfg(test)]
mod tests {
    use super::compute;
    use crate::config::{ModelPricing, Pricing};
    use crate::provider::Usage;
    use std::collections::HashMap;

    #[test]
    fn pricing_table_yields_per_thousand_token_cost() {
        let mut pricing = Pricing::default();
        let mut openai = HashMap::new();
        openai.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input_per_1k_tokens_usd: 2.5,
                output_per_1k_tokens_usd: 10.0,
            },
        );
        pricing.providers.insert("openai".to_string(), openai);

        let usage = Usage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
        };
        let cost = compute(&pricing, "openai", "gpt-4o", &usage).unwrap();
        assert!((cost - 7.5).abs() < 1e-9);
        assert!(compute(&pricing, "openai", "unknown", &usage).is_none());
        assert!(compute(&pricing, "anthropic", "gpt-4o", &usage).is_none());
    }
}

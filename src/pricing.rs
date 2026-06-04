use crate::codex::TokenUsage;

#[derive(Debug, Clone, Copy)]
struct CodexPricing {
    input_cost_per_token: f64,
    output_cost_per_token: f64,
    cache_read_input_cost_per_token: Option<f64>,
    threshold_tokens: Option<u64>,
    input_cost_per_token_above_threshold: Option<f64>,
    output_cost_per_token_above_threshold: Option<f64>,
    cache_read_input_cost_per_token_above_threshold: Option<f64>,
}

impl CodexPricing {
    const fn new(
        input_cost_per_token: f64,
        output_cost_per_token: f64,
        cache_read_input_cost_per_token: Option<f64>,
    ) -> Self {
        Self {
            input_cost_per_token,
            output_cost_per_token,
            cache_read_input_cost_per_token,
            threshold_tokens: None,
            input_cost_per_token_above_threshold: None,
            output_cost_per_token_above_threshold: None,
            cache_read_input_cost_per_token_above_threshold: None,
        }
    }

    const fn with_threshold(
        input_cost_per_token: f64,
        output_cost_per_token: f64,
        cache_read_input_cost_per_token: Option<f64>,
        threshold_tokens: u64,
        input_cost_per_token_above_threshold: f64,
        output_cost_per_token_above_threshold: f64,
        cache_read_input_cost_per_token_above_threshold: f64,
    ) -> Self {
        Self {
            input_cost_per_token,
            output_cost_per_token,
            cache_read_input_cost_per_token,
            threshold_tokens: Some(threshold_tokens),
            input_cost_per_token_above_threshold: Some(input_cost_per_token_above_threshold),
            output_cost_per_token_above_threshold: Some(output_cost_per_token_above_threshold),
            cache_read_input_cost_per_token_above_threshold: Some(
                cache_read_input_cost_per_token_above_threshold,
            ),
        }
    }
}

pub fn codex_cost_usd(model: &str, usage: TokenUsage) -> Option<f64> {
    let pricing = codex_pricing(&normalize_codex_model(model))?;
    Some(cost_usd(pricing, usage))
}

pub fn normalize_codex_model(raw: &str) -> String {
    let trimmed = raw.trim().strip_prefix("openai/").unwrap_or(raw.trim());
    if codex_pricing(trimmed).is_some() {
        return trimmed.to_string();
    }

    if let Some(base) = trimmed.strip_suffix_date()
        && codex_pricing(base).is_some()
    {
        return base.to_string();
    }

    trimmed.to_string()
}

fn cost_usd(pricing: CodexPricing, usage: TokenUsage) -> f64 {
    let input = usage.input_tokens;
    let cached = usage.cached_input_tokens.min(input);
    let output = usage.output_tokens;
    let non_cached = input.saturating_sub(cached);

    let default_cached_rate = pricing
        .cache_read_input_cost_per_token
        .unwrap_or(pricing.input_cost_per_token);
    let uses_long_context_rates = pricing
        .threshold_tokens
        .is_some_and(|threshold| input > threshold);
    let input_rate = if uses_long_context_rates {
        pricing
            .input_cost_per_token_above_threshold
            .unwrap_or(pricing.input_cost_per_token)
    } else {
        pricing.input_cost_per_token
    };
    let cached_rate = if uses_long_context_rates {
        pricing
            .cache_read_input_cost_per_token_above_threshold
            .unwrap_or(default_cached_rate)
    } else {
        default_cached_rate
    };
    let output_rate = if uses_long_context_rates {
        pricing
            .output_cost_per_token_above_threshold
            .unwrap_or(pricing.output_cost_per_token)
    } else {
        pricing.output_cost_per_token
    };

    (non_cached as f64 * input_rate) + (cached as f64 * cached_rate) + (output as f64 * output_rate)
}

fn codex_pricing(model: &str) -> Option<CodexPricing> {
    Some(match model {
        "gpt-5" | "gpt-5-codex" | "gpt-5.1" | "gpt-5.1-codex" | "gpt-5.1-codex-max" => {
            CodexPricing::new(1.25e-6, 1e-5, Some(1.25e-7))
        }
        "gpt-5-mini" | "gpt-5.1-codex-mini" => CodexPricing::new(2.5e-7, 2e-6, Some(2.5e-8)),
        "gpt-5-nano" => CodexPricing::new(5e-8, 4e-7, Some(5e-9)),
        "gpt-5-pro" => CodexPricing::new(1.5e-5, 1.2e-4, None),
        "gpt-5.2" | "gpt-5.2-codex" | "gpt-5.3-codex" => {
            CodexPricing::new(1.75e-6, 1.4e-5, Some(1.75e-7))
        }
        "gpt-5.2-pro" => CodexPricing::new(2.1e-5, 1.68e-4, None),
        "gpt-5.3-codex-spark" => CodexPricing::new(0.0, 0.0, Some(0.0)),
        "gpt-5.4" => {
            CodexPricing::with_threshold(2.5e-6, 1.5e-5, Some(2.5e-7), 272_000, 5e-6, 2.25e-5, 5e-7)
        }
        "gpt-5.4-mini" => CodexPricing::new(7.5e-7, 4.5e-6, Some(7.5e-8)),
        "gpt-5.4-nano" => CodexPricing::new(2e-7, 1.25e-6, Some(2e-8)),
        "gpt-5.4-pro" | "gpt-5.5-pro" => CodexPricing::new(3e-5, 1.8e-4, None),
        "gpt-5.5" => {
            CodexPricing::with_threshold(5e-6, 3e-5, Some(5e-7), 272_000, 1e-5, 4.5e-5, 1e-6)
        }
        _ => return None,
    })
}

trait StripSuffixDate {
    fn strip_suffix_date(&self) -> Option<&str>;
}

impl StripSuffixDate for str {
    fn strip_suffix_date(&self) -> Option<&str> {
        let suffix = self.get(self.len().checked_sub(11)?..)?;
        let bytes = suffix.as_bytes();
        if bytes.first() == Some(&b'-')
            && bytes.get(5) == Some(&b'-')
            && bytes.get(8) == Some(&b'-')
            && bytes[1..5].iter().all(u8::is_ascii_digit)
            && bytes[6..8].iter().all(u8::is_ascii_digit)
            && bytes[9..11].iter().all(u8::is_ascii_digit)
        {
            self.get(..self.len() - 11)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_codex_cost_with_long_context_rate() {
        let cost = codex_cost_usd(
            "openai/gpt-5.5-2026-06-01",
            TokenUsage {
                input_tokens: 300_000,
                cached_input_tokens: 100_000,
                output_tokens: 10_000,
                ..TokenUsage::default()
            },
        )
        .expect("known model");

        assert!((cost - 2.55).abs() < 1e-9);
    }

    #[test]
    fn estimates_codex_cost_with_cached_input_discount_below_threshold() {
        let cost = codex_cost_usd(
            "gpt-5.5",
            TokenUsage {
                input_tokens: 200_000,
                cached_input_tokens: 100_000,
                output_tokens: 10_000,
                ..TokenUsage::default()
            },
        )
        .expect("known model");

        assert!((cost - 0.85).abs() < 1e-9);
    }

    #[test]
    fn unknown_models_are_unpriced() {
        assert_eq!(codex_cost_usd("unknown-model", TokenUsage::default()), None);
    }
}

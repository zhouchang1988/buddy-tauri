use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache: Option<u64>,
}

pub fn normalize_token_usage(value: Option<&Value>) -> TokenUsage {
    let Some(object) = value.and_then(Value::as_object) else {
        return TokenUsage::default();
    };
    let count = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            object.get(*key).and_then(Value::as_f64).and_then(|value| {
                if value.is_finite() && value >= 0.0 {
                    Some(value as u64)
                } else {
                    None
                }
            })
        })
    };
    let nested_count = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_object)
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
    };
    let cache_read = object
        .get("cache")
        .and_then(Value::as_object)
        .and_then(|cache| cache.get("read"))
        .and_then(Value::as_u64);
    TokenUsage {
        input: count(&[
            "inputTokens",
            "input_tokens",
            "prompt_tokens",
            "inputOther",
            "input",
        ]),
        output: count(&[
            "outputTokens",
            "output_tokens",
            "completion_tokens",
            "output",
        ]),
        cache: count(&[
            "cachedReadTokens",
            "cacheReadTokens",
            "cache_read_input_tokens",
            "cache_read_tokens",
            "cached_input_tokens",
        ])
        .or_else(|| nested_count("input_tokens_details"))
        .or_else(|| nested_count("prompt_tokens_details"))
        .or(cache_read),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_explicit_counters_and_nested_cache() {
        let usage = normalize_token_usage(Some(&json!({
            "prompt_tokens": 12,
            "completion_tokens": 4,
            "prompt_tokens_details": { "cached_tokens": 8 },
            "used": 999,
            "size": 1000
        })));
        assert_eq!(
            usage,
            TokenUsage {
                input: Some(12),
                output: Some(4),
                cache: Some(8)
            }
        );
    }

    #[test]
    fn ignores_non_billing_context_metrics() {
        assert_eq!(
            normalize_token_usage(Some(&json!({ "used": 10, "size": 20 }))),
            TokenUsage::default()
        );
    }
}

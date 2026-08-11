//! Secret redaction for event logs, port of `src/main/buddy/redact.ts`.

use once_cell_regex::regexes;

mod once_cell_regex {
    use regex::Regex;
    use std::sync::OnceLock;

    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

    pub fn regexes() -> &'static Vec<Regex> {
        PATTERNS.get_or_init(|| {
            vec![
                Regex::new(r"sk-ant-[A-Za-z0-9_-]{20,}").unwrap(),
                Regex::new(r"sk-[A-Za-z0-9_-]{20,}").unwrap(),
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                Regex::new(r"(?i)token=[A-Za-z0-9_-]+").unwrap(),
                Regex::new(r"(?i)cookie=[^\s;]+").unwrap(),
                Regex::new(r"(?i)Bearer\s+[A-Za-z0-9_-]{20,}").unwrap(),
                Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
            ]
        })
    }
}

pub fn redact_sensitive_text(input: &str) -> String {
    let mut text = input.to_string();
    for pattern in regexes() {
        text = pattern.replace_all(&text, "[REDACTED]").into_owned();
    }
    text
}

pub fn redact_json_value(value: &serde_json::Value) -> serde_json::Value {
    let text = serde_json::to_string(value).unwrap_or_default();
    let redacted = redact_sensitive_text(&text);
    serde_json::from_str(&redacted).unwrap_or_else(|_| value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_anthropic_keys() {
        let out = redact_sensitive_text("key = sk-ant-api03-abcdefabcdefabcdefabcdef");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("abcdefabcdef"));
    }

    #[test]
    fn redacts_openai_style_keys() {
        let out = redact_sensitive_text("sk-0123456789abcdef0123456789");
        assert_eq!(out, "[REDACTED]");
    }

    #[test]
    fn redacts_aws_access_keys() {
        let out = redact_sensitive_text("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(out, "[REDACTED]");
    }

    #[test]
    fn leaves_normal_text_alone() {
        let text = "hello world, no secrets here";
        assert_eq!(redact_sensitive_text(text), text);
    }

    #[test]
    fn redacts_token_query_params_case_insensitively() {
        assert_eq!(
            redact_sensitive_text("https://example.com/cb?token=abc123_X-y"),
            "https://example.com/cb?[REDACTED]"
        );
        assert_eq!(redact_sensitive_text("TOKEN=abc123"), "[REDACTED]");
    }

    #[test]
    fn redacts_cookie_params_up_to_whitespace_or_semicolon() {
        assert_eq!(
            redact_sensitive_text("cookie=session-value; other=1"),
            "[REDACTED]; other=1"
        );
        assert_eq!(redact_sensitive_text("COOKIE=abc def"), "[REDACTED] def");
    }

    #[test]
    fn redacts_bearer_tokens_of_20_plus_chars() {
        let out = redact_sensitive_text("Authorization: Bearer abcdefghij0123456789");
        assert_eq!(out, "Authorization: [REDACTED]");
        // Short bearer values are left alone (pattern requires 20+ chars).
        let short = "Bearer abc123";
        assert_eq!(redact_sensitive_text(short), short);
    }

    #[test]
    fn redacts_pem_private_key_blocks() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvwIBADANBgkqhkiG9w0BAQEFAA\n-----END PRIVATE KEY-----";
        assert_eq!(redact_sensitive_text(pem), "[REDACTED]");
        let rsa = format!("before\n-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\nafter");
        assert_eq!(redact_sensitive_text(&rsa), "before\n[REDACTED]\nafter");
    }
}

//! Reliability helpers: redaction and codepoint-based truncation.
//!
//! The environment secret list and token patterns match `app/reliability.py`
//! so the same strings are scrubbed from logs, stored rows and event payloads.

use std::sync::LazyLock;
use std::time::Instant;

use regex::{Captures, Regex};
use serde_json::Value;

const SECRET_ENV_VARS: [&str; 3] = ["API_KEY", "TAVILY_API_KEY", "TELEGRAM_BOT_TOKEN"];

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"sk-[A-Za-z0-9_\-]{8,}",
        r"(bot\d+:)[A-Za-z0-9_\-]{10,}",
        r"(?i)(bearer\s+)[A-Za-z0-9._\-]{8,}",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("static redaction pattern is valid"))
    .collect()
});

/// Remove known secrets and token-shaped strings; keep captures such as `bot<id>:`.
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut result = text.to_string();
    for name in SECRET_ENV_VARS {
        let value = std::env::var(name).unwrap_or_default();
        let value = value.trim();
        if value.chars().count() >= 8 {
            result = result.replace(value, "***");
        }
    }
    for pattern in PATTERNS.iter() {
        result = pattern
            .replace_all(&result, |captures: &Captures<'_>| match captures.get(1) {
                Some(prefix) => format!("{}***", prefix.as_str()),
                None => "***".to_string(),
            })
            .into_owned();
    }
    result
}

/// Recursively redact strings so secrets never reach the browser or SQLite.
pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), redact_value(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Hard cap stored text by Unicode scalars, never splitting a character.
pub fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Wall-clock budget for one orchestration run, like `app.reliability.Deadline`.
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    started: Instant,
    pub seconds: f64,
}

impl Deadline {
    pub fn new(started: Instant, seconds: f64) -> Self {
        Self { started, seconds }
    }

    pub fn remaining(&self) -> f64 {
        (self.seconds - self.started.elapsed().as_secs_f64()).max(0.0)
    }

    pub fn expired(&self) -> bool {
        self.remaining() <= 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncate_counts_unicode_scalars_not_bytes() {
        assert_eq!(truncate("éééé", 2), "éé");
        assert_eq!(truncate("plain", 10), "plain");
        assert_eq!(truncate("plain", 0), "");
        assert_eq!(truncate("", 5), "");
        assert_eq!(truncate("🪐🪐", 1), "🪐");
    }

    #[test]
    fn deadlines_count_down_and_expire_at_zero() {
        let future = Deadline::new(Instant::now(), 10.0);
        assert!(future.remaining() > 9.0 && future.remaining() <= 10.0);
        assert!(!future.expired());

        let past = Deadline::new(Instant::now() - std::time::Duration::from_secs(1), 0.5);
        assert_eq!(past.remaining(), 0.0);
        assert!(past.expired());
    }

    #[test]
    fn redact_removes_long_environment_secrets_only() {
        std::env::set_var("TAVILY_API_KEY", "tvly-secret-123456");
        std::env::set_var("API_KEY", "short");
        let cleaned = redact("a tvly-secret-123456 b short");
        std::env::remove_var("TAVILY_API_KEY");
        std::env::remove_var("API_KEY");

        assert_eq!(cleaned, "a *** b short");
    }

    #[test]
    fn redact_removes_token_shaped_strings_and_keeps_their_prefix() {
        let cleaned = redact(
            "key sk-abcdefghijklmn and https://api.telegram.org/bot123456:ABCDEFGHIJKLMNOP/send \
             Authorization: Bearer abc.def-ghi_jkl",
        );

        assert!(!cleaned.contains("sk-abcdefghijklmn"));
        assert!(cleaned.contains("bot123456:***"));
        assert!(!cleaned.contains("ABCDEFGHIJKLMNOP"));
        assert!(cleaned.contains("Bearer ***"));
        assert!(!cleaned.contains("abc.def-ghi_jkl"));
    }

    #[test]
    fn redact_leaves_ordinary_text_alone() {
        assert_eq!(redact("plain task text"), "plain task text");
        assert_eq!(redact(""), "");
    }

    #[test]
    fn redact_value_recurses_through_objects_and_arrays() {
        std::env::set_var("TELEGRAM_BOT_TOKEN", "123456:ABCDEFGHIJKLMNOPQRSTUV");
        let value = json!({
            "message": "token 123456:ABCDEFGHIJKLMNOPQRSTUV",
            "nested": ["sk-abcdefghijklmn"],
            "count": 2,
            "flag": true,
            "missing": null,
        });
        let cleaned = redact_value(&value);
        std::env::remove_var("TELEGRAM_BOT_TOKEN");

        assert_eq!(cleaned["message"], "token ***");
        assert_eq!(cleaned["nested"][0], "***");
        assert_eq!(cleaned["count"], 2);
        assert_eq!(cleaned["flag"], true);
        assert!(cleaned["missing"].is_null());
    }
}

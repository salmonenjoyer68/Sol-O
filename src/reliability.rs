//! Reliability helpers: redaction and codepoint-based truncation.
//!
//! Secret values come from the startup settings snapshot; the token patterns
//! match `app/reliability.py` so the same strings are scrubbed from logs,
//! stored rows and event payloads.

use std::sync::{LazyLock, OnceLock};
use std::time::Instant;

use regex::{Captures, Regex};
use serde_json::Value;

use crate::config::Settings;

const SECRET_ENV_VARS: [&str; 3] = ["API_KEY", "TAVILY_API_KEY", "TELEGRAM_BOT_TOKEN"];

/// Secret values snapshotted once at process start; empty until initialised.
static SECRETS: OnceLock<Vec<String>> = OnceLock::new();

/// Snapshot the settings' secrets so `redact` never touches the environment.
pub fn init_secrets(settings: &Settings) {
    let _ = SECRETS.set(settings_secrets(settings));
}

fn settings_secrets(settings: &Settings) -> Vec<String> {
    secret_values([
        settings.api_key.as_str(),
        settings.tavily_api_key.as_str(),
        settings.telegram_bot_token.as_str(),
    ])
}

/// Fallback for a library caller that never initialised: one environment read.
fn secrets() -> &'static [String] {
    SECRETS.get_or_init(|| {
        secret_values(SECRET_ENV_VARS.map(|name| std::env::var(name).unwrap_or_default()))
    })
}

fn secret_values<I, S>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    values
        .into_iter()
        .map(|value| value.as_ref().trim().to_string())
        .filter(|value| value.chars().count() >= 8)
        .collect()
}

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
    redact_with(text, secrets())
}

fn redact_with(text: &str, secrets: &[String]) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut result = text.to_string();
    for value in secrets {
        result = result.replace(value, "***");
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
    redact_value_with(value, secrets())
}

fn redact_value_with(value: &Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(redact_with(text, secrets)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_value_with(item, secrets))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), redact_value_with(item, secrets)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Hard cap stored text by Unicode scalars, never splitting a character.
pub fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Round a duration to the two decimals stored in rows and emitted in results.
pub fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
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
    fn round2_rounds_a_duration_to_two_decimals() {
        assert_eq!(round2(0.0), 0.0);
        assert_eq!(round2(1.234), 1.23);
        assert_eq!(round2(1.235), 1.24);
        assert_eq!(round2(12.3456), 12.35);
        assert_eq!(round2(-1.234), -1.23);
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
    fn redact_removes_secrets_of_eight_codepoints_or_more_only() {
        let secrets = secret_values(["tvly-secret-123456", "short", "12345678", "1234567"]);

        assert_eq!(
            redact_with("a tvly-secret-123456 b short 12345678 1234567", &secrets),
            "a *** b short *** 1234567"
        );
    }

    #[test]
    fn redact_scrubs_secrets_taken_from_settings_without_environment() {
        let settings = Settings {
            api_key: "settings-secret-123".to_string(),
            ..Settings::default()
        };

        assert_eq!(
            redact_with("x settings-secret-123 y", &settings_secrets(&settings)),
            "x *** y"
        );
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
        let secrets = secret_values(["123456:ABCDEFGHIJKLMNOPQRSTUV"]);
        let value = json!({
            "message": "token 123456:ABCDEFGHIJKLMNOPQRSTUV",
            "nested": ["sk-abcdefghijklmn"],
            "count": 2,
            "flag": true,
            "missing": null,
        });
        let cleaned = redact_value_with(&value, &secrets);

        assert_eq!(cleaned["message"], "token ***");
        assert_eq!(cleaned["nested"][0], "***");
        assert_eq!(cleaned["count"], 2);
        assert_eq!(cleaned["flag"], true);
        assert!(cleaned["missing"].is_null());
    }
}

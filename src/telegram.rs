//! Telegram delivery for the notifier, mirroring `tools/telegram.py`.
//!
//! `DRY_RUN` defaults to true: the message is printed, never sent, so the whole
//! pipeline runs without bot credentials. The endpoint is overridable so tests
//! stay offline.

use std::time::Duration;

use serde::Serialize;
use serde_json::json;

use crate::config::Settings;
use crate::reliability::redact;

pub const TELEGRAM_LIMIT: usize = 4000;

#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub dry_run: bool,
    pub delivered: bool,
    pub parts: usize,
}

impl Delivery {
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("delivery serializes")
    }
}

#[derive(Debug, Clone)]
pub struct TelegramError(String);

impl TelegramError {
    pub fn new(message: impl AsRef<str>) -> Self {
        Self(redact(message.as_ref()))
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TelegramError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Split on paragraph boundaries, then hard-wrap, never exceeding `limit`.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let candidate = if current.is_empty() {
            paragraph.to_string()
        } else {
            format!("{current}\n\n{paragraph}")
        };
        if candidate.chars().count() <= limit {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        let mut rest = paragraph;
        while rest.chars().count() > limit {
            let cut = rest
                .char_indices()
                .nth(limit)
                .map(|(index, _)| index)
                .unwrap_or(rest.len());
            chunks.push(rest[..cut].to_string());
            rest = &rest[cut..];
        }
        current = rest.to_string();
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Send `text` (or print it under DRY_RUN); return delivery metadata.
pub fn send_message(settings: &Settings, text: &str) -> Result<Delivery, TelegramError> {
    let parts = split_message(text, TELEGRAM_LIMIT);
    if settings.dry_run {
        println!("\n--- DRY_RUN telegram message ---");
        for (index, part) in parts.iter().enumerate() {
            println!("[part {}/{}] {}", index + 1, parts.len(), part);
        }
        println!("--- end telegram message ---");
        tracing::info!(
            parts = parts.len(),
            characters = text.chars().count(),
            "telegram dry_run"
        );
        return Ok(Delivery {
            dry_run: true,
            delivered: false,
            parts: parts.len(),
        });
    }

    let token = settings.telegram_bot_token.as_str();
    let chat_id = settings.telegram_chat_id.as_str();
    if token.is_empty() || chat_id.is_empty() {
        return Err(TelegramError::new(
            "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be set",
        ));
    }
    let endpoint = settings
        .telegram_endpoint
        .clone()
        .unwrap_or_else(|| format!("https://api.telegram.org/bot{token}/sendMessage"));

    let timeout = Duration::from_secs_f64(settings.tool_timeout_seconds.max(0.001));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| TelegramError::new(format!("telegram send failed: {error}")))?;

    for part in &parts {
        send_with_retry(&client, &endpoint, &chat_id, part)?;
    }
    tracing::info!(
        parts = parts.len(),
        characters = text.chars().count(),
        "telegram sent"
    );
    Ok(Delivery {
        dry_run: false,
        delivered: true,
        parts: parts.len(),
    })
}

fn send_with_retry(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    chat_id: &str,
    text: &str,
) -> Result<(), TelegramError> {
    let body = json!({"chat_id": chat_id, "text": text, "disable_web_page_preview": true});
    let mut last_error = String::new();
    for attempt in 0..=1 {
        let response = client.post(endpoint).json(&body).send();
        let (error, transient) = match response {
            Ok(response) => {
                let status = response.status().as_u16();
                if (200..300).contains(&status) {
                    return Ok(());
                }
                (format!("HTTP {status}"), status >= 500)
            }
            Err(error) => (error.to_string(), true),
        };
        last_error = error;
        if !transient || attempt == 1 {
            break;
        }
        tracing::warn!(
            retry = attempt + 1,
            "telegram retry after transient failure"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(TelegramError::new(format!(
        "telegram send failed: {last_error}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_messages_are_one_part() {
        assert_eq!(split_message("hello", 10), vec!["hello".to_string()]);
    }

    #[test]
    fn long_paragraphs_are_hard_wrapped_at_the_limit() {
        let parts = split_message(&"x".repeat(25), 10);
        assert_eq!(parts, vec!["x".repeat(10), "x".repeat(10), "x".repeat(5)]);
        assert!(parts.iter().all(|part| part.chars().count() <= 10));
    }

    #[test]
    fn paragraphs_pack_greedily_then_wrap() {
        let parts = split_message("aaaa\n\nbbbb\n\ncccccccccccc", 10);
        assert_eq!(parts, vec!["aaaa\n\nbbbb", "cccccccccc", "cc"]);
    }
}

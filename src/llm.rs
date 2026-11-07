//! Chat-completions provider boundary, mirroring `app/llm.py`.
//!
//! One logical call walks the response-format ladder (strict schema, JSON
//! object, no format). Transient failures (timeout, connection, 429, 5xx) are
//! retried with 1s/2s/4s backoff; the ladder restarts on every retry.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Settings;
use crate::reliability::redact;

const BACKOFF_BASE_SECONDS: u64 = 1;
const MAX_DETAIL_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Timeout,
    Error,
}

#[derive(Debug, Clone)]
pub struct LlmError {
    pub kind: FailureKind,
    pub detail: String,
    pub attempts: usize,
    pub retryable: bool,
}

impl LlmError {
    fn new(kind: FailureKind, detail: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            detail: redact(&detail.into()),
            attempts: 1,
            retryable,
        }
    }

    fn fatal(detail: impl Into<String>) -> Self {
        Self::new(FailureKind::Error, detail, false)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
        }
    }
}

/// The per-agent `AGENT_MODELS` override, if any.
pub fn model_for(settings: &Settings, agent: &str) -> Option<String> {
    settings.agent_models.get(agent).cloned()
}

fn provider_settings(settings: &Settings) -> Result<(&str, &str), LlmError> {
    let base_url = settings.base_url.as_str();
    let api_key = settings.api_key.as_str();
    let missing: Vec<&str> = [("BASE_URL", base_url), ("API_KEY", api_key)]
        .iter()
        .filter(|(_, value)| value.is_empty())
        .map(|(name, _)| *name)
        .collect();
    if !missing.is_empty() {
        return Err(LlmError::fatal(format!(
            "missing environment variable(s): {}",
            missing.join(", ")
        )));
    }
    Ok((base_url, api_key))
}

/// A 400/422 rejected only because it does not support the response format.
fn rejects_response_format(status: u16, body: &str) -> bool {
    if status != 400 && status != 422 {
        return false;
    }
    let body = body.to_lowercase();
    [
        "response_format",
        "json_object",
        "json_schema",
        "schema",
        "unsupported",
    ]
    .iter()
    .any(|token| body.contains(token))
}

fn describe(status: u16, body: &str) -> String {
    let body = redact(body);
    let body: String = body.chars().take(MAX_DETAIL_CHARS).collect();
    format!("HTTP {status}: {body}")
}

fn transport_error(error: &reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::new(FailureKind::Timeout, error.to_string(), true)
    } else {
        // Connection refused, DNS failure and protocol errors behave like
        // `APIConnectionError`: transient.
        LlmError::new(FailureKind::Error, error.to_string(), true)
    }
}

/// One provider request, falling back through weaker response formats.
fn request_with_ladder(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    response_schema: Option<&Value>,
) -> Result<String, LlmError> {
    let mut formats: Vec<Option<Value>> = Vec::new();
    if let Some(schema) = response_schema {
        formats.push(Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "agent_output", "strict": true, "schema": schema}
        })));
    }
    formats.push(Some(json!({"type": "json_object"})));
    formats.push(None);

    let last = formats.len() - 1;
    for (index, format) in formats.iter().enumerate() {
        let mut body = json!({"model": model, "messages": messages});
        if let Some(format) = format {
            body["response_format"] = format.clone();
        }
        let response = client
            .post(endpoint)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .map_err(|error| transport_error(&error))?;
        let status = response.status().as_u16();
        let text = response.text().unwrap_or_default();
        if !(200..300).contains(&status) {
            if index != last && rejects_response_format(status, &text) {
                tracing::info!(
                    status,
                    "provider rejected response_format; trying a fallback"
                );
                continue;
            }
            return Err(LlmError::new(
                FailureKind::Error,
                describe(status, &text),
                status == 429 || status >= 500,
            ));
        }
        let completion: Value = serde_json::from_str(&text)
            .map_err(|error| LlmError::fatal(format!("invalid provider response: {error}")))?;
        let content = completion["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice["message"]["content"].as_str())
            .unwrap_or("");
        if content.trim().is_empty() {
            return Err(LlmError::fatal("LLM returned an empty response"));
        }
        return Ok(content.to_string());
    }
    // Every rung either returns or raises.
    Err(LlmError::fatal("no completion returned"))
}

/// Send chat messages and return the raw assistant content.
pub fn call_llm(
    settings: &Settings,
    messages: &[Message],
    response_schema: Option<&Value>,
    timeout: Duration,
    model_override: Option<&str>,
) -> Result<String, LlmError> {
    let (base_url, api_key) = provider_settings(settings)?;
    let model = model_override
        .map(str::to_string)
        .or_else(|| Some(settings.model.clone()))
        .filter(|model| !model.is_empty())
        .ok_or_else(|| LlmError::fatal("missing environment variable(s): MODEL"))?;
    let timeout = timeout.max(Duration::from_millis(1));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| LlmError::fatal(error.to_string()))?;
    let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let max_retries = settings.llm_max_retries.max(0);

    let mut attempt = 0_usize;
    loop {
        attempt += 1;
        match request_with_ladder(
            &client,
            &endpoint,
            &api_key,
            &model,
            messages,
            response_schema,
        ) {
            Ok(content) => return Ok(content),
            Err(mut error) => {
                if !error.retryable || (attempt as i64) > max_retries {
                    error.attempts = attempt;
                    if error.kind == FailureKind::Timeout {
                        error.detail = format!(
                            "LLM call exceeded {:.1}s after {attempt} attempt(s)",
                            timeout.as_secs_f64()
                        );
                    }
                    return Err(error);
                }
                let delay = BACKOFF_BASE_SECONDS * 2_u64.pow(attempt as u32 - 1);
                tracing::warn!(attempt, delay, "LLM retry after {}", error.detail);
                std::thread::sleep(Duration::from_secs(delay));
            }
        }
    }
}

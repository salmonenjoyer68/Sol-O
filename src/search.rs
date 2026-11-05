//! Tavily web search for the researcher step, mirroring `tools/search.py`.
//!
//! The Sun calls this once per run and injects the results into the researcher's
//! message; the model never calls it. Failures become a no-results note instead
//! of failing the run.

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Settings;
use crate::reliability::redact;

pub const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
const MAX_QUERY_CHARS: usize = 400;

/// One Tavily search. `SOL_O_TAVILY_ENDPOINT` points process tests at a loopback
/// stand-in so they never reach the network.
pub fn search(settings: &Settings, query: &str) -> Result<Vec<Value>, String> {
    let api_key = std::env::var("TAVILY_API_KEY")
        .unwrap_or_default()
        .trim()
        .to_string();
    if api_key.is_empty() {
        return Err(redact("TAVILY_API_KEY is not set"));
    }
    let cleaned = collapse(query);
    if cleaned.is_empty() {
        return Err(redact("empty search query"));
    }
    let endpoint = std::env::var("SOL_O_TAVILY_ENDPOINT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| TAVILY_ENDPOINT.to_string());

    let body = json!({
        "query": cleaned.chars().take(MAX_QUERY_CHARS).collect::<String>(),
        "max_results": 5,
        "search_depth": "basic",
        "include_answer": false,
    });
    let payload = post_with_retry(settings, &endpoint, &api_key, &body)?;
    let raw_results = payload.get("results").and_then(Value::as_array);
    let Some(raw_results) = raw_results else {
        return Err(redact("unexpected Tavily response shape"));
    };
    let mut results = Vec::new();
    for item in raw_results {
        let url = item.get("url").and_then(Value::as_str).unwrap_or("").trim();
        if url.is_empty() {
            continue;
        }
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let content = collapse(item.get("content").and_then(Value::as_str).unwrap_or(""));
        results.push(json!({"title": title, "url": url, "content": content}));
    }
    results.truncate(5);
    Ok(results)
}

fn post_with_retry(
    settings: &Settings,
    endpoint: &str,
    api_key: &str,
    body: &Value,
) -> Result<Value, String> {
    let timeout = Duration::from_secs_f64(settings.tool_timeout_seconds.max(0.001));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| redact(&format!("search request failed: {error}")))?;

    let mut last_error = String::new();
    for attempt in 0..=1 {
        let response = client
            .post(endpoint)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(body)
            .send();
        let (error, transient) = match response {
            Ok(response) => {
                let status = response.status().as_u16();
                let text = response.text().unwrap_or_default();
                if (200..300).contains(&status) {
                    return serde_json::from_str(&text)
                        .map_err(|error| redact(&format!("search request failed: {error}")));
                }
                (
                    format!("search request failed: HTTP {status}"),
                    status >= 500,
                )
            }
            // Connection refused, DNS failure and timeouts are transient.
            Err(error) => (format!("search request failed: {error}"), true),
        };
        last_error = error;
        if !transient || attempt == 1 {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(redact(&last_error))
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

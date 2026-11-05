//! One file-backed agent: prompt loading, output parsing and retry loop.
//!
//! Mirrors `app/agents.py`: the prompt is read per request, the output contract
//! is appended to the system prompt, and malformed output is retried with the
//! validation error fed back as a user turn.

use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use tracing::warn;

use crate::config::{Settings, ROOT};
use crate::llm::{self, Message};
use crate::reliability::redact;
use crate::schemas::{agent_output_schema, AgentOutput};

/// Must match `app/agents.py` byte for byte.
pub const OUTPUT_CONTRACT: &str = "\n\n## Runtime output contract (highest priority)\n\nReturn exactly ONE valid JSON object and nothing else. No prose, no markdown, no code fences.\nIt must have these keys: agent, status, task, summary, findings, conflicts, gaps, clarification_needed.\n- status: \"complete\", \"partial\", or \"needs_clarification\".\n- findings: a list of objects with exactly claim, source_url, confidence.\n- confidence: \"high\", \"medium\", or \"low\".\n- conflicts and gaps: lists of strings (use [] when empty).\n- clarification_needed: null unless status is \"needs_clarification\".\nDo not add extra keys or enum values.\n";

pub const RETRY_INSTRUCTION: &str = "Your previous response could not be used: {error}\n\nReturn the corrected, complete JSON object only. No prose and no markdown fences.";

static AGENT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").expect("static agent name pattern is valid"));

static FENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)^\s*```(?:json)?\s*(.*?)\s*```\s*$").expect("static fence pattern is valid")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureType {
    InvalidJson,
    SchemaMismatch,
    Timeout,
    LlmError,
}

impl FailureType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::SchemaMismatch => "schema_mismatch",
            Self::Timeout => "timeout",
            Self::LlmError => "llm_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRunError {
    pub failure_type: FailureType,
    pub detail: String,
    pub raw: String,
    pub attempts: usize,
}

#[derive(Debug, Clone)]
pub enum RunError {
    /// No readable `agents/<id>.md` exists, or the id is unsafe.
    NotFound(String),
    /// The prompt exists but could not be read.
    Unreadable(String),
    Run(AgentRunError),
}

#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub output: AgentOutput,
    /// Validation attempts; the successful call counts.
    pub attempts: usize,
    /// The validated output serialized as JSON, for the audit row.
    pub raw: String,
    pub duration_ms: i64,
}

#[derive(Debug)]
struct ParseError {
    failure_type: FailureType,
    message: String,
}

/// Read `agents/<agent>.md` on every request.
fn load_system_prompt(agent: &str) -> Result<String, RunError> {
    if !AGENT_NAME.is_match(agent) {
        return Err(RunError::NotFound(agent.to_string()));
    }
    let path = Path::new(ROOT).join("agents").join(format!("{agent}.md"));
    match std::fs::read_to_string(path) {
        Ok(prompt) => Ok(prompt),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(RunError::NotFound(agent.to_string()))
        }
        Err(error) => Err(RunError::Unreadable(error.to_string())),
    }
}

/// Strip optional markdown fences, parse JSON and validate the contract.
fn parse_output(raw: &str) -> Result<AgentOutput, ParseError> {
    let text = raw.trim();
    let text = FENCE
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|body| body.as_str().trim())
        .unwrap_or(text);
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| ParseError {
        failure_type: FailureType::InvalidJson,
        message: format!("invalid JSON: {error}"),
    })?;
    serde_json::from_value(value).map_err(|error| ParseError {
        failure_type: FailureType::SchemaMismatch,
        message: format!("schema validation failed: {error}"),
    })
}

/// Call the provider, validate, and retry malformed output up to `LLM_MAX_RETRIES`.
pub fn run_agent(settings: &Settings, agent: &str, task: &str) -> Result<AgentOutcome, RunError> {
    let started = Instant::now();
    let system_prompt = load_system_prompt(agent)?;
    let schema = agent_output_schema();
    let model_override = llm::model_for(settings, agent);
    let timeout = Duration::from_secs_f64(settings.llm_timeout_seconds.max(0.001));
    let max_attempts = 1 + settings.llm_max_retries.max(0) as usize;

    let mut messages = vec![
        Message::new("system", format!("{system_prompt}{OUTPUT_CONTRACT}")),
        Message::new("user", task),
    ];
    let mut attempts = 0_usize;
    loop {
        let raw = match llm::call_llm(
            settings,
            &messages,
            Some(&schema),
            timeout,
            model_override.as_deref(),
        ) {
            Ok(raw) => raw,
            Err(error) => {
                let failure_type = match error.kind {
                    llm::FailureKind::Timeout => FailureType::Timeout,
                    llm::FailureKind::Error => FailureType::LlmError,
                };
                return Err(RunError::Run(AgentRunError {
                    failure_type,
                    detail: error.detail,
                    raw: String::new(),
                    attempts: (attempts + 1).max(error.attempts),
                }));
            }
        };
        match parse_output(&raw) {
            Ok(output) => {
                return Ok(AgentOutcome {
                    raw: serde_json::to_string(&output).unwrap_or_default(),
                    output,
                    attempts: attempts + 1,
                    duration_ms: started.elapsed().as_millis() as i64,
                })
            }
            Err(error) => {
                attempts += 1;
                if attempts >= max_attempts {
                    return Err(RunError::Run(AgentRunError {
                        failure_type: error.failure_type,
                        detail: redact(&error.message),
                        raw: redact(&raw),
                        attempts,
                    }));
                }
                warn!(
                    attempt = attempts,
                    max_attempts,
                    kind = error.failure_type.as_str(),
                    "invalid output: {}",
                    error.message
                );
                messages.push(Message::new("assistant", raw));
                messages.push(Message::new(
                    "user",
                    RETRY_INSTRUCTION.replace("{error}", &error.message),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_matches_the_reference_text() {
        assert!(OUTPUT_CONTRACT.starts_with("\n\n## Runtime output contract"));
        assert!(OUTPUT_CONTRACT.ends_with("Do not add extra keys or enum values.\n"));
        assert_eq!(
            RETRY_INSTRUCTION,
            "Your previous response could not be used: {error}\n\nReturn the corrected, complete JSON object only. No prose and no markdown fences."
        );
    }

    #[test]
    fn fences_are_stripped_before_parsing() {
        let fenced = "```json\n{\"agent\":\"researcher\",\"status\":\"complete\",\"task\":\"t\",\"summary\":\"s\",\"findings\":[],\"conflicts\":[],\"gaps\":[],\"clarification_needed\":null}\n```";
        assert_eq!(parse_output(fenced).unwrap().agent, "researcher");
    }

    #[test]
    fn unsafe_agent_names_are_unknown_agents() {
        for name in ["../researcher", "researcher/../x", "", "a b"] {
            match load_system_prompt(name) {
                Err(RunError::NotFound(_)) => {}
                other => panic!("{name:?} should be unknown, got {other:?}"),
            }
        }
    }
}

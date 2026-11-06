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

use crate::config::Settings;
use crate::llm::{self, Message};
use crate::reliability::redact;
use crate::schemas::{agent_output_schema, AgentOutput, ToolRequests};
use crate::toolbox::{Toolset, TOOL_BUDGET_INSTRUCTION, TOOL_RESULTS_HEADER};

/// Must match `app/agents.py` byte for byte.
pub const OUTPUT_CONTRACT: &str = "\n\n## Runtime output contract (highest priority)\n\nReturn exactly ONE valid JSON object and nothing else. No prose, no markdown, no code fences.\nIt must have these keys: agent, status, task, summary, findings, conflicts, gaps, clarification_needed.\n- status: \"complete\", \"partial\", or \"needs_clarification\".\n- findings: a list of objects with exactly claim, source_url, confidence.\n- confidence: \"high\", \"medium\", or \"low\".\n- conflicts and gaps: lists of strings (use [] when empty).\n- clarification_needed: null unless status is \"needs_clarification\".\nDo not add extra keys or enum values.\n";

pub const RETRY_INSTRUCTION: &str = "Your previous response could not be used: {error}\n\nReturn the corrected, complete JSON object only. No prose and no markdown fences.";

/// Appended to orchestration steps; `/run` uses [`OUTPUT_CONTRACT`] instead.
pub const STEP_CONTRACT: &str = "\n\n## Runtime output contract (highest priority)\n\nReturn exactly ONE valid JSON object and nothing else. No prose, no markdown, no code fences.\nUse the exact keys from your output format above. Do not add, rename, or omit keys.\n- agent: your own name.\n- status: \"complete\", \"partial\", or \"needs_clarification\".\n- clarification_needed: one question when status is \"needs_clarification\", otherwise null.\nUse only the allowed enum values.\n";

/// The planner is a planet like any other, but never plans itself.
pub const PLANNER_AGENT: &str = "sun";

/// Appended to the planner call; the plan shape must match `SunPlan`.
pub const PLAN_CONTRACT: &str = "\n\n## Runtime output contract (highest priority)\n\nReturn exactly ONE valid JSON object and nothing else. No prose, no markdown, no code fences.\nShape: {\"steps\": [{\"agent\": \"<agent id>\", \"task\": \"<instruction>\"}]}\n- 1 to 7 steps, executed in order.\n- Every agent must come from the available agents list in the user message.\n- Do not add extra keys.\n";

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
    ToolError,
}

impl FailureType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::SchemaMismatch => "schema_mismatch",
            Self::Timeout => "timeout",
            Self::LlmError => "llm_error",
            Self::ToolError => "tool_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRunError {
    pub failure_type: FailureType,
    pub detail: String,
    pub raw: String,
    pub attempts: usize,
    pub tool_turns: usize,
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
    pub duration_ms: i64,
}

#[derive(Debug)]
pub struct ParseError {
    pub failure_type: FailureType,
    pub message: String,
}

impl ParseError {
    pub fn new(failure_type: FailureType, message: impl Into<String>) -> Self {
        Self {
            failure_type,
            message: message.into(),
        }
    }
}

/// The normalized JSON output of one validated agent call.
#[derive(Debug, Clone)]
pub struct ValidatedOutput {
    pub output: serde_json::Value,
    /// Validation attempts; the successful call counts.
    pub attempts: usize,
    pub tool_turns: usize,
    pub duration_ms: i64,
}

/// A file-safe agent id, like `AGENT_NAME_RE.fullmatch`.
pub fn valid_agent_name(name: &str) -> bool {
    AGENT_NAME.is_match(name)
}

/// Read `<agents_dir>/<agent>.md` on every request.
fn load_system_prompt(agents_dir: &Path, agent: &str) -> Result<String, RunError> {
    if !AGENT_NAME.is_match(agent) {
        return Err(RunError::NotFound(agent.to_string()));
    }
    let path = agents_dir.join(format!("{agent}.md"));
    match std::fs::read_to_string(&path) {
        Ok(prompt) => Ok(prompt),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(RunError::NotFound(agent.to_string()))
        }
        Err(error) => Err(RunError::Unreadable(format!("{}: {error}", path.display()))),
    }
}

/// Strip optional markdown fences and parse the payload.
fn decode(raw: &str) -> Result<serde_json::Value, ParseError> {
    let text = raw.trim();
    let text = FENCE
        .captures(text)
        .and_then(|captures| captures.get(1))
        .map(|body| body.as_str().trim())
        .unwrap_or(text);
    serde_json::from_str(text).map_err(|error| {
        ParseError::new(FailureType::InvalidJson, format!("invalid JSON: {error}"))
    })
}

/// Strip fences, parse JSON and validate against the `/run` contract.
#[cfg(test)]
fn parse_output(raw: &str) -> Result<AgentOutput, ParseError> {
    let value = decode(raw)?;
    serde_json::from_value(value).map_err(|error| {
        ParseError::new(
            FailureType::SchemaMismatch,
            format!("schema validation failed: {error}"),
        )
    })
}

/// Call the provider, validate, and retry malformed output up to `LLM_MAX_RETRIES`.
///
/// `on_attempt(call, attempt)` runs before every provider call. A tool envelope
/// consumes a call but not a validation attempt; tools run for at most
/// `max_tool_turns` turns per step. `call_timeout` overrides the configured
/// per-call limit (the Sun passes the smaller of that limit and the remaining
/// run budget); `None` uses the setting.
#[allow(clippy::too_many_arguments)]
pub fn run_validated(
    settings: &Settings,
    agents_dir: &Path,
    agent: &str,
    user_content: &str,
    contract: &str,
    schema: &serde_json::Value,
    parse: &dyn Fn(serde_json::Value) -> Result<serde_json::Value, ParseError>,
    toolset: Option<&Toolset<'_>>,
    call_timeout: Option<Duration>,
    on_attempt: &mut dyn FnMut(usize, usize),
) -> Result<ValidatedOutput, RunError> {
    let started = Instant::now();
    let system_prompt = load_system_prompt(agents_dir, agent)?;
    let model_override = llm::model_for(settings, agent);
    let timeout = call_timeout
        .unwrap_or_else(|| Duration::from_secs_f64(settings.llm_timeout_seconds))
        .max(Duration::from_millis(1));
    let max_attempts = 1 + settings.llm_max_retries.max(0) as usize;
    let max_calls = max_attempts
        + if toolset.is_some() {
            settings.max_tool_turns.max(0) as usize
        } else {
            0
        };
    let tool_section = toolset.map(Toolset::contract).unwrap_or_default();

    let mut messages = vec![
        Message::new("system", format!("{system_prompt}{contract}{tool_section}")),
        Message::new("user", user_content),
    ];
    let mut attempts = 0_usize;
    let mut tool_turns = 0_usize;
    let mut calls = 0_usize;
    while calls < max_calls {
        calls += 1;
        let validation_attempts = calls - tool_turns;
        on_attempt(calls, attempts + 1);
        let raw = match llm::call_llm(
            settings,
            &messages,
            Some(schema),
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
                    attempts: validation_attempts.max(error.attempts),
                    tool_turns,
                }));
            }
        };

        let error: Option<ParseError>;
        match decode(&raw) {
            Err(failure) => error = Some(failure),
            Ok(value) => {
                let is_envelope = value.as_object().is_some_and(|object| {
                    object.contains_key("tool_requests") && !object.contains_key("status")
                });
                if !is_envelope {
                    match parse(value) {
                        Ok(output) => {
                            return Ok(ValidatedOutput {
                                output,
                                attempts: validation_attempts,
                                tool_turns,
                                duration_ms: started.elapsed().as_millis() as i64,
                            })
                        }
                        Err(failure) => error = Some(failure),
                    }
                } else if toolset.is_none() {
                    error = Some(ParseError::new(
                        FailureType::SchemaMismatch,
                        "tool_requests is not available for this agent; return the normal output object",
                    ));
                } else {
                    match serde_json::from_value::<ToolRequests>(value) {
                        Err(failure) => {
                            error = Some(ParseError::new(
                                FailureType::SchemaMismatch,
                                format!("invalid tool_requests: {failure}"),
                            ))
                        }
                        Ok(requests)
                            if requests.tool_requests.is_empty()
                                || requests
                                    .tool_requests
                                    .iter()
                                    .any(|request| request.tool.is_empty()) =>
                        {
                            error = Some(ParseError::new(
                                FailureType::SchemaMismatch,
                                "invalid tool_requests: tool and tool_requests must be non-empty",
                            ))
                        }
                        Ok(requests) => {
                            if tool_turns >= settings.max_tool_turns.max(0) as usize
                                || calls >= max_calls
                            {
                                error = Some(ParseError::new(
                                    FailureType::SchemaMismatch,
                                    TOOL_BUDGET_INSTRUCTION,
                                ));
                            } else {
                                tool_turns += 1;
                                let results: Vec<serde_json::Value> = requests
                                    .tool_requests
                                    .iter()
                                    .map(|request| {
                                        toolset.expect("toolset checked above").run(request)
                                    })
                                    .collect();
                                messages.push(Message::new("assistant", raw));
                                messages.push(Message::new(
                                    "user",
                                    format!(
                                        "{TOOL_RESULTS_HEADER}{}",
                                        serde_json::to_string_pretty(&results)
                                            .expect("tool results serialize")
                                    ),
                                ));
                                continue;
                            }
                        }
                    }
                }
            }
        }

        let error = error.expect("every branch sets an error or continues");
        attempts += 1;
        if attempts >= max_attempts || calls >= max_calls {
            return Err(RunError::Run(AgentRunError {
                failure_type: error.failure_type,
                detail: redact(&error.message),
                raw: redact(&raw),
                attempts: validation_attempts,
                tool_turns,
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

    Err(RunError::Run(AgentRunError {
        failure_type: FailureType::InvalidJson,
        detail: format!("no usable output after {calls} LLM call(s)"),
        raw: String::new(),
        attempts: calls.saturating_sub(tool_turns),
        tool_turns,
    }))
}

/// `/run`: one file-backed agent validated against the full [`AgentOutput`] contract.
pub fn run_agent(
    settings: &Settings,
    agents_dir: &Path,
    agent: &str,
    task: &str,
) -> Result<AgentOutcome, RunError> {
    let schema = agent_output_schema();
    let parse = |value: serde_json::Value| {
        serde_json::from_value::<AgentOutput>(value)
            .map(|output| serde_json::to_value(output).expect("validated output serializes"))
            .map_err(|error| {
                ParseError::new(
                    FailureType::SchemaMismatch,
                    format!("schema validation failed: {error}"),
                )
            })
    };
    let validated = run_validated(
        settings,
        agents_dir,
        agent,
        task,
        OUTPUT_CONTRACT,
        &schema,
        &parse,
        None,
        None,
        &mut |_, _| {},
    )?;
    let output = serde_json::from_value(validated.output)
        .expect("run_validated returns a validated AgentOutput");
    Ok(AgentOutcome {
        output,
        attempts: validated.attempts,
        duration_ms: validated.duration_ms,
    })
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
        let agents_dir = crate::config::root().join("agents");
        for name in ["../researcher", "researcher/../x", "", "a b"] {
            match load_system_prompt(&agents_dir, name) {
                Err(RunError::NotFound(_)) => {}
                other => panic!("{name:?} should be unknown, got {other:?}"),
            }
        }
    }
}

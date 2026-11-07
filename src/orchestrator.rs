//! The Sun: run the hardcoded researcher -> writer -> checker plan end to end.
//!
//! Mirrors the success path of `app/orchestrator.py`: events flow through the
//! bus (persisted, streamed, replayable), every LLM call is audited in
//! `agent_calls`, the logger step stores the run, and the notifier sends the
//! Telegram message, or stores a pending action for human approval, or skips a
//! failed run.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::agents::{
    self, FailureType, ParseError, RunError, PLANNER_AGENT, PLAN_CONTRACT, STEP_CONTRACT,
};
use crate::approvals::{approval_required, create_pending};
use crate::config::{self, Settings};
use crate::events::EventBus;
use crate::llm;
use crate::persistence::{self, Db, NewAgentCall, NewToolCall, MAX_RAW_CHARS};
use crate::reliability::{redact, truncate, Deadline};
use crate::schemas::{self, step_schema, PlannedStep};
use crate::search;
use crate::telegram::{self, TelegramError};
use crate::toolbox::{has_toolset, Toolset};

pub const DEFAULT_PLAN: [&str; 3] = ["researcher", "writer", "checker"];
pub const SYSTEM_STEPS: [&str; 2] = ["logger", "notifier"];
pub const LOGGER_TASK: &str = "Save this run, its draft, verdict and outputs to SQLite.";
pub const NOTIFIER_TASK: &str =
    "Send the task, draft, verdict and issues to the operator on Telegram.";

const SUMMARY_LIMIT: usize = 140;
const SEARCH_QUERY_CHARS: usize = 400;
const REVISION_CHECK_TASK: &str = "Re-review the revised writer draft against the researcher findings and confirm whether every previously reported issue is fixed.";
const NO_RESULTS_NOTE: &str = "No search results are available. Do not invent sources: mark every claim 'unverified' and list what could not be checked in gaps.";
const EMPTY_RESULTS_NOTE: &str =
    "The search returned no results. Mark every claim 'unverified' and list gaps.";
/// A step may only run when the agents it consumes have already produced output.
const REQUIRED_PRIOR_AGENTS: [(&str, &[&str]); 1] = [("checker", &["writer"])];

/// Why `plan_with_llm` stopped, before it is turned into run state.
enum PlannerStop {
    Timeout(String),
    Run(agents::AgentRunError),
    MissingPrompt,
    /// The prompt exists but could not be read; the reference lets this escape.
    Aborted(String),
}

/// A stopped run's structured failure, mirroring `schemas.Failure`.
#[derive(Clone, Debug)]
struct Failure {
    agent: String,
    error_type: FailureType,
    message: String,
    raw_output: String,
    attempts: usize,
    tool_turns: i64,
}

impl Failure {
    fn new(
        agent: &str,
        error_type: FailureType,
        message: &str,
        raw: &str,
        attempts: usize,
        tool_turns: i64,
    ) -> Self {
        Self {
            agent: agent.to_string(),
            error_type,
            message: truncate(&redact(message), MAX_RAW_CHARS),
            raw_output: truncate(&redact(raw), MAX_RAW_CHARS),
            attempts,
            tool_turns,
        }
    }
}

#[derive(Clone, Debug)]
struct StepResult {
    step: i64,
    agent: String,
    task: String,
    attempts: i64,
    duration_seconds: f64,
    output: Value,
    tool_turns: i64,
}

/// Result of one Sun run, matching `schemas.OrchestrationResult` field order.
#[derive(Debug, Serialize)]
pub struct OrchestrationResult {
    pub status: String,
    pub task: String,
    pub planner: &'static str,
    pub plan: Vec<PlannedStep>,
    pub outputs: Vec<Value>,
    pub stopped_at: Option<String>,
    pub reason: Option<String>,
    pub failed_at: Option<String>,
    pub failure: Option<FailureView>,
    pub warning: Option<String>,
    pub run_id: String,
    pub duration_seconds: f64,
}

#[derive(Debug, Serialize)]
pub struct FailureView {
    pub agent: String,
    pub error_type: String,
    pub message: String,
    pub raw_output: String,
    pub attempts: usize,
    pub tool_turns: i64,
}

impl StepResult {
    fn to_value(&self) -> Value {
        json!({
            "step": self.step,
            "agent": self.agent,
            "task": self.task,
            "attempts": self.attempts,
            "duration_seconds": self.duration_seconds,
            "output": self.output,
            "tool_turns": self.tool_turns,
        })
    }
}

/// Emits events from the blocking orchestration thread onto the async bus.
struct Emitter<'a> {
    bus: &'a EventBus,
    handle: &'a tokio::runtime::Handle,
    run_id: &'a str,
}

impl Emitter<'_> {
    fn emit(&self, event: &str, data: Value) {
        let _ = self
            .handle
            .block_on(self.bus.publish(self.run_id, event, data));
    }
}

pub struct Orchestrator {
    pub settings: Arc<Settings>,
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub agents_dir: PathBuf,
}

impl Orchestrator {
    pub fn new(settings: Arc<Settings>, db: Db, bus: Arc<EventBus>) -> Self {
        Self {
            settings,
            db,
            bus,
            agents_dir: config::agents_dir(),
        }
    }

    /// Agents the planner may choose: prompt files with a validation model, minus Sun.
    fn available_agents(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.agents_dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("md")
            })
            .filter_map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .collect();
        names.sort();
        names.retain(|name| {
            name != PLANNER_AGENT
                && agents::valid_agent_name(name)
                && schemas::step_schema(name).is_some()
        });
        names
    }

    /// Ask the Sun planner for a plan, then drop unknown or out-of-order steps
    /// and truncate it at the configured cap.
    fn plan_with_llm(
        &self,
        task: &str,
        run_id: &str,
        emitter: &Emitter<'_>,
        deadline: Deadline,
    ) -> Result<Vec<PlannedStep>, PlannerStop> {
        let agents = self.available_agents();
        let available = agents
            .iter()
            .map(|name| format!("- {name}"))
            .collect::<Vec<_>>()
            .join("\n");
        let user_content = format!("Task:\n{}\n\nAvailable agents:\n{available}", task.trim());

        // The reference refuses to start a call it cannot finish.
        let remaining = deadline.remaining();
        if remaining <= 0.0 {
            return Err(PlannerStop::Timeout(format!(
                "run exceeded {:.1}s before agent={PLANNER_AGENT}",
                deadline.seconds
            )));
        }

        let schema = schemas::plan_schema(&agents, self.settings.max_agents);
        let started_at = persistence::now_iso();
        let started = Instant::now();
        let mut on_attempt = |call: usize, attempt: usize| {
            emitter.emit(
                "agent_working",
                json!({"step": 0, "agent": PLANNER_AGENT, "attempt": attempt, "call": call}),
            );
        };
        let parse = |value: Value| {
            schemas::validate_plan(value)
                .map_err(|message| ParseError::new(FailureType::SchemaMismatch, message))
        };
        let call_timeout =
            Duration::from_secs_f64(self.settings.llm_timeout_seconds.min(remaining).max(0.001));
        let model = resolved_model(&self.settings, PLANNER_AGENT);
        let validated = match agents::run_validated(
            &self.settings,
            &self.agents_dir,
            PLANNER_AGENT,
            &user_content,
            PLAN_CONTRACT,
            &schema,
            &parse,
            None,
            Some(call_timeout),
            &mut on_attempt,
        ) {
            Ok(validated) => validated,
            Err(RunError::Run(error)) => {
                self.record_agent_call(call_row(
                    run_id,
                    0,
                    PLANNER_AGENT,
                    started_at,
                    started,
                    error.attempts as i64,
                    0,
                    model,
                    &user_content,
                    error.raw.chars().count() as i64,
                    error.raw.clone(),
                    Some((&error.failure_type, &error.detail)),
                ));
                return Err(PlannerStop::Run(error));
            }
            Err(RunError::NotFound(_)) => return Err(PlannerStop::MissingPrompt),
            Err(other) => return Err(PlannerStop::Aborted(format!("{other:?}"))),
        };

        let raw_output =
            serde_json::to_string(&validated.output).expect("validated plan serializes");
        self.record_agent_call(call_row(
            run_id,
            0,
            PLANNER_AGENT,
            started_at,
            started,
            validated.attempts as i64,
            0,
            model,
            &user_content,
            raw_output.chars().count() as i64,
            raw_output,
            None,
        ));

        let plan: schemas::SunPlanOutput = serde_json::from_value(validated.output)
            .expect("run_validated returns a validated SunPlan");
        let mut seen: Vec<String> = Vec::new();
        let mut steps: Vec<PlannedStep> = Vec::new();
        for step in plan.steps {
            if !agents.contains(&step.agent) {
                tracing::warn!(agent = %step.agent, "planner dropped unknown agent");
                continue;
            }
            let missing: Vec<&str> = REQUIRED_PRIOR_AGENTS
                .iter()
                .filter(|(agent, _)| *agent == step.agent.as_str())
                .flat_map(|(_, required)| required.iter().copied())
                .filter(|required| !seen.iter().any(|kept| kept.as_str() == *required))
                .collect();
            if !missing.is_empty() {
                tracing::warn!(
                    agent = %step.agent,
                    missing = missing.join(", "),
                    "planner dropped step missing earlier output"
                );
                continue;
            }
            if steps.len() as i64 == self.settings.max_agents {
                tracing::warn!(
                    max_agents = self.settings.max_agents,
                    "planner exceeded the step cap; truncating"
                );
                break;
            }
            seen.push(step.agent.clone());
            steps.push(PlannedStep {
                agent: step.agent,
                task: step.task,
            });
        }
        Ok(steps)
    }

    /// Run one orchestration. Must be called from a blocking context.
    pub fn run(
        &self,
        task: &str,
        use_llm_planner: bool,
        run_id: &str,
        source: &str,
    ) -> Result<OrchestrationResult, String> {
        let started = Instant::now();
        let deadline = Deadline::new(started, self.settings.run_timeout_seconds);
        let handle = tokio::runtime::Handle::current();
        let emitter = Emitter {
            bus: &self.bus,
            handle: &handle,
            run_id,
        };

        // The reference truncates before writing the row, so both the stored
        // task and every prompt see the capped text.
        let mut task = task.to_string();
        let mut warning: Option<String> = None;
        let max_input_chars = self.settings.max_input_chars.max(0) as usize;
        let task_chars = task.chars().count();
        if task_chars > max_input_chars {
            warning = Some(format!(
                "task truncated from {task_chars} to {max_input_chars} characters"
            ));
            task = task.chars().take(max_input_chars).collect();
            tracing::warn!(%task_chars, max_input_chars, "task truncated");
        }

        // The HTTP layer already inserted the row; the Sun retries it like the
        // reference, and a failure here never fails the run.
        if let Err(error) = self.db.start_run(run_id, &task, source) {
            tracing::error!(run_id, %error, "could not create run row");
            emitter.emit(
                "logger_failed",
                json!({"phase": "start", "run_id": run_id, "reason": error}),
            );
        }

        emitter.emit("run_started", json!({"task": redact(&task)}));

        let planner = if use_llm_planner { "llm" } else { "hardcoded" };
        let mut outputs: Vec<StepResult> = Vec::new();
        let mut status = "complete";
        let mut stopped_at: Option<String> = None;
        let mut reason: Option<String> = None;
        let mut failed_at: Option<String> = None;
        let mut failure: Option<Failure> = None;
        let mut plan: Vec<PlannedStep> = Vec::new();

        if use_llm_planner {
            match self.plan_with_llm(&task, run_id, &emitter, deadline) {
                Ok(steps) => plan = steps,
                Err(stop) => {
                    let stop_reason = match stop {
                        PlannerStop::Timeout(message) => {
                            status = "failed";
                            failed_at = Some(PLANNER_AGENT.to_string());
                            stopped_at = Some(PLANNER_AGENT.to_string());
                            failure = Some(Failure::new(
                                PLANNER_AGENT,
                                FailureType::Timeout,
                                &format!("run_timeout: {message}"),
                                "",
                                0,
                                0,
                            ));
                            "run_timeout".to_string()
                        }
                        PlannerStop::Run(error) => {
                            status = "failed";
                            failed_at = Some(PLANNER_AGENT.to_string());
                            stopped_at = Some(PLANNER_AGENT.to_string());
                            failure = Some(Failure::new(
                                PLANNER_AGENT,
                                error.failure_type,
                                &format!("planner failed: {}", error.detail),
                                &error.raw,
                                error.attempts,
                                0,
                            ));
                            format!("{}: {}", error.failure_type.as_str(), error.detail)
                        }
                        PlannerStop::MissingPrompt => {
                            status = "failed";
                            failed_at = Some(PLANNER_AGENT.to_string());
                            stopped_at = Some(PLANNER_AGENT.to_string());
                            let text =
                                format!("llm_error: planner prompt missing ({PLANNER_AGENT})");
                            failure = Some(Failure::new(
                                PLANNER_AGENT,
                                FailureType::LlmError,
                                &text,
                                "",
                                1,
                                0,
                            ));
                            text
                        }
                        PlannerStop::Aborted(message) => return Err(message),
                    };
                    reason = Some(stop_reason.clone());
                    emitter.emit(
                        "stopped",
                        json!({
                            "step": 0,
                            "agent": PLANNER_AGENT,
                            "status": status,
                            "reason": stop_reason,
                        }),
                    );
                }
            }
            // An empty plan is a planner failure, not an empty run.
            if reason.is_none() && plan.is_empty() {
                status = "failed";
                failed_at = Some(PLANNER_AGENT.to_string());
                stopped_at = Some(PLANNER_AGENT.to_string());
                let text = "planner returned no usable steps".to_string();
                failure = Some(Failure::new(
                    PLANNER_AGENT,
                    FailureType::InvalidJson,
                    &text,
                    "",
                    1,
                    0,
                ));
                reason = Some(text.clone());
                emitter.emit(
                    "stopped",
                    json!({
                        "step": 0,
                        "agent": PLANNER_AGENT,
                        "status": status,
                        "reason": text,
                    }),
                );
            }
        } else {
            plan = DEFAULT_PLAN
                .iter()
                .map(|agent| PlannedStep {
                    agent: (*agent).to_string(),
                    task: task.clone(),
                })
                .collect();
        }

        if reason.is_none() {
            emitter.emit(
                "plan_ready",
                json!({
                    "planner": planner,
                    "agents": plan.iter().map(|step| step.agent.clone()).collect::<Vec<_>>(),
                    "steps": plan.iter().map(|step| json!({"agent": step.agent, "task": step.task})).collect::<Vec<_>>(),
                }),
            );
        }

        let mut search_block: Option<Value> = None;
        let mut revision_used = false;
        let mut index = 0;

        while reason.is_none() && index < plan.len() {
            let step = plan[index].clone();
            let number = outputs.len() as i64 + 1;
            emitter.emit("agent_called", json!({"step": number, "agent": step.agent}));

            // The whole-run budget is checked before the call, never mid-call:
            // a step that never starts records no audit row.
            if deadline.expired() {
                let message = format!(
                    "run exceeded {:.1}s before agent={}",
                    deadline.seconds, step.agent
                );
                status = "failed";
                failed_at = Some(step.agent.clone());
                stopped_at = Some(step.agent.clone());
                reason = Some("run_timeout".to_string());
                failure = Some(Failure::new(
                    &step.agent,
                    FailureType::Timeout,
                    &format!("run_timeout: {message}"),
                    "",
                    0,
                    0,
                ));
                emitter.emit(
                    "agent_failed",
                    json!({
                        "step": number,
                        "agent": step.agent,
                        "error": "timeout",
                        "error_type": "timeout",
                        "message": message,
                        "attempts": 0,
                    }),
                );
                emitter.emit(
                    "stopped",
                    json!({
                        "step": number,
                        "agent": step.agent,
                        "status": status,
                        "reason": reason,
                    }),
                );
                break;
            }

            let mut extra = String::new();
            if step.agent == "researcher" && search_block.is_none() {
                emitter.emit(
                    "agent_working",
                    json!({"step": number, "agent": step.agent, "attempt": 1, "call": 1}),
                );
                if deadline.expired() {
                    let message = "run exceeded its time budget before search";
                    status = "failed";
                    failed_at = Some(step.agent.clone());
                    stopped_at = Some(step.agent.clone());
                    reason = Some("run_timeout".to_string());
                    failure = Some(Failure::new(
                        &step.agent,
                        FailureType::Timeout,
                        &format!("run_timeout: {message}"),
                        "",
                        0,
                        0,
                    ));
                    emitter.emit(
                        "stopped",
                        json!({
                            "step": number,
                            "agent": step.agent,
                            "status": status,
                            "reason": reason,
                        }),
                    );
                    break;
                }
                let block = self.search_context(&task, run_id, &emitter);
                extra = format!(
                    "Web search results (JSON):\n{}",
                    serde_json::to_string_pretty(&block).expect("search context serializes")
                );
                search_block = Some(block);
            }

            let message = agent_message(&task, &step.task, &outputs, &extra);
            let Some(schema) = step_schema(&step.agent) else {
                return Err(format!("unknown agent: {}", step.agent));
            };
            let parse_error =
                |message: String| ParseError::new(FailureType::SchemaMismatch, message);
            let parse = |value: Value| {
                schemas::validate_step_output(&step.agent, value).map_err(parse_error)
            };
            let started_at = persistence::now_iso();
            let step_started = Instant::now();
            let mut on_attempt = |call: usize, attempt: usize| {
                emitter.emit(
                    "agent_working",
                    json!({
                        "step": number,
                        "agent": step.agent,
                        "attempt": attempt,
                        "call": call,
                    }),
                );
            };

            let tool_emit = |event: &str, data: Value| emitter.emit(event, data);
            let toolset = has_toolset(&step.agent).then(|| {
                Toolset::new(
                    &self.settings,
                    &self.db,
                    &tool_emit,
                    &step.agent,
                    number,
                    run_id,
                    source,
                    deadline,
                )
            });
            // Each call gets the smaller of the configured limit and what is
            // left of the run budget.
            let call_timeout = Duration::from_secs_f64(
                self.settings
                    .llm_timeout_seconds
                    .min(deadline.remaining())
                    .max(0.001),
            );
            let validated = agents::run_validated(
                &self.settings,
                &self.agents_dir,
                &step.agent,
                &message,
                STEP_CONTRACT,
                &schema,
                &parse,
                toolset.as_ref(),
                Some(call_timeout),
                &mut on_attempt,
            );

            let validated = match validated {
                Ok(validated) => validated,
                Err(RunError::Run(error)) => {
                    // A validation error that lands after the run budget ran
                    // out is reported as a timeout, like the reference.
                    let timed_out = deadline.expired();
                    let error_type = if timed_out {
                        FailureType::Timeout
                    } else {
                        error.failure_type
                    };
                    let reason_text = if timed_out {
                        "run_timeout".to_string()
                    } else {
                        format!("{}: {}", error.failure_type.as_str(), error.detail)
                    };
                    let failed = Failure::new(
                        &step.agent,
                        error_type,
                        &error.detail,
                        &error.raw,
                        error.attempts,
                        error.tool_turns as i64,
                    );
                    self.record_agent_call(call_row(
                        run_id,
                        number,
                        &failed.agent,
                        started_at,
                        step_started,
                        failed.attempts as i64,
                        failed.tool_turns,
                        resolved_model(&self.settings, &step.agent),
                        &message,
                        error.raw.chars().count() as i64,
                        failed.raw_output.clone(),
                        Some((&failed.error_type, &failed.message)),
                    ));
                    emitter.emit(
                        "agent_failed",
                        json!({
                            "step": number,
                            "agent": step.agent,
                            "error": failed.error_type.as_str(),
                            "error_type": failed.error_type.as_str(),
                            "message": error.detail,
                            "attempts": error.attempts,
                            "tool_turns": failed.tool_turns,
                        }),
                    );
                    emitter.emit(
                        "stopped",
                        json!({
                            "step": number,
                            "agent": step.agent,
                            "status": "failed",
                            "reason": reason_text,
                        }),
                    );
                    status = "failed";
                    failed_at = Some(step.agent.clone());
                    stopped_at = Some(step.agent.clone());
                    reason = Some(reason_text);
                    failure = Some(failed);
                    break;
                }
                Err(other) => return Err(format!("{other:?}")),
            };

            let raw_output =
                serde_json::to_string(&validated.output).expect("validated output serializes");
            self.record_agent_call(call_row(
                run_id,
                number,
                &step.agent,
                started_at,
                step_started,
                validated.attempts as i64,
                validated.tool_turns as i64,
                resolved_model(&self.settings, &step.agent),
                &message,
                raw_output.chars().count() as i64,
                raw_output,
                None,
            ));

            let duration_seconds = round2(validated.duration_ms as f64 / 1000.0);
            let summary = summary(&step.agent, &validated.output);
            let step_status = validated
                .output
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("complete")
                .to_string();
            outputs.push(StepResult {
                step: number,
                agent: step.agent.clone(),
                task: step.task.clone(),
                attempts: validated.attempts as i64,
                duration_seconds,
                output: validated.output,
                tool_turns: validated.tool_turns as i64,
            });
            emitter.emit(
                "agent_done",
                json!({
                    "step": number,
                    "agent": step.agent,
                    "status": step_status,
                    "attempts": validated.attempts,
                    "tool_turns": validated.tool_turns,
                    "duration_seconds": duration_seconds,
                    "summary": summary,
                }),
            );

            let output = &outputs.last().expect("agent step just pushed").output;
            if output.get("status").and_then(Value::as_str) == Some("needs_clarification") {
                let clarification = output
                    .get("clarification_needed")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .unwrap_or("agent requested clarification")
                    .to_string();
                status = "needs_clarification";
                stopped_at = Some(step.agent.clone());
                reason = Some(clarification);
                emitter.emit(
                    "stopped",
                    json!({
                        "step": number,
                        "agent": step.agent,
                        "status": status,
                        "reason": reason,
                    }),
                );
                break;
            }

            if step.agent == "checker"
                && output.get("verdict").and_then(Value::as_str) == Some("needs_changes")
            {
                let issues: Vec<Value> = output
                    .get("issues")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if revision_used {
                    status = "partial";
                    stopped_at = Some("checker".to_string());
                    reason = Some(format!(
                        "checker still rejected the revised draft with {} issue(s)",
                        issues.len()
                    ));
                    emitter.emit(
                        "stopped",
                        json!({
                            "step": number,
                            "agent": step.agent,
                            "status": status,
                            "reason": reason,
                        }),
                    );
                    break;
                }
                revision_used = true;
                emitter.emit(
                    "revision_requested",
                    json!({"step": number, "issues": issues.len()}),
                );
                plan.insert(
                    index + 1,
                    PlannedStep {
                        agent: "writer".to_string(),
                        task: revision_task(&issues),
                    },
                );
                plan.insert(
                    index + 2,
                    PlannedStep {
                        agent: "checker".to_string(),
                        task: REVISION_CHECK_TASK.to_string(),
                    },
                );
            }

            index += 1;
        }

        let final_draft = final_draft(&outputs);
        let final_verdict = final_verdict(&outputs);
        let final_issues = final_issues(&outputs);

        // Decide the notifier path before the logger row is written so its
        // status is accurate: failed runs skip, approval stores a pending row.
        let mut notify_mode = "send";
        let mut pending_action_id: Option<i64> = None;
        let mut notify_error: Option<String> = None;
        if status == "failed" {
            notify_mode = "skipped";
        } else if approval_required(&self.settings) {
            let payload = approval_payload(
                &task,
                status,
                &final_draft,
                final_verdict.as_deref(),
                &final_issues,
                reason.as_deref(),
            );
            match create_pending(&self.db, run_id, &payload) {
                Ok(action_id) => {
                    pending_action_id = Some(action_id);
                    notify_mode = "pending";
                    status = "awaiting_approval";
                    if stopped_at.is_none() {
                        stopped_at = Some("notifier".to_string());
                    }
                    if reason.is_none() {
                        reason = Some(format!(
                            "awaiting human approval for pending action {action_id}"
                        ));
                    }
                    let preview = payload["message"]
                        .as_str()
                        .unwrap_or_default()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    let preview: String = preview.chars().take(SUMMARY_LIMIT).collect();
                    emitter.emit(
                        "approval_required",
                        json!({
                            "run_id": run_id,
                            "action_id": action_id,
                            "status": status,
                            "payload": {
                                "task": payload["task"],
                                "draft": payload["draft"],
                                "verdict": &final_verdict,
                                "issues": &final_issues,
                            },
                        }),
                    );
                    emitter.emit(
                        "awaiting_approval",
                        json!({"pending_id": action_id, "preview": preview}),
                    );
                }
                Err(error) => {
                    // Never send without a stored approval.
                    tracing::error!(run_id, %error, "could not store pending action");
                    notify_mode = "error";
                    let notify_error_text = redact(&error);
                    notify_error = Some(notify_error_text.clone());
                    status = "failed";
                    failed_at = Some("notifier".to_string());
                    stopped_at = Some("notifier".to_string());
                    reason = Some(format!("tool_error: {notify_error_text}"));
                    failure = Some(Failure::new(
                        "notifier",
                        FailureType::ToolError,
                        &format!("tool_error: {notify_error_text}"),
                        "",
                        0,
                        0,
                    ));
                    emitter.emit(
                        "notifier_failed",
                        json!({"agent": "notifier", "reason": notify_error}),
                    );
                }
            }
        }

        // Logger: always runs, success or failure.
        let number = outputs.len() as i64 + 1;
        emitter.emit("agent_called", json!({"step": number, "agent": "logger"}));
        let step_started = Instant::now();
        let outputs_json = Value::Array(outputs.iter().map(StepResult::to_value).collect());
        let log_output = match self.db.save_run(
            run_id,
            &task,
            status,
            &final_draft,
            final_verdict.as_deref(),
            &outputs_json,
            started.elapsed().as_secs_f64(),
            warning.as_deref(),
        ) {
            Ok(()) => {
                emitter.emit(
                    "logger_saved",
                    json!({"step": number, "agent": "logger", "run_id": run_id}),
                );
                json!({
                    "agent": "logger",
                    "status": "complete",
                    "run_id": run_id,
                    "database": self.db.path().display().to_string(),
                })
            }
            Err(error) => {
                tracing::error!(run_id, %error, "could not save run");
                emitter.emit(
                    "logger_failed",
                    json!({"phase": "save", "run_id": run_id, "reason": error}),
                );
                json!({"agent": "logger", "status": "failed", "run_id": run_id, "error": error})
            }
        };
        let log_status = log_output["status"]
            .as_str()
            .unwrap_or("failed")
            .to_string();
        outputs.push(system_step(
            number,
            "logger",
            log_output,
            step_started.elapsed().as_secs_f64(),
        ));
        emitter.emit(
            "agent_done",
            json!({
                "step": number,
                "agent": "logger",
                "status": log_status,
                "attempts": 1,
                "summary": summary("logger", &outputs.last().expect("logger step").output),
            }),
        );

        // Notifier: sends, waits for approval, or skips failed runs.
        let number = outputs.len() as i64 + 1;
        emitter.emit("agent_called", json!({"step": number, "agent": "notifier"}));
        let step_started = Instant::now();
        let notify_output = match notify_mode {
            "pending" => {
                let action_id = pending_action_id.unwrap_or_default();
                emitter.emit(
                    "notifier_pending",
                    json!({"step": number, "action_id": action_id}),
                );
                json!({
                    "agent": "notifier",
                    "status": "awaiting_approval",
                    "pending_action_id": action_id,
                })
            }
            "skipped" => {
                emitter.emit("notifier_skipped", json!({"step": number}));
                json!({"agent": "notifier", "status": "skipped", "reason": "run failed; nothing sent"})
            }
            "error" => json!({
                "agent": "notifier",
                "status": "failed",
                "error": notify_error.unwrap_or_else(|| "could not store pending action".to_string()),
            }),
            _ => {
                let text = notification_text(
                    &task,
                    status,
                    &final_draft,
                    final_verdict.as_deref(),
                    &final_issues,
                    reason.as_deref(),
                );
                emitter.emit(
                    "agent_working",
                    json!({"step": number, "agent": "notifier", "attempt": 1, "call": 1}),
                );
                if deadline.expired() {
                    // The budget covers delivery too: never start a send it cannot finish.
                    let message = "run exceeded its time budget before sending";
                    status = "failed";
                    failed_at = Some("notifier".to_string());
                    stopped_at = Some("notifier".to_string());
                    reason = Some("run_timeout".to_string());
                    failure = Some(Failure::new(
                        "notifier",
                        FailureType::Timeout,
                        &format!("run_timeout: {message}"),
                        "",
                        0,
                        0,
                    ));
                    if let Err(error) = self.db.set_run_status(run_id, status) {
                        tracing::error!(run_id, %error, "could not mark run failed");
                    }
                    emitter.emit(
                        "notifier_failed",
                        json!({"step": number, "reason": message}),
                    );
                    json!({"agent": "notifier", "status": "failed", "error": message})
                } else {
                    match send_telegram(&self.db, run_id, &text, &self.settings) {
                        Ok(delivery) => {
                            emitter.emit(
                                "notifier_sent",
                                json!({
                                    "step": number,
                                    "dry_run": delivery.dry_run,
                                    "delivered": delivery.delivered,
                                    "parts": delivery.parts,
                                }),
                            );
                            json!({
                                "agent": "notifier",
                                "status": "complete",
                                "dry_run": delivery.dry_run,
                                "delivered": delivery.delivered,
                                "parts": delivery.parts,
                            })
                        }
                        Err(error) => {
                            let message = error.message().to_string();
                            status = "failed";
                            failed_at = Some("notifier".to_string());
                            stopped_at = Some("notifier".to_string());
                            reason = Some(format!("tool_error: {message}"));
                            failure = Some(Failure::new(
                                "notifier",
                                FailureType::ToolError,
                                &format!("tool_error: {message}"),
                                "",
                                0,
                                0,
                            ));
                            if let Err(error) = self.db.set_run_status(run_id, status) {
                                tracing::error!(run_id, %error, "could not mark run failed");
                            }
                            emitter.emit(
                                "notifier_failed",
                                json!({"step": number, "reason": message}),
                            );
                            json!({"agent": "notifier", "status": "failed", "error": message})
                        }
                    }
                }
            }
        };
        let notify_status = notify_output["status"]
            .as_str()
            .unwrap_or("skipped")
            .to_string();
        outputs.push(system_step(
            number,
            "notifier",
            notify_output,
            step_started.elapsed().as_secs_f64(),
        ));
        emitter.emit(
            "agent_done",
            json!({
                "step": number,
                "agent": "notifier",
                "status": notify_status,
                "attempts": 1,
                "summary": summary("notifier", &outputs.last().expect("notifier step").output),
            }),
        );

        tracing::info!(
            run_id,
            status,
            stopped_at = stopped_at.as_deref().unwrap_or(""),
            steps = outputs.len(),
            "orchestration finished"
        );

        if status == "failed" {
            let error_type = failure
                .as_ref()
                .map(|failed| failed.error_type.as_str())
                .unwrap_or("llm_error");
            emitter.emit(
                "run_failed",
                json!({
                    "failed_at": failed_at,
                    "error_type": error_type,
                    "message": reason.clone().unwrap_or_else(|| "run failed".to_string()),
                }),
            );
        } else if status != "awaiting_approval" {
            let duration = round2(started.elapsed().as_secs_f64());
            emitter.emit(
                "all_done",
                json!({
                    "status": status,
                    "seconds": duration,
                    "steps": outputs.len(),
                    "duration_seconds": duration,
                }),
            );
        }
        // The reference reports the plan it executed, system steps included.
        for agent in SYSTEM_STEPS {
            plan.push(PlannedStep {
                agent: agent.to_string(),
                task: if agent == "logger" {
                    LOGGER_TASK.to_string()
                } else {
                    NOTIFIER_TASK.to_string()
                },
            });
        }

        Ok(OrchestrationResult {
            status: status.to_string(),
            task,
            planner,
            plan,
            outputs: outputs.iter().map(StepResult::to_value).collect(),
            stopped_at,
            reason,
            failed_at,
            failure: failure.map(|failed| FailureView {
                agent: failed.agent,
                error_type: failed.error_type.as_str().to_string(),
                message: failed.message,
                raw_output: failed.raw_output,
                attempts: failed.attempts,
                tool_turns: failed.tool_turns,
            }),
            warning,
            run_id: run_id.to_string(),
            duration_seconds: round2(started.elapsed().as_secs_f64()),
        })
    }

    fn record_agent_call(&self, call: NewAgentCall) {
        if let Err(error) = self.db.save_agent_call(&call) {
            tracing::error!(agent = call.agent, %error, "could not write agent_calls row");
        }
    }

    fn record_tool_call(&self, call: NewToolCall) {
        if let Err(error) = self.db.save_tool_call(&call) {
            tracing::error!(tool = call.tool, %error, "could not write tool_calls row");
        }
    }

    /// One Tavily search per run; failures degrade to a no-results note.
    fn search_context(&self, task: &str, run_id: &str, emitter: &Emitter<'_>) -> Value {
        let query: String = task
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(SEARCH_QUERY_CHARS)
            .collect();
        let started_at = persistence::now_iso();
        let started = Instant::now();
        match search::search(&self.settings, &query) {
            Err(reason) => {
                self.record_tool_call(NewToolCall {
                    run_id: run_id.to_string(),
                    tool: "search".to_string(),
                    started_at,
                    duration_ms: started.elapsed().as_millis() as i64,
                    status: "failed".to_string(),
                    error_message: Some(reason.clone()),
                    args: String::new(),
                });
                emitter.emit("search_failed", json!({"query": query, "reason": reason}));
                json!({"query": query, "results": [], "note": NO_RESULTS_NOTE})
            }
            Ok(results) => {
                self.record_tool_call(NewToolCall {
                    run_id: run_id.to_string(),
                    tool: "search".to_string(),
                    started_at,
                    duration_ms: started.elapsed().as_millis() as i64,
                    status: "ok".to_string(),
                    error_message: None,
                    args: String::new(),
                });
                emitter.emit(
                    "search_done",
                    json!({"query": query, "results": results.len()}),
                );
                let note = if results.is_empty() {
                    EMPTY_RESULTS_NOTE
                } else {
                    ""
                };
                json!({"query": query, "results": results, "note": note})
            }
        }
    }
}

/// Telegram message shown to the operator, mirroring `_notification_text`.
fn notification_text(
    task: &str,
    status: &str,
    draft: &str,
    verdict: Option<&str>,
    issues: &[Value],
    reason: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("Sol-O run: {status}"),
        format!("Task: {task}"),
        format!("Checker verdict: {}", verdict.unwrap_or("n/a")),
        String::new(),
        "Draft:".to_string(),
        if draft.is_empty() {
            "(no draft)".to_string()
        } else {
            draft.to_string()
        },
    ];
    if !issues.is_empty() {
        lines.push(String::new());
        lines.push("Issues:".to_string());
        for issue in issues {
            lines.push(format!(
                "- {} ({})",
                issue.get("problem").and_then(Value::as_str).unwrap_or(""),
                issue.get("location").and_then(Value::as_str).unwrap_or(""),
            ));
        }
    } else if let Some(reason) = reason {
        lines.push(String::new());
        lines.push(format!("Note: {reason}"));
    }
    redact(&lines.join("\n"))
}

/// Payload stored for a pending action, mirroring `_approval_payload`.
fn approval_payload(
    task: &str,
    status: &str,
    draft: &str,
    verdict: Option<&str>,
    issues: &[Value],
    reason: Option<&str>,
) -> Value {
    json!({
        "run_status": status,
        "task": redact(task),
        "draft": redact(draft),
        "verdict": verdict,
        "issues": issues,
        "reason": reason.map(redact),
        "message": notification_text(task, status, draft, verdict, issues, reason),
    })
}

/// Send through the notifier and audit the tool call.
fn send_telegram(
    db: &Db,
    run_id: &str,
    text: &str,
    settings: &Settings,
) -> Result<telegram::Delivery, TelegramError> {
    let started_at = persistence::now_iso();
    let started = Instant::now();
    match telegram::send_message(settings, text) {
        Ok(delivery) => {
            record_telegram_call(db, run_id, &started_at, started, "ok", None);
            Ok(delivery)
        }
        Err(error) => {
            record_telegram_call(
                db,
                run_id,
                &started_at,
                started,
                "failed",
                Some(error.message()),
            );
            Err(error)
        }
    }
}

fn record_telegram_call(
    db: &Db,
    run_id: &str,
    started_at: &str,
    started: Instant,
    status: &str,
    error_message: Option<&str>,
) {
    let call = NewToolCall {
        run_id: run_id.to_string(),
        tool: "telegram".to_string(),
        started_at: started_at.to_string(),
        duration_ms: started.elapsed().as_millis() as i64,
        status: status.to_string(),
        error_message: error_message.map(str::to_string),
        args: String::new(),
    };
    if let Err(error) = db.save_tool_call(&call) {
        tracing::error!(run_id, %error, "could not write telegram tool_calls row");
    }
}

fn resolved_model(settings: &Settings, agent: &str) -> String {
    llm::model_for(settings, agent).unwrap_or_else(|| settings.model.clone())
}

/// One `agent_calls` row from the fields every call site already has.
#[allow(clippy::too_many_arguments)]
fn call_row(
    run_id: &str,
    step: i64,
    agent: &str,
    started_at: String,
    started: Instant,
    attempts: i64,
    tool_turns: i64,
    model: String,
    input: &str,
    output_chars: i64,
    raw_output: String,
    error: Option<(&FailureType, &str)>,
) -> NewAgentCall {
    NewAgentCall {
        run_id: run_id.to_string(),
        step,
        agent: agent.to_string(),
        started_at,
        duration_ms: started.elapsed().as_millis() as i64,
        attempts,
        tool_turns,
        status: if error.is_some() { "failed" } else { "ok" }.to_string(),
        error_type: error.map(|(kind, _)| kind.as_str().to_string()),
        error_message: error.map(|(_, message)| message.to_string()),
        input_chars: input.chars().count() as i64,
        output_chars,
        raw_output,
        model,
    }
}

/// Original task + this step's instruction + every earlier validated output.
fn agent_message(task: &str, instruction: &str, earlier: &[StepResult], extra: &str) -> String {
    let prior: Vec<Value> = earlier
        .iter()
        .map(|item| json!({"agent": item.agent, "output": item.output}))
        .collect();
    let mut message = format!(
        "Original task:\n{task}\n\nYour step instruction:\n{instruction}\n\nOutputs from earlier agents (JSON):\n{}",
        serde_json::to_string_pretty(&prior).expect("prior outputs serialize")
    );
    if !extra.is_empty() {
        message.push_str("\n\n");
        message.push_str(extra);
    }
    message
}

/// The writer's revision instruction, embedding the checker's issues verbatim.
fn revision_task(issues: &[Value]) -> String {
    let payload = serde_json::to_string_pretty(issues).expect("checker issues serialize");
    format!(
        "Revise your draft so it resolves every issue the checker reported. Use only facts from the earlier outputs; do not add new claims. Return the complete revised draft.\nIssues:\n{payload}"
    )
}

fn system_step(number: i64, agent: &str, output: Value, seconds: f64) -> StepResult {
    StepResult {
        step: number,
        agent: agent.to_string(),
        task: if agent == "logger" {
            LOGGER_TASK.to_string()
        } else {
            NOTIFIER_TASK.to_string()
        },
        attempts: 1,
        duration_seconds: round2(seconds),
        output,
        tool_turns: 0,
    }
}

fn final_draft(outputs: &[StepResult]) -> String {
    for item in outputs.iter().rev() {
        if item.agent == "writer" {
            if let Some(draft) = item.output.get("draft").and_then(Value::as_str) {
                if !draft.is_empty() {
                    return draft.to_string();
                }
            }
        }
    }
    String::new()
}

fn final_verdict(outputs: &[StepResult]) -> Option<String> {
    for item in outputs.iter().rev() {
        if item.agent == "checker" {
            if let Some(verdict) = item.output.get("verdict").and_then(Value::as_str) {
                if !verdict.is_empty() {
                    return Some(verdict.to_string());
                }
            }
        }
    }
    None
}

fn final_issues(outputs: &[StepResult]) -> Vec<Value> {
    for item in outputs.iter().rev() {
        if item.agent == "checker" {
            if let Some(issues) = item.output.get("issues").and_then(Value::as_array) {
                return issues
                    .iter()
                    .filter(|issue| issue.is_object())
                    .cloned()
                    .collect();
            }
        }
    }
    Vec::new()
}

/// One-line human summary for the planet label, capped like the reference.
fn summary(agent: &str, output: &Value) -> String {
    for key in ["summary", "draft", "verdict", "status"] {
        if let Some(value) = output.get(key) {
            let text = value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "));
            if let Some(text) = text {
                let count = text.chars().count();
                if count <= SUMMARY_LIMIT {
                    return text;
                }
                return format!("{}…", truncate(&text, SUMMARY_LIMIT - 1));
            }
        }
    }
    format!("{agent} finished")
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_prefer_summary_then_draft_then_verdict() {
        assert_eq!(
            summary("researcher", &json!({"summary": "  a   b "})),
            "a b"
        );
        assert_eq!(
            summary("writer", &json!({"draft": "the draft"})),
            "the draft"
        );
        assert_eq!(
            summary("checker", &json!({"verdict": "approved"})),
            "approved"
        );
        assert_eq!(
            summary("logger", &json!({"status": "complete"})),
            "complete"
        );
        assert_eq!(summary("notifier", &json!({})), "notifier finished");
    }

    #[test]
    fn summaries_are_capped_with_an_ellipsis() {
        let long = "x".repeat(200);
        let capped = summary("writer", &json!({"draft": long}));
        assert_eq!(capped.chars().count(), SUMMARY_LIMIT);
        assert!(capped.ends_with('…'));
    }

    #[test]
    fn messages_carry_the_task_instruction_and_prior_outputs() {
        let earlier = vec![StepResult {
            step: 1,
            agent: "researcher".to_string(),
            task: "t".to_string(),
            attempts: 1,
            duration_seconds: 0.1,
            output: json!({"agent": "researcher", "summary": "s"}),
            tool_turns: 0,
        }];
        let message = agent_message("the task", "the instruction", &earlier, "extra");
        assert!(message.starts_with("Original task:\nthe task\n\nYour step instruction:\nthe instruction\n\nOutputs from earlier agents (JSON):\n["));
        assert!(message.contains("\"agent\": \"researcher\""));
        assert!(message.ends_with("\n\nextra"));
    }

    #[test]
    fn default_plan_is_researcher_writer_checker() {
        assert_eq!(DEFAULT_PLAN, ["researcher", "writer", "checker"]);
        assert_eq!(SYSTEM_STEPS, ["logger", "notifier"]);
    }
}

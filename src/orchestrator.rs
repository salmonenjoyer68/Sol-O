//! The Sun: run the hardcoded researcher -> writer -> checker plan end to end.
//!
//! Mirrors the success path of `app/orchestrator.py`: events flow through the
//! bus (persisted, streamed, replayable), every LLM call is audited in
//! `agent_calls`, the logger step stores the run, and the notifier only records
//! a status for now.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::agents::{self, FailureType, ParseError, RunError, STEP_CONTRACT};
use crate::config::{Settings, ROOT};
use crate::events::EventBus;
use crate::llm;
use crate::persistence::{self, Db, NewAgentCall, NewToolCall};
use crate::reliability::{redact, truncate};
use crate::schemas::{self, step_schema};
use crate::search::{SearchTool, TavilySearch};

pub const DEFAULT_PLAN: [&str; 3] = ["researcher", "writer", "checker"];
pub const SYSTEM_STEPS: [&str; 2] = ["logger", "notifier"];
pub const LOGGER_TASK: &str = "Save this run, its draft, verdict and outputs to SQLite.";
pub const NOTIFIER_TASK: &str =
    "Send the task, draft, verdict and issues to the operator on Telegram.";

const SUMMARY_LIMIT: usize = 140;
const SEARCH_QUERY_CHARS: usize = 400;
const NO_RESULTS_NOTE: &str = "No search results are available. Do not invent sources: mark every claim 'unverified' and list what could not be checked in gaps.";
const EMPTY_RESULTS_NOTE: &str =
    "The search returned no results. Mark every claim 'unverified' and list gaps.";

#[derive(Clone, Debug)]
struct PlanStep {
    agent: String,
    task: String,
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
        let _ = self.handle.block_on(self.bus.publish(self.run_id, event, data));
    }
}

pub struct Orchestrator {
    pub settings: Arc<Settings>,
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub agents_dir: PathBuf,
    search: Arc<dyn SearchTool>,
}

impl Orchestrator {
    pub fn new(settings: Arc<Settings>, db: Db, bus: Arc<EventBus>) -> Self {
        let search: Arc<dyn SearchTool> = Arc::new(TavilySearch {
            settings: (*settings).clone(),
        });
        Self {
            settings,
            db,
            bus,
            agents_dir: PathBuf::from(ROOT).join("agents"),
            search,
        }
    }

    /// Replace the search implementation; tests inject a stub and never use the network.
    pub fn with_search(mut self, search: Arc<dyn SearchTool>) -> Self {
        self.search = search;
        self
    }

    /// Run one orchestration. Must be called from a blocking context.
    pub fn run(
        &self,
        task: &str,
        use_llm_planner: bool,
        run_id: &str,
        source: &str,
    ) -> Result<(), String> {
        // #30 adds the LLM planner; this ticket always plans hardcoded.
        let _ = use_llm_planner;
        let started = Instant::now();
        let handle = tokio::runtime::Handle::current();
        let emitter = Emitter {
            bus: &self.bus,
            handle: &handle,
            run_id,
        };

        // The HTTP layer already inserted the row; the Sun retries it like the
        // reference, and a failure here never fails the run.
        if let Err(error) = self.db.start_run(run_id, task, source) {
            tracing::error!(run_id, %error, "could not create run row");
            emitter.emit(
                "logger_failed",
                json!({"phase": "start", "run_id": run_id, "reason": error}),
            );
        }

        emitter.emit("run_started", json!({"task": redact(task)}));

        let plan: Vec<PlanStep> = DEFAULT_PLAN
            .iter()
            .map(|agent| PlanStep {
                agent: (*agent).to_string(),
                task: task.to_string(),
            })
            .collect();
        emitter.emit(
            "plan_ready",
            json!({
                "planner": "hardcoded",
                "agents": plan.iter().map(|step| step.agent.clone()).collect::<Vec<_>>(),
                "steps": plan.iter().map(|step| json!({"agent": step.agent, "task": step.task})).collect::<Vec<_>>(),
            }),
        );

        let mut outputs: Vec<StepResult> = Vec::new();
        let mut status = "complete";
        let mut search_block: Option<Value> = None;

        for step in &plan {
            let number = outputs.len() as i64 + 1;
            emitter.emit(
                "agent_called",
                json!({"step": number, "agent": step.agent}),
            );

            let mut extra = String::new();
            if step.agent == "researcher" && search_block.is_none() {
                emitter.emit(
                    "agent_working",
                    json!({"step": number, "agent": step.agent, "attempt": 1, "call": 1}),
                );
                let block = self.search_context(task, run_id, &emitter);
                extra = format!(
                    "Web search results (JSON):\n{}",
                    serde_json::to_string_pretty(&block)
                        .expect("search context serializes")
                );
                search_block = Some(block);
            }

            let message = agent_message(task, &step.agent, &step.task, &outputs, &extra);
            let Some(schema) = step_schema(&step.agent) else {
                return Err(format!("unknown agent: {}", step.agent));
            };
            let parse_error = |message: String| {
                ParseError::new(FailureType::SchemaMismatch, message)
            };
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

            let validated = agents::run_validated(
                &self.settings,
                &self.agents_dir,
                &step.agent,
                &message,
                STEP_CONTRACT,
                &schema,
                &parse,
                &mut on_attempt,
            );

            let validated = match validated {
                Ok(validated) => validated,
                Err(RunError::Run(error)) => {
                    let error_type = error.failure_type.as_str();
                    self.record_agent_call(NewAgentCall {
                        run_id: run_id.to_string(),
                        step: number,
                        agent: step.agent.clone(),
                        started_at,
                        duration_ms: step_started.elapsed().as_millis() as i64,
                        attempts: error.attempts as i64,
                        tool_turns: 0,
                        status: "failed".to_string(),
                        error_type: Some(error_type.to_string()),
                        error_message: Some(error.detail.clone()),
                        input_chars: message.chars().count() as i64,
                        output_chars: error.raw.chars().count() as i64,
                        raw_output: error.raw.clone(),
                        model: resolved_model(&self.settings, &step.agent),
                    });
                    emitter.emit(
                        "agent_failed",
                        json!({
                            "step": number,
                            "agent": step.agent,
                            "error": error_type,
                            "error_type": error_type,
                            "message": error.detail,
                            "attempts": error.attempts,
                            "tool_turns": 0,
                        }),
                    );
                    emitter.emit(
                        "stopped",
                        json!({
                            "step": number,
                            "agent": step.agent,
                            "status": "failed",
                            "reason": format!("{error_type}: {}", error.detail),
                        }),
                    );
                    status = "failed";
                    break;
                }
                Err(other) => return Err(format!("{other:?}")),
            };

            let raw_output = serde_json::to_string(&validated.output)
                .expect("validated output serializes");
            self.record_agent_call(NewAgentCall {
                run_id: run_id.to_string(),
                step: number,
                agent: step.agent.clone(),
                started_at,
                duration_ms: step_started.elapsed().as_millis() as i64,
                attempts: validated.attempts as i64,
                tool_turns: 0,
                status: "ok".to_string(),
                error_type: None,
                error_message: None,
                input_chars: message.chars().count() as i64,
                output_chars: raw_output.chars().count() as i64,
                raw_output,
                model: resolved_model(&self.settings, &step.agent),
            });

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
                tool_turns: 0,
            });
            emitter.emit(
                "agent_done",
                json!({
                    "step": number,
                    "agent": step.agent,
                    "status": step_status,
                    "attempts": validated.attempts,
                    "tool_turns": 0,
                    "duration_seconds": duration_seconds,
                    "summary": summary,
                }),
            );
        }

        let final_draft = final_draft(&outputs);
        let final_verdict = final_verdict(&outputs);

        // Logger: always runs, success or failure.
        let number = outputs.len() as i64 + 1;
        emitter.emit("agent_called", json!({"step": number, "agent": "logger"}));
        let step_started = Instant::now();
        let outputs_json = Value::Array(outputs.iter().map(StepResult::to_value).collect());
        let log_output = match self.db.save_run(
            run_id,
            task,
            status,
            &final_draft,
            final_verdict.as_deref(),
            &outputs_json,
            started.elapsed().as_secs_f64(),
            None,
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

        // Notifier: #31 wires approvals and Telegram delivery; until then it
        // records a skip and sends nothing.
        let number = outputs.len() as i64 + 1;
        emitter.emit("agent_called", json!({"step": number, "agent": "notifier"}));
        let step_started = Instant::now();
        let notify_output = if status == "failed" {
            json!({"agent": "notifier", "status": "skipped", "reason": "run failed; nothing sent"})
        } else {
            json!({
                "agent": "notifier",
                "status": "skipped",
                "reason": "delivery and approvals are not wired yet",
            })
        };
        emitter.emit("notifier_skipped", json!({"step": number}));
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

        if status != "failed" {
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
        Ok(())
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
        match self.search.search(&query) {
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
                emitter.emit(
                    "search_failed",
                    json!({"query": query, "reason": reason}),
                );
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

fn resolved_model(settings: &Settings, agent: &str) -> String {
    llm::model_for(settings, agent)
        .unwrap_or_else(|| std::env::var("MODEL").unwrap_or_default().trim().to_string())
}

/// Original task + this step's instruction + every earlier validated output.
fn agent_message(
    task: &str,
    _agent: &str,
    instruction: &str,
    earlier: &[StepResult],
    extra: &str,
) -> String {
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
        assert_eq!(summary("researcher", &json!({"summary": "  a   b "})), "a b");
        assert_eq!(summary("writer", &json!({"draft": "the draft"})), "the draft");
        assert_eq!(summary("checker", &json!({"verdict": "approved"})), "approved");
        assert_eq!(summary("logger", &json!({"status": "complete"})), "complete");
        assert_eq!(summary("notifier", &json!({})), "notifier finished");
    }

    #[test]
    fn summaries_are_capped_with_an_ellipsis() {
        let long = "x".repeat(200);
        let capped = summary("writer", &json!({"draft": long}));
        assert_eq!(capped.chars().count(), 140);
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
        let message = agent_message("the task", "writer", "the instruction", &earlier, "extra");
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

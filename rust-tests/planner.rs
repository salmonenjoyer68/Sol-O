//! Black-box tests for the optional LLM planner: provider schema, filtering,
//! the step cap and every planner stop point.

mod support;

use std::path::Path;

use serde_json::{json, Value};
use support::fake_provider::{FakeProvider, Reply};
use support::{Server, ROOT};
use tempfile::TempDir;

const TASK: &str = "Explain online ordering for cafes.";

fn writer_output() -> String {
    json!({
        "agent": "writer",
        "status": "complete",
        "draft": "Dear operator, online ordering helps your cafe.",
        "facts_used": ["orders rise"],
        "notes": "",
        "clarification_needed": null
    })
    .to_string()
}

fn checker_output() -> String {
    json!({
        "agent": "checker",
        "status": "complete",
        "verdict": "approved",
        "issues": [],
        "clarification_needed": null
    })
    .to_string()
}

fn start_server(database: &Path, provider: &FakeProvider, extra: &[(&str, &str)]) -> Server {
    let mut envs = vec![
        ("BASE_URL".to_string(), provider.base_url().to_string()),
        ("API_KEY".to_string(), "test-key".to_string()),
        ("MODEL".to_string(), "test-model".to_string()),
        ("AUTO_APPROVE".to_string(), "true".to_string()),
    ];
    for (name, value) in extra {
        envs.push((name.to_string(), value.to_string()));
    }
    let envs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Server::start(database, &envs)
}

/// Start with the LLM planner and return the run detail plus saved events.
fn run_planner(
    script: Vec<Reply>,
    extra: &[(&str, &str)],
) -> (TempDir, FakeProvider, Value, Vec<Value>) {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(script);
    let server = start_server(&database, &provider, extra);
    let accepted: Value = server
        .post(
            "/orchestrate",
            &json!({"task": TASK, "use_llm_planner": true}),
        )
        .json()
        .unwrap();
    let run_id = accepted["run_id"].as_str().unwrap().to_string();
    let detail = server.wait_for_run(&run_id);
    let saved = server.wait_for_terminal(&run_id);
    (dir, provider, detail, saved)
}

fn events(saved: &[Value]) -> Vec<&str> {
    saved
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect()
}

fn plan_ready(saved: &[Value]) -> &Value {
    &saved
        .iter()
        .find(|event| event["event"] == "plan_ready")
        .expect("plan_ready event")["data"]
}

#[test]
fn a_planner_plan_is_filtered_and_run_as_step_one() {
    let (_dir, provider, detail, saved) = run_planner(
        vec![
            Reply::content(
                json!({
                    "steps": [
                        {"agent": "mars", "task": "visit a planet that does not exist"},
                        {"agent": "writer", "task": "draft it"},
                        {"agent": "checker", "task": "check it"}
                    ]
                })
                .to_string(),
            ),
            Reply::content(writer_output()),
            Reply::content(checker_output()),
        ],
        &[],
    );

    assert_eq!(detail["run"]["status"], "complete");
    let plan = plan_ready(&saved);
    assert_eq!(plan["planner"], "llm");
    assert_eq!(
        plan["agents"],
        json!(["writer", "checker"]),
        "mars is dropped"
    );
    assert_eq!(plan["steps"][0]["task"], "draft it");
    assert_eq!(plan["steps"][1]["agent"], "checker");

    // The planner is step 0 and audited before the first planet.
    let calls = detail["agent_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["step"], 0);
    assert_eq!(calls[0]["agent"], "sun");
    assert_eq!(calls[0]["status"], "ok");
    assert_eq!(calls[0]["attempts"], 1);
    assert_eq!(calls[0]["model"], "test-model");
    let raw = calls[0]["raw_output"].as_str().unwrap();
    assert_eq!(calls[0]["output_chars"], raw.chars().count() as i64);
    let parsed: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(parsed["steps"].as_array().unwrap().len(), 3);
    assert_eq!(parsed["steps"][0]["agent"], "mars");

    // The provider schema is the plan model restricted to the runnable roster.
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "planner plus two planets");
    assert_eq!(requests[0].response_format_type(), Some("json_schema"));
    let schema = &requests[0].response_format().unwrap()["json_schema"]["schema"];
    assert_eq!(
        schema["$defs"]["PlanStep"]["properties"]["agent"]["enum"],
        json!(["checker", "researcher", "writer"]),
        "sun is not a runnable choice"
    );
    assert_eq!(schema["properties"]["steps"]["minItems"], 1);
    assert_eq!(schema["properties"]["steps"]["maxItems"], 7);
    let plan_contract = format!(
        "{}{}",
        std::fs::read_to_string(Path::new(ROOT).join("agents/sun.md")).unwrap(),
        sol_o::agents::plan_contract(7)
    );
    assert_eq!(requests[0].contents()[0], plan_contract);
    assert!(
        requests[0].contents()[0].contains("- 1 to 7 steps, executed in order."),
        "the default contract wording changed"
    );
    assert_eq!(
        requests[0].contents()[1],
        format!("Task:\n{TASK}\n\nAvailable agents:\n- checker\n- researcher\n- writer")
    );
    assert_eq!(
        calls[0]["input_chars"],
        requests[0].contents()[1].chars().count() as i64
    );

    // Only the two surviving steps run.
    assert_eq!(detail["run"]["outputs"].as_array().unwrap().len(), 2);
    assert_eq!(events(&saved)[1], "agent_working");
    assert_eq!(saved[1]["data"]["step"], 0);
    assert_eq!(saved[1]["data"]["agent"], "sun");
}

#[test]
fn a_step_missing_its_required_prior_output_is_dropped() {
    let (_dir, provider, detail, saved) = run_planner(
        vec![
            Reply::content(
                json!({
                    "steps": [
                        {"agent": "checker", "task": "too early"},
                        {"agent": "writer", "task": "draft it"},
                        {"agent": "checker", "task": "check it"}
                    ]
                })
                .to_string(),
            ),
            Reply::content(writer_output()),
            Reply::content(checker_output()),
        ],
        &[],
    );

    let plan = plan_ready(&saved);
    assert_eq!(plan["planner"], "llm");
    assert_eq!(plan["agents"], json!(["writer", "checker"]));
    assert_eq!(plan["steps"][1]["task"], "check it");
    assert_eq!(detail["run"]["status"], "complete");
    assert_eq!(provider.requests().len(), 3);
}

#[test]
fn a_plan_over_the_cap_is_truncated() {
    let (_dir, provider, detail, saved) = run_planner(
        vec![
            Reply::content(
                json!({
                    "steps": [
                        {"agent": "writer", "task": "draft it"},
                        {"agent": "checker", "task": "check it"},
                        {"agent": "writer", "task": "revise it"}
                    ]
                })
                .to_string(),
            ),
            Reply::content(writer_output()),
            Reply::content(checker_output()),
        ],
        &[("MAX_AGENTS", "2")],
    );

    let plan = plan_ready(&saved);
    assert_eq!(plan["agents"], json!(["writer", "checker"]));
    assert_eq!(plan["steps"].as_array().unwrap().len(), 2);
    let requests = provider.requests();
    let schema = &requests[0].response_format().unwrap()["json_schema"]["schema"];
    assert_eq!(schema["properties"]["steps"]["maxItems"], 2);
    assert_eq!(detail["run"]["status"], "complete");
    assert_eq!(provider.requests().len(), 3);
}

#[test]
fn the_plan_contract_states_the_configured_cap() {
    let (_dir, provider, detail, _saved) = run_planner(
        vec![
            Reply::content(json!({"steps": [{"agent": "writer", "task": "draft it"}]}).to_string()),
            Reply::content(writer_output()),
        ],
        &[("MAX_AGENTS", "3")],
    );

    let requests = provider.requests();
    let schema = &requests[0].response_format().unwrap()["json_schema"]["schema"];
    assert_eq!(schema["properties"]["steps"]["maxItems"], 3);
    let system = &requests[0].contents()[0];
    assert!(
        system.contains("- 1 to 3 steps, executed in order."),
        "the contract does not name the configured cap: {system}"
    );
    assert!(
        !system.contains("7 steps") && !system.contains("1-7"),
        "the prompt still states a competing cap: {system}"
    );
    assert_eq!(detail["run"]["status"], "complete");
}

#[test]
fn a_planner_timeout_fails_before_any_planet() {
    let (_dir, provider, detail, saved) = run_planner(vec![], &[("RUN_TIMEOUT_SECONDS", "0.001")]);

    assert_eq!(detail["run"]["status"], "failed");
    assert_eq!(detail["agent_calls"], json!([]), "no call was started");
    assert!(provider.requests().is_empty());
    assert_eq!(
        events(&saved),
        [
            "run_started",
            "stopped",
            "agent_called",
            "logger_saved",
            "agent_done",
            "agent_called",
            "notifier_skipped",
            "agent_done",
            "run_failed",
        ]
    );
    assert_eq!(
        saved[1]["data"],
        json!({"step": 0, "agent": "sun", "status": "failed", "reason": "run_timeout"})
    );
    assert_eq!(
        saved[8]["data"],
        json!({"failed_at": "sun", "error_type": "timeout", "message": "run_timeout"})
    );
}

#[test]
fn unusable_planner_output_fails_after_the_retries() {
    let raw = "x".repeat(5000);
    let (_dir, provider, detail, saved) = run_planner(
        vec![
            Reply::content(raw.clone()),
            Reply::content(raw.clone()),
            Reply::content(raw),
        ],
        &[("LLM_MAX_RETRIES", "2")],
    );

    assert_eq!(detail["run"]["status"], "failed");
    assert_eq!(provider.requests().len(), 3, "three planner attempts");
    let calls = detail["agent_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call["step"], 0);
    assert_eq!(call["agent"], "sun");
    assert_eq!(call["status"], "failed");
    assert_eq!(call["error_type"], "invalid_json");
    assert_eq!(call["attempts"], 3);
    assert_eq!(call["output_chars"], 5000);
    assert_eq!(call["raw_output"], "x".repeat(2000));

    assert_eq!(
        events(&saved),
        [
            "run_started",
            "agent_working",
            "agent_working",
            "agent_working",
            "stopped",
            "agent_called",
            "logger_saved",
            "agent_done",
            "agent_called",
            "notifier_skipped",
            "agent_done",
            "run_failed",
        ]
    );
    let stopped = &saved[4]["data"];
    assert_eq!(stopped["step"], 0);
    assert_eq!(stopped["agent"], "sun");
    assert_eq!(stopped["status"], "failed");
    let reason = stopped["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("invalid_json: invalid JSON:"),
        "{reason}"
    );
    assert_eq!(
        saved[11]["data"],
        json!({"failed_at": "sun", "error_type": "invalid_json", "message": reason})
    );
}

#[test]
fn a_missing_planner_prompt_fails_the_run() {
    let prompt_dir = TempDir::new().unwrap();
    let (_dir, provider, detail, saved) = run_planner(
        vec![],
        &[("SOL_O_AGENTS_DIR", prompt_dir.path().to_str().unwrap())],
    );

    assert_eq!(detail["run"]["status"], "failed");
    assert!(provider.requests().is_empty());
    assert_eq!(detail["agent_calls"], json!([]));
    assert_eq!(
        events(&saved),
        [
            "run_started",
            "stopped",
            "agent_called",
            "logger_saved",
            "agent_done",
            "agent_called",
            "notifier_skipped",
            "agent_done",
            "run_failed",
        ]
    );
    let reason = "llm_error: planner prompt missing (sun)";
    assert_eq!(
        saved[1]["data"],
        json!({"step": 0, "agent": "sun", "status": "failed", "reason": reason})
    );
    assert_eq!(
        saved[8]["data"],
        json!({"failed_at": "sun", "error_type": "llm_error", "message": reason})
    );
}

#[test]
fn an_empty_filtered_plan_fails_the_run() {
    let (_dir, provider, detail, saved) = run_planner(vec![Reply::content(r#"{"steps":[]}"#)], &[]);

    assert_eq!(detail["run"]["status"], "failed");
    assert_eq!(provider.requests().len(), 1, "the planner was called");
    let calls = detail["agent_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["step"], 0);
    assert_eq!(calls[0]["agent"], "sun");
    assert_eq!(calls[0]["status"], "ok");
    assert_eq!(calls[0]["raw_output"], r#"{"steps":[]}"#);

    assert_eq!(
        events(&saved),
        [
            "run_started",
            "agent_working",
            "stopped",
            "agent_called",
            "logger_saved",
            "agent_done",
            "agent_called",
            "notifier_skipped",
            "agent_done",
            "run_failed",
        ]
    );
    let reason = "planner returned no usable steps";
    assert_eq!(
        saved[2]["data"],
        json!({"step": 0, "agent": "sun", "status": "failed", "reason": reason})
    );
    assert_eq!(
        saved[9]["data"],
        json!({"failed_at": "sun", "error_type": "invalid_json", "message": reason})
    );
}

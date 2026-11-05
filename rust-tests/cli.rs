//! Black-box tests for the operator CLI: every subcommand is the real binary,
//! asserting stdout, stderr and the process exit code.

mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use support::fake_provider::{FakeProvider, Reply};
use tempfile::TempDir;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const TASK: &str = "Explain online ordering for cafes.";

fn command(database: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sol-o"));
    for name in [
        "API_KEY",
        "BASE_URL",
        "MODEL",
        "AGENT_MODELS",
        "LLM_MAX_RETRIES",
        "LLM_TIMEOUT_SECONDS",
        "TAVILY_API_KEY",
        "SOL_O_TAVILY_ENDPOINT",
        "REQUIRE_APPROVAL",
        "AUTO_APPROVE",
        "PENDING_TTL_HOURS",
        "DRY_RUN",
        "TELEGRAM_BOT_TOKEN",
        "TELEGRAM_CHAT_ID",
        "SOL_O_TELEGRAM_ENDPOINT",
        "TASK_DESCRIPTION",
    ] {
        command.env_remove(name);
    }
    command
        .env("SOL_O_DB", database)
        .env("SOL_O_ENV_FILE", support::hermetic_env_file(database))
        .env("DRY_RUN", "true")
        .current_dir(ROOT);
    command
}

fn cli(database: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = command(database);
    command.args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("run sol-o")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

/// The CLI may print a dry-run Telegram block first; the result JSON is last.
fn json_tail(text: &str) -> Value {
    let start = text
        .rfind("\n{\n")
        .map(|index| index + 1)
        .unwrap_or_else(|| text.find('{').expect("JSON object in stdout"));
    serde_json::from_str(&text[start..])
        .unwrap_or_else(|error| panic!("result JSON ({error}): {text}"))
}

fn researcher_output() -> String {
    json!({
        "agent": "researcher",
        "status": "complete",
        "task": TASK,
        "summary": "Online ordering helps cafes.",
        "findings": [],
        "conflicts": [],
        "gaps": [],
        "clarification_needed": null
    })
    .to_string()
}

fn writer_output() -> String {
    json!({
        "agent": "writer",
        "status": "complete",
        "draft": "Dear operator, online ordering helps your cafe.",
        "facts_used": [],
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

fn happy_provider() -> FakeProvider {
    FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ])
}

/// Provider environment plus optional extras; the return value borrows `provider`.
fn provider_args<'a>(
    provider: &'a FakeProvider,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut envs = vec![
        ("BASE_URL", provider.base_url()),
        ("API_KEY", "test-key"),
        ("MODEL", "test-model"),
    ];
    envs.extend_from_slice(extra);
    envs
}

fn pending_id(text: &str) -> i64 {
    text.lines()
        .find(|line| line.starts_with('#'))
        .expect("a pending row")
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .expect("action id")
        .parse()
        .expect("numeric action id")
}

#[test]
fn bare_invocation_prints_usage_and_fails_without_serving() {
    let dir = TempDir::new().unwrap();
    let output = cli(&dir.path().join("sol-o.db"), &[], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("Usage"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn run_prints_the_reference_result_and_exits_zero() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let output = cli(
        &database,
        &["run", TASK, "--source", "terminal"],
        &provider_args(&provider, &[("REQUIRE_APPROVAL", "false")]),
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let result = json_tail(&stdout(&output));
    assert_eq!(result["status"], "complete");
    assert_eq!(result["task"], TASK);
    assert_eq!(result["planner"], "hardcoded");
    // Three planned steps plus the logger and notifier system steps.
    assert_eq!(result["plan"].as_array().unwrap().len(), 5);
    assert_eq!(result["outputs"].as_array().unwrap().len(), 5);
    assert!(result["run_id"].as_str().is_some());
    assert!(result["stopped_at"].is_null());
    assert!(result["failure"].is_null());
    assert!(result["duration_seconds"].is_number());
}

#[test]
fn run_reads_the_task_from_a_file_and_accepts_the_plan_flag() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let task_file = dir.path().join("task.txt");
    std::fs::write(&task_file, "  Plan the weekend launch  \n").unwrap();
    let plan = json!({
        "steps": [
            {"agent": "researcher", "task": "Research the launch"},
            {"agent": "writer", "task": "Write it up"},
            {"agent": "checker", "task": "Check the draft"}
        ]
    })
    .to_string();
    let provider = FakeProvider::start(vec![
        Reply::content(plan),
        Reply::content(researcher_output()),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let output = cli(
        &database,
        &[
            "run",
            "--file",
            task_file.to_str().unwrap(),
            "--plan",
            "--source",
            "team-alpha",
        ],
        &provider_args(&provider, &[("REQUIRE_APPROVAL", "false")]),
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let result = json_tail(&stdout(&output));
    assert_eq!(result["task"], "Plan the weekend launch");
    assert_eq!(result["planner"], "llm");
    // Three planned steps plus the logger and notifier system steps.
    assert_eq!(result["plan"].as_array().unwrap().len(), 5);
    assert_eq!(result["plan"][0]["agent"], "researcher");
}

#[test]
fn run_falls_back_to_the_configured_task_description() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let output = cli(
        &database,
        &["run"],
        &provider_args(
            &provider,
            &[("REQUIRE_APPROVAL", "false"), ("TASK_DESCRIPTION", TASK)],
        ),
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let result = json_tail(&stdout(&output));
    assert_eq!(result["task"], TASK);
}

#[test]
fn run_without_a_task_fails_with_the_reference_message() {
    let dir = TempDir::new().unwrap();
    let output = cli(&dir.path().join("sol-o.db"), &["run"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(
        stderr(&output).contains("no task: pass text, use --file, or set TASK_DESCRIPTION in .env"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn show_prints_the_stored_run_and_rejects_an_unknown_id() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let run = cli(
        &database,
        &["run", TASK],
        &provider_args(&provider, &[("REQUIRE_APPROVAL", "false")]),
    );
    let result = json_tail(&stdout(&run));
    let run_id = result["run_id"].as_str().unwrap();

    let shown = cli(&database, &["show", run_id], &[]);
    assert_eq!(shown.status.code(), Some(0));
    let text = stdout(&shown);
    assert!(text.contains(&format!("Run {run_id}  status=complete  created=")));
    assert!(text.contains(&format!("task: {TASK}")));
    assert!(text.contains("timeline:"));
    assert!(text.contains("step  1  researcher"));
    assert!(text.contains("tool      search"));
    assert!(!text.contains("(no agent calls recorded)"));

    let missing = cli(&database, &["show", "deadbeef"], &[]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(stdout(&missing), "run deadbeef not found\n");
}

#[test]
fn pending_lists_only_actionable_requests_and_empty_reads_cleanly() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let run = cli(&database, &["run", TASK], &provider_args(&provider, &[]));
    assert_eq!(run.status.code(), Some(4), "awaiting approval");
    let run_id = json_tail(&stdout(&run))["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let listed = cli(&database, &["pending"], &[]);
    assert_eq!(listed.status.code(), Some(0));
    let text = stdout(&listed);
    assert!(text.contains(&format!("  run={run_id}  type=telegram  created=")));
    assert!(text.contains(&format!("  task: {TASK}")));
    assert!(text.contains("  verdict: approved"));
    assert!(text.contains("  message to approve:"));

    let id = pending_id(&text).to_string();
    let approved = cli(&database, &["approve", &id], &[]);
    assert_eq!(
        approved.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&approved)
    );

    let empty = cli(&database, &["pending"], &[]);
    assert_eq!(empty.status.code(), Some(0));
    assert_eq!(stdout(&empty), "no pending actions\n");
}

#[test]
fn approve_and_reject_print_the_outcome_and_fail_on_decided_or_missing_actions() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");

    let provider = happy_provider();
    let first = cli(&database, &["run", TASK], &provider_args(&provider, &[]));
    assert_eq!(first.status.code(), Some(4));
    let id = pending_id(&stdout(&cli(&database, &["pending"], &[]))).to_string();

    let approved = cli(&database, &["approve", &id], &[]);
    assert_eq!(
        approved.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&approved)
    );
    let body = json_tail(&stdout(&approved));
    assert_eq!(body["id"], id.parse::<i64>().unwrap());
    assert_eq!(body["status"], "approved");
    assert_eq!(body["run_status"], "complete");
    assert_eq!(body["action_type"], "telegram");
    assert_eq!(body["delivery"]["dry_run"], true);

    let again = cli(&database, &["approve", &id], &[]);
    assert_eq!(again.status.code(), Some(1));
    assert!(
        stdout(&again).contains(&format!(
            "approve failed (conflict): pending action {id} was already approved"
        )),
        "stdout: {}",
        stdout(&again)
    );

    let provider = happy_provider();
    let second = cli(&database, &["run", TASK], &provider_args(&provider, &[]));
    assert_eq!(second.status.code(), Some(4));
    let second_id = pending_id(&stdout(&cli(&database, &["pending"], &[]))).to_string();
    let rejected = cli(&database, &["reject", &second_id], &[]);
    assert_eq!(
        rejected.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&rejected)
    );
    let body = json_tail(&stdout(&rejected));
    assert_eq!(body["status"], "rejected");
    assert_eq!(body["action_type"], "telegram");

    let missing = cli(&database, &["reject", "999"], &[]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stdout(&missing).contains("reject failed (not_found): pending action 999 does not exist"),
        "stdout: {}",
        stdout(&missing)
    );
}

#[test]
fn expired_actions_cannot_be_approved() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let run = cli(&database, &["run", TASK], &provider_args(&provider, &[]));
    assert_eq!(run.status.code(), Some(4));
    let id = pending_id(&stdout(&cli(&database, &["pending"], &[]))).to_string();

    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE pending_actions SET created_at = '2000-01-01T00:00:00+00:00' WHERE id = ?",
            [id.parse::<i64>().unwrap()],
        )
        .unwrap();
    drop(connection);

    let approved = cli(&database, &["approve", &id], &[]);
    assert_eq!(approved.status.code(), Some(1));
    assert!(
        stdout(&approved).contains(&format!(
            "approve failed (conflict): pending action {id} was already expired"
        )),
        "stdout: {}",
        stdout(&approved)
    );
}

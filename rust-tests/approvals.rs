//! Process-seam tests for the approval gate: pending creation, approve, reject,
//! double decisions, expiry, dry-run, auto-approved sends and failed-run skips.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use support::fake_provider::{FakeProvider, Reply};
use support::Server;
use tempfile::TempDir;

const TASK: &str = "Explain online ordering for cafes.";

impl Server {
    fn start_run(&self) -> String {
        let accepted: Value = self
            .post("/orchestrate", &json!({"task": TASK}))
            .json()
            .unwrap();
        accepted["run_id"].as_str().unwrap().to_string()
    }
}

/// A loopback Telegram stand-in: records sends, no network.
struct FakeTelegram {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl FakeTelegram {
    fn start(status: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake telegram");
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut content_length = 0_usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut request_body = vec![0_u8; content_length];
                let _ = reader.read_exact(&mut request_body);
                if let Ok(parsed) = serde_json::from_slice::<Value>(&request_body) {
                    seen.lock().unwrap().push(parsed);
                }
                let body = json!({"ok": status < 300}).to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            endpoint: format!("http://{address}/botTEST/sendMessage"),
            requests,
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
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

fn provider_env(base_url: &str) -> Vec<(String, String)> {
    vec![
        ("BASE_URL".to_string(), base_url.to_string()),
        ("API_KEY".to_string(), "test-key".to_string()),
        ("MODEL".to_string(), "test-model".to_string()),
    ]
}

fn start_server(database: &Path, provider: &FakeProvider, extra: &[(&str, &str)]) -> Server {
    let mut envs = provider_env(provider.base_url());
    for (name, value) in extra {
        envs.push((name.to_string(), value.to_string()));
    }
    let envs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Server::start(database, &envs)
}

fn happy_provider() -> FakeProvider {
    FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ])
}

#[test]
fn approve_executes_once_restores_the_run_and_conflicts_on_a_second_decision() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let telegram = FakeTelegram::start(200);
    let server = start_server(
        &database,
        &provider,
        &[
            ("DRY_RUN", "false"),
            ("TELEGRAM_BOT_TOKEN", "test-token"),
            ("TELEGRAM_CHAT_ID", "42"),
            ("SOL_O_TELEGRAM_ENDPOINT", &telegram.endpoint),
        ],
    );

    let run_id = server.start_run();
    let detail = server.wait_for_run(&run_id);
    assert_eq!(detail["run"]["status"], "awaiting_approval");
    assert_eq!(telegram.count(), 0, "nothing sent before approval");

    let events = server.events(&run_id);
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"approval_required"));
    assert!(names.contains(&"awaiting_approval"));
    assert!(names.contains(&"notifier_pending"));
    assert!(
        !names.contains(&"all_done"),
        "no terminal event while pending"
    );
    assert!(!names.contains(&"notifier_sent"));
    let required = events
        .iter()
        .find(|event| event["event"] == "approval_required")
        .expect("approval_required event");
    assert_eq!(required["data"]["status"], "awaiting_approval");
    assert_eq!(required["data"]["run_id"], run_id.as_str());
    assert_eq!(
        required["data"]["payload"]["draft"],
        "Dear operator, online ordering helps your cafe."
    );
    assert_eq!(required["data"]["payload"]["verdict"], "approved");

    let pending = server.pending();
    assert_eq!(pending.len(), 1);
    let action_id = pending[0]["id"].as_i64().unwrap();
    assert_eq!(pending[0]["run_id"], run_id.as_str());
    assert_eq!(pending[0]["action_type"], "telegram");
    assert_eq!(pending[0]["status"], "pending");
    assert_eq!(pending[0]["payload"]["run_status"], "complete");
    assert_eq!(pending[0]["payload"]["verdict"], "approved");
    assert!(pending[0]["payload"]["message"]
        .as_str()
        .unwrap()
        .contains("Dear operator"));
    assert!(detail["pending_actions"][0]["payload"]["message"]
        .as_str()
        .unwrap()
        .contains("Dear operator"));

    let approved = server.empty_post(&format!("/pending/{action_id}/approve"));
    assert_eq!(approved.status(), 200);
    let body: Value = approved.json().unwrap();
    assert_eq!(body["id"], action_id);
    assert_eq!(body["status"], "approved");
    assert_eq!(body["run_id"], run_id.as_str());
    assert_eq!(body["run_status"], "complete");
    assert_eq!(body["action_type"], "telegram");
    assert_eq!(
        body["delivery"],
        json!({"dry_run": false, "delivered": true, "parts": 1})
    );
    assert_eq!(telegram.count(), 1, "approved payload sent exactly once");
    let sent = &telegram.requests()[0];
    assert_eq!(sent["chat_id"], "42");
    assert!(sent["text"].as_str().unwrap().contains("Dear operator"));
    assert_eq!(sent["disable_web_page_preview"], true);

    let after = server.wait_for_run(&run_id);
    assert_eq!(
        after["run"]["status"], "complete",
        "stored run status restored"
    );
    let telegram_calls: Vec<&Value> = after["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|call| call["tool"] == "telegram")
        .collect();
    assert_eq!(telegram_calls.len(), 1);
    assert_eq!(telegram_calls[0]["status"], "ok");

    let decided: Vec<Value> = server
        .events(&run_id)
        .into_iter()
        .filter(|event| event["event"] == "approval_decided")
        .collect();
    assert_eq!(decided.len(), 1);
    assert_eq!(
        decided[0]["data"],
        json!({"pending_id": action_id, "decision": "approved", "run_status": "complete"})
    );
    assert!(server.pending().is_empty());

    let second = server.empty_post(&format!("/pending/{action_id}/approve"));
    assert_eq!(second.status(), 409);
    let conflict: Value = second.json().unwrap();
    assert_eq!(conflict["error"], "conflict");
    assert!(conflict["detail"]
        .as_str()
        .unwrap()
        .contains("already approved"));
    assert_eq!(telegram.count(), 1, "a second decision sends nothing");
}

#[test]
fn reject_marks_the_run_rejected_and_sends_nothing() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let telegram = FakeTelegram::start(200);
    let server = start_server(
        &database,
        &provider,
        &[
            ("DRY_RUN", "false"),
            ("TELEGRAM_BOT_TOKEN", "test-token"),
            ("TELEGRAM_CHAT_ID", "42"),
            ("SOL_O_TELEGRAM_ENDPOINT", &telegram.endpoint),
        ],
    );

    let run_id = server.start_run();
    server.wait_for_run(&run_id);
    let action_id = server.pending()[0]["id"].as_i64().unwrap();

    let rejected = server.empty_post(&format!("/pending/{action_id}/reject"));
    assert_eq!(rejected.status(), 200);
    let body: Value = rejected.json().unwrap();
    assert_eq!(body["id"], action_id);
    assert_eq!(body["status"], "rejected");
    assert_eq!(body["run_id"], run_id.as_str());
    assert_eq!(body["action_type"], "telegram");
    assert_eq!(telegram.count(), 0, "rejection executes nothing");

    let after = server.wait_for_run(&run_id);
    assert_eq!(after["run"]["status"], "rejected");
    let decided: Vec<Value> = server
        .events(&run_id)
        .into_iter()
        .filter(|event| event["event"] == "approval_decided")
        .collect();
    assert_eq!(
        decided[0]["data"],
        json!({"pending_id": action_id, "decision": "rejected", "run_status": "rejected"})
    );
    assert!(server.pending().is_empty());

    let second = server.empty_post(&format!("/pending/{action_id}/reject"));
    assert_eq!(second.status(), 409);
}

#[test]
fn expired_actions_are_not_listed_and_cannot_be_decided() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let telegram = FakeTelegram::start(200);
    let server = start_server(
        &database,
        &provider,
        &[
            ("PENDING_TTL_HOURS", "0.0003"),
            ("DRY_RUN", "false"),
            ("TELEGRAM_BOT_TOKEN", "test-token"),
            ("TELEGRAM_CHAT_ID", "42"),
            ("SOL_O_TELEGRAM_ENDPOINT", &telegram.endpoint),
        ],
    );

    let run_id = server.start_run();
    server.wait_for_run(&run_id);
    let action_id = server.pending()[0]["id"].as_i64().unwrap();
    // Timestamps have second precision, so wait out the ~1s TTL plus the
    // sub-second phase the stored `created_at` was truncated at.
    std::thread::sleep(Duration::from_millis(2600));

    assert!(server.pending().is_empty(), "expired actions are hidden");
    let approved = server.empty_post(&format!("/pending/{action_id}/approve"));
    assert_eq!(approved.status(), 409);
    let conflict: Value = approved.json().unwrap();
    assert_eq!(conflict["error"], "conflict");
    assert!(conflict["detail"].as_str().unwrap().contains("expired"));
    assert_eq!(telegram.count(), 0);

    let detail = server.wait_for_run(&run_id);
    assert_eq!(detail["run"]["status"], "awaiting_approval");
    assert_eq!(detail["pending_actions"][0]["status"], "expired");
}

#[test]
fn dry_run_is_the_default_and_never_reaches_the_endpoint() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let telegram = FakeTelegram::start(200);
    // No DRY_RUN, no credentials: dry run wins and the endpoint stays untouched.
    let server = start_server(
        &database,
        &provider,
        &[("SOL_O_TELEGRAM_ENDPOINT", &telegram.endpoint)],
    );

    let run_id = server.start_run();
    server.wait_for_run(&run_id);
    let action_id = server.pending()[0]["id"].as_i64().unwrap();

    let approved = server.empty_post(&format!("/pending/{action_id}/approve"));
    assert_eq!(approved.status(), 200);
    let body: Value = approved.json().unwrap();
    assert_eq!(
        body["delivery"],
        json!({"dry_run": true, "delivered": false, "parts": 1})
    );
    assert_eq!(telegram.count(), 0);
    assert_eq!(server.wait_for_run(&run_id)["run"]["status"], "complete");
}

#[test]
fn auto_approved_runs_send_without_a_pending_row() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let server = start_server(&database, &provider, &[("AUTO_APPROVE", "true")]);

    let run_id = server.start_run();
    let detail = server.wait_for_run(&run_id);
    assert_eq!(detail["run"]["status"], "complete");
    assert_eq!(detail["pending_actions"], json!([]));
    assert!(server.pending().is_empty());

    let events = server.wait_for_event(&run_id, "all_done");
    let sent = events
        .iter()
        .find(|event| event["event"] == "notifier_sent")
        .expect("notifier_sent event");
    assert_eq!(
        sent["data"],
        json!({"dry_run": true, "delivered": false, "parts": 1, "step": 5})
    );
    assert!(events.iter().any(|event| event["event"] == "all_done"));
}

#[test]
fn failed_runs_skip_the_notifier_and_emit_the_skipped_event() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content("not json"),
        Reply::content("still not json"),
        Reply::content("never json"),
    ]);
    let telegram = FakeTelegram::start(200);
    let server = start_server(
        &database,
        &provider,
        &[
            ("DRY_RUN", "false"),
            ("TELEGRAM_BOT_TOKEN", "test-token"),
            ("TELEGRAM_CHAT_ID", "42"),
            ("SOL_O_TELEGRAM_ENDPOINT", &telegram.endpoint),
        ],
    );

    let run_id = server.start_run();
    let detail = server.wait_for_run(&run_id);
    assert_eq!(detail["run"]["status"], "failed");
    assert_eq!(detail["pending_actions"], json!([]));

    let events = server.events(&run_id);
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"notifier_skipped"));
    assert!(!names.contains(&"notifier_sent"));
    assert!(!names.contains(&"approval_required"));
    assert!(!names.contains(&"all_done"));
    assert_eq!(telegram.count(), 0);
}

#[test]
fn a_pending_action_that_cannot_be_stored_fails_the_run_instead_of_sending() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    // A pending_actions table missing its columns makes the INSERT fail;
    // the schema's CREATE TABLE IF NOT EXISTS leaves it alone.
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("CREATE TABLE pending_actions (id INTEGER PRIMARY KEY, run_id TEXT)")
        .unwrap();
    drop(connection);

    let provider = happy_provider();
    let telegram = FakeTelegram::start(200);
    let server = start_server(
        &database,
        &provider,
        &[
            ("DRY_RUN", "false"),
            ("TELEGRAM_BOT_TOKEN", "test-token"),
            ("TELEGRAM_CHAT_ID", "42"),
            ("SOL_O_TELEGRAM_ENDPOINT", &telegram.endpoint),
        ],
    );

    let run_id = server.start_run();
    let detail = server.wait_for_run(&run_id);
    assert_eq!(detail["run"]["status"], "failed");
    assert_eq!(telegram.count(), 0, "never send without a stored approval");

    let events = server.wait_for_event(&run_id, "notifier_failed");
    let failed = events
        .iter()
        .find(|event| event["event"] == "notifier_failed")
        .expect("notifier_failed event");
    assert_eq!(failed["data"]["agent"], "notifier");
    assert!(!events
        .iter()
        .any(|event| event["event"] == "approval_required"));
    assert!(!events.iter().any(|event| event["event"] == "notifier_sent"));

    let run_failed = events
        .iter()
        .find(|event| event["event"] == "run_failed")
        .expect("run_failed event");
    assert_eq!(run_failed["data"]["failed_at"], "notifier");
    assert_eq!(run_failed["data"]["error_type"], "tool_error");
    assert!(run_failed["data"]["message"]
        .as_str()
        .unwrap()
        .starts_with("tool_error: "));
}

#[test]
fn a_failed_delivery_is_reported_and_still_consumes_the_action() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = happy_provider();
    let telegram = FakeTelegram::start(500);
    let server = start_server(
        &database,
        &provider,
        &[
            ("DRY_RUN", "false"),
            ("TELEGRAM_BOT_TOKEN", "test-token"),
            ("TELEGRAM_CHAT_ID", "42"),
            ("SOL_O_TELEGRAM_ENDPOINT", &telegram.endpoint),
        ],
    );

    let run_id = server.start_run();
    server.wait_for_run(&run_id);
    let action_id = server.pending()[0]["id"].as_i64().unwrap();

    let approved = server.empty_post(&format!("/pending/{action_id}/approve"));
    assert_eq!(approved.status(), 502);
    let body: Value = approved.json().unwrap();
    assert_eq!(body["error"], "execution_failed");
    assert!(body["detail"]
        .as_str()
        .unwrap()
        .contains("telegram send failed"));

    let detail = server.wait_for_run(&run_id);
    assert_eq!(detail["run"]["status"], "failed");
    let telegram_calls: Vec<&Value> = detail["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|call| call["tool"] == "telegram")
        .collect();
    assert_eq!(telegram_calls[0]["status"], "failed");
    let decided = server
        .events(&run_id)
        .into_iter()
        .find(|event| event["event"] == "approval_decided")
        .expect("decision event");
    assert_eq!(decided["data"]["run_status"], "failed");
    // Approved is final: no retry.
    assert_eq!(
        server
            .empty_post(&format!("/pending/{action_id}/approve"))
            .status(),
        409
    );
}

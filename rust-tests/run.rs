//! Black-box tests for `POST /run`: prompt loading, provider ladder/retries,
//! output validation and the response contract.

mod support;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};
use support::fake_provider::{FakeProvider, Reply};
use tempfile::TempDir;

use support::ROOT;

struct Server {
    child: Child,
    _stderr: BufReader<ChildStderr>,
    base: String,
}

impl Server {
    /// Boot the binary on an ephemeral port with the given provider environment.
    fn start(database: &Path, envs: &[(&str, &str)]) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sol-o"));
        command.args(["serve", "--host", "127.0.0.1", "--port", "0"]);
        for name in [
            "API_KEY",
            "BASE_URL",
            "MODEL",
            "AGENT_MODELS",
            "LLM_MAX_RETRIES",
            "LLM_TIMEOUT_SECONDS",
        ] {
            command.env_remove(name);
        }
        command
            .env("SOL_O_DB", database)
            .env("SOL_O_ENV_FILE", support::hermetic_env_file(database))
            .current_dir(ROOT);
        for (name, value) in envs {
            command.env(name, value);
        }
        let mut child = command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sol-o serve");
        let stderr = child.stderr.take().expect("stderr is piped");
        let mut stderr = BufReader::new(stderr);
        let pattern = Regex::new(r"address=([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+:[0-9]+)").unwrap();
        let address = loop {
            let mut line = String::new();
            let read = stderr.read_line(&mut line).expect("read server stderr");
            assert!(read > 0, "server exited before logging its bound address");
            if let Some(captures) = pattern.captures(&line) {
                break captures[1].to_string();
            }
        };
        Server {
            child,
            _stderr: stderr,
            base: format!("http://{address}"),
        }
    }

    fn post(&self, body: &Value) -> reqwest::blocking::Response {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client")
            .post(format!("{}/run", self.base))
            .json(body)
            .send()
            .expect("POST /run")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn valid_output() -> String {
    json!({
        "agent": "researcher",
        "status": "complete",
        "task": "Explain online ordering for cafes.",
        "summary": "It helps.",
        "findings": [{"claim": "c", "source_url": "https://example.com", "confidence": "high"}],
        "conflicts": [],
        "gaps": [],
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

fn start(database: &Path, provider: &FakeProvider, extra: &[(&str, &str)]) -> Server {
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

/// `/run` performs no audit writes; the table stays empty.
fn assert_no_audit_rows(database: &Path) {
    let db = sol_o::persistence::Db::new(database);
    let connection = db.connect().expect("open database");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM agent_calls", [], |row| row.get(0))
        .expect("count agent_calls");
    assert_eq!(count, 0, "/run writes no agent_calls rows");
}

#[test]
fn a_valid_request_returns_the_validated_object() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![Reply::content(valid_output())]);
    let server = start(&database, &provider, &[]);

    let response = server.post(&json!({
        "agent": "researcher",
        "task": "Explain online ordering for cafes."
    }));
    assert_eq!(response.status(), 200);
    let body: Value = response.json().unwrap();
    assert_eq!(body["agent"], "researcher");
    assert_eq!(body["status"], "complete");
    assert_eq!(body["findings"][0]["confidence"], "high");
    assert!(body["clarification_needed"].is_null());

    let sent = provider.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].model(), "test-model");
    assert_eq!(sent[0].roles(), ["system", "user"]);
    assert_eq!(sent[0].response_format_type(), Some("json_schema"));
    assert_eq!(
        sent[0].response_format().unwrap()["json_schema"]["strict"],
        true
    );
    assert_eq!(sent[0].contents()[1], "Explain online ordering for cafes.");
    let prompt = std::fs::read_to_string(Path::new(ROOT).join("agents/researcher.md")).unwrap();
    assert_eq!(
        sent[0].contents()[0],
        format!("{prompt}{}", sol_o::agents::OUTPUT_CONTRACT),
        "system prompt is the file plus the reference contract"
    );

    assert_no_audit_rows(&database);
}

#[test]
fn unknown_and_unsafe_agent_names_are_404_and_never_reach_the_provider() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![]);
    let server = start(&database, &provider, &[]);

    for agent in ["ghost", "../researcher"] {
        let response = server.post(&json!({"agent": agent, "task": "do it"}));
        assert_eq!(response.status(), 404, "agent {agent:?}");
        let body: Value = response.json().unwrap();
        assert_eq!(body["error"], "agent_not_found");
        assert_eq!(body["agent"], agent);
    }
    let empty = server.post(&json!({"agent": "", "task": "do it"}));
    assert_eq!(empty.status(), 422, "empty agent fails request validation");
    assert!(provider.requests().is_empty());

    assert_no_audit_rows(&database);
}

#[test]
fn a_provider_outage_maps_to_the_documented_error() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![Reply::status_message(500, "provider is down")]);
    let server = start(&database, &provider, &[("LLM_MAX_RETRIES", "0")]);

    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    assert_eq!(response.status(), 502);
    let body: Value = response.json().unwrap();
    assert_eq!(body["error"], "llm_error");
    assert!(
        body["detail"].as_str().unwrap().starts_with("HTTP 500"),
        "detail {}",
        body["detail"]
    );
    assert_eq!(provider.requests().len(), 1);

    assert_no_audit_rows(&database);
}

#[test]
fn a_scripted_timeout_maps_to_the_timeout_status() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(valid_output()).delayed(Duration::from_secs(3))
    ]);
    let server = start(
        &database,
        &provider,
        &[("LLM_TIMEOUT_SECONDS", "0.2"), ("LLM_MAX_RETRIES", "0")],
    );

    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    assert_eq!(response.status(), 504);
    let body: Value = response.json().unwrap();
    assert_eq!(body["error"], "llm_timeout");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("LLM call exceeded"),
        "detail {}",
        body["detail"]
    );

    assert_no_audit_rows(&database);
}

#[test]
fn transient_failures_are_retried_with_backoff_then_succeed() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::status_message(429, "slow down"),
        Reply::content(valid_output()),
    ]);
    let server = start(&database, &provider, &[("LLM_MAX_RETRIES", "1")]);

    let started = Instant::now();
    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    let elapsed = started.elapsed();
    assert_eq!(response.status(), 200);
    assert!(
        elapsed >= Duration::from_millis(900),
        "one retry waits about a second, took {elapsed:?}"
    );
    assert_eq!(provider.requests().len(), 2);

    assert_no_audit_rows(&database);
}

#[test]
fn the_response_format_ladder_falls_back_on_a_format_rejection() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::status_message(400, "response_format json_schema is unsupported"),
        Reply::content(valid_output()),
    ]);
    let server = start(&database, &provider, &[("LLM_MAX_RETRIES", "0")]);

    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    assert_eq!(response.status(), 200);

    let sent = provider.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].response_format_type(), Some("json_schema"));
    assert_eq!(sent[1].response_format_type(), Some("json_object"));
}

#[test]
fn an_unrelated_4xx_fails_immediately_without_falling_back() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![Reply::status_message(401, "invalid api key")]);
    let server = start(&database, &provider, &[("LLM_MAX_RETRIES", "2")]);

    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    assert_eq!(response.status(), 502);
    let body: Value = response.json().unwrap();
    assert_eq!(body["error"], "llm_error");
    assert!(body["detail"].as_str().unwrap().starts_with("HTTP 401"));
    assert_eq!(provider.requests().len(), 1);
}

#[test]
fn a_malformed_response_is_retried_with_the_error_appended() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content("not json"),
        Reply::content(valid_output()),
    ]);
    let server = start(&database, &provider, &[]);

    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    assert_eq!(response.status(), 200);

    let sent = provider.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].roles(), ["system", "user", "assistant", "user"]);
    assert_eq!(sent[1].contents()[2], "not json");
    let retry = sent[1].contents()[3];
    assert!(
        retry.starts_with("Your previous response could not be used: invalid JSON"),
        "retry turn: {retry}"
    );
    assert_eq!(sent[1].response_format_type(), Some("json_schema"));

    assert_no_audit_rows(&database);
}

#[test]
fn permanently_malformed_output_returns_the_invalid_output_body_with_raw_text() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content("not json"),
        Reply::content("still not json"),
        Reply::content("never json"),
    ]);
    let server = start(&database, &provider, &[]);

    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    assert_eq!(response.status(), 502);
    let body: Value = response.json().unwrap();
    assert_eq!(body["error"], "invalid_agent_output");
    assert_eq!(body["raw"], "never json");
    assert_eq!(provider.requests().len(), 3, "three validation attempts");

    assert_no_audit_rows(&database);
}

#[test]
fn schema_mismatches_are_rejected_as_invalid_agent_output() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let mut output: Value = serde_json::from_str(&valid_output()).unwrap();
    output["surprise"] = true.into();
    let provider = FakeProvider::start(vec![Reply::content(output.to_string())]);
    let server = start(&database, &provider, &[("LLM_MAX_RETRIES", "0")]);

    let response = server.post(&json!({"agent": "researcher", "task": "do it"}));
    assert_eq!(response.status(), 502);
    let body: Value = response.json().unwrap();
    assert_eq!(body["error"], "invalid_agent_output");
    assert_eq!(body["raw"], output.to_string());

    assert_no_audit_rows(&database);
}

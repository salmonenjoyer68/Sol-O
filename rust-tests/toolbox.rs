//! Black-box tests for planet-requested tools: the envelope round trip, the
//! allowlist, the turn budget and the requested-limit clamp.

mod support;

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};
use support::fake_provider::{FakeProvider, Reply};
use tempfile::TempDir;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const TASK: &str = "Explain online ordering for cafes.";

struct Server {
    child: Child,
    _stderr: BufReader<ChildStderr>,
    base: String,
}

impl Server {
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
            "TAVILY_API_KEY",
            "SOL_O_TAVILY_ENDPOINT",
            "MAX_TOOL_TURNS",
            "MAX_RESULT_ITEMS",
            "MAX_RESULT_CHARS",
            "MAX_ARGS_CHARS",
        ] {
            command.env_remove(name);
        }
        command.env("SOL_O_DB", database).current_dir(ROOT);
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

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client")
    }

    fn post(&self, path: &str, body: &Value) -> reqwest::blocking::Response {
        Self::client()
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .expect("POST")
    }

    fn get(&self, path: &str) -> reqwest::blocking::Response {
        Self::client()
            .get(format!("{}{path}", self.base))
            .send()
            .expect("GET")
    }

    fn wait_for_run(&self, run_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let response = self.get(&format!("/runs/{run_id}"));
            assert_eq!(response.status(), 200, "run detail");
            let detail: Value = response.json().unwrap();
            let status = detail["run"]["status"].as_str().unwrap_or_default();
            if status != "running" {
                return detail;
            }
            assert!(Instant::now() < deadline, "run stayed running: {detail}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A loopback Tavily stand-in so the run never touches the network.
struct FakeTavily;

impl FakeTavily {
    fn start() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake tavily");
        let address = listener.local_addr().unwrap();
        let body = json!({"results": []}).to_string();
        std::thread::spawn(move || {
            use std::io::Write;
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut content_length = 0_usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body_bytes = vec![0_u8; content_length];
                let _ = std::io::Read::read_exact(&mut reader, &mut body_bytes);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/search")
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
        "draft": "Online ordering helps your cafe.",
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

fn envelope(tool: &str, args: Value) -> String {
    json!({"tool_requests": [{"tool": tool, "args": args}]}).to_string()
}

fn start_server(database: &Path, provider: &FakeProvider, extra: &[(&str, &str)]) -> Server {
    let tavily = FakeTavily::start();
    let mut envs = vec![
        ("BASE_URL".to_string(), provider.base_url().to_string()),
        ("API_KEY".to_string(), "test-key".to_string()),
        ("MODEL".to_string(), "test-model".to_string()),
        ("TAVILY_API_KEY".to_string(), "tvly-test-key".to_string()),
        ("SOL_O_TAVILY_ENDPOINT".to_string(), tavily),
    ];
    envs.extend(
        extra
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string())),
    );
    let envs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Server::start(database, &envs)
}

fn run_orchestration(server: &Server) -> (String, Value) {
    let response = server.post("/orchestrate", &json!({"task": TASK}));
    assert_eq!(response.status(), 200);
    let accepted: Value = response.json().unwrap();
    let run_id = accepted["run_id"].as_str().expect("run id").to_string();
    let detail = server.wait_for_run(&run_id);
    (run_id, detail)
}

fn events(server: &Server, run_id: &str) -> Vec<Value> {
    server
        .get(&format!("/runs/{run_id}/events"))
        .json()
        .unwrap()
}

fn events_named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event"] == name)
        .collect()
}

#[test]
fn an_allowed_tool_runs_and_the_researcher_sees_the_value() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(envelope("calc", json!({"expression": "3 * 19.99"}))),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let server = start_server(&database, &provider, &[]);

    let (run_id, detail) = run_orchestration(&server);

    assert_eq!(detail["run"]["status"], "complete");
    let calls = detail["agent_calls"].as_array().unwrap();
    let writer = calls.iter().find(|call| call["agent"] == "writer").unwrap();
    assert_eq!(writer["attempts"], 1, "a tool turn is not a validation attempt");
    assert_eq!(writer["tool_turns"], 1);
    assert_eq!(writer["status"], "ok");

    let tools: Vec<&Value> = detail["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["tool"] == "calc")
        .collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["tool"], "calc");
    assert_eq!(tools[0]["status"], "ok");
    assert!(tools[0]["started_at"].as_str().unwrap().contains('T'));
    assert!(tools[0]["duration_ms"].as_i64().unwrap() >= 0);
    let args: Value = serde_json::from_str(tools[0]["args"].as_str().unwrap()).unwrap();
    assert_eq!(args["expression"], "3 * 19.99");

    let saved = events(&server, &run_id);
    let requested = events_named(&saved, "tool_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(
        requested[0]["data"],
        json!({"step": 2, "agent": "writer", "tool": "calc"})
    );
    let done = events_named(&saved, "tool_done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["data"]["step"], 2);
    assert_eq!(done[0]["data"]["agent"], "writer");
    assert_eq!(done[0]["data"]["tool"], "calc");
    assert_eq!(done[0]["data"]["ok"], true);
    assert_eq!(done[0]["data"]["truncated"], false);
    assert!(done[0]["data"]["ms"].as_i64().unwrap() >= 0);
    assert!(done[0]["data"]["chars"].as_i64().unwrap() > 0);

    // The writer's next call carries the tool result, not the envelope.
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let writer_turn = *requests[2].contents().last().unwrap();
    assert!(writer_turn.contains("Tool results (JSON):"));
    assert!(writer_turn.contains("59.97"));
}

#[test]
fn a_tool_outside_the_allowlist_is_a_failed_call_and_never_raises() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(envelope("kb_search", json!({"query": "weekend"}))),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let server = start_server(&database, &provider, &[]);

    let (run_id, detail) = run_orchestration(&server);

    assert_eq!(detail["run"]["status"], "complete");
    let tools: Vec<&Value> = detail["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["tool"] == "kb_search")
        .collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["status"], "failed");
    assert_eq!(tools[0]["error_message"], "tool not allowed: kb_search");

    let doc = events(&server, &run_id);
    let done = events_named(&doc, "tool_done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["data"]["ok"], false);

    let requests = provider.requests();
    let writer_turn = *requests[2].contents().last().unwrap();
    assert!(writer_turn.contains("tool not allowed: kb_search"));
    assert!(writer_turn.contains("\"ok\": false"));
}

#[test]
fn an_envelope_from_an_agent_without_tools_goes_down_the_validation_path() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let envelope = envelope("calc", json!({"expression": "1 + 1"}));
    let provider = FakeProvider::start(vec![
        Reply::content(envelope.clone()),
        Reply::content(envelope.clone()),
        Reply::content(envelope),
    ]);
    let server = start_server(&database, &provider, &[]);

    let response = server.post(
        "/run",
        &json!({"agent": "writer", "task": "write the reply"}),
    );
    assert_eq!(response.status(), 502);
    let body: Value = response.json().unwrap();
    assert_eq!(body["error"], "invalid_agent_output");
    assert!(body["raw"].as_str().unwrap_or_default().contains("tool_requests"));
    assert_eq!(provider.requests().len(), 3, "no tool call was made");
}

#[test]
fn an_exhausted_tool_budget_returns_the_budget_instruction() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let envelope = envelope("calc", json!({"expression": "1 + 1"}));
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(envelope.clone()),
        Reply::content(envelope.clone()),
        Reply::content(envelope.clone()),
        Reply::content(envelope),
    ]);
    let server = start_server(&database, &provider, &[("MAX_TOOL_TURNS", "1")]);

    let (_run_id, detail) = run_orchestration(&server);

    assert_eq!(detail["run"]["status"], "failed");
    let tools: Vec<&Value> = detail["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["tool"] == "calc")
        .collect();
    assert_eq!(tools.len(), 1, "only the one allowed turn ran");
    assert_eq!(tools[0]["status"], "ok");

    let writer = detail["agent_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|call| call["agent"] == "writer")
        .unwrap();
    assert_eq!(writer["status"], "failed");
    assert_eq!(writer["tool_turns"], 1);
    assert_eq!(writer["attempts"], 3);
    assert!(writer["error_message"]
        .as_str()
        .unwrap()
        .contains("Tool budget is exhausted"));

    let requests = provider.requests();
    assert!(
        requests.iter().any(|request| request.contents().last().is_some_and(|text| text.contains("Tool budget is exhausted"))),
        "the budget instruction reaches the model through the retry path"
    );
}

#[test]
fn a_requested_limit_is_clamped_before_the_handler_and_the_audit() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(envelope(
            "calc",
            json!({"expression": "1 + 1", "limit": 10_000}),
        )),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let server = start_server(&database, &provider, &[("MAX_RESULT_ITEMS", "4")]);

    let (run_id, detail) = run_orchestration(&server);

    assert_eq!(detail["run"]["status"], "complete");
    let tools: Vec<&Value> = detail["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["tool"] == "calc")
        .collect();
    let args: Value = serde_json::from_str(tools[0]["args"].as_str().unwrap()).unwrap();
    assert_eq!(args["limit"], 4);
    let saved = events(&server, &run_id);
    let done = events_named(&saved, "tool_done");
    assert_eq!(done[0]["data"]["ok"], true);
    let requests = provider.requests();
    assert!(requests[2].contents().last().is_some_and(|text| text.contains("\"value\": \"2\"")));
}

#[test]
fn an_oversized_result_is_clipped_and_flagged_truncated() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(envelope("calc", json!({"expression": "'a' * 500"}))),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let server = start_server(&database, &provider, &[("MAX_RESULT_CHARS", "200")]);

    let (run_id, _detail) = run_orchestration(&server);

    let requests = provider.requests();
    let writer_turn = *requests[2].contents().last().unwrap();
    assert!(writer_turn.contains("truncated"));
    assert!(!writer_turn.contains(&"a".repeat(500)));
    let fed: Value = serde_json::from_str(writer_turn.splitn(2, "\n").nth(1).unwrap()).unwrap();
    let payload = &fed[0];
    assert_eq!(payload["tool"], "calc");
    assert_eq!(payload["truncated"], true);
    assert!(payload["result"]["value"].as_str().unwrap().ends_with('…'));
    assert!(serde_json::to_string(payload).unwrap().len() <= 200 + 40);

    let saved = events(&server, &run_id);
    let done = events_named(&saved, "tool_done");
    assert_eq!(done[0]["data"]["truncated"], true);
}

#[test]
fn the_audited_args_are_capped() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(envelope("calc", json!({"expression": "1 + 1"}))),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let server = start_server(&database, &provider, &[("MAX_ARGS_CHARS", "17")]);

    let (_run_id, detail) = run_orchestration(&server);

    let calc = detail["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["tool"] == "calc")
        .unwrap();
    assert!(calc["args"].as_str().unwrap().chars().count() <= 17);
}

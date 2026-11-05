//! Black-box tests for one orchestration run: acceptance, plan order, events,
//! audit rows, the run detail shape and replay of the saved event stream.

mod support;

use std::io::{BufRead, BufReader, Read};
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
            "REQUIRE_APPROVAL",
            "AUTO_APPROVE",
            "PENDING_TTL_HOURS",
            "DRY_RUN",
            "TELEGRAM_BOT_TOKEN",
            "TELEGRAM_CHAT_ID",
            "SOL_O_TELEGRAM_ENDPOINT",
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

    /// Poll saved events until `name` appears; the logger saves the run row
    /// before the notifier step finishes, so status alone is not a barrier.
    fn wait_for_event(&self, run_id: &str, name: &str) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let saved: Vec<Value> = self
                .get(&format!("/runs/{run_id}/events"))
                .json()
                .expect("saved events");
            if saved.iter().any(|event| event["event"] == name) || Instant::now() > deadline {
                return saved;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
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

/// A loopback Tavily stand-in: fixed results, no network.
struct FakeTavily {
    endpoint: String,
    queries: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl FakeTavily {
    fn start(results: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake tavily");
        let address = listener.local_addr().unwrap();
        let queries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = queries.clone();
        let body = json!({"results": results}).to_string();
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
                    seen.lock()
                        .unwrap()
                        .push(parsed["query"].as_str().unwrap_or_default().to_string());
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                use std::io::Write;
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            endpoint: format!("http://{address}/search"),
            queries,
        }
    }

    fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}

fn researcher_output() -> String {
    json!({
        "agent": "researcher",
        "status": "complete",
        "task": TASK,
        "summary": "Online ordering helps cafes.",
        "findings": [{"claim": "orders rise", "source_url": "https://example.com/a", "confidence": "high"}],
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

fn provider_env(base_url: &str) -> Vec<(String, String)> {
    vec![
        ("BASE_URL".to_string(), base_url.to_string()),
        ("API_KEY".to_string(), "test-key".to_string()),
        ("MODEL".to_string(), "test-model".to_string()),
    ]
}

fn start_server(database: &Path, provider: &FakeProvider, extra: &[(&str, &str)]) -> Server {
    let mut envs = provider_env(provider.base_url());
    // These tests exercise the auto-approved path; approval has its own suite.
    envs.push(("AUTO_APPROVE".to_string(), "true".to_string()));
    for (name, value) in extra {
        envs.push((name.to_string(), value.to_string()));
    }
    let envs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Server::start(database, &envs)
}

fn read_sse_events(stream: reqwest::blocking::Response) -> Vec<Value> {
    let mut reader = BufReader::new(stream);
    let mut events = Vec::new();
    loop {
        let mut block = String::new();
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read SSE line");
            if read == 0 {
                return events;
            }
            if line == "\n" {
                break;
            }
            block.push_str(&line);
        }
        let Some(data) = block
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .map(|value| serde_json::from_str::<Value>(value).expect("SSE data JSON"))
        else {
            continue; // keepalive
        };
        let done = data["event"] == "all_done" || data["event"] == "run_failed";
        events.push(data);
        if done {
            return events;
        }
    }
}

#[test]
fn one_run_accepts_then_completes_with_the_documented_events_and_rows() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let tavily = FakeTavily::start(json!([
        {"title": "Source A", "url": "https://example.com/a", "content": "orders  rise"},
        {"title": "Source B", "url": "https://example.com/b", "content": "another source"}
    ]));
    // A slow first reply lets the test observe the accepted-but-running state.
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()).delayed(Duration::from_millis(300)),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let server = start_server(
        &database,
        &provider,
        &[
            ("TAVILY_API_KEY", "tvly-test-key"),
            ("SOL_O_TAVILY_ENDPOINT", &tavily.endpoint),
        ],
    );

    // Subscribe to the live stream before the run starts.
    let live = Server::client()
        .get(format!("{}/stream", server.base))
        .send()
        .expect("GET /stream");

    let response = server.post("/orchestrate", &json!({"task": TASK}));
    assert_eq!(response.status(), 200);
    let accepted: Value = response.json().unwrap();
    let run_id = accepted["run_id"].as_str().expect("run id").to_string();
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(run_id.len(), 32, "uuid4().hex length: {run_id}");
    assert!(run_id
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

    // The row exists as running before the first agent answers.
    let early = server.get(&format!("/runs/{run_id}"));
    assert_eq!(early.status(), 200);
    let early: Value = early.json().unwrap();
    assert_eq!(early["run"]["status"], "running");
    assert_eq!(early["run"]["id"], run_id.as_str());

    let live_events = read_sse_events(live);
    let detail = server.wait_for_run(&run_id);
    let saved: Vec<Value> = server
        .get(&format!("/runs/{run_id}/events"))
        .json()
        .unwrap();

    // One saved event per live event, same order and payload.
    assert_eq!(
        saved.len(),
        live_events.len(),
        "replay matches the live stream"
    );
    for (index, (live_event, saved_event)) in live_events.iter().zip(&saved).enumerate() {
        assert_eq!(saved_event["seq"], live_event["seq"], "seq {index}");
        assert_eq!(saved_event["event"], live_event["event"], "event {index}");
        assert_eq!(saved_event["data"], live_event["data"], "data {index}");
        assert_eq!(saved_event["run_id"], run_id.as_str());
        assert_eq!(
            saved_event["seq"],
            index as i64 + 1,
            "per-run seq starts at 1"
        );
    }

    let events: Vec<&str> = saved
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        [
            "run_started",
            "plan_ready",
            "agent_called",
            "agent_working",
            "search_done",
            "agent_working",
            "agent_done",
            "agent_called",
            "agent_working",
            "agent_done",
            "agent_called",
            "agent_working",
            "agent_done",
            "agent_called",
            "logger_saved",
            "agent_done",
            "agent_called",
            "agent_working",
            "notifier_sent",
            "agent_done",
            "all_done",
        ]
    );

    assert_eq!(saved[0]["data"]["task"], TASK);
    let plan = &saved[1]["data"];
    assert_eq!(plan["planner"], "hardcoded");
    assert_eq!(plan["agents"], json!(["researcher", "writer", "checker"]));
    assert_eq!(plan["steps"][0]["agent"], "researcher");
    assert_eq!(plan["steps"][0]["task"], TASK);
    assert_eq!(plan["steps"][2]["agent"], "checker");

    assert_eq!(saved[2]["data"]["step"], 1);
    assert_eq!(saved[2]["data"]["agent"], "researcher");
    assert_eq!(
        saved[3]["data"],
        json!({"agent": "researcher", "attempt": 1, "call": 1, "step": 1})
    );
    assert_eq!(saved[4]["data"]["query"], TASK);
    assert_eq!(saved[4]["data"]["results"], 2);
    assert_eq!(
        saved[5]["data"],
        json!({"agent": "researcher", "attempt": 1, "call": 1, "step": 1})
    );
    assert_eq!(saved[6]["data"]["status"], "complete");
    assert_eq!(saved[6]["data"]["attempts"], 1);
    assert_eq!(saved[6]["data"]["tool_turns"], 0);
    assert_eq!(saved[6]["data"]["summary"], "Online ordering helps cafes.");
    assert_eq!(saved[8]["data"]["step"], 2);
    assert_eq!(saved[8]["data"]["agent"], "writer");
    assert_eq!(
        saved[9]["data"]["summary"],
        "Dear operator, online ordering helps your cafe."
    );
    assert_eq!(saved[12]["data"]["summary"], "approved");
    assert_eq!(saved[13]["data"], json!({"agent": "logger", "step": 4}));
    assert_eq!(
        saved[14]["data"],
        json!({"agent": "logger", "run_id": run_id, "step": 4})
    );
    assert_eq!(saved[15]["data"]["status"], "complete");
    assert_eq!(saved[15]["data"]["summary"], "complete");
    assert_eq!(saved[16]["data"], json!({"agent": "notifier", "step": 5}));
    assert_eq!(
        saved[17]["data"],
        json!({"agent": "notifier", "attempt": 1, "call": 1, "step": 5})
    );
    assert_eq!(
        saved[18]["data"],
        json!({"dry_run": true, "delivered": false, "parts": 1, "step": 5})
    );
    assert_eq!(saved[19]["data"]["status"], "complete");
    assert_eq!(saved[19]["data"]["summary"], "complete");
    assert_eq!(saved[20]["data"]["status"], "complete");
    assert_eq!(saved[20]["data"]["steps"], 5);
    assert_eq!(
        saved[20]["data"]["seconds"],
        saved[20]["data"]["duration_seconds"]
    );

    // The run row and its stored outputs.
    let run = &detail["run"];
    assert_eq!(run["status"], "complete");
    assert_eq!(run["task"], TASK);
    assert_eq!(
        run["final_draft"],
        "Dear operator, online ordering helps your cafe."
    );
    assert_eq!(run["verdict"], "approved");
    assert!(run["warning"].is_null());
    assert_eq!(run["source"], "");
    // The logger stores the agent steps; the system steps live in the timeline.
    let outputs = run["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 3);
    let agents: Vec<&str> = outputs
        .iter()
        .map(|output| output["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, ["researcher", "writer", "checker"]);
    for (index, output) in outputs.iter().enumerate() {
        assert_eq!(output["step"], index as i64 + 1);
    }
    assert_eq!(outputs[0]["task"], TASK);
    assert_eq!(outputs[0]["attempts"], 1);
    assert_eq!(outputs[0]["tool_turns"], 0);
    assert_eq!(outputs[0]["output"]["agent"], "researcher");
    assert_eq!(outputs[1]["output"]["draft"], run["final_draft"]);
    assert_eq!(outputs[2]["output"]["verdict"], "approved");

    // Audit rows: one per agent step, with timings and counts.
    let calls = detail["agent_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3, "one agent_calls row per LLM call");
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(call["step"], index as i64 + 1);
        assert_eq!(call["status"], "ok");
        assert_eq!(call["attempts"], 1);
        assert_eq!(call["tool_turns"], 0);
        assert_eq!(call["model"], "test-model");
        assert_eq!(call["run_id"], run_id.as_str());
        assert!(call["error_type"].is_null());
        assert!(call["error_message"].is_null());
        let raw = call["raw_output"].as_str().unwrap();
        assert_eq!(call["output_chars"], raw.chars().count() as i64);
        assert_eq!(
            call["input_chars"],
            provider.requests()[index].contents()[1].chars().count() as i64
        );
        let parsed: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed["agent"], call["agent"]);
        assert!(call["started_at"].as_str().unwrap().ends_with("+00:00"));
    }
    let agents: Vec<&str> = calls
        .iter()
        .map(|call| call["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, ["researcher", "writer", "checker"]);

    let tool_calls = detail["tool_calls"].as_array().unwrap();
    assert_eq!(
        tool_calls.len(),
        2,
        "search plus the auto-approved telegram send"
    );
    assert_eq!(tool_calls[0]["tool"], "search");
    assert_eq!(tool_calls[0]["status"], "ok");
    assert!(tool_calls[0]["error_message"].is_null());
    assert_eq!(tool_calls[1]["tool"], "telegram");
    assert_eq!(tool_calls[1]["status"], "ok");
    assert_eq!(detail["pending_actions"], json!([]));

    // Plan order and the reference message layout.
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "one LLM call per step");
    for request in &requests {
        assert_eq!(request.response_format_type(), Some("json_schema"));
        assert_eq!(request.roles(), ["system", "user"]);
    }
    let researcher = std::fs::read_to_string(Path::new(ROOT).join("agents/researcher.md")).unwrap();
    let system = requests[0].contents()[0];
    assert!(system.starts_with(&format!("{researcher}{}", sol_o::agents::STEP_CONTRACT)));
    assert!(system.contains("## Tool envelope (runtime contract)"));
    assert!(system.contains("at most 3 tool turns"));
    let first = requests[0].contents()[1];
    assert!(first.starts_with(&format!(
        "Original task:\n{TASK}\n\nYour step instruction:\n{TASK}\n\nOutputs from earlier agents (JSON):\n[]"
    )));
    assert!(first.contains("Web search results (JSON):"));
    assert!(first.contains("https://example.com/a"));
    assert!(first.contains("\"note\": \"\""));

    let second = requests[1].contents()[1];
    assert!(second.starts_with(&format!(
        "Original task:\n{TASK}\n\nYour step instruction:\n{TASK}\n\nOutputs from earlier agents (JSON):\n["
    )));
    assert!(second.contains("\"agent\": \"researcher\""));
    assert!(second.contains("Online ordering helps cafes."));
    assert!(!second.contains("Web search results (JSON):"));

    let third = requests[2].contents()[1];
    assert!(third.contains("\"agent\": \"researcher\""));
    assert!(third.contains("\"agent\": \"writer\""));
    assert!(third.contains("Dear operator, online ordering helps your cafe."));

    assert_eq!(
        tavily.queries(),
        vec![TASK.to_string()],
        "one search per run"
    );
}

#[test]
fn a_search_failure_still_completes_the_run_with_the_no_results_note() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    // No TAVILY_API_KEY: search degrades instead of failing the run.
    let server = start_server(&database, &provider, &[]);

    let accepted: Value = server
        .post("/orchestrate", &json!({"task": TASK}))
        .json()
        .unwrap();
    let run_id = accepted["run_id"].as_str().unwrap();
    let detail = server.wait_for_run(run_id);
    assert_eq!(detail["run"]["status"], "complete");

    let saved: Vec<Value> = server
        .get(&format!("/runs/{run_id}/events"))
        .json()
        .unwrap();
    let failed = saved
        .iter()
        .find(|event| event["event"] == "search_failed")
        .expect("search_failed event");
    assert_eq!(failed["data"]["query"], TASK);
    assert_eq!(failed["data"]["reason"], "TAVILY_API_KEY is not set");

    let tool_calls = detail["tool_calls"].as_array().unwrap();
    let search_calls: Vec<&Value> = tool_calls
        .iter()
        .filter(|call| call["tool"] == "search")
        .collect();
    assert_eq!(search_calls.len(), 1);
    assert_eq!(search_calls[0]["status"], "failed");
    assert_eq!(
        search_calls[0]["error_message"],
        "TAVILY_API_KEY is not set"
    );
    assert!(tool_calls.iter().any(|call| call["tool"] == "telegram"));

    let requests = provider.requests();
    let user = requests[0].contents()[1];
    assert!(user.contains(
        "No search results are available. Do not invent sources: mark every claim 'unverified' and list what could not be checked in gaps."
    ));
    assert!(user.contains("\"results\": []"));
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(
        detail["run"]["final_draft"],
        "Dear operator, online ordering helps your cafe."
    );
}

#[test]
fn inbound_accepts_with_202_and_runs_the_same_plan() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![
        Reply::content(researcher_output()),
        Reply::content(writer_output()),
        Reply::content(checker_output()),
    ]);
    let server = start_server(&database, &provider, &[]);

    let response = server.post("/inbound", &json!({"message": TASK, "source": "telegram"}));
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().unwrap();
    assert_eq!(accepted["status"], "accepted");
    let run_id = accepted["run_id"].as_str().unwrap();

    let detail = server.wait_for_run(run_id);
    assert_eq!(detail["run"]["status"], "complete");
    assert_eq!(detail["run"]["source"], "telegram");
    let saved = server.wait_for_event(run_id, "all_done");
    assert!(saved.iter().any(|event| event["event"] == "plan_ready"));
    assert!(saved.iter().any(|event| event["event"] == "all_done"));
    assert_eq!(detail["agent_calls"].as_array().unwrap().len(), 3);
}

#[test]
fn orchestrate_validates_its_body() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let provider = FakeProvider::start(vec![]);
    let server = start_server(&database, &provider, &[]);

    let empty = server.post("/orchestrate", &json!({"task": ""}));
    assert_eq!(empty.status(), 422);
    assert_eq!(
        empty.json::<Value>().unwrap()["detail"][0]["loc"],
        json!(["body", "task"])
    );

    let unknown = server.post("/orchestrate", &json!({"task": "x", "surprise": true}));
    assert_eq!(unknown.status(), 422);
    assert!(provider.requests().is_empty());

    let missing = server.get("/runs/nope");
    assert_eq!(missing.status(), 404);
    assert_eq!(missing.json::<Value>().unwrap()["error"], "run_not_found");
}

//! Shared test support for the black-box API tests.

pub mod fake_provider;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

/// The checkout root; tests run from here and the binary resolves it as its cwd.
pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Settings a developer `.env` could leak into a spawned server.
const SCRUBBED: [&str; 25] = [
    "API_KEY",
    "BASE_URL",
    "MODEL",
    "AGENT_MODELS",
    "LLM_MAX_RETRIES",
    "LLM_TIMEOUT_SECONDS",
    "TOOL_TIMEOUT_SECONDS",
    "RUN_TIMEOUT_SECONDS",
    "MAX_AGENTS",
    "MAX_INPUT_CHARS",
    "TAVILY_API_KEY",
    "SOL_O_TAVILY_ENDPOINT",
    "SOL_O_AGENTS_DIR",
    "REQUIRE_APPROVAL",
    "AUTO_APPROVE",
    "PENDING_TTL_HOURS",
    "DRY_RUN",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_CHAT_ID",
    "SOL_O_TELEGRAM_ENDPOINT",
    "MAX_TOOL_TURNS",
    "MAX_RESULT_ITEMS",
    "MAX_RESULT_CHARS",
    "MAX_ARGS_CHARS",
    "SOL_O_ROOT",
];

/// An empty env file inside the test's temp dir, so a developer `.env` never
/// leaks into a black-box server.
pub fn hermetic_env_file(database: &Path) -> PathBuf {
    let path = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("empty.env");
    std::fs::write(&path, []).expect("write empty test env file");
    path
}

/// `sol-o serve` on an ephemeral port, plus the request helpers the suites share.
pub struct Server {
    child: Child,
    _stderr: BufReader<ChildStderr>,
    pub base: String,
}

#[allow(dead_code)]
impl Server {
    pub fn start(database: &Path, envs: &[(&str, &str)]) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sol-o"));
        command.args(["serve", "--host", "127.0.0.1", "--port", "0"]);
        for name in SCRUBBED {
            command.env_remove(name);
        }
        command
            .env("SOL_O_DB", database)
            .env("SOL_O_ENV_FILE", hermetic_env_file(database))
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

    pub fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client")
    }

    pub fn post(&self, path: &str, body: &Value) -> reqwest::blocking::Response {
        Self::client()
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .expect("POST")
    }

    pub fn empty_post(&self, path: &str) -> reqwest::blocking::Response {
        Self::client()
            .post(format!("{}{path}", self.base))
            .send()
            .expect("POST")
    }

    pub fn get(&self, path: &str) -> reqwest::blocking::Response {
        Self::client()
            .get(format!("{}{path}", self.base))
            .send()
            .expect("GET")
    }

    pub fn events(&self, run_id: &str) -> Vec<Value> {
        self.get(&format!("/runs/{run_id}/events"))
            .json()
            .expect("saved events")
    }

    /// Poll saved events until `name` appears; the logger saves the run row
    /// before the notifier step finishes, so status alone is not a barrier.
    pub fn wait_for_event(&self, run_id: &str, name: &str) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let saved = self.events(run_id);
            if saved.iter().any(|event| event["event"] == name) || Instant::now() > deadline {
                return saved;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Poll saved events until the run publishes a terminal event.
    pub fn wait_for_terminal(&self, run_id: &str) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let saved = self.events(run_id);
            let done = saved
                .iter()
                .any(|event| event["event"] == "all_done" || event["event"] == "run_failed");
            if done || Instant::now() > deadline {
                return saved;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_for_run(&self, run_id: &str) -> Value {
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

    pub fn pending(&self) -> Vec<Value> {
        self.get("/pending").json().expect("pending list")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

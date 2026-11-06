//! Black-box tests: boot the binary against temporary and hand-built legacy databases.

mod support;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::Duration;

use regex::Regex;
use rusqlite::Connection;
use serde_json::{json, Value};
use tempfile::TempDir;

use support::ROOT;
const RUN_KEYS: [&str; 10] = [
    "created_at",
    "duration_seconds",
    "final_draft",
    "id",
    "outputs",
    "source",
    "status",
    "task",
    "verdict",
    "warning",
];

struct Server {
    child: Child,
    _stderr: BufReader<ChildStderr>,
    base: String,
}

impl Server {
    /// Boot `sol-o serve` on an ephemeral port and wait for the address log line.
    fn start(database: &Path) -> Server {
        Server::start_at(Path::new(ROOT), database, &[])
    }

    /// Boot with an explicit working directory (the runtime root) and extra env.
    fn start_at(root: &Path, database: &Path, envs: &[(&str, &str)]) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sol-o"));
        command
            .args(["serve", "--host", "127.0.0.1", "--port", "0"])
            .env_remove("API_KEY")
            .env_remove("BASE_URL")
            .env_remove("MODEL")
            .env_remove("SOL_O_ROOT")
            .env("SOL_O_DB", database)
            .env("SOL_O_ENV_FILE", support::hermetic_env_file(database))
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (name, value) in envs {
            command.env(name, value);
        }
        let mut child = command.spawn().expect("spawn sol-o serve");
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

    fn client(&self) -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("http client")
    }

    fn get(&self, path: &str) -> reqwest::blocking::Response {
        self.client()
            .get(format!("{}{path}", self.base))
            .send()
            .expect("GET request")
    }

    fn json(&self, path: &str) -> Value {
        let response = self.get(path);
        assert_eq!(response.status(), 200, "GET {path}");
        response.json().expect("JSON body")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn resolves_its_root_at_runtime_not_at_build_time() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("web")).unwrap();
    std::fs::write(root.path().join("web/index.html"), "<p>runtime root</p>").unwrap();

    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let server = Server::start_at(root.path(), &database, &[]);
    assert_eq!(
        server.get("/").text().unwrap(),
        "<p>runtime root</p>",
        "the binary serves web/index.html from its working directory"
    );

    // `SOL_O_ROOT` wins over the working directory.
    let elsewhere = TempDir::new().unwrap();
    let database = dir.path().join("override.db");
    let root_text = root.path().to_str().unwrap();
    let server = Server::start_at(elsewhere.path(), &database, &[("SOL_O_ROOT", root_text)]);
    assert_eq!(
        server.get("/").text().unwrap(),
        "<p>runtime root</p>",
        "SOL_O_ROOT overrides the working directory"
    );
}

#[test]
fn serves_the_ui_from_disk_and_browses_an_empty_history() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("sol-o.db");
    let server = Server::start(&database);
    let client = server.client();

    let page = server.get("/");
    assert_eq!(page.status(), 200);
    let content_type = page.headers()["content-type"].to_str().unwrap().to_string();
    assert!(
        content_type.starts_with("text/html"),
        "content type {content_type}"
    );
    let served = page.bytes().unwrap();
    let stored = std::fs::read(Path::new(ROOT).join("web/index.html")).unwrap();
    assert_eq!(
        served.as_ref(),
        stored.as_slice(),
        "UI is served byte-for-byte from disk"
    );

    assert_eq!(server.json("/runs"), json!([]));
    assert!(database.exists(), "schema is created on first database use");

    let allowed = client
        .get(format!("{}/runs", server.base))
        .header("Origin", "http://localhost:3000")
        .send()
        .unwrap();
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "http://localhost:3000"
    );
    assert_eq!(
        allowed.headers()["access-control-allow-credentials"],
        "true"
    );

    let denied = client
        .get(format!("{}/runs", server.base))
        .header("Origin", "https://example.com")
        .send()
        .unwrap();
    assert!(denied
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{}/runs", server.base))
        .header("Origin", "http://127.0.0.1:4000")
        .header("Access-Control-Request-Method", "GET")
        .send()
        .unwrap();
    assert_eq!(preflight.status(), 200);
    assert_eq!(
        preflight.headers()["access-control-allow-origin"],
        "http://127.0.0.1:4000"
    );
}

#[test]
fn opens_a_database_written_by_the_python_service() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("legacy.db");
    write_legacy_database(&database);

    let server = Server::start(&database);

    let runs = server.json("/runs");
    let rows = runs.as_array().expect("runs is a list");
    assert_eq!(rows.len(), 2, "no rows are lost");
    let newest = &rows[0];
    assert_eq!(newest["id"], "old-2");
    assert_eq!(newest["status"], "failed");
    assert_eq!(newest["warning"], Value::Null);
    assert_eq!(newest["verdict"], Value::Null);
    assert_eq!(newest["source"], "");
    assert_eq!(newest["outputs"], json!([]));
    assert_eq!(newest["duration_seconds"], 0.5);
    let mut keys: Vec<&str> = newest
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, RUN_KEYS, "run payload has exactly the reference keys");

    let older = &rows[1];
    assert_eq!(older["id"], "old-1");
    assert_eq!(older["outputs"][0]["agent"], "researcher");
    assert_eq!(older["outputs"][0]["output"]["summary"], "s");
    assert_eq!(older["duration_seconds"], 1.25);

    assert_eq!(server.json("/runs?limit=1").as_array().unwrap().len(), 1);
    assert_eq!(server.json("/runs?limit=1")[0]["id"], "old-2");
    let failed = server.json("/runs?status=failed");
    assert_eq!(failed.as_array().unwrap().len(), 1);
    assert_eq!(failed[0]["id"], "old-2");

    let detail = server.json("/runs/old-1");
    let mut detail_keys: Vec<&str> = detail
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    detail_keys.sort_unstable();
    assert_eq!(
        detail_keys,
        ["agent_calls", "pending_actions", "run", "tool_calls"]
    );
    assert_eq!(detail["run"]["id"], "old-1");
    assert_eq!(detail["run"]["source"], "");
    let agent_call = &detail["agent_calls"][0];
    assert_eq!(agent_call["agent"], "researcher");
    assert_eq!(agent_call["tool_turns"], 0);
    assert_eq!(agent_call["model"], "");
    assert_eq!(agent_call["error_type"], Value::Null);
    let tool_call = &detail["tool_calls"][0];
    assert_eq!(tool_call["tool"], "calc");
    assert_eq!(tool_call["args"], "");
    let pending = &detail["pending_actions"][0];
    assert_eq!(pending["payload"]["message"], "hi");
    assert!(pending.get("payload_json").is_none());
    assert_eq!(pending["decided_at"], Value::Null);

    let events = server.json("/runs/old-1/events");
    let saved = events.as_array().unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["seq"], 1);
    assert_eq!(saved[0]["event"], "run_started");
    assert_eq!(saved[0]["data"]["task"], "older task");

    let missing = server.get("/runs/missing");
    assert_eq!(missing.status(), 404);
    assert_eq!(missing.json::<Value>().unwrap()["error"], "run_not_found");
    assert_eq!(server.get("/runs/missing/events").status(), 404);

    let bad_limit = server.get("/runs?limit=0");
    assert_eq!(bad_limit.status(), 422);

    let connection = Connection::open(&database).unwrap();
    for (table, expected) in [
        ("runs", vec!["warning", "source"]),
        ("agent_calls", vec!["model", "tool_turns"]),
        ("tool_calls", vec!["args"]),
    ] {
        let columns = table_columns(&connection, table);
        for column in expected {
            assert!(
                columns.contains(&column.to_string()),
                "{table} gained column {column}"
            );
        }
    }
    assert_eq!(count_rows(&connection, "runs"), 2);
    assert_eq!(count_rows(&connection, "agent_calls"), 1);
    assert_eq!(count_rows(&connection, "tool_calls"), 1);
    assert_eq!(count_rows(&connection, "pending_actions"), 1);
    assert_eq!(count_rows(&connection, "run_events"), 1);
}

fn table_columns(connection: &Connection, table: &str) -> Vec<String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    names
}

fn count_rows(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// A database written before the Phase 5/7 columns existed.
fn write_legacy_database(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            r#"
            CREATE TABLE runs (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                task TEXT NOT NULL,
                status TEXT NOT NULL,
                final_draft TEXT NOT NULL DEFAULT '',
                verdict TEXT,
                outputs_json TEXT NOT NULL DEFAULT '[]',
                duration_seconds REAL NOT NULL DEFAULT 0
            );
            CREATE TABLE agent_calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                step INTEGER NOT NULL,
                agent TEXT NOT NULL,
                started_at TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                attempts INTEGER NOT NULL,
                status TEXT NOT NULL,
                error_type TEXT,
                error_message TEXT,
                input_chars INTEGER NOT NULL,
                output_chars INTEGER NOT NULL,
                raw_output TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE tool_calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                tool TEXT NOT NULL,
                started_at TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                status TEXT NOT NULL,
                error_message TEXT
            );
            CREATE TABLE pending_actions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                decided_at TEXT
            );
            CREATE TABLE run_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                ts TEXT NOT NULL,
                event TEXT NOT NULL,
                data_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX idx_run_events_run ON run_events(run_id, seq);
            "#,
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs (id, created_at, task, status, final_draft, verdict, outputs_json, duration_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "old-1",
                "2026-01-01T10:00:00+00:00",
                "older task",
                "complete",
                "draft one",
                Option::<String>::None,
                r#"[{"step":1,"agent":"researcher","output":{"summary":"s"}}]"#,
                1.25
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO runs (id, created_at, task, status, final_draft, verdict, outputs_json, duration_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "old-2",
                "2026-01-02T10:00:00+00:00",
                "newest task",
                "failed",
                "",
                Option::<String>::None,
                "[]",
                0.5
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO agent_calls (run_id, step, agent, started_at, duration_ms, attempts, status, input_chars, output_chars, raw_output)
             VALUES ('old-1', 1, 'researcher', '2026-01-01T10:00:01+00:00', 120, 1, 'ok', 10, 20, '{\"summary\":\"s\"}')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tool_calls (run_id, tool, started_at, duration_ms, status)
             VALUES ('old-1', 'calc', '2026-01-01T10:00:02+00:00', 5, 'ok')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO pending_actions (run_id, action_type, payload_json, status, created_at)
             VALUES ('old-1', 'telegram', '{\"message\":\"hi\",\"run_status\":\"complete\"}', 'pending', '2026-01-01T10:00:03+00:00')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO run_events (run_id, seq, ts, event, data_json)
             VALUES ('old-1', 1, '2026-01-01T10:00:01.123+00:00', 'run_started', '{\"task\":\"older task\"}')",
            [],
        )
        .unwrap();
}

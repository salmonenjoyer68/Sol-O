//! SQLite persistence for orchestration runs.
//!
//! The five-table schema and the additive column check mirror `app/logger.py`
//! so databases written by the Python service open without manual migration.
//! Every operation opens, uses, commits and closes its own connection.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::config::ROOT;
use crate::reliability::{redact, redact_value, truncate};

pub type DbResult<T> = Result<T, String>;

/// Stored raw output cap, matching `MAX_RAW_CHARS` in the reference.
const MAX_RAW_CHARS: usize = 2000;

/// UTC ISO-8601 with second precision and a `+00:00` suffix.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// A 32-character lowercase hex run id, unique within and across processes.
pub fn new_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    // shortcut: time+counter, not uuid4; switch to a uuid crate if ids must be
    // unguessable rather than merely unique.
    format!("{:016x}{:016x}", nanos, count)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    task TEXT NOT NULL,
    status TEXT NOT NULL,
    final_draft TEXT NOT NULL DEFAULT '',
    verdict TEXT,
    outputs_json TEXT NOT NULL DEFAULT '[]',
    duration_seconds REAL NOT NULL DEFAULT 0,
    warning TEXT,
    source TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS agent_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    step INTEGER NOT NULL,
    agent TEXT NOT NULL,
    started_at TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    tool_turns INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    error_type TEXT,
    error_message TEXT,
    input_chars INTEGER NOT NULL,
    output_chars INTEGER NOT NULL,
    raw_output TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS tool_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    started_at TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    status TEXT NOT NULL,
    error_message TEXT,
    args TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS pending_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    action_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    decided_at TEXT
);

CREATE TABLE IF NOT EXISTS run_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    ts TEXT NOT NULL,
    event TEXT NOT NULL,
    data_json TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_run_events_run ON run_events(run_id, seq);
"#;

#[derive(Clone, Debug)]
pub struct Db {
    path: PathBuf,
}

impl Db {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Database file; `SOL_O_DB` overrides for tests and alternate deployments.
    pub fn from_env() -> Self {
        let override_path = std::env::var("SOL_O_DB").unwrap_or_default();
        let override_path = override_path.trim();
        if override_path.is_empty() {
            Self::new(Path::new(ROOT).join("data").join("sol-o.db"))
        } else {
            Self::new(expand_home(override_path))
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a connection, apply the schema and upgrade any older file.
    pub fn connect(&self) -> DbResult<Connection> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("create {}: {error}", parent.display()))?;
            }
        }
        let connection = Connection::open(&self.path)
            .map_err(|error| format!("open {}: {error}", self.path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(10))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|error| error.to_string())?;
        ensure_columns(&connection).map_err(|error| error.to_string())?;
        Ok(connection)
    }

    /// Append one streamed event so `GET /runs/{id}/events` can replay it.
    pub fn save_run_event(
        &self,
        run_id: &str,
        seq: i64,
        ts: &str,
        event: &str,
        data: &Value,
    ) -> DbResult<()> {
        let payload = serde_json::to_string(&redact_value(data))
            .map_err(|error| format!("serialize event: {error}"))?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO run_events (run_id, seq, ts, event, data_json) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![run_id, seq, ts, event, payload],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// One audit row per agent call, written on success and failure.
    pub fn save_agent_call(&self, call: &NewAgentCall<'_>) -> DbResult<()> {
        let error_message = call
            .error_message
            .filter(|message| !message.is_empty())
            .map(redact);
        let connection = self.connect()?;
        connection
            .execute(
                concat!(
                    "INSERT INTO agent_calls",
                    " (run_id, step, agent, started_at, duration_ms, attempts, tool_turns, status,",
                    "  error_type, error_message, input_chars, output_chars, raw_output, model)",
                    " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ),
                rusqlite::params![
                    call.run_id,
                    call.step,
                    call.agent,
                    call.started_at,
                    call.duration_ms,
                    call.attempts,
                    call.tool_turns,
                    call.status,
                    call.error_type,
                    error_message,
                    call.input_chars,
                    call.output_chars,
                    truncate(&redact(call.raw_output), MAX_RAW_CHARS),
                    redact(call.model),
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Newest runs first, optionally filtered by status, for `GET /runs`.
    pub fn recent_runs(&self, limit: i64, status: Option<&str>) -> DbResult<Vec<RunRow>> {
        let connection = self.connect()?;
        let mut rows = Vec::new();
        if let Some(status) = status {
            let mut statement = connection
                .prepare(
                    "SELECT * FROM runs WHERE status = ?
                     ORDER BY created_at DESC, rowid DESC LIMIT ?",
                )
                .map_err(|error| error.to_string())?;
            let mapped = statement
                .query_map(rusqlite::params![status, limit], run_from_row)
                .map_err(|error| error.to_string())?;
            for row in mapped {
                rows.push(row.map_err(|error| error.to_string())?);
            }
        } else {
            let mut statement = connection
                .prepare("SELECT * FROM runs ORDER BY created_at DESC, rowid DESC LIMIT ?")
                .map_err(|error| error.to_string())?;
            let mapped = statement
                .query_map(rusqlite::params![limit], run_from_row)
                .map_err(|error| error.to_string())?;
            for row in mapped {
                rows.push(row.map_err(|error| error.to_string())?);
            }
        }
        Ok(rows)
    }

    /// One run by id, or `None`.
    pub fn get_run(&self, run_id: &str) -> DbResult<Option<RunRow>> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT * FROM runs WHERE id = ?",
                rusqlite::params![run_id],
                run_from_row,
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.to_string()),
            })?;
        Ok(row)
    }

    /// Run plus its agent calls, tool calls and pending actions.
    pub fn get_run_detail(&self, run_id: &str) -> DbResult<Option<RunDetail>> {
        let Some(run) = self.get_run(run_id)? else {
            return Ok(None);
        };
        let connection = self.connect()?;
        let agent_calls = collect(
            &connection,
            "SELECT * FROM agent_calls WHERE run_id = ? ORDER BY id",
            run_id,
            |row| {
                Ok(AgentCallRow {
                    id: row.get("id")?,
                    run_id: row.get("run_id")?,
                    step: row.get("step")?,
                    agent: row.get("agent")?,
                    started_at: row.get("started_at")?,
                    duration_ms: row.get("duration_ms")?,
                    attempts: row.get("attempts")?,
                    tool_turns: row.get("tool_turns")?,
                    status: row.get("status")?,
                    error_type: row.get("error_type")?,
                    error_message: row.get("error_message")?,
                    input_chars: row.get("input_chars")?,
                    output_chars: row.get("output_chars")?,
                    raw_output: row.get("raw_output")?,
                    model: row.get("model")?,
                })
            },
        )?;
        let tool_calls = collect(
            &connection,
            "SELECT * FROM tool_calls WHERE run_id = ? ORDER BY id",
            run_id,
            |row| {
                Ok(ToolCallRow {
                    id: row.get("id")?,
                    run_id: row.get("run_id")?,
                    tool: row.get("tool")?,
                    started_at: row.get("started_at")?,
                    duration_ms: row.get("duration_ms")?,
                    status: row.get("status")?,
                    error_message: row.get("error_message")?,
                    args: row.get("args")?,
                })
            },
        )?;
        let pending_actions = collect(
            &connection,
            "SELECT * FROM pending_actions WHERE run_id = ? ORDER BY id",
            run_id,
            pending_from_row,
        )?;
        Ok(Some(RunDetail {
            run,
            agent_calls,
            tool_calls,
            pending_actions,
        }))
    }

    /// Saved events for one run, in original order.
    pub fn get_run_events(&self, run_id: &str) -> DbResult<Vec<SavedEvent>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT id, run_id, seq, ts, event, data_json FROM run_events
                 WHERE run_id = ? ORDER BY seq, id",
            )
            .map_err(|error| error.to_string())?;
        let mapped = statement
            .query_map(rusqlite::params![run_id], |row| {
                let data_json: String = row.get("data_json")?;
                Ok(SavedEvent {
                    id: row.get("id")?,
                    run_id: row.get("run_id")?,
                    seq: row.get("seq")?,
                    ts: row.get("ts")?,
                    event: row.get("event")?,
                    data: serde_json::from_str(&data_json)
                        .unwrap_or_else(|_| Value::Object(Default::default())),
                })
            })
            .map_err(|error| error.to_string())?;
        let mut events = Vec::new();
        for row in mapped {
            events.push(row.map_err(|error| error.to_string())?);
        }
        Ok(events)
    }
}

fn collect<T>(
    connection: &Connection,
    sql: &str,
    run_id: &str,
    map: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> DbResult<Vec<T>> {
    let mut statement = connection.prepare(sql).map_err(|error| error.to_string())?;
    let mapped = statement
        .query_map(rusqlite::params![run_id], map)
        .map_err(|error| error.to_string())?;
    let mut items = Vec::new();
    for item in mapped {
        items.push(item.map_err(|error| error.to_string())?);
    }
    Ok(items)
}

fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    let outputs_json: String = row.get("outputs_json")?;
    Ok(RunRow {
        id: row.get("id")?,
        created_at: row.get("created_at")?,
        task: row.get("task")?,
        status: row.get("status")?,
        final_draft: row.get("final_draft")?,
        verdict: row.get("verdict")?,
        outputs: serde_json::from_str(&outputs_json).unwrap_or_else(|_| Value::Array(Vec::new())),
        duration_seconds: row.get("duration_seconds")?,
        warning: row.get("warning")?,
        source: row.get("source")?,
    })
}

fn pending_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingActionRow> {
    let payload_json: String = row.get("payload_json")?;
    Ok(PendingActionRow {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        action_type: row.get("action_type")?,
        payload: serde_json::from_str(&payload_json)
            .unwrap_or_else(|_| Value::Object(Default::default())),
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        decided_at: row.get("decided_at")?,
    })
}

/// Add Phase 5/7 columns to databases created by earlier phases.
fn ensure_columns(connection: &Connection) -> rusqlite::Result<()> {
    let runs = table_columns(connection, "runs")?;
    if !runs.contains("warning") {
        connection.execute_batch("ALTER TABLE runs ADD COLUMN warning TEXT")?;
    }
    if !runs.contains("source") {
        connection.execute_batch("ALTER TABLE runs ADD COLUMN source TEXT NOT NULL DEFAULT ''")?;
    }

    let tool_calls = table_columns(connection, "tool_calls")?;
    if !tool_calls.contains("args") {
        connection
            .execute_batch("ALTER TABLE tool_calls ADD COLUMN args TEXT NOT NULL DEFAULT ''")?;
    }

    let agent_calls = table_columns(connection, "agent_calls")?;
    if !agent_calls.contains("model") {
        connection
            .execute_batch("ALTER TABLE agent_calls ADD COLUMN model TEXT NOT NULL DEFAULT ''")?;
    }
    if !agent_calls.contains("tool_turns") {
        connection.execute_batch(
            "ALTER TABLE agent_calls ADD COLUMN tool_turns INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(names)
}

fn expand_home(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRow {
    pub id: String,
    pub created_at: String,
    pub task: String,
    pub status: String,
    pub final_draft: String,
    pub verdict: Option<String>,
    pub outputs: Value,
    pub duration_seconds: f64,
    pub warning: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunDetail {
    pub run: RunRow,
    pub agent_calls: Vec<AgentCallRow>,
    pub tool_calls: Vec<ToolCallRow>,
    pub pending_actions: Vec<PendingActionRow>,
}

/// Values for one `agent_calls` insert, mirroring `save_agent_call` in the reference.
pub struct NewAgentCall<'a> {
    pub run_id: &'a str,
    pub step: i64,
    pub agent: &'a str,
    pub started_at: &'a str,
    pub duration_ms: i64,
    pub attempts: i64,
    pub tool_turns: i64,
    pub status: &'a str,
    pub error_type: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub input_chars: i64,
    pub output_chars: i64,
    pub raw_output: &'a str,
    pub model: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentCallRow {
    pub id: i64,
    pub run_id: String,
    pub step: i64,
    pub agent: String,
    pub started_at: String,
    pub duration_ms: i64,
    pub attempts: i64,
    pub tool_turns: i64,
    pub status: String,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub input_chars: i64,
    pub output_chars: i64,
    pub raw_output: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRow {
    pub id: i64,
    pub run_id: String,
    pub tool: String,
    pub started_at: String,
    pub duration_ms: i64,
    pub status: String,
    pub error_message: Option<String>,
    pub args: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingActionRow {
    pub id: i64,
    pub run_id: String,
    pub action_type: String,
    pub payload: Value,
    pub status: String,
    pub created_at: String,
    pub decided_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavedEvent {
    pub id: i64,
    pub run_id: String,
    pub seq: i64,
    pub ts: String,
    pub event: String,
    pub data: Value,
}

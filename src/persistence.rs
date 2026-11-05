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

use crate::reliability::{redact, redact_value, truncate};

/// Stored raw model output and tool args are capped, like `MAX_RAW_CHARS` in
/// `app/reliability.py`.
pub const MAX_RAW_CHARS: usize = 2000;

pub type DbResult<T> = Result<T, String>;

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

/// UTC timestamp with second precision and a `+00:00` suffix, as stored in rows.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// A v4 UUID as 32 lowercase hex characters, like `uuid4().hex`.
pub fn new_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[derive(Clone, Debug)]
pub struct Db {
    path: PathBuf,
}

/// One `agent_calls` audit row.
#[derive(Clone, Debug)]
pub struct NewAgentCall {
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

/// One `tool_calls` audit row.
#[derive(Clone, Debug)]
pub struct NewToolCall {
    pub run_id: String,
    pub tool: String,
    pub started_at: String,
    pub duration_ms: i64,
    pub status: String,
    pub error_message: Option<String>,
    pub args: String,
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
            Self::new(crate::config::root().join("data").join("sol-o.db"))
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

    /// Insert or replace a `running` row before execution starts.
    pub fn start_run(&self, run_id: &str, task: &str, source: &str) -> DbResult<()> {
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT OR REPLACE INTO runs
                 (id, created_at, task, status, final_draft, verdict, outputs_json,
                  duration_seconds, warning, source)
                 VALUES (?, ?, ?, 'running', '', NULL, '[]', 0.0, NULL, ?)",
                rusqlite::params![run_id, now_iso(), redact(task), redact(source)],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Insert or update the finished run; never touches `task`, `created_at` or `source`.
    #[allow(clippy::too_many_arguments)]
    pub fn save_run(
        &self,
        run_id: &str,
        task: &str,
        status: &str,
        final_draft: &str,
        verdict: Option<&str>,
        outputs: &Value,
        duration_seconds: f64,
        warning: Option<&str>,
    ) -> DbResult<()> {
        let outputs_json = serde_json::to_string(&redact_value(outputs))
            .map_err(|error| format!("serialize outputs: {error}"))?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO runs
                 (id, created_at, task, status, final_draft, verdict, outputs_json,
                  duration_seconds, warning)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                   status = excluded.status,
                   final_draft = excluded.final_draft,
                   verdict = excluded.verdict,
                   outputs_json = excluded.outputs_json,
                   duration_seconds = excluded.duration_seconds,
                   warning = excluded.warning",
                rusqlite::params![
                    run_id,
                    now_iso(),
                    redact(task),
                    status,
                    redact(final_draft),
                    verdict,
                    outputs_json,
                    round2(duration_seconds),
                    warning,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn set_run_status(&self, run_id: &str, status: &str) -> DbResult<()> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE runs SET status = ? WHERE id = ?",
                rusqlite::params![status, run_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Audit one agent step. Callers tolerate failures: auditing never fails a run.
    pub fn save_agent_call(&self, call: &NewAgentCall) -> DbResult<()> {
        let error_message = call
            .error_message
            .as_deref()
            .filter(|message| !message.is_empty())
            .map(redact);
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO agent_calls
                 (run_id, step, agent, started_at, duration_ms, attempts, tool_turns,
                  status, error_type, error_message, input_chars, output_chars,
                  raw_output, model)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                    truncate(&redact(&call.raw_output), MAX_RAW_CHARS),
                    redact(&call.model),
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Audit one tool execution. Callers tolerate failures.
    pub fn save_tool_call(&self, call: &NewToolCall) -> DbResult<()> {
        let error_message = call
            .error_message
            .as_deref()
            .filter(|message| !message.is_empty())
            .map(redact);
        let args = if call.args.is_empty() {
            String::new()
        } else {
            truncate(&redact(&call.args), MAX_RAW_CHARS)
        };
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO tool_calls
                 (run_id, tool, started_at, duration_ms, status, error_message, args)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    call.run_id,
                    call.tool,
                    call.started_at,
                    call.duration_ms,
                    call.status,
                    error_message,
                    args,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Store a human decision request; returns its rowid.
    pub fn create_pending_action(
        &self,
        run_id: &str,
        action_type: &str,
        payload: &Value,
    ) -> DbResult<i64> {
        let payload_json = serde_json::to_string(&redact_value(payload))
            .map_err(|error| format!("serialize payload: {error}"))?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO pending_actions
                 (run_id, action_type, payload_json, status, created_at, decided_at)
                 VALUES (?, ?, ?, 'pending', ?, NULL)",
                rusqlite::params![run_id, action_type, payload_json, now_iso()],
            )
            .map_err(|error| error.to_string())?;
        Ok(connection.last_insert_rowid())
    }

    /// Mark stale pending rows expired; returns how many changed.
    pub fn expire_pending(&self, ttl_hours: f64) -> DbResult<usize> {
        let cutoff = chrono::Utc::now()
            - chrono::Duration::milliseconds((ttl_hours * 3_600_000.0).round() as i64);
        let cutoff = cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE pending_actions SET status = 'expired', decided_at = ?
                 WHERE status = 'pending' AND created_at < ?",
                rusqlite::params![now_iso(), cutoff],
            )
            .map_err(|error| error.to_string())
    }

    /// Unexpired pending actions, oldest first, for `GET /pending`.
    pub fn list_pending(&self, ttl_hours: f64) -> DbResult<Vec<PendingActionRow>> {
        self.expire_pending(ttl_hours)?;
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT * FROM pending_actions WHERE status = 'pending' ORDER BY id")
            .map_err(|error| error.to_string())?;
        let mapped = statement
            .query_map([], pending_from_row)
            .map_err(|error| error.to_string())?;
        let mut rows = Vec::new();
        for row in mapped {
            rows.push(row.map_err(|error| error.to_string())?);
        }
        Ok(rows)
    }

    /// One pending action by id, whatever its status.
    pub fn get_pending(&self, action_id: i64) -> DbResult<Option<PendingActionRow>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT * FROM pending_actions WHERE id = ?",
                rusqlite::params![action_id],
                pending_from_row,
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.to_string()),
            })
    }

    /// Claim a pending action exactly once; the status check lives in the SQL.
    pub fn claim_pending(
        &self,
        action_id: i64,
        decision: &str,
        ttl_hours: f64,
    ) -> DbResult<ClaimOutcome> {
        self.expire_pending(ttl_hours)?;
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE pending_actions SET status = ?, decided_at = ?
                 WHERE id = ? AND status = 'pending'",
                rusqlite::params![decision, now_iso(), action_id],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Ok(match self.get_pending(action_id)? {
                None => ClaimOutcome::NotFound,
                Some(row) => ClaimOutcome::Already(row.status),
            });
        }
        match self.get_pending(action_id)? {
            Some(row) => Ok(ClaimOutcome::Ok(row)),
            None => Ok(ClaimOutcome::NotFound),
        }
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

    /// Finished runs from the same conversation that mention any term, newest first.
    pub fn recall_runs(
        &self,
        source: &str,
        exclude_run_id: &str,
        terms: &[String],
        limit: i64,
    ) -> DbResult<Vec<RecallRunRow>> {
        let cleaned: Vec<String> = terms
            .iter()
            .map(|term| {
                term.to_lowercase()
                    .replace(['%', '_'], "")
                    .trim()
                    .to_string()
            })
            .filter(|term| term.chars().count() >= 3)
            .collect();
        if source.is_empty() || cleaned.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.connect()?;
        let clause = vec!["(task LIKE ? OR final_draft LIKE ?)"; cleaned.len()].join(" OR ");
        let mut values = vec![
            rusqlite::types::Value::Text(source.to_string()),
            rusqlite::types::Value::Text(exclude_run_id.to_string()),
        ];
        for term in &cleaned {
            let pattern = rusqlite::types::Value::Text(format!("%{term}%"));
            values.push(pattern.clone());
            values.push(pattern);
        }
        values.push(rusqlite::types::Value::Integer(limit.max(1)));
        let mut statement = connection
            .prepare(&format!(
                "SELECT id, created_at, task, final_draft FROM runs
                 WHERE source = ? AND id != ? AND status IN ('complete','partial') AND ({clause})
                 ORDER BY created_at DESC, rowid DESC LIMIT ?"
            ))
            .map_err(|error| error.to_string())?;
        let mapped = statement
            .query_map(rusqlite::params_from_iter(values), |row| {
                Ok(RecallRunRow {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    task: row.get(2)?,
                    final_draft: row.get(3)?,
                })
            })
            .map_err(|error| error.to_string())?;
        mapped
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())
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

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
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

/// One recall hit: a finished run from the same conversation.
#[derive(Debug, Clone)]
pub struct RecallRunRow {
    pub id: String,
    pub created_at: String,
    pub task: String,
    pub final_draft: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunDetail {
    pub run: RunRow,
    pub agent_calls: Vec<AgentCallRow>,
    pub tool_calls: Vec<ToolCallRow>,
    pub pending_actions: Vec<PendingActionRow>,
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

/// Result of trying to decide a pending action.
#[derive(Debug)]
pub enum ClaimOutcome {
    Ok(PendingActionRow),
    NotFound,
    Already(String),
}

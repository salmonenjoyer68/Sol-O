"""SQLite persistence for orchestration runs (Phases 3-5).

Stores runs, per-agent calls, tool calls and pending human-approval actions in
``data/sol-o.db`` (``SOL_O_DB`` overrides). Logging never takes a run down: the
orchestrator catches storage failures and records them as structured events.
"""

from __future__ import annotations

import json
import logging
import os
import sqlite3
import uuid
from collections.abc import Iterator
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

logger = logging.getLogger("sol_o.logger")

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_DB_PATH = ROOT / "data" / "sol-o.db"


def db_path() -> Path:
    """Database file; ``SOL_O_DB`` overrides for tests and alternate deploys."""
    override = os.getenv("SOL_O_DB", "").strip()
    return Path(override).expanduser() if override else DEFAULT_DB_PATH


SCHEMA = """
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
"""


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def new_run_id() -> str:
    """A run id that is known before the run starts (used by POST /inbound)."""
    return uuid.uuid4().hex


def _ensure_columns(connection: sqlite3.Connection) -> None:
    """Add Phase 5/7 columns to databases created by earlier phases."""
    columns = {row[1] for row in connection.execute("PRAGMA table_info(runs)")}
    if "warning" not in columns:
        connection.execute("ALTER TABLE runs ADD COLUMN warning TEXT")
    if "source" not in columns:
        connection.execute("ALTER TABLE runs ADD COLUMN source TEXT NOT NULL DEFAULT ''")

    tool_columns = {row[1] for row in connection.execute("PRAGMA table_info(tool_calls)")}
    if "args" not in tool_columns:
        connection.execute("ALTER TABLE tool_calls ADD COLUMN args TEXT NOT NULL DEFAULT ''")

    agent_columns = {row[1] for row in connection.execute("PRAGMA table_info(agent_calls)")}
    if "model" not in agent_columns:
        connection.execute("ALTER TABLE agent_calls ADD COLUMN model TEXT NOT NULL DEFAULT ''")


@contextmanager
def _connect() -> Iterator[sqlite3.Connection]:
    path = db_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    connection = sqlite3.connect(path, timeout=10)
    connection.row_factory = sqlite3.Row
    try:
        connection.executescript(SCHEMA)
        _ensure_columns(connection)
        yield connection
        connection.commit()
    finally:
        connection.close()


def _redact_text(text: str) -> str:
    from app.reliability import redact

    return redact(text)


# --------------------------------------------------------------------------
# Runs
# --------------------------------------------------------------------------


def start_run(run_id: str, task: str, source: str = "") -> None:
    """Insert the pending row so a run is visible in GET /runs immediately."""
    with _connect() as connection:
        connection.execute(
            "INSERT OR REPLACE INTO runs"
            " (id, created_at, task, status, final_draft, verdict, outputs_json,"
            " duration_seconds, warning, source)"
            " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                run_id,
                now_iso(),
                _redact_text(task),
                "running",
                "",
                None,
                "[]",
                0.0,
                None,
                _redact_text(source or ""),
            ),
        )
    logger.info("run started id=%s", run_id)


def save_run(
    run_id: str,
    *,
    task: str,
    status: str,
    final_draft: str,
    verdict: str | None,
    outputs: list[dict[str, Any]],
    duration_seconds: float,
    warning: str | None = None,
) -> None:
    """Finalize the row; inserts it too if ``start_run`` never ran."""
    payload = _redact_text(json.dumps(outputs, ensure_ascii=False))
    with _connect() as connection:
        connection.execute(
            "INSERT INTO runs"
            " (id, created_at, task, status, final_draft, verdict, outputs_json,"
            " duration_seconds, warning)"
            " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
            " ON CONFLICT(id) DO UPDATE SET"
            " status=excluded.status, final_draft=excluded.final_draft,"
            " verdict=excluded.verdict, outputs_json=excluded.outputs_json,"
            " duration_seconds=excluded.duration_seconds, warning=excluded.warning",
            (
                run_id,
                now_iso(),
                _redact_text(task),
                status,
                _redact_text(final_draft),
                verdict,
                payload,
                round(duration_seconds, 2),
                warning,
            ),
        )
    logger.info("run saved id=%s status=%s", run_id, status)


def set_run_status(run_id: str, status: str) -> None:
    """Update a stored run's status after a human decision."""
    with _connect() as connection:
        connection.execute("UPDATE runs SET status = ? WHERE id = ?", (status, run_id))


def _row_to_dict(row: sqlite3.Row) -> dict[str, Any]:
    data = dict(row)
    try:
        data["outputs"] = json.loads(data.pop("outputs_json") or "[]")
    except json.JSONDecodeError:
        data["outputs"] = []
    return data


def recent_runs(limit: int = 20, status: str | None = None) -> list[dict[str, Any]]:
    """Newest runs first, optionally filtered by status, for ``GET /runs``."""
    with _connect() as connection:
        if status:
            rows = connection.execute(
                "SELECT * FROM runs WHERE status = ?"
                " ORDER BY created_at DESC, rowid DESC LIMIT ?",
                (status, limit),
            ).fetchall()
        else:
            rows = connection.execute(
                "SELECT * FROM runs ORDER BY created_at DESC, rowid DESC LIMIT ?", (limit,)
            ).fetchall()
    return [_row_to_dict(row) for row in rows]


def get_run(run_id: str) -> dict[str, Any] | None:
    """One run by id, or ``None``."""
    with _connect() as connection:
        row = connection.execute("SELECT * FROM runs WHERE id = ?", (run_id,)).fetchone()
    return _row_to_dict(row) if row is not None else None


def get_run_detail(run_id: str) -> dict[str, Any] | None:
    """Run plus its agent calls, tool calls and pending actions."""
    run = get_run(run_id)
    if run is None:
        return None
    with _connect() as connection:
        agent_calls = [
            dict(row)
            for row in connection.execute(
                "SELECT * FROM agent_calls WHERE run_id = ? ORDER BY id", (run_id,)
            )
        ]
        tool_calls = [
            dict(row)
            for row in connection.execute(
                "SELECT * FROM tool_calls WHERE run_id = ? ORDER BY id", (run_id,)
            )
        ]
        pending = [
            dict(row)
            for row in connection.execute(
                "SELECT * FROM pending_actions WHERE run_id = ? ORDER BY id", (run_id,)
            )
        ]
    for item in pending:
        try:
            item["payload"] = json.loads(item.pop("payload_json"))
        except json.JSONDecodeError:
            item["payload"] = {}
    return {
        "run": run,
        "agent_calls": agent_calls,
        "tool_calls": tool_calls,
        "pending_actions": pending,
    }


# --------------------------------------------------------------------------
# Memory recall (same-conversation runs only)
# --------------------------------------------------------------------------


def recall_runs(
    *, source: str, exclude_run_id: str, terms: list[str], limit: int = 5
) -> list[dict[str, Any]]:
    """Finished runs from the same conversation whose task or draft matches any term."""
    cleaned = []
    for term in terms:
        text = "".join(ch for ch in term.lower() if ch not in "%_").strip()
        if len(text) >= 3:
            cleaned.append(text)
    if not source or not cleaned:
        return []

    clause = " OR ".join("(task LIKE ? OR final_draft LIKE ?)" for _ in cleaned)
    params: list[Any] = [source, exclude_run_id]
    for text in cleaned:
        pattern = f"%{text}%"
        params.extend([pattern, pattern])
    params.append(max(1, int(limit)))

    with _connect() as connection:
        rows = connection.execute(
            "SELECT id, created_at, task, final_draft FROM runs"
            " WHERE source = ? AND id != ? AND status IN ('complete','partial')"
            f" AND ({clause})"
            " ORDER BY created_at DESC, rowid DESC LIMIT ?",
            params,
        ).fetchall()
    return [dict(row) for row in rows]


# --------------------------------------------------------------------------
# Run events (Phase 6 replay)
# --------------------------------------------------------------------------


def save_run_event(
    *, run_id: str, seq: int, ts: str, event: str, data: dict[str, Any]
) -> None:
    """Append one streamed event so ``GET /runs/{id}/events`` can replay it."""
    payload = _redact_text(json.dumps(data, ensure_ascii=False, default=str))
    with _connect() as connection:
        connection.execute(
            "INSERT INTO run_events (run_id, seq, ts, event, data_json)"
            " VALUES (?, ?, ?, ?, ?)",
            (run_id, int(seq), ts, event, payload),
        )


def get_run_events(run_id: str) -> list[dict[str, Any]]:
    """Saved events for one run, in original order."""
    with _connect() as connection:
        rows = connection.execute(
            "SELECT id, run_id, seq, ts, event, data_json FROM run_events"
            " WHERE run_id = ? ORDER BY seq, id",
            (run_id,),
        ).fetchall()
    events: list[dict[str, Any]] = []
    for row in rows:
        item = dict(row)
        try:
            item["data"] = json.loads(item.pop("data_json") or "{}")
        except json.JSONDecodeError:
            item["data"] = {}
        events.append(item)
    return events


# --------------------------------------------------------------------------
# Agent and tool calls
# --------------------------------------------------------------------------


def save_agent_call(
    *,
    run_id: str,
    step: int,
    agent: str,
    started_at: str,
    duration_ms: int,
    attempts: int,
    status: str,
    error_type: str | None = None,
    error_message: str | None = None,
    input_chars: int = 0,
    output_chars: int = 0,
    raw_output: str = "",
    model: str = "",
) -> None:
    """One row per LLM agent call, written on success and failure."""
    from app.reliability import truncate

    with _connect() as connection:
        connection.execute(
            "INSERT INTO agent_calls"
            " (run_id, step, agent, started_at, duration_ms, attempts, status,"
            " error_type, error_message, input_chars, output_chars, raw_output, model)"
            " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                run_id,
                step,
                agent,
                started_at,
                int(duration_ms),
                attempts,
                status,
                error_type,
                _redact_text(error_message) if error_message else None,
                input_chars,
                output_chars,
                truncate(_redact_text(raw_output)),
                _redact_text(model or ""),
            ),
        )


def save_tool_call(
    *,
    run_id: str,
    tool: str,
    started_at: str,
    duration_ms: int,
    status: str,
    error_message: str | None = None,
    args: str = "",
) -> None:
    """One row per external tool call (search, Telegram, planet tool)."""
    from app.reliability import truncate

    with _connect() as connection:
        connection.execute(
            "INSERT INTO tool_calls"
            " (run_id, tool, started_at, duration_ms, status, error_message, args)"
            " VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                run_id,
                tool,
                started_at,
                int(duration_ms),
                status,
                _redact_text(error_message) if error_message else None,
                truncate(_redact_text(args)) if args else "",
            ),
        )


# --------------------------------------------------------------------------
# Pending human approvals
# --------------------------------------------------------------------------


def expire_pending(ttl_hours: float) -> int:
    """Mark pending actions older than the TTL as expired; return how many."""
    cutoff = (datetime.now(timezone.utc) - timedelta(hours=ttl_hours)).isoformat(
        timespec="seconds"
    )
    with _connect() as connection:
        cursor = connection.execute(
            "UPDATE pending_actions SET status = 'expired', decided_at = ?"
            " WHERE status = 'pending' AND created_at < ?",
            (now_iso(), cutoff),
        )
        return cursor.rowcount


def create_pending_action(run_id: str, action_type: str, payload: dict[str, Any]) -> int:
    payload_json = _redact_text(json.dumps(payload, ensure_ascii=False))
    with _connect() as connection:
        cursor = connection.execute(
            "INSERT INTO pending_actions"
            " (run_id, action_type, payload_json, status, created_at, decided_at)"
            " VALUES (?, ?, ?, 'pending', ?, NULL)",
            (run_id, action_type, payload_json, now_iso()),
        )
        action_id = int(cursor.lastrowid)
    logger.info("pending action created id=%d run_id=%s", action_id, run_id)
    return action_id


def _pending_row(row: sqlite3.Row) -> dict[str, Any]:
    data = dict(row)
    try:
        data["payload"] = json.loads(data.pop("payload_json") or "{}")
    except json.JSONDecodeError:
        data["payload"] = {}
    return data


def list_pending(ttl_hours: float) -> list[dict[str, Any]]:
    """Pending actions, expired ones excluded."""
    expire_pending(ttl_hours)
    with _connect() as connection:
        rows = connection.execute(
            "SELECT * FROM pending_actions WHERE status = 'pending' ORDER BY id"
        ).fetchall()
    return [_pending_row(row) for row in rows]


def get_pending(action_id: int) -> dict[str, Any] | None:
    with _connect() as connection:
        row = connection.execute(
            "SELECT * FROM pending_actions WHERE id = ?", (action_id,)
        ).fetchone()
    return _pending_row(row) if row is not None else None


def claim_pending(action_id: int, decision: str, ttl_hours: float) -> tuple[str, dict[str, Any] | None]:
    """Atomically move a pending action to ``decision``.

    Returns ``("ok", row)`` on success or one of ``not_found``, ``expired``,
    ``already_<status>``. An action can only be decided once.
    """
    expire_pending(ttl_hours)
    with _connect() as connection:
        cursor = connection.execute(
            "UPDATE pending_actions SET status = ?, decided_at = ?"
            " WHERE id = ? AND status = 'pending'",
            (decision, now_iso(), action_id),
        )
        if cursor.rowcount != 1:
            row = connection.execute(
                "SELECT * FROM pending_actions WHERE id = ?", (action_id,)
            ).fetchone()
            if row is None:
                return "not_found", None
            return f"already_{row['status']}", _pending_row(row)
        row = connection.execute(
            "SELECT * FROM pending_actions WHERE id = ?", (action_id,)
        ).fetchone()
    return "ok", _pending_row(row)

"""Human approval gate for irreversible actions (Phase 5).

The only irreversible action today is sending the Telegram notification. When
approval is required, the orchestrator stores the exact message payload and
stops; approving re-sends that stored payload without re-running any agent.
"""

from __future__ import annotations

import json
import logging
import time
from typing import Any

from app import events
from app import logger as store
from app.config import settings
from app.reliability import redact
from tools.telegram import TelegramError, send_message

logger = logging.getLogger("sol_o.approvals")

ACTION_TELEGRAM = "telegram"


class ApprovalError(RuntimeError):
    """Base class for approval failures; subclasses map to API status codes."""

    code = "approval_error"
    status_code = 400


class ApprovalNotFound(ApprovalError):
    code = "not_found"
    status_code = 404


class ApprovalConflict(ApprovalError):
    code = "conflict"
    status_code = 409


class ApprovalExecutionError(ApprovalError):
    code = "execution_failed"
    status_code = 502


def approval_required() -> bool:
    """AUTO_APPROVE wins over REQUIRE_APPROVAL."""
    return settings.require_approval and not settings.auto_approve


def create_pending(run_id: str, payload: dict[str, Any]) -> int:
    return store.create_pending_action(run_id, ACTION_TELEGRAM, payload)


def list_pending() -> list[dict[str, Any]]:
    return store.list_pending(settings.pending_ttl_hours)


def _emit_decision(
    action_id: int, run_id: str, decision: str, run_status: str
) -> None:
    """Tell live streams which way a pending action went (never fatal)."""
    try:
        events.publish_run(
            run_id,
            "approval_decided",
            {
                "pending_id": action_id,
                "decision": decision,
                "run_status": run_status,
            },
        )
    except Exception:  # telemetry must never block the decision
        logger.exception("could not publish approval_decided action=%s", action_id)


def _claim(action_id: int, decision: str) -> dict[str, Any]:
    outcome, row = store.claim_pending(action_id, decision, settings.pending_ttl_hours)
    if outcome == "not_found":
        raise ApprovalNotFound(f"pending action {action_id} does not exist")
    if outcome == "expired":
        raise ApprovalConflict(f"pending action {action_id} expired and cannot be approved")
    if outcome != "ok":
        status = outcome.removeprefix("already_")
        raise ApprovalConflict(f"pending action {action_id} was already {status}")
    if row is None:  # pragma: no cover - defensive
        raise ApprovalNotFound(f"pending action {action_id} disappeared")
    return row


def approve(action_id: int) -> dict[str, Any]:
    """Execute the stored payload once and update the run status."""
    row = _claim(action_id, "approved")
    payload = row.get("payload") or {}
    message = str(payload.get("message") or "")
    run_id = str(row["run_id"])
    run_status = str(payload.get("run_status") or "complete")

    started_at = store.now_iso()
    started = time.perf_counter()
    try:
        delivery = send_message(message)
    except TelegramError as exc:
        store.save_tool_call(
            run_id=run_id,
            tool=ACTION_TELEGRAM,
            started_at=started_at,
            duration_ms=int((time.perf_counter() - started) * 1000),
            status="failed",
            error_message=str(exc),
        )
        store.set_run_status(run_id, "failed")
        _emit_decision(action_id, run_id, "approved", "failed")
        raise ApprovalExecutionError(str(exc)) from exc
    except Exception as exc:  # report, never swallow
        logger.exception("unexpected failure while executing approved action")
        store.save_tool_call(
            run_id=run_id,
            tool=ACTION_TELEGRAM,
            started_at=started_at,
            duration_ms=int((time.perf_counter() - started) * 1000),
            status="failed",
            error_message=str(exc),
        )
        store.set_run_status(run_id, "failed")
        _emit_decision(action_id, run_id, "approved", "failed")
        raise ApprovalExecutionError(redact(str(exc))) from exc

    store.save_tool_call(
        run_id=run_id,
        tool=ACTION_TELEGRAM,
        started_at=started_at,
        duration_ms=int((time.perf_counter() - started) * 1000),
        status="ok",
    )
    store.set_run_status(run_id, run_status)
    _emit_decision(action_id, run_id, "approved", run_status)
    logger.info("pending action approved id=%s run_id=%s", action_id, run_id)
    return {
        "id": action_id,
        "status": "approved",
        "run_id": run_id,
        "run_status": run_status,
        "action_type": row["action_type"],
        "delivery": delivery,
    }


def reject(action_id: int) -> dict[str, Any]:
    """Reject a pending action; nothing is executed."""
    row = _claim(action_id, "rejected")
    run_id = str(row["run_id"])
    store.set_run_status(run_id, "rejected")
    _emit_decision(action_id, run_id, "rejected", "rejected")
    logger.info("pending action rejected id=%s run_id=%s", action_id, run_id)
    return {
        "id": action_id,
        "status": "rejected",
        "run_id": run_id,
        "action_type": row["action_type"],
    }


def payload_for_display(row: dict[str, Any]) -> dict[str, Any]:
    """Redacted payload safe to print or return to an operator."""
    payload = row.get("payload") or {}
    return {
        "task": redact(str(payload.get("task") or "")),
        "draft": redact(str(payload.get("draft") or "")),
        "verdict": payload.get("verdict"),
        "issues": payload.get("issues") or [],
        "message": redact(str(payload.get("message") or "")),
        "run_status": payload.get("run_status"),
    }


def load_pending(action_id: int) -> dict[str, Any] | None:
    return store.get_pending(action_id)


def dumps(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2, default=str)

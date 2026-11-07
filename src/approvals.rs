//! Human approval gate in front of the notifier, mirroring `app/approvals.py`.
//!
//! A successful run stores a pending action and sends nothing; deciding it once
//! executes the stored message (approve) or nothing (reject) and republishes the
//! decision on the original run's stream.

use std::time::Instant;

use serde_json::{json, Value};

use crate::config::Settings;
use crate::persistence::{self, ClaimOutcome, Db, NewToolCall, PendingActionRow};
use crate::reliability::redact;
use crate::telegram::{self, TelegramError};

pub const ACTION_TELEGRAM: &str = "telegram";

/// `AUTO_APPROVE` wins over `REQUIRE_APPROVAL`.
pub fn approval_required(settings: &Settings) -> bool {
    settings.require_approval && !settings.auto_approve
}

pub fn create_pending(db: &Db, run_id: &str, payload: &Value) -> persistence::DbResult<i64> {
    db.create_pending_action(run_id, ACTION_TELEGRAM, payload)
}

/// Exact error shape returned by the decision routes.
#[derive(Debug, Clone)]
pub struct ApprovalError {
    pub status: u16,
    pub code: &'static str,
    pub detail: String,
}

impl ApprovalError {
    fn not_found(action_id: i64) -> Self {
        Self {
            status: 404,
            code: "not_found",
            detail: format!("pending action {action_id} does not exist"),
        }
    }

    fn conflict(action_id: i64, status: &str) -> Self {
        Self {
            status: 409,
            code: "conflict",
            detail: format!("pending action {action_id} was already {status}"),
        }
    }

    fn execution_failed(detail: String) -> Self {
        Self {
            status: 502,
            code: "execution_failed",
            detail: redact(&detail),
        }
    }
}

/// A decision: what the route returns, plus the event to publish on the run.
pub struct Decision {
    pub run_id: Option<String>,
    pub event: Option<Value>,
    pub response: Result<Value, ApprovalError>,
}

fn claim(
    db: &Db,
    action_id: i64,
    decision: &str,
    ttl_hours: f64,
) -> persistence::DbResult<Result<PendingActionRow, ApprovalError>> {
    Ok(match db.claim_pending(action_id, decision, ttl_hours)? {
        ClaimOutcome::Ok(row) => Ok(row),
        ClaimOutcome::NotFound => Err(ApprovalError::not_found(action_id)),
        ClaimOutcome::Already(status) => Err(ApprovalError::conflict(action_id, &status)),
    })
}

fn decided(action_id: i64, decision: &str, run_status: &str) -> Value {
    json!({
        "pending_id": action_id,
        "decision": decision,
        "run_status": run_status,
    })
}

fn claim_error(error: ApprovalError) -> Decision {
    Decision {
        run_id: None,
        event: None,
        response: Err(error),
    }
}

/// Execute the stored payload once and restore the stored run status.
pub fn approve(db: &Db, settings: &Settings, action_id: i64) -> persistence::DbResult<Decision> {
    let row = match claim(db, action_id, "approved", settings.pending_ttl_hours)? {
        Ok(row) => row,
        Err(error) => return Ok(claim_error(error)),
    };
    let payload = &row.payload;
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let run_id = row.run_id.clone();
    let run_status = payload
        .get("run_status")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("complete")
        .to_string();

    let started_at = persistence::now_iso();
    let started = Instant::now();
    let delivery = telegram::send_message(settings, message);
    let duration_ms = started.elapsed().as_millis() as i64;

    let delivery = match delivery {
        Ok(delivery) => delivery,
        Err(error) => {
            audit(
                db,
                &run_id,
                &started_at,
                duration_ms,
                "failed",
                Some(error.message()),
            );
            db.set_run_status(&run_id, "failed")?;
            return Ok(Decision {
                run_id: Some(run_id.clone()),
                event: Some(decided(action_id, "approved", "failed")),
                response: Err(ApprovalError::execution_failed(error_text(&error))),
            });
        }
    };

    audit(db, &run_id, &started_at, duration_ms, "ok", None);
    db.set_run_status(&run_id, &run_status)?;
    tracing::info!(action_id, run_id, "pending action approved");
    Ok(Decision {
        run_id: Some(run_id.clone()),
        event: Some(decided(action_id, "approved", &run_status)),
        response: Ok(json!({
            "id": action_id,
            "status": "approved",
            "run_id": run_id,
            "run_status": run_status,
            "action_type": row.action_type,
            "delivery": delivery.to_value(),
        })),
    })
}

/// Reject a pending action; nothing is executed.
pub fn reject(db: &Db, settings: &Settings, action_id: i64) -> persistence::DbResult<Decision> {
    let row = match claim(db, action_id, "rejected", settings.pending_ttl_hours)? {
        Ok(row) => row,
        Err(error) => return Ok(claim_error(error)),
    };
    let run_id = row.run_id.clone();
    db.set_run_status(&run_id, "rejected")?;
    tracing::info!(action_id, run_id, "pending action rejected");
    Ok(Decision {
        run_id: Some(run_id.clone()),
        event: Some(decided(action_id, "rejected", "rejected")),
        response: Ok(json!({
            "id": action_id,
            "status": "rejected",
            "run_id": run_id,
            "action_type": row.action_type,
        })),
    })
}

fn error_text(error: &TelegramError) -> String {
    error.message().to_string()
}

fn audit(
    db: &Db,
    run_id: &str,
    started_at: &str,
    duration_ms: i64,
    status: &str,
    error_message: Option<&str>,
) {
    let call = NewToolCall {
        run_id: run_id.to_string(),
        tool: ACTION_TELEGRAM.to_string(),
        started_at: started_at.to_string(),
        duration_ms,
        status: status.to_string(),
        error_message: error_message.map(str::to_string),
        args: String::new(),
    };
    if let Err(error) = db.save_tool_call(&call) {
        tracing::error!(run_id, %error, "could not write telegram tool_calls row");
    }
}

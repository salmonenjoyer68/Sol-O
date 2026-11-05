//! HTTP surface: the single-file UI, run history and the live SSE stream.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

use crate::agents::{self, RunError};
use crate::approvals::{self, ApprovalError};
use crate::config::{Settings, ROOT};
use crate::events::{sse_stream, EventBus};
use crate::orchestrator::Orchestrator;
use crate::persistence::{self, Db, PendingActionRow, RunDetail, RunRow, SavedEvent};
use crate::reliability::redact;
use crate::schemas::{AgentOutput, InboundRequest, OrchestrationRequest, RunRequest};

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub orchestrator: Arc<Orchestrator>,
    pub web_index: PathBuf,
    pub keepalive: Duration,
}

impl AppState {
    pub fn new(settings: Settings, db: Db) -> Self {
        let bus = Arc::new(EventBus::new(db.clone()));
        let settings = Arc::new(settings);
        let orchestrator = Arc::new(Orchestrator::new(settings.clone(), db.clone(), bus.clone()));
        Self {
            settings,
            db,
            bus,
            orchestrator,
            web_index: PathBuf::from(ROOT).join("web").join("index.html"),
            keepalive: Duration::from_secs(15),
        }
    }

    pub fn with_keepalive(mut self, keepalive: Duration) -> Self {
        self.keepalive = keepalive;
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/run", post(run))
        .route("/orchestrate", post(orchestrate))
        .route("/inbound", post(inbound))
        .route("/runs", get(runs))
        .route("/runs/{run_id}", get(run_detail))
        .route("/runs/{run_id}/events", get(run_events))
        .route("/stream", get(stream))
        .route("/pending", get(pending_actions))
        .route("/pending/{action_id}/approve", post(approve_pending))
        .route("/pending/{action_id}/reject", post(reject_pending))
        .layer(cors())
        .with_state(state)
}

/// The browser UI is same-origin; CORS only admits local tooling.
pub fn is_localhost_origin(origin: &str) -> bool {
    static LOCALHOST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^https?://(localhost|127\.0\.0\.1)(:\d+)?$")
            .expect("static localhost origin pattern is valid")
    });
    LOCALHOST.is_match(origin)
}

fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            |origin: &HeaderValue, _parts: &_| {
                origin.to_str().map(is_localhost_origin).unwrap_or(false)
            },
        ))
        .allow_credentials(true)
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
}

async fn index(State(state): State<AppState>) -> Result<Response, ApiError> {
    let body = tokio::fs::read(&state.web_index).await.map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "ui_unreadable", "detail": error.to_string()}),
        )
    })?;
    Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response())
}

/// Run one file-backed agent and return its validated JSON.
async fn run(
    State(state): State<AppState>,
    payload: Result<Json<RunRequest>, JsonRejection>,
) -> Result<Json<AgentOutput>, ApiError> {
    let Json(request) = payload.map_err(unprocessable)?;
    require_non_empty("agent", &request.agent)?;
    require_non_empty("task", &request.task)?;
    let settings = state.settings.clone();
    let agents_dir = crate::config::agents_dir();
    match tokio::task::spawn_blocking(move || execute_run(&settings, &agents_dir, request)).await {
        Ok(result) => result.map(Json),
        Err(error) => Err(database_error(&format!("run task failed: {error}"))),
    }
}

fn require_non_empty(field: &str, value: &str) -> Result<(), ApiError> {
    if !value.is_empty() {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"detail": [{
            "type": "string_too_short",
            "loc": ["body", field],
            "msg": "String should have at least 1 character"
        }]}),
    ))
}

fn unprocessable(rejection: JsonRejection) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"detail": [{"type": "json_invalid", "loc": ["body"], "msg": rejection.body_text()}]}),
    )
}

fn execute_run(
    settings: &Settings,
    agents_dir: &Path,
    request: RunRequest,
) -> Result<AgentOutput, ApiError> {
    match agents::run_agent(settings, agents_dir, &request.agent, &request.task) {
        Ok(outcome) => Ok(outcome.output),
        Err(RunError::NotFound(agent)) => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            json!({"error": "agent_not_found", "agent": agent}),
        )),
        Err(RunError::Unreadable(detail)) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "agent_unreadable", "detail": detail}),
        )),
        Err(RunError::Run(error)) => Err(match error.failure_type {
            agents::FailureType::Timeout => ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                json!({"error": "llm_timeout", "detail": error.detail}),
            ),
            agents::FailureType::LlmError => ApiError::new(
                StatusCode::BAD_GATEWAY,
                json!({"error": "llm_error", "detail": error.detail}),
            ),
            _ => ApiError::new(
                StatusCode::BAD_GATEWAY,
                json!({"error": "invalid_agent_output", "raw": error.raw}),
            ),
        }),
    }
}

/// Queue a full run and return its id so the browser can watch it live.
async fn orchestrate(
    State(state): State<AppState>,
    payload: Result<Json<OrchestrationRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) = payload.map_err(unprocessable)?;
    require_non_empty("task", &request.task)?;
    let run_id = persistence::new_run_id();
    insert_run(&state.db, &run_id, &request.task, &request.source).await;
    spawn_run(
        state.clone(),
        request.task,
        request.use_llm_planner,
        run_id.clone(),
        request.source,
    );
    Ok(Json(json!({"run_id": run_id, "status": "accepted"})))
}

/// Accept an inbound message, queue the orchestration, return the run id.
async fn inbound(
    State(state): State<AppState>,
    payload: Result<Json<InboundRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Json(request) = payload.map_err(unprocessable)?;
    require_non_empty("message", &request.message)?;
    let run_id = persistence::new_run_id();
    insert_run(&state.db, &run_id, &request.message, &request.source).await;
    spawn_run(
        state.clone(),
        request.message,
        false,
        run_id.clone(),
        request.source,
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"run_id": run_id, "status": "accepted"})),
    ))
}

/// Insert the `running` row before the request returns; a failure still accepts.
async fn insert_run(db: &Db, run_id: &str, task: &str, source: &str) {
    let db = db.clone();
    let label = run_id.to_string();
    let (id, task, source) = (label.clone(), task.to_string(), source.to_string());
    let outcome = tokio::task::spawn_blocking(move || db.start_run(&id, &task, &source)).await;
    match outcome {
        Ok(Err(error)) => {
            tracing::error!(run_id = label, %error, "could not create run row; run still accepted");
        }
        Err(error) => tracing::error!(run_id = label, %error, "run insert task failed"),
        Ok(Ok(())) => {}
    }
}

/// Run the orchestration off the request path, marking the run failed on escape.
fn spawn_run(state: AppState, task: String, use_llm_planner: bool, run_id: String, source: String) {
    tokio::spawn(async move {
        let orchestrator = state.orchestrator.clone();
        let db = state.db.clone();
        let bus = state.bus.clone();
        let job = {
            let run_id = run_id.clone();
            tokio::task::spawn_blocking(move || {
                orchestrator.run(&task, use_llm_planner, &run_id, &source)
            })
        };
        let outcome = match job.await {
            Ok(Ok(_)) => None,
            Ok(Err(message)) => Some(message),
            Err(error) => Some(error.to_string()),
        };
        if let Some(message) = outcome {
            fail_run(&db, &bus, &run_id, message).await;
        }
    });
}

async fn fail_run(db: &Db, bus: &EventBus, run_id: &str, message: String) {
    let db = db.clone();
    let id = run_id.to_string();
    let _ = tokio::task::spawn_blocking(move || db.set_run_status(&id, "failed")).await;
    let _ = bus
        .publish(
            run_id,
            "run_failed",
            json!({
                "failed_at": "sun",
                "error_type": "llm_error",
                "message": redact(&message),
            }),
        )
        .await;
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<String>,
    status: Option<String>,
}

async fn runs(
    State(state): State<AppState>,
    Query(query): Query<RunsQuery>,
) -> Result<Json<Vec<RunRow>>, ApiError> {
    let limit = parse_limit(query.limit.as_deref())?;
    let status = query.status.filter(|status| !status.is_empty());
    let db = state.db.clone();
    let rows = blocking(move || db.recent_runs(limit, status.as_deref())).await?;
    Ok(Json(rows))
}

async fn run_detail(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<RunDetail>, ApiError> {
    let db = state.db.clone();
    let lookup = run_id.clone();
    let detail = blocking(move || db.get_run_detail(&lookup)).await?;
    detail.map(Json).ok_or_else(|| run_not_found(&run_id))
}

async fn run_events(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<Vec<SavedEvent>>, ApiError> {
    let db = state.db.clone();
    let lookup = run_id.clone();
    let exists = blocking(move || db.get_run(&lookup)).await?;
    if exists.is_none() {
        return Err(run_not_found(&run_id));
    }
    let db = state.db.clone();
    let events = blocking(move || db.get_run_events(&run_id)).await?;
    Ok(Json(events))
}

async fn stream(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let (backlog, receiver) = state.bus.subscribe(last_event_id).await;
    let body = Body::from_stream(sse_stream(backlog, receiver, state.keepalive));
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

async fn pending_actions(
    State(state): State<AppState>,
) -> Result<Json<Vec<PendingActionRow>>, ApiError> {
    let db = state.db.clone();
    let ttl = state.settings.pending_ttl_hours;
    let rows = blocking(move || db.list_pending(ttl)).await?;
    Ok(Json(rows))
}

async fn approve_pending(
    State(state): State<AppState>,
    AxumPath(action_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    decide(state, action_id, true).await
}

async fn reject_pending(
    State(state): State<AppState>,
    AxumPath(action_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    decide(state, action_id, false).await
}

/// Claim the action, execute on approve, then publish the decision on the run.
async fn decide(state: AppState, raw_id: String, approve: bool) -> Result<Json<Value>, ApiError> {
    let action_id = raw_id.parse::<i64>().map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"detail": [{
                "type": "int_parsing",
                "loc": ["path", "action_id"],
                "msg": "Input should be a valid integer, unable to parse string as an integer",
                "input": raw_id,
            }]}),
        )
    })?;
    let db = state.db.clone();
    let settings = state.settings.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        if approve {
            approvals::approve(&db, &settings, action_id)
        } else {
            approvals::reject(&db, &settings, action_id)
        }
    })
    .await
    .map_err(|error| database_error(&format!("approval task failed: {error}")))?
    .map_err(|error| database_error(&error))?;

    if let (Some(run_id), Some(event)) = (outcome.run_id, outcome.event) {
        let _ = state.bus.publish(&run_id, "approval_decided", event).await;
    }
    outcome.response.map(Json).map_err(approval_error)
}

fn approval_error(error: ApprovalError) -> ApiError {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    ApiError::new(
        status,
        json!({"error": error.code, "detail": redact(&error.detail)}),
    )
}

fn parse_limit(raw: Option<&str>) -> Result<i64, ApiError> {
    match raw {
        None => Ok(DEFAULT_LIMIT),
        Some(text) => text
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    json!({"error": "invalid_request", "detail": "limit must be between 1 and 100"}),
                )
            }),
    }
}

fn run_not_found(run_id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        json!({"error": "run_not_found", "run_id": run_id}),
    )
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(job).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(database_error(&error)),
        Err(error) => Err(database_error(&format!("database task failed: {error}"))),
    }
}

fn database_error(detail: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": "database_error", "detail": redact(detail)}),
    )
}

/// Exact JSON error body, mirroring the reference's `ApiError`.
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{execute_run, is_localhost_origin};
    use crate::config::Settings;
    use crate::schemas::RunRequest;
    use axum::http::StatusCode;

    #[test]
    fn cors_only_admits_localhost_origins() {
        for origin in [
            "http://localhost",
            "http://localhost:3000",
            "https://localhost:8443",
            "http://127.0.0.1",
            "https://127.0.0.1:8001",
        ] {
            assert!(is_localhost_origin(origin), "{origin} should be allowed");
        }
        for origin in [
            "https://example.com",
            "http://localhost.evil.com",
            "https://127.0.0.1.example.com:80",
            "http://[::1]:3000",
            "ftp://localhost",
            "http://localhost:notaport",
        ] {
            assert!(!is_localhost_origin(origin), "{origin} should be refused");
        }
    }

    #[test]
    fn a_prompt_that_cannot_be_read_maps_to_agent_unreadable() {
        let dir = tempfile::TempDir::new().unwrap();
        // A directory where `broken.md` is expected: it exists but cannot be read.
        std::fs::create_dir(dir.path().join("broken.md")).unwrap();
        let request = RunRequest {
            agent: "broken".to_string(),
            task: "do it".to_string(),
        };
        let error = execute_run(&Settings::default(), dir.path(), request).unwrap_err();
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.body["error"], "agent_unreadable");
        assert!(
            error.body["detail"].as_str().unwrap().contains("broken.md"),
            "detail {}",
            error.body["detail"]
        );
    }
}

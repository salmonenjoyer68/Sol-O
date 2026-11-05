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
use crate::config::{Settings, ROOT};
use crate::events::{sse_stream, EventBus};
use crate::persistence::{Db, RunDetail, RunRow, SavedEvent};
use crate::reliability::redact;
use crate::schemas::{AgentOutput, RunRequest};

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub web_index: PathBuf,
    pub keepalive: Duration,
}

impl AppState {
    pub fn new(settings: Settings, db: Db) -> Self {
        let bus = Arc::new(EventBus::new(db.clone()));
        Self {
            settings: Arc::new(settings),
            db,
            bus,
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
        .route("/runs", get(runs))
        .route("/runs/{run_id}", get(run_detail))
        .route("/runs/{run_id}/events", get(run_events))
        .route("/stream", get(stream))
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
    for (field, value) in [("agent", &request.agent), ("task", &request.task)] {
        if value.is_empty() {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"detail": [{
                    "type": "string_too_short",
                    "loc": ["body", field],
                    "msg": "String should have at least 1 character"
                }]}),
            ));
        }
    }
    let settings = state.settings.clone();
    let agents_dir = PathBuf::from(ROOT).join("agents");
    match tokio::task::spawn_blocking(move || execute_run(&settings, &agents_dir, request)).await {
        Ok(result) => result.map(Json),
        Err(error) => Err(database_error(&format!("run task failed: {error}"))),
    }
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

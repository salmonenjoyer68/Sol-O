//! Unit-level HTTP/SSE tests: backlog replay, live delivery, keepalives, persistence.
//!
//! Builds the router in-process rather than spawning the binary: keepalive timing
//! is not a binary setting, so the test can shorten it here.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use sol_o::config::Settings;
use sol_o::events::EventBus;
use sol_o::http::{router, AppState};
use sol_o::persistence::Db;

struct TestApp {
    base: String,
    bus: Arc<EventBus>,
    db: Db,
}

impl TestApp {
    async fn start(keepalive: Duration) -> TestApp {
        let dir = TempDir::new().unwrap();
        let db = Db::new(dir.path().join("stream.db"));
        let state = AppState::new(Settings::default(), db.clone()).with_keepalive(keepalive);
        let bus = state.bus.clone();
        let app = router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // The database path must outlive the test, so the temp directory is leaked.
        std::mem::forget(dir);
        TestApp {
            base: format!("http://{address}"),
            bus,
            db,
        }
    }
}

/// Read the next SSE block that carries `id:` and `data:` lines, skipping keepalives.
async fn next_event(response: &mut reqwest::Response, buffer: &mut String) -> (u64, Value) {
    loop {
        let block = next_block(response, buffer).await;
        let id = block
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .and_then(|value| value.parse::<u64>().ok());
        let data = block
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .map(|value| serde_json::from_str::<Value>(value).expect("event JSON"));
        if let (Some(id), Some(data)) = (id, data) {
            return (id, data);
        }
    }
}

async fn next_block(response: &mut reqwest::Response, buffer: &mut String) -> String {
    loop {
        if let Some(index) = buffer.find("\n\n") {
            let block = buffer[..index].to_string();
            buffer.drain(..index + 2);
            return block;
        }
        let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .expect("timed out waiting for SSE data")
            .expect("SSE stream read")
            .expect("SSE stream ended");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

#[tokio::test]
async fn stream_replays_missed_events_then_delivers_live_ones_to_every_tab() {
    let app = TestApp::start(Duration::from_secs(30)).await;
    let client = reqwest::Client::new();

    let first = app
        .bus
        .publish("run-a", "run_started", json!({"task": "x"}))
        .await;
    let second = app
        .bus
        .publish(
            "run-a",
            "agent_called",
            json!({"step": 1, "agent": "researcher"}),
        )
        .await;

    let mut resumed = client
        .get(format!("{}/stream", app.base))
        .header("Last-Event-ID", first.id.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), 200);
    assert_eq!(
        resumed.headers()["content-type"],
        "text/event-stream",
        "stream content type"
    );
    assert_eq!(resumed.headers()["cache-control"], "no-cache");

    let mut buffer = String::new();
    let (id, data) = next_event(&mut resumed, &mut buffer).await;
    assert_eq!(id, second.id, "backlog after Last-Event-ID only");
    assert_eq!(data["event"], "agent_called");
    assert_eq!(data["run_id"], "run-a");
    assert_eq!(data["seq"], 2);

    let mut fresh = client
        .get(format!("{}/stream", app.base))
        .send()
        .await
        .unwrap();
    let mut fresh_buffer = String::new();
    assert_eq!(next_event(&mut fresh, &mut fresh_buffer).await.0, first.id);
    assert_eq!(next_event(&mut fresh, &mut fresh_buffer).await.0, second.id);

    let third = app
        .bus
        .publish(
            "run-a",
            "agent_done",
            json!({"step": 1, "status": "complete"}),
        )
        .await;
    let (resumed_id, resumed_data) = next_event(&mut resumed, &mut buffer).await;
    let (fresh_id, _) = next_event(&mut fresh, &mut fresh_buffer).await;
    assert_eq!(resumed_id, third.id);
    assert_eq!(fresh_id, third.id, "each tab has its own receiver");
    assert_eq!(resumed_data["seq"], 3);
    assert_eq!(resumed_data["data"]["status"], "complete");

    let saved = tokio::task::spawn_blocking({
        let db = app.db.clone();
        move || db.get_run_events("run-a").unwrap()
    })
    .await
    .unwrap();
    let sequences: Vec<i64> = saved.iter().map(|event| event.seq).collect();
    assert_eq!(
        sequences,
        vec![1, 2, 3],
        "every published event is persisted"
    );
    assert_eq!(saved[0].event, "run_started");
    assert_eq!(saved[2].data["status"], "complete");
}

#[tokio::test]
async fn quiet_streams_receive_keepalives() {
    let app = TestApp::start(Duration::from_millis(100)).await;
    let client = reqwest::Client::new();

    let mut response = client
        .get(format!("{}/stream", app.base))
        .send()
        .await
        .unwrap();
    let mut buffer = String::new();
    let block = next_block(&mut response, &mut buffer).await;
    assert!(
        block.trim_start().starts_with(':'),
        "keepalive comment expected, got {block:?}"
    );

    let event = app
        .bus
        .publish("run-b", "run_started", json!({"task": "quiet"}))
        .await;
    let (id, data) = next_event(&mut response, &mut buffer).await;
    assert_eq!(id, event.id);
    assert_eq!(data["event"], "run_started");
}

#[tokio::test]
async fn published_events_are_redacted_before_storage() {
    let app = TestApp::start(Duration::from_secs(30)).await;

    let record = app
        .bus
        .publish(
            "run-redacted",
            "agent_done",
            json!({"summary": "token sk-abcdefghijklmn"}),
        )
        .await;
    assert_eq!(record.data["summary"], "token ***");

    let saved = tokio::task::spawn_blocking({
        let db = app.db.clone();
        move || db.get_run_events("run-redacted").unwrap()
    })
    .await
    .unwrap();

    assert_eq!(saved[0].data["summary"], "token ***");
    assert!(!serde_json::to_string(&saved[0].data)
        .unwrap()
        .contains("sk-abcdefghijklmn"));
}

#[test]
fn default_keepalive_is_fifteen_seconds() {
    let dir = TempDir::new().unwrap();
    let state = AppState::new(
        Settings::default(),
        Db::new(dir.path().join("keepalive.db")),
    );
    assert_eq!(state.keepalive, Duration::from_secs(15));
}

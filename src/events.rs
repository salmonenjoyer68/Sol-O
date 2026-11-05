//! In-memory event bus and SSE fan-out.
//!
//! Events are stamped with a per-run sequence number and a process-wide id,
//! redacted before publication, persisted to `run_events`, buffered for
//! reconnecting streams and fanned out to every connected tab.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::time::Duration;

use async_stream::stream;
use futures_core::Stream;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, Mutex};

use crate::persistence::Db;
use crate::reliability::redact_value;

/// How many recent events a reconnecting stream may replay.
pub const HISTORY_SIZE: usize = 2000;

const CHANNEL_CAPACITY: usize = 1024;

#[derive(Clone, Debug, Serialize)]
pub struct EventRecord {
    /// Process-wide event id, used as the SSE event id.
    pub id: u64,
    pub run_id: String,
    /// Per-run increasing sequence number.
    pub seq: i64,
    pub ts: String,
    pub event: String,
    pub data: Value,
}

/// UTC timestamp with millisecond precision and a `+00:00` suffix.
pub fn now_iso_millis() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

struct BusState {
    seq: HashMap<String, i64>,
    history: VecDeque<EventRecord>,
    next_event: u64,
    sender: broadcast::Sender<EventRecord>,
}

pub struct EventBus {
    state: Mutex<BusState>,
    db: Db,
}

impl EventBus {
    pub fn new(db: Db) -> Self {
        let (sender, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            state: Mutex::new(BusState {
                seq: HashMap::new(),
                history: VecDeque::with_capacity(HISTORY_SIZE),
                next_event: 0,
                sender,
            }),
            db,
        }
    }

    /// Stamp, persist, buffer and fan out one event; returns the stored record.
    pub async fn publish(&self, run_id: &str, event: &str, data: Value) -> EventRecord {
        let mut state = self.state.lock().await;
        let seq = state.seq.get(run_id).copied().unwrap_or(0) + 1;
        state.seq.insert(run_id.to_string(), seq);
        state.next_event += 1;
        let record = EventRecord {
            id: state.next_event,
            run_id: run_id.to_string(),
            seq,
            ts: now_iso_millis(),
            event: event.to_string(),
            data: redact_value(&data),
        };
        state.history.push_back(record.clone());
        while state.history.len() > HISTORY_SIZE {
            state.history.pop_front();
        }

        let db = self.db.clone();
        let saved = record.clone();
        let persisted = tokio::task::spawn_blocking(move || {
            db.save_run_event(
                &saved.run_id,
                saved.seq,
                &saved.ts,
                &saved.event,
                &saved.data,
            )
        })
        .await;
        if let Ok(Err(error)) = persisted {
            tracing::error!(
                run_id = record.run_id,
                event = record.event,
                %error,
                "could not persist run event"
            );
        }

        // Sending under the lock keeps backlog and live delivery from duplicating.
        let _ = state.sender.send(record.clone());
        record
    }

    /// Register a client and hand it the buffered backlog after `last_event_id`.
    pub async fn subscribe(
        &self,
        last_event_id: u64,
    ) -> (Vec<EventRecord>, broadcast::Receiver<EventRecord>) {
        let state = self.state.lock().await;
        let backlog = state
            .history
            .iter()
            .filter(|record| record.id > last_event_id)
            .cloned()
            .collect();
        let receiver = state.sender.subscribe();
        (backlog, receiver)
    }
}

/// SSE body: buffered backlog, then live events, with a keepalive every interval.
pub fn sse_stream(
    backlog: Vec<EventRecord>,
    mut receiver: broadcast::Receiver<EventRecord>,
    keepalive: Duration,
) -> impl Stream<Item = Result<String, Infallible>> + Send + 'static {
    stream! {
        for record in backlog {
            yield Ok(frame(&record));
        }
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + keepalive, keepalive);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                received = receiver.recv() => match received {
                    Ok(record) => yield Ok(frame(&record)),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "SSE client lagged behind the event bus");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = ticker.tick() => yield Ok(": keepalive\n\n".to_string()),
            }
        }
    }
}

fn frame(record: &EventRecord) -> String {
    let data = serde_json::to_string(record).unwrap_or_else(|_| "{}".to_string());
    format!("id: {}\ndata: {}\n\n", record.id, data)
}

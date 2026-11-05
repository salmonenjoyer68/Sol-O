//! Scriptable fake OpenAI-compatible provider. No network: a loopback listener.
//!
//! Usage from any `[[test]]` target:
//!
//! ```ignore
//! mod support;
//! use support::fake_provider::{FakeProvider, Reply};
//!
//! let provider = FakeProvider::start(vec![
//!     Reply::content("{\"agent\":\"researcher\", ...}"),
//!     Reply::status_message(500, "provider is down"),
//! ]);
//! // Boot the binary with BASE_URL=provider.base_url(), API_KEY / MODEL set.
//! // Each request pops the next Reply; an exhausted script answers 500.
//! let sent = provider.requests(); // parsed JSON bodies, arrival order
//! assert_eq!(sent[0].model(), "test-model");
//! assert_eq!(sent[0].roles(), ["system", "user"]);
//! assert_eq!(sent[0].response_format_type(), Some("json_schema"));
//! ```
//!
//! `Recorded` exposes the model, message roles, message contents and the
//! attempted `response_format` so tests can assert what the client sent.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}

impl Reply {
    /// A successful chat completion whose message content is `content`.
    pub fn content(content: impl Into<String>) -> Self {
        Self {
            status: 200,
            body:
                json!({"choices": [{"message": {"role": "assistant", "content": content.into()}}]})
                    .to_string(),
            delay: Duration::ZERO,
        }
    }

    /// An error status with a generic OpenAI-shaped error message.
    pub fn status(status: u16) -> Self {
        Self::status_message(status, "scripted provider failure")
    }

    /// An error status whose body mentions `message` (used for fallback tests).
    pub fn status_message(status: u16, message: &str) -> Self {
        Self {
            status,
            body: json!({"error": {"message": message}}).to_string(),
            delay: Duration::ZERO,
        }
    }

    /// Wait before answering, to exercise the client timeout.
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// One request the client sent, with its parsed JSON body.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub body: Value,
}

impl Recorded {
    pub fn model(&self) -> &str {
        self.body["model"].as_str().unwrap_or_default()
    }

    pub fn messages(&self) -> &[Value] {
        self.body["messages"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Roles in order, e.g. `["system", "user", "assistant", "user"]`.
    pub fn roles(&self) -> Vec<&str> {
        self.messages()
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default())
            .collect()
    }

    pub fn contents(&self) -> Vec<&str> {
        self.messages()
            .iter()
            .map(|message| message["content"].as_str().unwrap_or_default())
            .collect()
    }

    /// The `response_format` sent, or `None` when that rung is skipped.
    pub fn response_format(&self) -> Option<&Value> {
        self.body
            .get("response_format")
            .filter(|value| !value.is_null())
    }

    pub fn response_format_type(&self) -> Option<&str> {
        self.response_format()?.get("type")?.as_str()
    }
}

pub struct FakeProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeProvider {
    pub fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
        let address = listener.local_addr().expect("fake provider address");
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shared_requests = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if let Err(error) = handle(stream, &script, &shared_requests) {
                    eprintln!("fake provider: {error}");
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            requests,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("requests lock").clone()
    }
}

fn handle(
    mut stream: std::net::TcpStream,
    script: &Mutex<VecDeque<Reply>>,
    requests: &Mutex<Vec<Recorded>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut content_length = 0_usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body)?;
    let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
    requests
        .lock()
        .expect("requests lock")
        .push(Recorded { body: parsed });

    let reply = script
        .lock()
        .expect("script lock")
        .pop_front()
        .unwrap_or_else(|| Reply::status(500));
    if !reply.delay.is_zero() {
        std::thread::sleep(reply.delay);
    }
    let response = format!(
        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

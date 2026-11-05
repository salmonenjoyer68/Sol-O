//! Operator CLI: `serve`, `run`, `show`, `pending`, `approve` and `reject`.
//!
//! Mirrors `app/cli.py`; every command shares the server's persistence and
//! orchestration code, so a run started in the terminal is the run the API sees.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::approvals;
use crate::config::{self, ROOT};
use crate::events::EventBus;
use crate::http::{router, AppState};
use crate::orchestrator::Orchestrator;
use crate::persistence::{self, Db};
use crate::reliability::redact;

/// Status -> process exit code, from `EXIT_CODES` in the reference CLI.
fn exit_code(status: &str) -> u8 {
    match status {
        "complete" => 0,
        "failed" => 1,
        "needs_clarification" => 2,
        "partial" => 3,
        "awaiting_approval" => 4,
        "rejected" => 5,
        _ => 1,
    }
}

#[derive(Parser)]
#[command(name = "sol-o", about = "Solar-system multi-agent orchestrator")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Serve the HTTP API, the SSE stream and the UI.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8000)]
        port: u16,
    },
    /// Run a task through the orchestrator and print its result.
    Run {
        /// The task text; joined with spaces when spread over several words.
        #[arg(num_args = 0..)]
        task: Vec<String>,
        /// Let agents/sun.md choose the agents instead of the hardcoded flow.
        #[arg(long)]
        plan: bool,
        /// Read the task from a text file instead of the command line.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Conversation source; omit for a per-invocation source so recall
        /// cannot cross terminal tasks.
        #[arg(long)]
        source: Option<String>,
    },
    /// Print one run and its timeline.
    Show { run_id: String },
    /// List actions awaiting approval.
    Pending,
    /// Execute one stored action once.
    Approve { id: i64 },
    /// Reject one stored action.
    Reject { id: i64 },
}

pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    if let Some(path) = env_file() {
        dotenvy::from_path(path).ok();
    }
    match cli.command {
        Commands::Serve { host, port } => match serve(&host, port).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("sol-o: {error}");
                ExitCode::FAILURE
            }
        },
        Commands::Run {
            task,
            plan,
            file,
            source,
        } => run_task(&task, plan, file.as_deref(), source).await,
        Commands::Show { run_id } => show_run(&run_id),
        Commands::Pending => show_pending(),
        Commands::Approve { id } => decide(id, true).await,
        Commands::Reject { id } => decide(id, false).await,
    }
}

async fn serve(host: &str, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let settings = config::load_settings();
    let db = Db::from_env();
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let address = listener.local_addr()?;
    tracing::info!(address = %address, "listening");
    let app = router(AppState::new(settings, db));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn run_task(
    words: &[String],
    plan: bool,
    file: Option<&Path>,
    source: Option<String>,
) -> ExitCode {
    let task = match task_from_args(words, file) {
        Ok(task) => task,
        Err(message) => return usage_error(&message),
    };
    let settings = Arc::new(config::load_settings());
    let db = Db::from_env();
    let bus = Arc::new(EventBus::new(db.clone()));
    let run_id = persistence::new_run_id();
    let source = source.unwrap_or_else(|| format!("cli:{run_id}"));
    let orchestrator = Orchestrator::new(settings, db, bus);
    let outcome =
        tokio::task::spawn_blocking(move || orchestrator.run(&task, plan, &run_id, &source)).await;
    match outcome {
        Ok(Ok(result)) => {
            let status = result.status.clone();
            match serde_json::to_string_pretty(&result) {
                Ok(text) => println!("{text}"),
                Err(error) => {
                    eprintln!("sol-o: {error}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::from(exit_code(&status))
        }
        Ok(Err(error)) => {
            eprintln!("sol-o: {error}");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("sol-o: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `--file`, then the positional words, then `TASK_DESCRIPTION`, like the reference.
fn task_from_args(words: &[String], file: Option<&Path>) -> Result<String, String> {
    let task = if let Some(path) = file {
        match std::fs::read_to_string(path) {
            Ok(text) => text.trim().to_string(),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        }
    } else if !words.is_empty() {
        words.join(" ").trim().to_string()
    } else {
        std::env::var("TASK_DESCRIPTION")
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    if task.is_empty() {
        return Err("no task: pass text, use --file, or set TASK_DESCRIPTION in .env".to_string());
    }
    Ok(task)
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("usage: sol-o run [TASK]... [--plan] [--file FILE] [--source SOURCE]");
    eprintln!("sol-o: error: {message}");
    ExitCode::from(2)
}

fn show_run(run_id: &str) -> ExitCode {
    let db = Db::from_env();
    let detail = match db.get_run_detail(run_id) {
        Ok(Some(detail)) => detail,
        Ok(None) => {
            println!("run {run_id} not found");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("sol-o: {error}");
            return ExitCode::FAILURE;
        }
    };
    let run = &detail.run;
    println!(
        "Run {}  status={}  created={}  duration={}s",
        run.id,
        run.status,
        run.created_at,
        python_float(run.duration_seconds)
    );
    println!("task: {}", take(&run.task, 300));
    if let Some(warning) = run.warning.as_deref().filter(|text| !text.is_empty()) {
        println!("warning: {warning}");
    }
    println!("timeline:");
    if detail.agent_calls.is_empty() {
        println!("  (no agent calls recorded)");
    }
    for call in &detail.agent_calls {
        let mut line = format!(
            "  step {:>2}  {:<10} {:<7} {:>7}ms  attempts={}",
            call.step, call.agent, call.status, call.duration_ms, call.attempts
        );
        if let Some(error_type) = call.error_type.as_deref().filter(|text| !text.is_empty()) {
            line.push_str(&format!(
                "  error={error_type}: {}",
                call.error_message.as_deref().unwrap_or("")
            ));
        }
        println!("{line}");
    }
    for tool in &detail.tool_calls {
        let mut line = format!(
            "  tool      {:<10} {:<7} {:>7}ms",
            tool.tool, tool.status, tool.duration_ms
        );
        if let Some(message) = tool
            .error_message
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            line.push_str(&format!("  error={message}"));
        }
        println!("{line}");
    }
    for action in &detail.pending_actions {
        println!(
            "  approval  #{} {} status={} decided={}",
            action.id,
            action.action_type,
            action.status,
            action.decided_at.as_deref().unwrap_or("-")
        );
    }
    ExitCode::SUCCESS
}

fn show_pending() -> ExitCode {
    let settings = config::load_settings();
    let db = Db::from_env();
    let rows = match db.list_pending(settings.pending_ttl_hours) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("sol-o: {error}");
            return ExitCode::FAILURE;
        }
    };
    if rows.is_empty() {
        println!("no pending actions");
        return ExitCode::SUCCESS;
    }
    for row in rows {
        let payload = &row.payload;
        let verdict = payload.get("verdict").and_then(Value::as_str).unwrap_or("");
        println!(
            "#{}  run={}  type={}  created={}  run_status={}",
            row.id,
            row.run_id,
            row.action_type,
            row.created_at,
            payload
                .get("run_status")
                .and_then(Value::as_str)
                .unwrap_or("")
        );
        println!(
            "  task: {}",
            take(&redact(&payload_text(payload, "task")), 200)
        );
        println!(
            "  verdict: {}",
            if verdict.is_empty() { "n/a" } else { verdict }
        );
        println!("  message to approve:");
        for line in redact(&payload_text(payload, "message")).lines() {
            println!("    {line}");
        }
    }
    ExitCode::SUCCESS
}

async fn decide(action_id: i64, approve: bool) -> ExitCode {
    let settings = config::load_settings();
    let db = Db::from_env();
    let bus = Arc::new(EventBus::new(db.clone()));
    let outcome = if approve {
        approvals::approve(&db, &settings, action_id)
    } else {
        approvals::reject(&db, &settings, action_id)
    };
    let decision = if approve { "approve" } else { "reject" };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("sol-o: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let (Some(run_id), Some(event)) = (outcome.run_id.as_deref(), outcome.event) {
        bus.publish(run_id, "approval_decided", event).await;
    }
    match outcome.response {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("sol-o: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            println!("{decision} failed ({}): {}", error.code, error.detail);
            ExitCode::FAILURE
        }
    }
}

fn payload_text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn take(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Python's `str(float)`: whole numbers keep a `.0` suffix.
fn python_float(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// `SOL_O_ENV_FILE` selects the dotenv file: unset keeps `<ROOT>/.env`, an
/// empty value loads nothing, and any other value loads that path.
fn env_file() -> Option<PathBuf> {
    match std::env::var_os("SOL_O_ENV_FILE") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => Some(Path::new(ROOT).join(".env")),
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

//! Operator CLI. Only `serve` exists so far; the run subcommands arrive later.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::config::{self, ROOT};
use crate::http::{router, AppState};
use crate::persistence::Db;

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
        #[arg(long, default_value_t = 8001)]
        port: u16,
    },
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { host, port } => serve(&host, port).await,
    }
}

async fn serve(host: &str, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = env_file() {
        dotenvy::from_path(path).ok();
    }
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

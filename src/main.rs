use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use mumpeer::config::{ApiSecret, Config, SECRET_ENV};
use tokio::signal::unix::{SignalKind, signal};

/// Relay server for Mumcord peer-to-peer features.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path of the TOML config.
    #[arg(
        long,
        env = "MUMPEER_CONFIG",
        default_value = "/etc/mumpeer/mumpeer.toml"
    )]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let secret = ApiSecret::load(&config, std::env::var(SECRET_ENV).ok())?;
    let running = mumpeer::start(config, secret).await?;
    // Docker stops containers with SIGTERM; without a handler the relay would be killed only
    // after the stop timeout.
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = running.wait() => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate.recv() => Ok(()),
    }
}

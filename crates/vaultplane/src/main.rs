//! VaultPlane Gateway data plane entry point.
//!
//! This binary will host the OpenAI-compatible proxy API and the admin API. The
//! runtime is not yet implemented; today it initializes logging, reports its
//! configuration source, and exits.

use clap::Parser;
use vaultplane_core::config::Config;

/// VaultPlane Gateway: every model call, on policy.
#[derive(Debug, Parser)]
#[command(name = "vaultplane", version, about)]
struct Args {
    /// Path to the YAML configuration file.
    #[arg(long, value_name = "PATH", env = "VAULTPLANE_CONFIG")]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args = Args::parse();
    let _config = Config::default();

    tracing::info!(
        version = vaultplane_core::VERSION,
        "VaultPlane Gateway starting"
    );
    match args.config.as_deref() {
        Some(path) => tracing::info!(config = path, "configuration file specified"),
        None => tracing::info!("no configuration file specified; using defaults"),
    }
    tracing::warn!("the gateway runtime is not yet implemented; exiting");

    Ok(())
}

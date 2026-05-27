//! `vaultplane-ctl`: operator CLI for VaultPlane Gateway.
//!
//! The subcommands mirror the operations an operator performs against a running
//! gateway or its configuration. The implementations are stubs today; each reports
//! that it is not yet implemented while the command surface is established.

use clap::{Parser, Subcommand};

/// Operate VaultPlane Gateway from the command line.
#[derive(Debug, Parser)]
#[command(name = "vaultplane-ctl", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect and validate gateway configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Manage virtual keys.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Inspect the virtual model registry.
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Report the status of a running gateway.
    Status {
        /// Admin API endpoint to query.
        #[arg(long, value_name = "URL")]
        admin_endpoint: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Validate a configuration file.
    Validate {
        /// Path to the configuration file.
        path: String,
    },
    /// Show the difference between two configuration files.
    Diff {
        /// The original configuration file.
        old: String,
        /// The new configuration file.
        new: String,
    },
}

#[derive(Debug, Subcommand)]
enum KeyAction {
    /// Create a virtual key.
    Create {
        /// Team the key is attributed to.
        #[arg(long)]
        team: String,
        /// Application the key is attributed to.
        #[arg(long)]
        app: String,
        /// Environment the key is attributed to (for example prod).
        #[arg(long)]
        env: String,
        /// Allowed model. Repeat to allow several.
        #[arg(long = "model")]
        models: Vec<String>,
        /// Requests-per-second rate limit.
        #[arg(long)]
        rps: Option<u32>,
        /// Spend limit, given as AMOUNT/PERIOD, for example 500/day.
        #[arg(long)]
        spend: Option<String>,
    },
    /// List virtual keys.
    List,
    /// Revoke a virtual key.
    Revoke {
        /// The id of the key to revoke.
        key_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum ModelAction {
    /// List configured virtual models.
    List,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let action = match cli.command {
        Command::Config { action } => match action {
            ConfigAction::Validate { .. } => "config validate",
            ConfigAction::Diff { .. } => "config diff",
        },
        Command::Key { action } => match action {
            KeyAction::Create { .. } => "key create",
            KeyAction::List => "key list",
            KeyAction::Revoke { .. } => "key revoke",
        },
        Command::Model { action } => match action {
            ModelAction::List => "model list",
        },
        Command::Status { .. } => "status",
    };
    anyhow::bail!("`vaultplane-ctl {action}` is not yet implemented")
}

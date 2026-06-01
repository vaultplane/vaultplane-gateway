//! `vaultplane-ctl`: operator CLI for VaultPlane Gateway.
//!
//! Today this implements `key create` (generate a virtual key and print a YAML
//! record to drop into the gateway's `auth.keys` list); the other subcommands are
//! stubbed while the surface is established.

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
    /// Create a virtual key and print a YAML record for the gateway config.
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
        /// Spend limit (not yet enforced), given as AMOUNT/PERIOD, for example 500/day.
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

fn action_name(command: &Command) -> &'static str {
    match command {
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
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let name = action_name(&cli.command);
    match cli.command {
        Command::Key {
            action:
                KeyAction::Create {
                    team,
                    app,
                    env,
                    models,
                    rps,
                    spend,
                },
        } => key_create(team, app, env, models, rps, spend),
        _ => anyhow::bail!("`vaultplane-ctl {name}` is not yet implemented"),
    }
}

fn key_create(
    team: String,
    app: String,
    env: String,
    models: Vec<String>,
    rps: Option<u32>,
    spend: Option<String>,
) -> anyhow::Result<()> {
    let key = vaultplane_core::auth::generate_key();

    println!("Token: {}", key.token);
    println!("Key id: {}", key.id);
    println!();
    println!("Save the token now; it cannot be recovered (the gateway only stores the hash).");
    println!();
    println!("Add this entry to the gateway's auth.keys list:");
    println!();
    println!("  - id: \"{}\"", key.id);
    println!("    hash: \"{}\"", key.hash);
    println!("    team: \"{team}\"");
    println!("    app: \"{app}\"");
    println!("    env: \"{env}\"");
    if models.is_empty() {
        println!("    models: []");
    } else {
        let list = models
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ");
        println!("    models: [{list}]");
    }
    if let Some(rps) = rps {
        println!("    rate_limit_rps: {rps}");
    }
    if let Some(spend) = spend {
        println!("    # spend_limit: {spend}  (configured but not yet enforced)");
    }

    Ok(())
}

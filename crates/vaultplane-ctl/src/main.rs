//! `vaultplane-ctl`: operator CLI for VaultPlane Gateway.
//!
//! Two modes:
//!
//! * **Offline** (no `--endpoint`): `key create` generates a fresh virtual key and
//!   prints a YAML record the operator can paste into `auth.keys` to bootstrap a
//!   gateway that does not yet have an admin API reachable.
//! * **Online** (`--endpoint <url>`): the CLI talks to a running gateway's admin
//!   API to issue, list, and revoke keys, and to read status. Authentication uses
//!   `--token`, or the `VAULTPLANE_ADMIN_TOKEN` environment variable.

use anyhow::{Context, anyhow, bail};
use clap::{Parser, Subcommand};
use reqwest::header::AUTHORIZATION;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use vaultplane_core::auth::{Period, SpendLimit};

/// Operate VaultPlane Gateway from the command line.
#[derive(Debug, Parser)]
#[command(name = "vaultplane-ctl", version, about)]
struct Cli {
    /// Admin API endpoint, for example `http://localhost:9091`. Required for
    /// every subcommand except offline `key create`.
    #[arg(long, global = true, env = "VAULTPLANE_ADMIN_ENDPOINT")]
    endpoint: Option<String>,
    /// Bearer token presented to the admin API.
    #[arg(long, global = true, env = "VAULTPLANE_ADMIN_TOKEN")]
    token: Option<String>,
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
    Status,
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
    /// Create a virtual key. With `--endpoint`, the gateway issues the key over
    /// the admin API. Without, the CLI generates one locally and prints a YAML
    /// record to add to the gateway's `auth.keys` list.
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
        /// Spend limit, given as `AMOUNT/PERIOD`, for example `500/day`.
        #[arg(long)]
        spend: Option<String>,
        /// RFC3339 expiry timestamp, for example `2026-12-31T23:59:59Z`.
        #[arg(long)]
        expires_at: Option<String>,
    },
    /// List virtual keys currently configured on the gateway.
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Key { action } => match action {
            KeyAction::Create {
                team,
                app,
                env,
                models,
                rps,
                spend,
                expires_at,
            } => {
                let spend_limit = spend.as_deref().map(parse_spend).transpose()?;
                match cli.endpoint {
                    Some(endpoint) => {
                        let client = AdminClient::new(endpoint, cli.token)?;
                        key_create_online(
                            &client,
                            team,
                            app,
                            env,
                            models,
                            rps,
                            spend_limit,
                            expires_at,
                        )
                        .await
                    }
                    None => key_create_offline(team, app, env, models, rps, spend_limit),
                }
            }
            KeyAction::List => {
                let endpoint = require_endpoint(cli.endpoint)?;
                let client = AdminClient::new(endpoint, cli.token)?;
                key_list(&client).await
            }
            KeyAction::Revoke { key_id } => {
                let endpoint = require_endpoint(cli.endpoint)?;
                let client = AdminClient::new(endpoint, cli.token)?;
                key_revoke(&client, &key_id).await
            }
        },
        Command::Status => {
            let endpoint = require_endpoint(cli.endpoint)?;
            let client = AdminClient::new(endpoint, cli.token)?;
            status(&client).await
        }
        Command::Config { action } => {
            let name = match action {
                ConfigAction::Validate { .. } => "config validate",
                ConfigAction::Diff { .. } => "config diff",
            };
            bail!("`vaultplane-ctl {name}` is not yet implemented")
        }
        Command::Model {
            action: ModelAction::List,
        } => bail!("`vaultplane-ctl model list` is not yet implemented"),
    }
}

fn require_endpoint(endpoint: Option<String>) -> anyhow::Result<String> {
    endpoint.ok_or_else(|| {
        anyhow!("this subcommand requires --endpoint (or the VAULTPLANE_ADMIN_ENDPOINT env var)")
    })
}

// ---------------------------------------------------------------------------
// Admin API client
// ---------------------------------------------------------------------------

/// Thin client for the VaultPlane Gateway admin API.
pub struct AdminClient {
    base: String,
    token: Option<String>,
    http: Client,
}

impl AdminClient {
    pub fn new(endpoint: String, token: Option<String>) -> anyhow::Result<Self> {
        let http = Client::builder()
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            base: endpoint.trim_end_matches('/').to_string(),
            token,
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => builder.header(AUTHORIZATION, format!("Bearer {t}")),
            None => builder,
        }
    }

    pub async fn create_key(&self, body: &CreateKeyRequest) -> anyhow::Result<CreateKeyResponse> {
        let response = self
            .auth(self.http.post(self.url("/admin/keys")).json(body))
            .send()
            .await
            .context("failed to call POST /admin/keys")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("admin API returned {status}: {body}");
        }
        response
            .json::<CreateKeyResponse>()
            .await
            .context("failed to decode key create response")
    }

    pub async fn list_keys(&self) -> anyhow::Result<ListKeysResponse> {
        let response = self
            .auth(self.http.get(self.url("/admin/keys")))
            .send()
            .await
            .context("failed to call GET /admin/keys")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("admin API returned {status}: {body}");
        }
        response
            .json::<ListKeysResponse>()
            .await
            .context("failed to decode key list response")
    }

    pub async fn revoke_key(&self, id: &str) -> anyhow::Result<()> {
        let response = self
            .auth(self.http.delete(self.url(&format!("/admin/keys/{id}"))))
            .send()
            .await
            .context("failed to call DELETE /admin/keys/:id")?;
        match response.status() {
            StatusCode::NO_CONTENT => Ok(()),
            StatusCode::NOT_FOUND => bail!("no key with id {id}"),
            status => {
                let body = response.text().await.unwrap_or_default();
                bail!("admin API returned {status}: {body}")
            }
        }
    }

    pub async fn status(&self) -> anyhow::Result<serde_json::Value> {
        let response = self
            .auth(self.http.get(self.url("/admin/status")))
            .send()
            .await
            .context("failed to call GET /admin/status")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("admin API returned {status}: {body}");
        }
        response
            .json::<serde_json::Value>()
            .await
            .context("failed to decode status response")
    }
}

// ---------------------------------------------------------------------------
// Admin API DTOs. These mirror the shapes the gateway's admin module produces.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreateKeyRequest {
    pub team: String,
    pub app: String,
    pub env: String,
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_rps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit: Option<SpendLimit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateKeyResponse {
    pub token: String,
    pub key: KeySummary,
}

#[derive(Debug, Deserialize)]
pub struct KeySummary {
    pub id: String,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub rate_limit_rps: Option<u32>,
    #[serde(default)]
    pub spend_limit: Option<SpendLimit>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListKeysResponse {
    pub data: Vec<KeySummary>,
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn key_create_online(
    client: &AdminClient,
    team: String,
    app: String,
    env: String,
    models: Vec<String>,
    rate_limit_rps: Option<u32>,
    spend_limit: Option<SpendLimit>,
    expires_at: Option<String>,
) -> anyhow::Result<()> {
    let response = client
        .create_key(&CreateKeyRequest {
            team,
            app,
            env,
            models,
            rate_limit_rps,
            spend_limit,
            expires_at,
        })
        .await?;

    println!("Issued key {}", response.key.id);
    println!();
    println!("Token: {}", response.token);
    println!("Save the token now; it cannot be recovered.");
    println!();
    println!("Scope:");
    println!("  team:   {}", response.key.team);
    println!("  app:    {}", response.key.app);
    println!("  env:    {}", response.key.env);
    println!("  models: {}", format_models(&response.key.models));
    if let Some(rps) = response.key.rate_limit_rps {
        println!("  rate_limit_rps: {rps}");
    }
    if let Some(limit) = response.key.spend_limit {
        println!(
            "  spend_limit: {}/{}",
            limit.amount_usd,
            period_label(limit.period)
        );
    }
    if let Some(expires) = response.key.expires_at {
        println!("  expires_at: {expires}");
    }
    Ok(())
}

fn key_create_offline(
    team: String,
    app: String,
    env: String,
    models: Vec<String>,
    rps: Option<u32>,
    spend_limit: Option<SpendLimit>,
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
    if let Some(limit) = spend_limit {
        println!("    spend_limit:");
        println!("      amount_usd: {}", limit.amount_usd);
        println!("      period: {}", period_label(limit.period));
    }

    Ok(())
}

async fn key_list(client: &AdminClient) -> anyhow::Result<()> {
    let response = client.list_keys().await?;
    if response.data.is_empty() {
        println!("(no keys configured)");
        return Ok(());
    }
    println!(
        "{:<22} {:<12} {:<12} {:<8} MODELS",
        "ID", "TEAM", "APP", "ENV"
    );
    for k in response.data {
        println!(
            "{:<22} {:<12} {:<12} {:<8} {}",
            k.id,
            k.team,
            k.app,
            k.env,
            format_models(&k.models)
        );
    }
    Ok(())
}

async fn key_revoke(client: &AdminClient, id: &str) -> anyhow::Result<()> {
    client.revoke_key(id).await?;
    println!("Revoked {id}");
    Ok(())
}

async fn status(client: &AdminClient) -> anyhow::Result<()> {
    let body = client.status().await?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

fn format_models(models: &[String]) -> String {
    if models.is_empty() {
        "(any)".to_string()
    } else {
        models.join(", ")
    }
}

fn period_label(period: Period) -> &'static str {
    match period {
        Period::Day => "day",
        Period::Week => "week",
        Period::Month => "month",
    }
}

/// Parse a spend limit specifier of the form `AMOUNT/PERIOD`, for example `500/day`.
fn parse_spend(spec: &str) -> anyhow::Result<SpendLimit> {
    let (amount, period) = spec
        .split_once('/')
        .ok_or_else(|| anyhow!("spend must be AMOUNT/PERIOD, got '{spec}'"))?;
    let amount_usd: f64 = amount
        .parse()
        .map_err(|e| anyhow!("invalid spend amount '{amount}': {e}"))?;
    let period = match period {
        "day" => Period::Day,
        "week" => Period::Week,
        "month" => Period::Month,
        other => bail!("unknown period '{other}'; expected day, week, or month"),
    };
    Ok(SpendLimit { amount_usd, period })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_spend_accepts_amount_and_period() {
        let limit = parse_spend("500/day").unwrap();
        assert_eq!(limit.amount_usd, 500.0);
        assert_eq!(limit.period, Period::Day);

        let limit = parse_spend("12.5/week").unwrap();
        assert_eq!(limit.amount_usd, 12.5);
        assert_eq!(limit.period, Period::Week);

        assert!(parse_spend("nope").is_err());
        assert!(parse_spend("10/year").is_err());
        assert!(parse_spend("abc/day").is_err());
    }

    #[tokio::test]
    async fn create_key_sends_bearer_token_and_decodes_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/admin/keys"))
            .and(header("authorization", "Bearer T"))
            .and(body_partial_json(
                json!({"team": "core", "models": ["gpt-4o"]}),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "token": "vp_secret",
                "key": {
                    "id": "vp_id",
                    "team": "core",
                    "app": "web",
                    "env": "prod",
                    "models": ["gpt-4o"],
                },
            })))
            .mount(&server)
            .await;

        let client = AdminClient::new(server.uri(), Some("T".to_string())).unwrap();
        let response = client
            .create_key(&CreateKeyRequest {
                team: "core".to_string(),
                app: "web".to_string(),
                env: "prod".to_string(),
                models: vec!["gpt-4o".to_string()],
                rate_limit_rps: None,
                spend_limit: None,
                expires_at: None,
            })
            .await
            .unwrap();
        assert_eq!(response.token, "vp_secret");
        assert_eq!(response.key.id, "vp_id");
    }

    #[tokio::test]
    async fn list_keys_decodes_data_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/admin/keys"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "vp_a", "team": "t1", "app": "a1", "env": "prod", "models": []},
                    {"id": "vp_b", "team": "t2", "app": "a2", "env": "dev", "models": ["gpt-4o"]},
                ],
            })))
            .mount(&server)
            .await;

        let client = AdminClient::new(server.uri(), None).unwrap();
        let response = client.list_keys().await.unwrap();
        assert_eq!(response.data.len(), 2);
        assert_eq!(response.data[0].id, "vp_a");
        assert_eq!(response.data[1].models, vec!["gpt-4o".to_string()]);
    }

    #[tokio::test]
    async fn revoke_key_treats_204_as_success_and_404_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/admin/keys/vp_present"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/admin/keys/vp_missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = AdminClient::new(server.uri(), None).unwrap();
        client.revoke_key("vp_present").await.unwrap();
        let err = client.revoke_key("vp_missing").await.unwrap_err();
        assert!(err.to_string().contains("no key with id"));
    }

    #[tokio::test]
    async fn status_passes_through_the_admin_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/admin/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "0.0.0",
                "ready": true,
                "key_count": 3,
            })))
            .mount(&server)
            .await;

        let client = AdminClient::new(server.uri(), None).unwrap();
        let body = client.status().await.unwrap();
        assert_eq!(body["version"], "0.0.0");
        assert_eq!(body["key_count"], 3);
    }

    #[tokio::test]
    async fn admin_client_surfaces_non_success_status_with_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/admin/keys"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let client = AdminClient::new(server.uri(), None).unwrap();
        let err = client.list_keys().await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("unauthorized"), "{message}");
    }
}

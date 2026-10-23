mod config;
mod db;
mod discord;
mod format;
mod media;
mod relay;
mod slack;

use std::sync::Arc;

use anyhow::Result;
use serenity::model::id::GuildId;
use serenity::prelude::GatewayIntents;
use slack_morphism::prelude::*;

use crate::config::Config;
use crate::db::Db;
use crate::relay::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env in local dev if present (ignored in container).
    let _ = dotenv_lite();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Arc::new(Config::from_env()?);
    let db = Db::connect(&config.database_url).await?;
    let http = reqwest::Client::builder().build()?;

    // Discord: REST client for provisioning + a gateway client for Discord→Slack.
    let discord_http = Arc::new(serenity::http::Http::new(&config.discord_bot_token));
    let discord_guild = GuildId::new(config.discord_guild_id);

    // Slack: one shared client; sessions are opened per-call with each token.
    let slack_client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
    let slack_bot_token = SlackApiToken::new(config.slack_bot_token.clone().into());
    let slack_user_token = SlackApiToken::new(config.slack_user_token.clone().into());

    // Identify our own bot user id, so we recognise the bot joining a channel.
    let bot_user_id = {
        let session = slack_client.open_session(&slack_bot_token);
        session.auth_test().await?.user_id.to_string()
    };
    tracing::info!(%bot_user_id, "slack bot identity resolved");

    let credit_re = Arc::new(relay::build_credit_regex(&config.credit_template));

    let state = AppState {
        config: config.clone(),
        db,
        http,
        discord: discord_http,
        discord_guild,
        slack_bot_token,
        slack_user_token,
        slack: slack_client,
        bot_user_id,
        credit_re,
    };

    // Discord gateway client (Discord→Slack direction).
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILDS;
    let mut discord_client = serenity::Client::builder(&config.discord_bot_token, intents)
        .event_handler(discord::Handler {
            state: state.clone(),
        })
        .await?;

    // Heartbeat loop for the container healthcheck.
    let hb_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = hb_state.heartbeat().await {
                tracing::warn!(error = ?e, "heartbeat write failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    });

    // Run both sides concurrently; exit if either stops.
    let slack_state = state.clone();
    tokio::select! {
        r = discord_client.start() => {
            tracing::error!(result = ?r, "discord gateway exited");
        }
        r = slack::run_socket_mode(slack_state) => {
            tracing::error!(result = ?r, "slack socket mode exited");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown signal received");
        }
    }
    Ok(())
}

/// Minimal .env loader (KEY=VALUE lines) so we avoid an extra dependency.
fn dotenv_lite() -> Result<()> {
    let content = std::fs::read_to_string(".env")?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if std::env::var(k).is_err() {
                std::env::set_var(k.trim(), v.trim());
            }
        }
    }
    Ok(())
}

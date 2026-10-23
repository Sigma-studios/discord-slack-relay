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
        // Plain text: façade logs don't render ANSI colours.
        .with_ansi(false)
        // Without RUST_LOG, EnvFilter defaults to errors only, which hid every
        // relay warning in production. Default to our debug + library warnings.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("warn,discord_slack_relay=debug")
            }),
        )
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

    // Identify our own bot: user id (to spot it joining channels) and bot id
    // (to drop its own posts when they echo back as events).
    let (bot_user_id, bot_id) = {
        let session = slack_client.open_session(&slack_bot_token);
        let me = session.auth_test().await?;
        let bot_id = me.bot_id.map(|b| b.to_string()).unwrap_or_default();
        (me.user_id.to_string(), bot_id)
    };
    tracing::info!(%bot_user_id, %bot_id, "slack bot identity resolved");

    let state = AppState {
        config: config.clone(),
        db,
        http,
        discord: discord_http,
        discord_guild,
        slack_bot_token,
        slack: slack_client,
        bot_user_id,
        bot_id,
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

    // Mirror channels the bot was invited to while the relay wasn't running.
    let reconcile_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = slack::reconcile_memberships(&reconcile_state).await {
            tracing::warn!(error = ?e, "membership reconciliation failed");
        }
    });

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

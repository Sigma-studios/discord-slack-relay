use anyhow::{Context, Result};

/// All runtime configuration, read from the environment once at startup.
#[derive(Clone)]
pub struct Config {
    pub slack_bot_token: String,
    pub slack_app_token: String,
    pub discord_bot_token: String,
    pub discord_guild_id: u64,
    pub database_url: String,
    /// Slack display name for relayed Discord messages. `{name}` = Discord name.
    pub name_template: String,
    pub heartbeat_file: String,
}

fn env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("missing required env var {key}"))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            slack_bot_token: env("SLACK_BOT_TOKEN")?,
            slack_app_token: env("SLACK_APP_TOKEN")?,
            discord_bot_token: env("DISCORD_BOT_TOKEN")?,
            discord_guild_id: env("DISCORD_GUILD_ID")?
                .parse()
                .context("DISCORD_GUILD_ID must be a u64")?,
            database_url: env("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://relay.db?mode=rwc".to_string()),
            name_template: std::env::var("SLACK_NAME_TEMPLATE")
                .unwrap_or_else(|_| "{name} (Discord)".to_string()),
            heartbeat_file: std::env::var("HEARTBEAT_FILE")
                .unwrap_or_else(|_| "/tmp/heartbeat".to_string()),
        })
    }

    /// Slack username shown on a relayed Discord message.
    pub fn slack_username(&self, discord_name: &str) -> String {
        self.name_template.replace("{name}", discord_name)
    }
}

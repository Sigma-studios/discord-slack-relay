use anyhow::{Context, Result};

/// All runtime configuration, read from the environment once at startup.
#[derive(Clone)]
pub struct Config {
    pub slack_bot_token: String,
    pub slack_app_token: String,
    pub slack_user_token: String,
    pub discord_bot_token: String,
    pub discord_guild_id: u64,
    pub database_url: String,
    pub credit_template: String,
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
            slack_user_token: env("SLACK_USER_TOKEN")?,
            discord_bot_token: env("DISCORD_BOT_TOKEN")?,
            discord_guild_id: env("DISCORD_GUILD_ID")?
                .parse()
                .context("DISCORD_GUILD_ID must be a u64")?,
            database_url: env("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://relay.db?mode=rwc".to_string()),
            // `\n` in the env value is taken literally by the shell, so unescape it.
            credit_template: std::env::var("SLACK_CREDIT_TEMPLATE")
                .unwrap_or_else(|_| "\n\n_— {name} via Discord_".to_string())
                .replace("\\n", "\n"),
            heartbeat_file: std::env::var("HEARTBEAT_FILE")
                .unwrap_or_else(|_| "/tmp/heartbeat".to_string()),
        })
    }

    /// Render the credit line appended to Discord→Slack messages.
    pub fn credit_line(&self, discord_name: &str) -> String {
        self.credit_template.replace("{name}", discord_name)
    }
}

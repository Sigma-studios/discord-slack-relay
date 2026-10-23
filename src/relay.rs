//! Shared state and the direction-agnostic relay logic.
//!
//! The SDK adapters (`slack.rs`, `discord.rs`) extract data from events and call
//! into here; this module owns the mapping/formatting decisions.

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use std::sync::Arc;

use slack_morphism::prelude::*;

use crate::config::Config;
use crate::db::Db;
use crate::format;

type SlackHyperClient =
    SlackClient<slack_morphism::prelude::SlackClientHyperHttpsConnector>;

/// Everything the relay needs at runtime, cloned cheaply via `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub http: reqwest::Client,
    pub discord: Arc<serenity::http::Http>,
    pub discord_guild: serenity::model::id::GuildId,
    pub slack: Arc<SlackHyperClient>,
    pub slack_bot_token: SlackApiToken,
    pub slack_user_token: SlackApiToken,
    /// The bot's own Slack user id, so we recognise our bot joining a channel.
    pub bot_user_id: String,
    /// Matches text the relay itself posted into Slack (the credit line),
    /// so those messages are not mirrored back to Discord.
    pub credit_re: Arc<Regex>,
}

// ── mention rewriting ────────────────────────────────────────────────────────

static SLACK_USER_MENTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@([UW][A-Z0-9]+)>").unwrap());
static SLACK_CHANNEL_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<#C[A-Z0-9]+\|([^>]+)>").unwrap());
static DISCORD_TO_SLACK_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"@s:([A-Za-z0-9_.-]+)").unwrap());
static SLACK_TO_DISCORD_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"@d:([A-Za-z0-9_.-]+)").unwrap());

/// Build a regex that matches the rendered credit line for any sender name.
pub fn build_credit_regex(template: &str) -> Regex {
    // Escape the template, then turn the escaped `{name}` placeholder into `.+?`.
    let escaped = regex::escape(template.trim());
    let pattern = escaped.replace(r"\{name\}", ".+?");
    Regex::new(&pattern).unwrap_or_else(|_| Regex::new(r"\A\z").unwrap())
}

impl AppState {
    /// Resolve a Slack user id to a display name, calling users.info if needed.
    pub async fn slack_display_name(
        &self,
        user_id: &str,
        hint: Option<&SlackUserProfile>,
    ) -> String {
        if let Some(p) = hint {
            if let Some(n) = p.display_name.clone().filter(|s| !s.is_empty()) {
                return n;
            }
            if let Some(n) = p.real_name.clone().filter(|s| !s.is_empty()) {
                return n;
            }
        }
        match self.lookup_user_profile(user_id).await {
            Some(u) => u
                .profile
                .as_ref()
                .and_then(|p| p.display_name.clone().filter(|s| !s.is_empty()))
                .or_else(|| u.real_name.clone())
                .unwrap_or_else(|| user_id.to_string()),
            None => user_id.to_string(),
        }
    }

    /// Resolve a Slack user's avatar url, preferring the message hint.
    pub async fn slack_avatar(
        &self,
        user_id: &str,
        hint: Option<&SlackUserProfile>,
    ) -> Option<String> {
        if let Some(url) = hint.and_then(icon_url) {
            return Some(url);
        }
        self.lookup_user_profile(user_id)
            .await
            .and_then(|u| u.profile.as_ref().and_then(icon_url))
    }

    async fn lookup_user_profile(&self, user_id: &str) -> Option<SlackUser> {
        let session = self.slack.open_session(&self.slack_bot_token);
        let req = SlackApiUsersInfoRequest::new(user_id.into());
        match session.users_info(&req).await {
            Ok(resp) => Some(resp.user),
            Err(e) => {
                tracing::debug!(user_id, error = ?e, "users.info failed");
                None
            }
        }
    }

    /// Prepare Slack message text for posting into Discord.
    pub async fn slack_text_for_discord(&self, raw: &str) -> String {
        let mut out = raw.to_string();
        for cap in SLACK_USER_MENTION.captures_iter(raw) {
            let name = self.slack_display_name(&cap[1], None).await;
            out = out.replace(&cap[0], &format!("**@{name}**"));
        }
        out = SLACK_CHANNEL_REF.replace_all(&out, "#$1").into_owned();
        for cap in SLACK_TO_DISCORD_TAG.captures_iter(&out.clone()) {
            if let Ok(Some(id)) = self.db.discord_id_for_alias(&cap[1]).await {
                out = out.replace(&cap[0], &format!("<@{id}>"));
            }
        }
        format::slack_to_discord(&out)
    }

    /// Prepare Discord message text for posting into Slack, with the credit line.
    pub async fn discord_text_for_slack(&self, raw: &str, author_name: &str) -> String {
        let mut out = raw.to_string();
        for cap in DISCORD_TO_SLACK_TAG.captures_iter(raw) {
            if let Ok(Some(id)) = self.db.slack_id_for_alias(&cap[1]).await {
                out = out.replace(&cap[0], &format!("<@{id}>"));
            }
        }
        let converted = format::discord_to_slack(&out);
        format!("{converted}{}", self.config.credit_line(author_name))
    }

    /// True if this inbound Slack text was posted by the relay (credit line).
    pub fn is_relay_origin(&self, text: &str) -> bool {
        self.credit_re.is_match(text)
    }

    pub async fn heartbeat(&self) -> Result<()> {
        tokio::fs::write(&self.config.heartbeat_file, b"ok").await?;
        Ok(())
    }
}

/// Pick the largest available icon image url from a profile.
fn icon_url(p: &SlackUserProfile) -> Option<String> {
    let icon = p.icon.as_ref()?;
    if let Some(images) = &icon.images {
        if let Some((_, url)) = images.resolutions.iter().max_by_key(|(res, _)| *res) {
            return Some(url.clone());
        }
    }
    icon.image_original.clone()
}

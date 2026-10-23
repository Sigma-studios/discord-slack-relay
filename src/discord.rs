//! Discord side: the serenity gateway handler (Discord→Slack) plus the
//! channel/webhook provisioning used when the bot joins a Slack channel.

use anyhow::{Context, Result};
use secrecy::ExposeSecret;
use serenity::all::{
    ChannelType, CreateAllowedMentions, CreateAttachment, CreateChannel, CreateThread,
    CreateWebhook, EditChannel, EditWebhookMessage, ExecuteWebhook, GuildId, Webhook,
};
use serenity::async_trait;
use serenity::model::channel::Message;
use serenity::model::id::{ChannelId, MessageId, WebhookId};
use serenity::prelude::{Context as SerenityContext, EventHandler};

use crate::db::ChannelPair;
use crate::media;
use crate::relay::AppState;

/// Create (or find) the mirrored Discord text channel + its webhook for a Slack
/// channel, and persist the pairing. Idempotent.
pub async fn provision_channel(
    state: &AppState,
    slack_channel_id: &str,
    slack_channel_name: &str,
    is_private: bool,
) -> Result<ChannelPair> {
    if let Some(existing) = state.db.pair_by_slack(slack_channel_id).await? {
        return Ok(existing);
    }

    let discord_name = sanitize_channel_name(slack_channel_name);
    let guild: GuildId = state.discord_guild;

    let channel = guild
        .create_channel(
            &state.discord,
            CreateChannel::new(&discord_name).kind(ChannelType::Text),
        )
        .await
        .context("create discord channel")?;

    let webhook: Webhook = channel
        .create_webhook(&state.discord, CreateWebhook::new("slack-relay"))
        .await
        .context("create webhook")?;

    let pair = ChannelPair {
        slack_channel_id: slack_channel_id.to_string(),
        slack_channel_name: slack_channel_name.to_string(),
        discord_channel_id: channel.id.to_string(),
        discord_webhook_id: webhook.id.to_string(),
        discord_webhook_token: webhook
            .token
            .as_ref()
            .map(|t| t.expose_secret().to_string())
            .unwrap_or_default(),
        is_private: is_private as i64,
        archived: 0,
    };
    state.db.upsert_pair(&pair).await?;
    tracing::info!(%slack_channel_id, %discord_name, "provisioned mirror channel");
    Ok(pair)
}

/// Rename the mirrored Discord channel to track a Slack rename.
pub async fn rename_channel(state: &AppState, pair: &ChannelPair, new_slack_name: &str) -> Result<()> {
    let name = sanitize_channel_name(new_slack_name);
    let cid = ChannelId::new(pair.discord_channel_id.parse()?);
    cid.edit(&state.discord, EditChannel::new().name(&name)).await?;
    state.db.rename_pair(&pair.slack_channel_id, new_slack_name).await?;
    Ok(())
}

/// Mark a channel archived on bot removal; prefix its Discord name to signal it.
pub async fn archive_channel(state: &AppState, pair: &ChannelPair) -> Result<()> {
    state.db.set_archived(&pair.slack_channel_id, true).await?;
    let name = sanitize_channel_name(&format!("archived-{}", pair.slack_channel_name));
    let cid = ChannelId::new(pair.discord_channel_id.parse()?);
    // Best effort; ignore failures (e.g. channel already gone).
    let _ = cid.edit(&state.discord, EditChannel::new().name(&name)).await;
    Ok(())
}

/// Post a Slack message into the paired Discord channel via its webhook, using
/// the Slack sender's name and avatar. Returns the created Discord message id.
#[allow(clippy::too_many_arguments)]
pub async fn post_to_discord(
    state: &AppState,
    pair: &ChannelPair,
    sender_name: &str,
    sender_avatar: Option<&str>,
    content: &str,
    thread_id: Option<u64>,
    files: Vec<media::DownloadedFile>,
    ping_everyone: bool,
) -> Result<u64> {
    let webhook = pair_webhook(state, pair).await?;

    // Users may be pinged (alias tags); @everyone only when the Slack message
    // pinged our bot, so a literal "@everyone" from Slack can't mass-ping.
    let mentions = CreateAllowedMentions::new().all_users(true).everyone(ping_everyone);
    let mut builder = ExecuteWebhook::new().username(sender_name).allowed_mentions(mentions);
    // Discord rejects empty content; send a zero-width space if text is empty.
    builder = builder.content(if content.is_empty() { "\u{200b}" } else { content });
    if let Some(avatar) = sender_avatar {
        builder = builder.avatar_url(avatar);
    }
    if let Some(tid) = thread_id {
        builder = builder.in_thread(ChannelId::new(tid));
    }
    for f in files {
        builder = builder.add_file(CreateAttachment::bytes(f.bytes, f.filename));
    }

    let msg = webhook
        .execute(&state.discord, true, builder)
        .await?
        .context("webhook execute returned no message (need wait=true)")?;
    Ok(msg.id.get())
}

async fn pair_webhook(state: &AppState, pair: &ChannelPair) -> Result<Webhook> {
    Ok(Webhook::from_id_with_token(
        &state.discord,
        WebhookId::new(pair.discord_webhook_id.parse::<u64>().context("webhook id")?),
        &pair.discord_webhook_token,
    )
    .await?)
}

/// Edit a webhook message we posted (mirror of a Slack edit).
pub async fn edit_on_discord(
    state: &AppState,
    pair: &ChannelPair,
    message_id: u64,
    thread_id: Option<u64>,
    content: &str,
) -> Result<()> {
    let webhook = pair_webhook(state, pair).await?;
    let mut builder = EditWebhookMessage::new()
        .content(if content.is_empty() { "\u{200b}" } else { content })
        // Edits never re-ping on Discord; keep @everyone inert regardless.
        .allowed_mentions(CreateAllowedMentions::new().all_users(true).everyone(false));
    if let Some(tid) = thread_id {
        builder = builder.in_thread(ChannelId::new(tid));
    }
    webhook
        .edit_message(&state.discord, MessageId::new(message_id), builder)
        .await
        .context("edit webhook message")?;
    Ok(())
}

/// Delete a webhook message we posted (mirror of a Slack delete).
pub async fn delete_on_discord(
    state: &AppState,
    pair: &ChannelPair,
    message_id: u64,
    thread_id: Option<u64>,
) -> Result<()> {
    let webhook = pair_webhook(state, pair).await?;
    webhook
        .delete_message(&state.discord, thread_id.map(ChannelId::new), MessageId::new(message_id))
        .await
        .context("delete webhook message")?;
    Ok(())
}

/// Create a Discord thread hanging off the mirrored parent message, returning
/// the new thread's channel id.
pub async fn create_thread_on(
    state: &AppState,
    pair: &ChannelPair,
    parent_discord_msg_id: u64,
    name: &str,
) -> Result<u64> {
    let cid = ChannelId::new(pair.discord_channel_id.parse()?);
    let thread_name = {
        let n = name.trim();
        if n.len() < 2 { "thread".to_string() } else { n.chars().take(100).collect() }
    };
    let thread = cid
        .create_thread_from_message(
            &state.discord,
            MessageId::new(parent_discord_msg_id),
            CreateThread::new(thread_name),
        )
        .await
        .context("create thread from message")?;
    Ok(thread.id.get())
}

/// Lowercase, hyphenate, strip anything Discord disallows in a text channel name.
pub fn sanitize_channel_name(name: &str) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    let s = if s.is_empty() { "channel".to_string() } else { s };
    s.chars().take(100).collect()
}

/// Gateway handler for Discord→Slack.
pub struct Handler {
    pub state: AppState,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: SerenityContext, msg: Message) {
        if let Err(e) = handle_discord_message(&self.state, &ctx, &msg).await {
            tracing::error!(error = ?e, "discord→slack relay failed");
        }
    }
}

async fn handle_discord_message(
    state: &AppState,
    ctx: &SerenityContext,
    msg: &Message,
) -> Result<()> {
    // Ignore our own webhook posts (loop prevention) and other bots.
    if msg.webhook_id.is_some() || msg.author.bot {
        return Ok(());
    }

    // Alias registration commands (not relayed):
    //   !alias <name>              → map <name> to the author's Discord id
    //   !slackalias <name> <U123>  → map <name> to a Slack user id
    if let Some(rest) = msg.content.strip_prefix("!alias ") {
        let alias = rest.trim();
        if !alias.is_empty() {
            state.db.set_discord_alias(alias, &msg.author.id.to_string()).await?;
            reply(state, msg, &format!("Registered `@d:{alias}` → <@{}>", msg.author.id)).await;
        }
        return Ok(());
    }
    if let Some(rest) = msg.content.strip_prefix("!slackalias ") {
        let mut parts = rest.split_whitespace();
        if let (Some(alias), Some(slack_id)) = (parts.next(), parts.next()) {
            state.db.set_slack_alias(alias, slack_id).await?;
            reply(state, msg, &format!("Registered `@s:{alias}` → Slack `{slack_id}`")).await;
        }
        return Ok(());
    }

    // Resolve the channel: a thread message carries the thread's channel id, so
    // look up the parent to find the mirrored pairing.
    let this_channel = ChannelId::new(msg.channel_id.get());
    tracing::debug!(channel = %this_channel, author = %msg.author.name, "discord message received");
    let (pair, mut slack_thread_ts) = resolve_target(state, ctx, this_channel).await?;
    let Some(pair) = pair else {
        tracing::debug!(channel = %this_channel, "discord channel is not mirrored; ignoring");
        return Ok(());
    };
    if pair.archived != 0 {
        return Ok(());
    }

    // Discord "Reply" to a mirrored message → post into that message's Slack
    // thread (or start one under it).
    if slack_thread_ts.is_none() {
        if let Some(ref_id) = msg.message_reference.as_ref().and_then(|r| r.message_id) {
            slack_thread_ts = state
                .db
                .slack_thread_root_for_discord_msg(&ref_id.to_string())
                .await?;
        }
    }
    let in_discord_thread = this_channel.to_string() != pair.discord_channel_id;

    let author = msg
        .author
        .global_name
        .clone()
        .unwrap_or_else(|| msg.author.name.clone());

    // Attachments: download from the Discord CDN and re-upload to Slack.
    let mut files = Vec::new();
    for att in &msg.attachments {
        match media::download_discord_attachment(&state.http, &att.url, &att.filename).await {
            Ok(f) => files.push(f),
            Err(e) => tracing::warn!(error = ?e, "discord attachment download failed"),
        }
    }

    let text = state.discord_text_for_slack(&msg.content).await;
    if text.trim().is_empty() && files.is_empty() {
        tracing::debug!("discord message has no text or files (sticker/embed); skipping");
        return Ok(());
    }
    let username = state.config.slack_username(&author);
    let slack_ts = crate::slack::post_to_slack(
        state,
        &pair.slack_channel_id,
        &text,
        &username,
        Some(msg.author.face()),
        slack_thread_ts.clone(),
        files,
    )
    .await?;

    tracing::debug!(slack_channel = %pair.slack_channel_id, %slack_ts, "relayed discord message to slack");

    let discord_thread = in_discord_thread.then(|| this_channel.to_string());
    state
        .db
        .map_message(
            &pair.slack_channel_id,
            &slack_ts,
            &msg.id.to_string(),
            "discord",
            slack_thread_ts.as_deref(),
            discord_thread.as_deref(),
        )
        .await?;
    Ok(())
}

/// Best-effort acknowledgement in the same Discord channel.
async fn reply(state: &AppState, msg: &Message, text: &str) {
    let _ = msg.channel_id.say(&state.discord, text).await;
}

/// Given the Discord channel a message arrived in, find the mirrored Slack
/// channel and (if the message is in a thread) the Slack thread_ts to reply to.
async fn resolve_target(
    state: &AppState,
    ctx: &SerenityContext,
    channel: ChannelId,
) -> Result<(Option<ChannelPair>, Option<String>)> {
    // Direct hit: a top-level message in a mirrored text channel.
    if let Some(pair) = state.db.pair_by_discord(&channel.to_string()).await? {
        return Ok((Some(pair), None));
    }

    // Otherwise it may be a thread; fetch the channel to read its parent.
    let gc = match channel.to_channel(&ctx.http).await {
        Ok(serenity::model::channel::Channel::Guild(gc)) => gc,
        _ => return Ok((None, None)),
    };
    let is_thread = matches!(
        gc.kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    );
    if !is_thread {
        return Ok((None, None));
    }
    let Some(parent_id) = gc.parent_id else {
        return Ok((None, None));
    };
    let Some(pair) = state.db.pair_by_discord(&parent_id.to_string()).await? else {
        return Ok((None, None));
    };

    // Known thread mapping (created from a mirrored Slack thread)?
    if let Some((_, thread_ts)) = state.db.slack_thread_for_discord(&channel.to_string()).await? {
        return Ok((Some(pair), Some(thread_ts)));
    }

    // Thread created natively in Discord: anchor it to the Slack message that
    // mirrors the thread's starter (the thread id equals its starter message id).
    if let Some(ts) = state.db.slack_ts_for_discord_id(&channel.to_string()).await? {
        state
            .db
            .map_thread(&pair.slack_channel_id, &ts, &channel.to_string())
            .await?;
        return Ok((Some(pair), Some(ts)));
    }

    // Unknown thread: fall back to posting top-level in the channel.
    Ok((Some(pair), None))
}

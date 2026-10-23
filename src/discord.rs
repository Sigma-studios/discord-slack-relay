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
use serenity::model::event::MessageUpdateEvent;
use serenity::model::gateway::Ready;
use serenity::model::user::User;
use serenity::model::id::{ChannelId, MessageId, WebhookId};
use serenity::prelude::{Context as SerenityContext, EventHandler};

use once_cell::sync::Lazy;
use regex::Regex;

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
    files: &[media::DownloadedFile],
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
        builder = builder.add_file(CreateAttachment::bytes(f.bytes.clone(), f.filename.clone()));
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

/// Set the Discord channel topic (Discord caps topics at 1024 chars).
pub async fn set_topic(state: &AppState, pair: &ChannelPair, topic: &str) -> Result<()> {
    let topic: String = topic.chars().take(1024).collect();
    let cid = ChannelId::new(pair.discord_channel_id.parse()?);
    cid.edit(&state.discord, EditChannel::new().topic(topic))
        .await
        .context("set channel topic")?;
    Ok(())
}

/// Pin or unpin a Discord message (in its thread when it lives in one).
pub async fn set_pinned(
    state: &AppState,
    pair: &ChannelPair,
    message_id: u64,
    thread_id: Option<u64>,
    pinned: bool,
) -> Result<()> {
    let cid = match thread_id {
        Some(t) => ChannelId::new(t),
        None => ChannelId::new(pair.discord_channel_id.parse()?),
    };
    let mid = MessageId::new(message_id);
    if pinned {
        cid.pin(&state.discord, mid).await.context("pin message")?;
    } else {
        cid.unpin(&state.discord, mid).await.context("unpin message")?;
    }
    Ok(())
}

/// True when Discord says the mirrored channel or its webhook no longer exists
/// (someone deleted the channel), so the pairing must be recreated.
pub fn is_gone(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<serenity::Error>(),
            Some(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)))
                if matches!(r.error.code, 10003 | 10015)
        )
    })
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
    async fn ready(&self, _ctx: SerenityContext, ready: Ready) {
        tracing::info!(bot = %ready.user.name, guilds = ready.guilds.len(), "discord gateway connected");
    }

    async fn message(&self, ctx: SerenityContext, msg: Message) {
        if let Err(e) = handle_discord_message(&self.state, &ctx, &msg).await {
            tracing::error!(error = ?e, "discord→slack relay failed");
        }
    }

    async fn message_update(
        &self,
        ctx: SerenityContext,
        _old: Option<Message>,
        _new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        if let Err(e) = handle_discord_edit(&self.state, &ctx, &event).await {
            tracing::error!(error = ?e, "discord→slack edit failed");
        }
    }

    async fn message_delete(
        &self,
        _ctx: SerenityContext,
        _channel_id: ChannelId,
        deleted_message_id: MessageId,
        _guild_id: Option<GuildId>,
    ) {
        if let Err(e) = handle_discord_delete(&self.state, deleted_message_id).await {
            tracing::error!(error = ?e, "discord→slack delete failed");
        }
    }
}

/// A Discord message we relayed was edited → update the Slack copy.
async fn handle_discord_edit(
    state: &AppState,
    ctx: &SerenityContext,
    ev: &MessageUpdateEvent,
) -> Result<()> {
    // No content means an embed/preview-only update.
    let Some(content) = ev.content.as_ref() else { return Ok(()) };
    if ev.author.as_ref().is_some_and(|a| a.bot) {
        return Ok(());
    }
    let Some((slack_channel, ts, origin)) =
        state.db.slack_ref_for_discord_msg(&ev.id.to_string()).await?
    else {
        return Ok(());
    };
    // Only our bot's own Slack posts are editable; file shares have no ts.
    if origin != "discord" || ts.is_empty() {
        return Ok(());
    }
    let mentions = ev.mentions.clone().unwrap_or_default();
    let converted = discord_mentions_to_slack(state, ctx, content, &mentions).await;
    let text = state.discord_text_for_slack(&converted).await;
    crate::slack::update_on_slack(state, &slack_channel, &ts, &text).await?;
    tracing::debug!(%slack_channel, %ts, "mirrored discord edit to slack");
    Ok(())
}

/// A Discord message we relayed was deleted → delete the Slack copy.
async fn handle_discord_delete(state: &AppState, message_id: MessageId) -> Result<()> {
    let id = message_id.to_string();
    let Some((slack_channel, ts, origin)) = state.db.slack_ref_for_discord_msg(&id).await? else {
        return Ok(());
    };
    if origin != "discord" || ts.is_empty() {
        return Ok(());
    }
    crate::slack::delete_on_slack(state, &slack_channel, &ts).await?;
    state.db.delete_mapping_by_discord(&id).await?;
    tracing::debug!(%slack_channel, %ts, "mirrored discord delete to slack");
    Ok(())
}

static D_USER_MENTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@!?(\d+)>").unwrap());
static D_ROLE_MENTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@&(\d+)>").unwrap());
static D_CHANNEL_MENTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"<#(\d+)>").unwrap());

/// Rewrite Discord user/role/channel mentions (raw ids) into something Slack
/// shows meaningfully: `@Name`, `@role`, and `<#C…>` for mirrored channels.
async fn discord_mentions_to_slack(
    state: &AppState,
    ctx: &SerenityContext,
    content: &str,
    mentions: &[User],
) -> String {
    let mut out = content.to_string();

    for cap in D_USER_MENTION.captures_iter(content) {
        let name = mentions
            .iter()
            .find(|u| u.id.to_string() == cap[1])
            .map(|u| u.global_name.clone().unwrap_or_else(|| u.name.clone()))
            .unwrap_or_else(|| "user".to_string());
        out = out.replace(&cap[0], &format!("@{name}"));
    }

    if D_ROLE_MENTION.is_match(content) {
        let roles = state.discord_guild.roles(&ctx.http).await.unwrap_or_default();
        for cap in D_ROLE_MENTION.captures_iter(content) {
            let name = cap[1]
                .parse::<u64>()
                .ok()
                .and_then(|id| roles.get(&serenity::model::id::RoleId::new(id)))
                .map(|r| r.name.clone())
                .unwrap_or_else(|| "role".to_string());
            out = out.replace(&cap[0], &format!("@{name}"));
        }
    }

    for cap in D_CHANNEL_MENTION.captures_iter(content) {
        let replacement = match state.db.pair_by_discord(&cap[1]).await {
            Ok(Some(pair)) => format!("<#{}>", pair.slack_channel_id),
            _ => {
                let name = match cap[1].parse::<u64>() {
                    Ok(id) => match ChannelId::new(id).to_channel(&ctx.http).await {
                        Ok(serenity::model::channel::Channel::Guild(gc)) => gc.name,
                        _ => "channel".to_string(),
                    },
                    Err(_) => "channel".to_string(),
                };
                format!("#{name}")
            }
        };
        out = out.replace(&cap[0], &replacement);
    }
    out
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

    // Attachments: download from the Discord CDN and re-upload to Slack. Too
    // large (or failed) ones are posted as a link line instead.
    let mut files = Vec::new();
    let mut notices = Vec::new();
    for att in &msg.attachments {
        if u64::from(att.size) > media::INLINE_LIMIT {
            notices.push(format!(
                "📎 <{}|{}> (too large to mirror, {})",
                att.url, att.filename, media::human_size(Some(att.size.into()))
            ));
            continue;
        }
        match media::download_discord_attachment(&state.http, &att.url, &att.filename).await {
            Ok(media::Download::File(f)) => files.push(f),
            Ok(media::Download::TooLarge(size)) => notices.push(format!(
                "📎 <{}|{}> (too large to mirror, {})",
                att.url, att.filename, media::human_size(size)
            )),
            Err(e) => {
                tracing::warn!(error = ?e, "discord attachment download failed");
                notices.push(format!("📎 <{}|{}> (couldn't be mirrored)", att.url, att.filename));
            }
        }
    }

    let converted = discord_mentions_to_slack(state, ctx, &msg.content, &msg.mentions).await;
    let mut text = state.discord_text_for_slack(&converted).await;
    // Stickers have no Slack equivalent: name them.
    for sticker in &msg.sticker_items {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[sticker: {}]", sticker.name));
    }
    // Embed-only messages (no text): post the embed's link or title.
    if text.trim().is_empty() {
        for embed in &msg.embeds {
            if let Some(line) = embed.url.clone().or_else(|| embed.title.clone()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&line);
            }
        }
    }
    for notice in notices {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&notice);
    }
    if text.trim().is_empty() && files.is_empty() {
        tracing::debug!("discord message has nothing to relay; skipping");
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

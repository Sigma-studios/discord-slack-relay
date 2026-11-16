//! One-off repair, run in the container as `relay backfill-uploads [days]`:
//! link Discord uploads whose Slack copy was never mapped (before the relay
//! recognised its own upload events), so Slack reactions, edits and deletes
//! reach them. Each is matched to its Discord message by channel, time and
//! having attachments; its caption gets the sender's name; its reactions are
//! synced. Safe to run again: linked messages are skipped.

use anyhow::Result;
use serenity::builder::GetMessages;
use serenity::model::channel::Message;
use serenity::model::id::{ChannelId, MessageId};
use slack_morphism::prelude::*;

use crate::db::ChannelPair;
use crate::relay::AppState;

/// How long before its Slack copy a Discord upload may have been posted
/// (download from Discord + upload to Slack).
const MAX_DELAY_SECS: f64 = 300.0;

pub async fn backfill_uploads(state: &AppState, days: u64) -> Result<()> {
    let oldest = now_secs() - days as f64 * 86_400.0;
    let (mut linked, mut unmatched) = (0, 0);
    for pair in state.db.all_pairs().await? {
        if pair.archived != 0 {
            continue;
        }
        for msg in unlinked_uploads(state, &pair.slack_channel_id, oldest).await? {
            match link(state, &pair, &msg).await {
                Ok(true) => linked += 1,
                Ok(false) => unmatched += 1,
                Err(e) => {
                    unmatched += 1;
                    tracing::warn!(channel = %pair.slack_channel_id, ts = %msg.origin.ts, error = ?e, "backfill failed");
                }
            }
        }
    }
    tracing::info!(linked, unmatched, days, "upload backfill done");
    Ok(())
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

fn ts_secs(ts: &str) -> f64 {
    ts.parse().unwrap_or_default()
}

/// Our bot's Slack messages carrying files, top level and in threads, that
/// aren't mapped to a Discord message; oldest first.
async fn unlinked_uploads(state: &AppState, channel: &str, oldest: f64) -> Result<Vec<SlackHistoryMessage>> {
    let session = state.slack.open_session(&state.slack_bot_token);
    let oldest_ts: SlackTs = format!("{oldest:.6}").into();
    let mut all = Vec::new();
    let mut cursor = None;
    loop {
        let mut req = SlackApiConversationsHistoryRequest::new()
            .with_channel(channel.into())
            .with_oldest(oldest_ts.clone())
            .with_limit(200);
        if let Some(c) = cursor.take() {
            req = req.with_cursor(c);
        }
        let resp = session.conversations_history(&req).await?;
        for m in resp.messages {
            if m.parent.reply_count.unwrap_or(0) > 0 {
                let replies = SlackApiConversationsRepliesRequest::new(channel.into(), m.origin.ts.clone())
                    .with_limit(200);
                let r = session.conversations_replies(&replies).await?;
                all.extend(r.messages.into_iter().filter(|r| r.origin.ts != m.origin.ts));
            }
            all.push(m);
        }
        cursor = resp.response_metadata.and_then(|m| m.next_cursor).filter(|c| !c.to_string().is_empty());
        if cursor.is_none() {
            break;
        }
    }
    let mut out = Vec::new();
    for m in all {
        let ours = m.sender.bot_id.as_ref().map(|b| b.to_string()) == Some(state.bot_id.clone());
        let has_files = m.content.files.as_ref().is_some_and(|f| !f.is_empty());
        let ts = m.origin.ts.to_string();
        if ours && has_files && state.db.message_by_slack_ts(channel, &ts).await?.is_none() {
            out.push(m);
        }
    }
    out.sort_by(|a, b| ts_secs(a.origin.ts.as_ref()).total_cmp(&ts_secs(b.origin.ts.as_ref())));
    Ok(out)
}

/// Discord snowflake for a unix time (seconds).
fn snowflake(secs: f64) -> u64 {
    let ms = (secs * 1000.0) as u64;
    ms.saturating_sub(1_420_070_400_000) << 22
}

fn has_attachments(m: &Message) -> bool {
    !m.attachments.is_empty() || m.message_snapshots.iter().any(|s| !s.attachments.is_empty())
}

/// Find the Discord message a Slack upload mirrors and link them. `false`
/// when no Discord message fits.
async fn link(state: &AppState, pair: &ChannelPair, msg: &SlackHistoryMessage) -> Result<bool> {
    let channel = &pair.slack_channel_id;
    let ts = msg.origin.ts.to_string();
    let at = ts_secs(&ts);
    let thread_ts = msg.origin.thread_ts.as_ref().map(|t| t.to_string()).filter(|t| *t != ts);

    // A Slack thread reply was posted in a Discord thread or as a Discord reply.
    let mut channels = Vec::new();
    if let Some(tts) = &thread_ts {
        if let Some(t) = state.db.discord_thread_for(channel, tts).await? {
            channels.push(t);
        }
    }
    channels.push(pair.discord_channel_id.clone());

    let mut best: Option<Message> = None;
    for ch in &channels {
        let ch_id = ChannelId::new(ch.parse()?);
        let after = MessageId::new(snowflake(at - MAX_DELAY_SECS).max(1));
        let found = ch_id.messages(&state.discord, GetMessages::new().after(after).limit(100)).await?;
        for m in found {
            let posted = m.timestamp.unix_timestamp() as f64;
            let fits = !m.author.bot
                && m.webhook_id.is_none()
                && has_attachments(&m)
                && posted <= at + 1.0
                && at - posted <= MAX_DELAY_SECS;
            if !fits || state.db.slack_ref_for_discord_msg(&m.id.to_string()).await?.is_some() {
                continue;
            }
            if best.as_ref().map_or(true, |b| m.timestamp.unix_timestamp() > b.timestamp.unix_timestamp()) {
                best = Some(m);
            }
        }
        if best.is_some() {
            break;
        }
    }
    let Some(d) = best else {
        tracing::info!(%channel, %ts, "no discord message found for slack upload");
        return Ok(false);
    };

    let in_thread = d.channel_id.to_string() != pair.discord_channel_id;
    let discord_thread = in_thread.then(|| d.channel_id.to_string());
    state
        .db
        .map_message(channel, &ts, &d.id.to_string(), "discord", thread_ts.as_deref(), discord_thread.as_deref())
        .await?;
    let file_ids: Vec<String> = msg
        .content
        .files
        .iter()
        .flatten()
        .map(|f| f.id.to_string())
        .collect();
    state.db.set_slack_file_ids(channel, &ts, &file_ids).await?;

    // Add the sender's name to the caption.
    let author = d.author.global_name.clone().unwrap_or_else(|| d.author.name.clone());
    let username = state.config.slack_username(&author);
    let text = msg.content.text.clone().unwrap_or_default();
    let named = crate::slack::upload_caption(&username, "");
    if !text.starts_with(&named) {
        let caption = crate::slack::upload_caption(&username, &text);
        crate::slack::update_on_slack(state, channel, &ts, &caption).await?;
    }

    crate::reactions::sync_from_slack(state, channel, &ts).await?;
    tracing::info!(%channel, %ts, discord_msg = %d.id, %author, "linked old upload");
    Ok(true)
}

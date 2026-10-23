//! Slack side: Socket Mode listener (Slack→Discord + membership events) and
//! the user-token poster (Discord→Slack).

use std::sync::Arc;

use anyhow::{Context, Result};
use slack_morphism::prelude::*;

use crate::media::{self, DownloadedFile};
use crate::relay::AppState;

type SlackHyperConnector = slack_morphism::prelude::SlackClientHyperHttpsConnector;

/// Post a relayed Discord message into Slack as the bot, shown under the
/// Discord sender's name and avatar (`chat:write.customize`). `thread_ts` set →
/// threaded reply. `files` non-empty → external upload flow with the text as
/// the initial comment. Returns the Slack ts (empty for uploads).
pub async fn post_to_slack(
    state: &AppState,
    slack_channel_id: &str,
    text: &str,
    username: &str,
    icon_url: Option<String>,
    thread_ts: Option<String>,
    files: Vec<DownloadedFile>,
) -> Result<String> {
    let session = state.slack.open_session(&state.slack_bot_token);

    if files.is_empty() {
        let content = SlackMessageContent::new().with_text(text.to_string());
        let mut req = SlackApiChatPostMessageRequest::new(slack_channel_id.into(), content)
            .with_username(username.to_string());
        if let Some(url) = icon_url {
            req = req.with_icon_url(url);
        }
        if let Some(tts) = thread_ts {
            req = req.with_thread_ts(tts.into());
        }
        let resp = session
            .chat_post_message(&req)
            .await
            .context("chat.postMessage")?;
        let ts = resp.ts.to_string();
        state.db.mark_self_posted(&ts).await?;
        return Ok(ts);
    }

    // External upload flow: get url → PUT bytes → complete (shares to channel).
    let mut completed = Vec::new();
    for f in files {
        let get = SlackApiFilesGetUploadUrlExternalRequest::new(f.filename.clone(), f.bytes.len());
        let got = session
            .get_upload_url_external(&get)
            .await
            .context("files.getUploadURLExternal")?;
        let via = SlackApiFilesUploadViaUrlRequest::new(
            got.upload_url,
            f.bytes,
            f.mimetype.unwrap_or_else(|| "application/octet-stream".to_string()),
        );
        session
            .files_upload_via_url(&via)
            .await
            .context("files upload via url")?;
        completed.push(SlackApiFilesComplete::new(got.file_id));
    }

    let mut complete = SlackApiFilesCompleteUploadExternalRequest::new(completed)
        .with_channel_id(slack_channel_id.into())
        .with_initial_comment(text.to_string())
        .with_username(username.to_string());
    if let Some(tts) = thread_ts {
        complete = complete.with_thread_ts(tts.into());
    }
    session
        .files_complete_upload_external(&complete)
        .await
        .context("files.completeUploadExternal")?;

    // The share message's ts isn't returned; its echo is dropped by bot_id.
    Ok(String::new())
}

/// Mirror an inbound Slack message into Discord.
pub async fn handle_slack_message(state: &AppState, ev: &SlackMessageEvent) -> Result<()> {
    let Some(channel) = ev.origin.channel.as_ref() else {
        return Ok(());
    };
    let channel_id = channel.to_string();
    let ts = ev.origin.ts.to_string();

    // Mirror plain messages, file shares and thread broadcasts; route edits and
    // deletes; skip joins and other system subtypes.
    match &ev.subtype {
        None
        | Some(SlackMessageEventType::FileShare)
        | Some(SlackMessageEventType::ThreadBroadcast) => {}
        Some(SlackMessageEventType::MessageChanged) => {
            return handle_slack_edit(state, &channel_id, ev).await;
        }
        Some(SlackMessageEventType::MessageDeleted) => {
            return handle_slack_delete(state, &channel_id, ev).await;
        }
        Some(_) => return Ok(()),
    }

    // Loop prevention: anything our own bot posted (relayed Discord messages).
    if is_our_bot(state, &ev.sender) || state.db.is_self_posted(&ts).await? {
        return Ok(());
    }

    let content = ev.content.as_ref();
    let text = content.and_then(|c| c.text.clone()).unwrap_or_default();

    // We only receive messages for channels the bot is in, so an unpaired
    // channel means a missed join event: provision it now.
    let pair = match state.db.pair_by_slack(&channel_id).await? {
        Some(p) => p,
        None => {
            handle_bot_membership(state, &channel_id, true).await?;
            match state.db.pair_by_slack(&channel_id).await? {
                Some(p) => p,
                None => return Ok(()),
            }
        }
    };
    if pair.archived != 0 {
        return Ok(());
    }

    let user_id = ev.sender.user.as_ref().map(|u| u.to_string()).unwrap_or_default();
    let profile = ev.sender.user_profile.as_ref();
    let sender_name = state.slack_display_name(&user_id, profile).await;
    let sender_avatar = state.slack_avatar(&user_id, profile).await;
    let (discord_text, ping_everyone) = state.slack_text_for_discord(&text).await;

    // Files: download with the bot token, keep only those small enough inline.
    let mut files = Vec::new();
    if let Some(fs) = content.and_then(|c| c.files.as_ref()) {
        for sf in fs {
            let Some(url) = sf.url_private.as_ref() else { continue };
            let name = sf.name.clone().unwrap_or_else(|| "file".to_string());
            let token = token_value(&state.slack_bot_token);
            match media::download_slack_file(&state.http, url.as_str(), &name, &token).await {
                Ok(f) if (f.bytes.len() as u64) <= media::INLINE_LIMIT => files.push(f),
                Ok(_) => tracing::info!("slack file over inline limit; skipping upload"),
                Err(e) => tracing::warn!(error = ?e, "slack file download failed"),
            }
        }
    }

    // Thread handling: a reply (thread_ts present and != ts) goes into a Discord
    // thread hanging off the mirrored parent message.
    let slack_thread_ts = ev
        .origin
        .thread_ts
        .as_ref()
        .map(|t| t.to_string())
        .filter(|t| *t != ts);
    let discord_thread_id = match &slack_thread_ts {
        Some(thread_ts) => Some(resolve_discord_thread(state, &pair, &channel_id, thread_ts).await?),
        None => None,
    };

    let discord_msg_id = crate::discord::post_to_discord(
        state,
        &pair,
        &sender_name,
        sender_avatar.as_deref(),
        &discord_text,
        discord_thread_id,
        files,
        ping_everyone,
    )
    .await?;

    let discord_thread = discord_thread_id.map(|t| t.to_string());
    state
        .db
        .map_message(
            &channel_id,
            &ts,
            &discord_msg_id.to_string(),
            "slack",
            slack_thread_ts.as_deref(),
            discord_thread.as_deref(),
        )
        .await?;
    Ok(())
}

fn is_our_bot(state: &AppState, sender: &SlackMessageSender) -> bool {
    sender.bot_id.as_ref().map(|b| b.to_string()) == Some(state.bot_id.clone())
        || sender.user.as_ref().map(|u| u.to_string()) == Some(state.bot_user_id.clone())
}

/// A Slack message was edited → edit its Discord mirror. Only messages that
/// originated in Slack (our webhook posts) are editable on Discord.
async fn handle_slack_edit(state: &AppState, channel_id: &str, ev: &SlackMessageEvent) -> Result<()> {
    let Some(edited) = ev.message.as_ref() else { return Ok(()) };
    if is_our_bot(state, &edited.sender) {
        return Ok(());
    }
    let text = edited.content.as_ref().and_then(|c| c.text.clone()).unwrap_or_default();
    // message_changed also fires for link unfurls etc.; skip if the text is unchanged.
    let prev_text = ev
        .previous_message
        .as_ref()
        .and_then(|p| p.content.as_ref())
        .and_then(|c| c.text.clone());
    if prev_text.as_deref() == Some(text.as_str()) {
        return Ok(());
    }

    let ts = edited.ts.to_string();
    let Some(row) = state.db.message_by_slack_ts(channel_id, &ts).await? else {
        return Ok(());
    };
    if row.origin != "slack" {
        return Ok(());
    }
    let Some(pair) = state.db.pair_by_slack(channel_id).await? else { return Ok(()) };

    let (content, _) = state.slack_text_for_discord(&text).await;
    let thread_id = row.discord_thread_id.as_deref().map(str::parse).transpose()?;
    crate::discord::edit_on_discord(state, &pair, row.discord_msg_id.parse()?, thread_id, &content)
        .await?;
    tracing::debug!(%channel_id, %ts, "mirrored slack edit to discord");
    Ok(())
}

/// A Slack message was deleted → delete its Discord mirror.
async fn handle_slack_delete(state: &AppState, channel_id: &str, ev: &SlackMessageEvent) -> Result<()> {
    let Some(ts) = ev
        .deleted_ts
        .as_ref()
        .or(ev.previous_message.as_ref().map(|p| &p.ts))
        .map(|t| t.to_string())
    else {
        return Ok(());
    };
    let Some(row) = state.db.message_by_slack_ts(channel_id, &ts).await? else {
        return Ok(());
    };
    if row.origin != "slack" {
        return Ok(());
    }
    let Some(pair) = state.db.pair_by_slack(channel_id).await? else { return Ok(()) };

    let thread_id = row.discord_thread_id.as_deref().map(str::parse).transpose()?;
    crate::discord::delete_on_discord(state, &pair, row.discord_msg_id.parse()?, thread_id).await?;
    state.db.delete_message_mapping(channel_id, &ts).await?;
    tracing::debug!(%channel_id, %ts, "mirrored slack delete to discord");
    Ok(())
}

/// Find or create the Discord thread for a Slack thread_ts.
async fn resolve_discord_thread(
    state: &AppState,
    pair: &crate::db::ChannelPair,
    channel_id: &str,
    thread_ts: &str,
) -> Result<u64> {
    if let Some(tid) = state.db.discord_thread_for(channel_id, thread_ts).await? {
        return Ok(tid.parse()?);
    }
    // Need the Discord message mirroring the Slack thread parent to hang off.
    let parent_discord_id = state
        .db
        .discord_id_for_slack_ts(channel_id, thread_ts)
        .await?
        .context("thread parent not yet mirrored")?;
    let tid = crate::discord::create_thread_on(
        state,
        pair,
        parent_discord_id.parse()?,
        "thread",
    )
    .await?;
    state.db.map_thread(channel_id, thread_ts, &tid.to_string()).await?;
    Ok(tid)
}

/// The bot was added to / removed from a Slack channel → provision / archive.
pub async fn handle_bot_membership(state: &AppState, channel_id: &str, joined: bool) -> Result<()> {
    if joined {
        let session = state.slack.open_session(&state.slack_bot_token);
        let req = SlackApiConversationsInfoRequest::new(channel_id.into());
        let info = session.conversations_info(&req).await.context("conversations.info")?;
        let name = info.channel.name.unwrap_or_else(|| channel_id.to_string());
        let is_private = info.channel.flags.is_private.unwrap_or(false);
        crate::discord::provision_channel(state, channel_id, &name, is_private).await?;
    } else if let Some(pair) = state.db.pair_by_slack(channel_id).await? {
        crate::discord::archive_channel(state, &pair).await?;
    }
    Ok(())
}

/// Provision Discord mirrors for every Slack channel the bot is already in.
/// Covers invites that happened while the relay wasn't running (no
/// member_joined_channel event reached us), and resumes archived pairs.
pub async fn reconcile_memberships(state: &AppState) -> Result<()> {
    let session = state.slack.open_session(&state.slack_bot_token);
    let mut cursor: Option<SlackCursorId> = None;
    let mut provisioned = 0usize;
    loop {
        let mut req = SlackApiUsersConversationsRequest::new()
            .with_types(vec![SlackConversationType::Public, SlackConversationType::Private])
            .with_exclude_archived(true)
            .with_limit(200);
        if let Some(c) = cursor.take() {
            req = req.with_cursor(c);
        }
        let resp = session
            .users_conversations(&req)
            .await
            .context("users.conversations")?;
        for ch in resp.channels {
            let id = ch.id.to_string();
            match state.db.pair_by_slack(&id).await? {
                Some(pair) if pair.archived != 0 => state.db.set_archived(&id, false).await?,
                Some(_) => {}
                None => {
                    let name = ch.name.clone().unwrap_or_else(|| id.clone());
                    let is_private = ch.flags.is_private.unwrap_or(false);
                    match crate::discord::provision_channel(state, &id, &name, is_private).await {
                        Ok(_) => provisioned += 1,
                        Err(e) => tracing::warn!(channel = %id, error = ?e, "startup provisioning failed"),
                    }
                }
            }
        }
        match resp.response_metadata.and_then(|m| m.next_cursor) {
            Some(c) if !c.to_string().is_empty() => cursor = Some(c),
            _ => break,
        }
    }
    tracing::info!(provisioned, "reconciled slack channel memberships");
    Ok(())
}

fn token_value(token: &SlackApiToken) -> String {
    token.token_value.value().clone()
}

/// Connect the Socket Mode listener and run until the process is stopped.
pub async fn run_socket_mode(state: AppState) -> Result<()> {
    let app_token = SlackApiToken::new(state.config.slack_app_token.clone().into());

    let listener_environment = Arc::new(
        SlackClientEventsListenerEnvironment::new(state.slack.clone())
            .with_user_state(state.clone()),
    );

    let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(on_push_event);

    let listener = SlackClientSocketModeListener::new(
        &SlackClientSocketModeConfig::new(),
        listener_environment,
        callbacks,
    );

    listener.listen_for(&app_token).await?;
    listener.serve().await;
    Ok(())
}

/// Dispatch a Socket Mode push event to the right handler.
async fn on_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackClient<SlackHyperConnector>>,
    states: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = {
        let guard = states.read().await;
        guard
            .get_user_state::<AppState>()
            .cloned()
            .ok_or("AppState missing from listener user state")?
    };

    let result = match event.event {
        SlackEventCallbackBody::Message(ev) => handle_slack_message(&state, &ev).await,
        SlackEventCallbackBody::MemberJoinedChannel(ev) => {
            if ev.user.to_string() == state.bot_user_id {
                handle_bot_membership(&state, ev.channel.as_ref(), true).await
            } else {
                Ok(())
            }
        }
        SlackEventCallbackBody::MemberLeftChannel(ev) => {
            if ev.user.to_string() == state.bot_user_id {
                handle_bot_membership(&state, ev.channel.as_ref(), false).await
            } else {
                Ok(())
            }
        }
        SlackEventCallbackBody::ChannelArchive(ev) => {
            set_archived(&state, ev.channel.as_ref(), true).await
        }
        SlackEventCallbackBody::ChannelUnarchive(ev) => {
            set_archived(&state, ev.channel.as_ref(), false).await
        }
        SlackEventCallbackBody::ChannelRename(ev) => {
            if let (Some(pair), Some(name)) = (
                state.db.pair_by_slack(ev.channel.id.as_ref()).await?,
                ev.channel.name.clone(),
            ) {
                crate::discord::rename_channel(&state, &pair, &name).await
            } else {
                Ok(())
            }
        }
        other => {
            tracing::trace!(?other, "unhandled slack event");
            Ok(())
        }
    };

    if let Err(e) = result {
        tracing::error!(error = ?e, "slack event handler failed");
    }
    Ok(())
}

async fn set_archived(state: &AppState, channel_id: &str, archived: bool) -> Result<()> {
    state.db.set_archived(channel_id, archived).await?;
    Ok(())
}

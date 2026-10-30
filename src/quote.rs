//! Slack messages quoted inside another message: a pasted permalink (Slack
//! unfurls it with the author and text) or a message shared with "Forward".
//!
//! Slack attaches the quoted content to the message even when our bot isn't in
//! the source channel, so Discord can show who said what instead of a Slack
//! link nobody there can open. slack-morphism's attachment model drops the
//! author and origin fields, so the message is re-read from the raw Web API.
//!
//! The same read finds pictures that only exist as image blocks, such as GIFs
//! from Slack's picker: they have no text and no file to mirror.

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use slack_morphism::prelude::*;

use crate::relay::AppState;

/// One quoted message, as Slack's unfurl/share attachment describes it.
#[derive(Debug, Default, Deserialize)]
pub struct Quoted {
    #[serde(default)]
    is_msg_unfurl: bool,
    #[serde(default)]
    is_share: bool,
    author_id: Option<String>,
    author_name: Option<String>,
    author_subname: Option<String>,
    channel_id: Option<String>,
    channel_name: Option<String>,
    // A string for quoted messages, a number on some other attachments.
    ts: Option<serde_json::Value>,
    text: Option<String>,
    from_url: Option<String>,
    original_url: Option<String>,
    footer: Option<String>,
    #[serde(default)]
    files: Vec<QuotedFile>,
}

#[derive(Debug, Default, Deserialize)]
struct QuotedFile {
    name: Option<String>,
}

/// What a message carries outside its text and files.
#[derive(Debug, Default)]
pub struct Embeds {
    pub quotes: Vec<Quoted>,
    /// Picture urls, public: Discord previews them when posted as plain links.
    pub images: Vec<String>,
}

/// The messages quoted and pictures embedded by Slack message `ts` (none if it
/// has no attachments or image blocks). Best effort: a failed lookup just
/// leaves the plain text.
pub async fn embeds_in(
    state: &AppState,
    channel_id: &str,
    ts: &str,
    content: Option<&SlackMessageContent>,
) -> Embeds {
    let has_attachments = content.and_then(|c| c.attachments.as_ref()).is_some_and(|a| !a.is_empty());
    let has_image_block = content
        .and_then(|c| c.blocks.as_ref())
        .is_some_and(|b| b.iter().any(|b| matches!(b, SlackBlock::Image(_))));
    if !has_attachments && !has_image_block {
        return Embeds::default();
    }
    match fetch_message(state, channel_id, ts).await {
        Ok(msg) => {
            let attachments = msg["attachments"].as_array().map(Vec::as_slice).unwrap_or_default();
            for a in attachments {
                // Field names and block types only: Slack's quote format is undocumented.
                let keys: Vec<&str> = a.as_object().into_iter().flat_map(|o| o.keys()).map(String::as_str).collect();
                let blocks: Vec<&str> = blocks_of(a).filter_map(|b| b["type"].as_str()).collect();
                tracing::debug!(?keys, ?blocks, "slack attachment");
            }
            let quotes = attachments
                .iter()
                .filter_map(|a| serde_json::from_value::<Quoted>(a.clone()).ok())
                .filter(|q| q.is_msg_unfurl || q.is_share)
                .collect();
            Embeds { quotes, images: images_in(&msg) }
        }
        Err(e) => {
            tracing::warn!(%channel_id, %ts, error = ?e, "couldn't read embeds of a slack message");
            Embeds::default()
        }
    }
}

fn blocks_of(holder: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    holder["blocks"].as_array().into_iter().flatten()
}

fn image_blocks(holder: &serde_json::Value) -> Vec<String> {
    blocks_of(holder)
        .filter(|b| b["type"] == "image")
        .filter_map(|b| b["image_url"].as_str())
        .map(str::to_string)
        .collect()
}

/// Urls of the pictures a raw message embeds. Quotes are rendered on their own,
/// and link previews come back by themselves when Discord sees the link.
fn images_in(msg: &serde_json::Value) -> Vec<String> {
    let mut images = image_blocks(msg);
    for a in msg["attachments"].as_array().into_iter().flatten() {
        let quoted = a["is_msg_unfurl"] == true || a["is_share"] == true;
        let preview = a["from_url"].is_string() || a["original_url"].is_string();
        if quoted || preview {
            continue;
        }
        images.extend(image_blocks(a));
        images.extend(a["image_url"].as_str().map(str::to_string));
    }
    let mut seen = std::collections::HashSet::new();
    images.retain(|url| seen.insert(url.clone()));
    images
}

/// One raw message. `conversations.replies` finds it whether it's top-level or
/// a thread reply (the parent comes along; filtered by ts).
async fn fetch_message(state: &AppState, channel_id: &str, ts: &str) -> Result<serde_json::Value> {
    let token = state.slack_bot_token.token_value.value().clone();
    let resp: serde_json::Value = state
        .http
        .get("https://slack.com/api/conversations.replies")
        .bearer_auth(token)
        .query(&[
            ("channel", channel_id),
            ("ts", ts),
            ("oldest", ts),
            ("latest", ts),
            ("inclusive", "true"),
            ("limit", "10"),
        ])
        .send()
        .await?
        .json()
        .await?;
    if resp["ok"] != true {
        anyhow::bail!("conversations.replies: {}", resp["error"]);
    }
    let messages = resp["messages"].as_array().context("no messages")?;
    messages
        .iter()
        .find(|m| m["ts"] == ts)
        .cloned()
        .context("message not returned")
}

/// Drop the permalinks Slack turned into quotes: the quote replaces them.
pub fn strip_quoted_links(text: &str, quotes: &[Quoted]) -> String {
    let mut out = text.to_string();
    for q in quotes {
        for url in [&q.original_url, &q.from_url].into_iter().flatten() {
            // Slack escapes `&` inside message text.
            for url in [url.clone(), url.replace('&', "&amp;")] {
                out = strip_link(&out, &url);
            }
        }
    }
    out.trim().to_string()
}

fn strip_link(text: &str, url: &str) -> String {
    let mut out = text.replace(&format!("<{url}>"), "");
    let labeled = format!("<{url}|");
    while let Some(start) = out.find(&labeled) {
        match out[start..].find('>') {
            Some(len) => out.replace_range(start..start + len + 1, ""),
            None => break,
        }
    }
    out
}

/// Discord text for the quotes: a header naming the author and channel
/// (linking to the Discord mirror when there is one), then the quoted text.
pub async fn render(state: &AppState, quotes: &[Quoted]) -> String {
    let mut blocks = Vec::new();
    for q in quotes {
        let name = match &q.author_id {
            Some(id) => {
                let n = state.slack_display_name(id, None).await;
                // users.info failed: it falls back to the bare id.
                if n == *id { None } else { Some(n) }
            }
            None => None,
        }
        .or_else(|| q.author_name.clone())
        .or_else(|| q.author_subname.clone())
        .unwrap_or_else(|| "someone".to_string());

        let (channel_id, ts) = q.source();
        let channel = match &channel_id {
            Some(id) => channel_name(state, q, id).await,
            None => q.channel_name.clone().or_else(|| footer_channel(q)),
        };
        let mirrored = match (&channel_id, &ts) {
            (Some(c), Some(t)) => mirror_link(state, c, t).await.unwrap_or_default(),
            _ => None,
        };
        let link = mirrored
            .or_else(|| q.from_url.clone())
            .or_else(|| q.original_url.clone());

        let (mut body, _) = state.slack_text_for_discord(q.text.as_deref().unwrap_or_default()).await;
        let file_names: Vec<&str> = q.files.iter().filter_map(|f| f.name.as_deref()).collect();
        if !file_names.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&format!("📎 {}", file_names.join(", ")));
        }
        blocks.push(format_quote(&name, channel.as_deref(), link.as_deref(), &body));
    }
    blocks.join("\n")
}

// `…/archives/C0123/p1700000000000100` → channel C0123, ts 1700000000.000100.
static PERMALINK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/archives/([A-Z0-9]+)/p(\d{10})(\d{6})").unwrap());
// A footer like "Posted in #general".
static FOOTER_CHANNEL: Lazy<Regex> = Lazy::new(|| Regex::new(r"#([\w.-]+)").unwrap());

impl Quoted {
    /// Channel id and ts of the quoted message: the attachment's own fields,
    /// else read off the permalink.
    fn source(&self) -> (Option<String>, Option<String>) {
        let from_link = [&self.original_url, &self.from_url]
            .into_iter()
            .flatten()
            .find_map(|u| PERMALINK.captures(u))
            .map(|c| (c[1].to_string(), format!("{}.{}", &c[2], &c[3])));
        let channel = self.channel_id.clone().or_else(|| from_link.as_ref().map(|l| l.0.clone()));
        let ts = self
            .ts
            .as_ref()
            .and_then(|t| t.as_str().map(str::to_string))
            .or_else(|| from_link.map(|l| l.1));
        (channel, ts)
    }
}

/// Name of the quoted message's channel. The attachment doesn't always say, so
/// fall back to our mirror, then `conversations.info` (fine for public channels
/// the bot isn't in), then the footer.
async fn channel_name(state: &AppState, q: &Quoted, channel_id: &str) -> Option<String> {
    if let Some(name) = &q.channel_name {
        return Some(name.clone());
    }
    if let Ok(Some(pair)) = state.db.pair_by_slack(channel_id).await {
        return Some(pair.slack_channel_name);
    }
    let session = state.slack.open_session(&state.slack_bot_token);
    let req = SlackApiConversationsInfoRequest::new(channel_id.into());
    match session.conversations_info(&req).await {
        Ok(info) if info.channel.name.is_some() => info.channel.name,
        Ok(_) => footer_channel(q),
        Err(e) => {
            tracing::debug!(%channel_id, error = ?e, "conversations.info failed for a quoted message");
            footer_channel(q)
        }
    }
}

fn footer_channel(q: &Quoted) -> Option<String> {
    q.footer.as_deref().and_then(|f| FOOTER_CHANNEL.captures(f)).map(|c| c[1].to_string())
}

/// Link to the quoted message's Discord copy, if it was mirrored.
async fn mirror_link(state: &AppState, channel_id: &str, ts: &str) -> Result<Option<String>> {
    let Some(pair) = state.db.pair_by_slack(channel_id).await? else { return Ok(None) };
    Ok(state.db.message_by_slack_ts(channel_id, ts).await?.map(|row| {
        let container = row.discord_thread_id.unwrap_or(pair.discord_channel_id);
        format!("https://discord.com/channels/{}/{container}/{}", state.discord_guild, row.discord_msg_id)
    }))
}

fn format_quote(name: &str, channel: Option<&str>, link: Option<&str>, body: &str) -> String {
    let mut out = match (channel, link) {
        (Some(c), Some(l)) => format!("↪ **{name}** in [#{c}](<{l}>):"),
        (Some(c), None) => format!("↪ **{name}** in #{c}:"),
        (None, Some(l)) => format!("↪ **{name}** ([message](<{l}>)):"),
        (None, None) => format!("↪ **{name}**:"),
    };
    for line in body.lines() {
        out.push_str(&format!("\n> {line}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://acme.slack.com/archives/C1/p1700000000000100";

    fn quote(url: &str) -> Quoted {
        Quoted { is_msg_unfurl: true, original_url: Some(url.to_string()), ..Default::default() }
    }

    #[test]
    fn permalink_is_replaced_by_the_quote() {
        let text = format!("look at this <{URL}>");
        assert_eq!(strip_quoted_links(&text, &[quote(URL)]), "look at this");
        let labeled = format!("<{URL}|here> it is");
        assert_eq!(strip_quoted_links(&labeled, &[quote(URL)]), "it is");
    }

    #[test]
    fn escaped_ampersand_in_permalink_is_stripped() {
        let url = format!("{URL}?thread_ts=1700000000.000100&cid=C1");
        let text = format!("<{}>", url.replace('&', "&amp;"));
        assert_eq!(strip_quoted_links(&text, &[quote(&url)]), "");
    }

    #[test]
    fn other_links_stay() {
        let text = "<https://example.com>";
        assert_eq!(strip_quoted_links(text, &[quote(URL)]), text);
    }

    #[test]
    fn quote_header_and_body() {
        assert_eq!(
            format_quote("Ann", Some("general"), Some(URL), "hi\nthere"),
            format!("↪ **Ann** in [#general](<{URL}>):\n> hi\n> there")
        );
        assert_eq!(format_quote("Ann", None, None, "hi"), "↪ **Ann**:\n> hi");
    }

    #[test]
    fn source_falls_back_to_the_permalink() {
        let q = quote(&format!("{URL}?thread_ts=1699999999.000000&cid=C1"));
        assert_eq!(q.source(), (Some("C1".into()), Some("1700000000.000100".into())));
    }

    #[test]
    fn channel_from_footer() {
        let q = Quoted { footer: Some("Posted in #dev-ops".into()), ..Default::default() };
        assert_eq!(footer_channel(&q).as_deref(), Some("dev-ops"));
    }

    #[test]
    fn gif_attachment_gives_its_picture() {
        let gif = "https://media.giphy.com/media/abc/giphy.gif";
        let msg = serde_json::json!({
            "text": "",
            "attachments": [{
                "id": 1, "fallback": "480x270px image",
                "blocks": [{ "type": "image", "image_url": gif, "alt_text": "cat" }],
            }],
        });
        assert_eq!(images_in(&msg), [gif]);
    }

    #[test]
    fn other_picture_shapes_are_found_once() {
        let url = "https://example.com/a.gif";
        let msg = serde_json::json!({
            "blocks": [
                { "type": "rich_text", "elements": [] },
                { "type": "image", "image_url": url, "alt_text": "" },
            ],
            "attachments": [{ "id": 1, "image_url": url }],
        });
        assert_eq!(images_in(&msg), [url]);
    }

    #[test]
    fn previews_and_quotes_add_no_picture() {
        let msg = serde_json::json!({
            "attachments": [
                { "from_url": "https://example.com", "image_url": "https://example.com/p.png" },
                { "is_msg_unfurl": true, "blocks": [{ "type": "image", "image_url": "https://example.com/q.png" }] },
            ],
        });
        assert!(images_in(&msg).is_empty());
    }

    #[test]
    fn unfurl_attachment_parses() {
        let raw = serde_json::json!({
            "is_msg_unfurl": true, "is_share": true, "author_id": "U1",
            "author_name": "Ann", "channel_id": "C1", "channel_name": "general",
            "ts": "1700000000.000100", "text": "hello", "original_url": URL,
        });
        let q: Quoted = serde_json::from_value(raw).unwrap();
        assert!(q.is_msg_unfurl);
        assert_eq!(q.channel_name.as_deref(), Some("general"));
        // Link-preview attachments carry a numeric ts; they must still parse.
        let other = serde_json::json!({ "ts": 1700000000, "title": "Example" });
        assert!(!serde_json::from_value::<Quoted>(other).unwrap().is_msg_unfurl);
    }
}

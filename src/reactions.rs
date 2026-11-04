//! Slack reactions on the Discord copy: a small last line naming who reacted,
//! e.g. `-# 👍 3 Antonin, Antoine D, Antoine L · 🎉 Marie`.
//!
//! Discord bots can only add each emoji once, so native reactions can't carry
//! Slack's counts or reactors. Instead the webhook message we posted is edited
//! to end with this line. People are named by first name, plus as many letters
//! of the last name as it takes to tell apart others in the channel with the
//! same first name.
//!
//! Only messages that came from Slack get the line: a Discord user's own
//! message can't be edited by the bot.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use slack_morphism::prelude::*;
use tokio::sync::Mutex;

use crate::db::{ChannelPair, DiscordMsgRef};
use crate::format::DISCORD_MAX_CHARS;
use crate::relay::AppState;

/// Starts every reaction line: Discord's small-text marker plus a zero-width
/// space, so the line can be found and replaced later without being confused
/// with message text that happens to start with `-# `.
const MARK: &str = "-# \u{200b}";

/// A Slack user, split into the parts the short name is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub id: String,
    pub first: String,
    pub last: String,
}

/// One emoji's reactors, in Slack's order.
pub struct Group {
    /// Slack reaction name (`+1`, `tada`, `+1::skin-tone-3`, `partyparrot`).
    pub emoji: String,
    pub names: Vec<String>,
}

/// Shortest unambiguous name for each person in `pool`: the first name alone
/// when nobody else shares it, else the first name and the shortest last-name
/// prefix no one else with that first name has ("Antoine D" / "Antoine L",
/// "Antoine Dup" / "Antoine Dur"). Falls back to the full name when even that
/// can't tell them apart.
pub fn short_names(pool: &[Person]) -> HashMap<String, String> {
    let lower = |s: &str| s.to_lowercase();
    let mut out = HashMap::new();
    for p in pool {
        let namesakes: Vec<Vec<char>> = pool
            .iter()
            .filter(|o| o.id != p.id && lower(&o.first) == lower(&p.first))
            .map(|o| lower(&o.last).chars().collect())
            .collect();
        let name = if namesakes.is_empty() || p.last.is_empty() {
            p.first.clone()
        } else {
            let last: Vec<char> = p.last.chars().collect();
            let last_lower: Vec<char> = lower(&p.last).chars().collect();
            (1..=last.len())
                .find(|&k| {
                    namesakes
                        .iter()
                        .all(|o| o.iter().take(k).ne(last_lower.iter().take(k)))
                })
                .map(|k| format!("{} {}", p.first, last[..k].iter().collect::<String>()))
                .unwrap_or_else(|| format!("{} {}", p.first, p.last))
        };
        out.insert(p.id.clone(), name);
    }
    out
}

/// The reaction line (empty when there are no reactions).
pub fn render_line(groups: &[Group]) -> String {
    let parts: Vec<String> = groups
        .iter()
        .filter(|g| !g.names.is_empty())
        .map(|g| {
            let names: Vec<String> = g.names.iter().map(|n| escape_markdown(n)).collect();
            let emoji = emoji_text(&g.emoji);
            match names.len() {
                1 => format!("{emoji} {}", names[0]),
                n => format!("{emoji} {n} {}", names.join(", ")),
            }
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("{MARK}{}", parts.join(" · "))
    }
}

/// A Slack reaction name as an emoji; custom emoji stay as `:name:`.
fn emoji_text(name: &str) -> String {
    use emojis::SkinTone;
    let (base, tone) = match name.split_once("::skin-tone-") {
        Some((b, t)) => (b, t.parse::<u8>().ok()),
        None => (name, None),
    };
    let Some(e) = emojis::get_by_shortcode(base) else {
        return format!(":{name}:");
    };
    let tone = match tone {
        Some(2) => Some(SkinTone::Light),
        Some(3) => Some(SkinTone::MediumLight),
        Some(4) => Some(SkinTone::Medium),
        Some(5) => Some(SkinTone::MediumDark),
        Some(6) => Some(SkinTone::Dark),
        _ => None,
    };
    tone.and_then(|t| e.with_skin_tone(t)).unwrap_or(e).as_str().to_string()
}

fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '_' | '~' | '|' | '`' | '<' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Message content without its reaction line (and without the zero-width
/// space that stands in for an empty message).
pub fn detach(content: &str) -> &str {
    if content == "\u{200b}" || content.starts_with(MARK) {
        return "";
    }
    match content.rfind(&format!("\n{MARK}")) {
        Some(i) => &content[..i],
        None => content,
    }
}

/// `base` with `line` as its last line, within Discord's limit: the line is
/// cut short (ending in "…") rather than the message.
pub fn attach(base: &str, line: &str) -> String {
    let base = crate::format::truncate_for_discord(detach(base));
    if line.is_empty() {
        return base;
    }
    let sep = usize::from(!base.is_empty());
    let avail = DISCORD_MAX_CHARS.saturating_sub(base.chars().count() + sep);
    let line = if line.chars().count() <= avail {
        line.to_string()
    } else if avail > MARK.chars().count() + 1 {
        let mut cut: String = line.chars().take(avail - 1).collect();
        cut.push('…');
        cut
    } else {
        return base;
    };
    if base.is_empty() {
        line
    } else {
        format!("{base}\n{line}")
    }
}

// ── the Slack event ──────────────────────────────────────────────────────────

/// A reaction changed on a Slack message → rewrite the reaction line on its
/// Discord copy (the last part, and the main-channel copy of a thread reply
/// sent to the channel too).
pub async fn sync_from_slack(state: &AppState, channel_id: &str, ts: &str) -> Result<()> {
    let Some(row) = state.db.message_by_slack_ts(channel_id, ts).await? else {
        return Ok(());
    };
    if row.origin != "slack" {
        // A Discord user's message: the bot can't edit it.
        return Ok(());
    }
    let Some(pair) = state.db.pair_by_slack(channel_id).await? else { return Ok(()) };

    let line = current_line(state, channel_id, ts).await?;
    let stored = state.db.reaction_line(channel_id, ts).await?.unwrap_or_default();
    if line == stored {
        return Ok(());
    }

    let mut parts = vec![crate::slack::primary_ref(&row)?];
    parts.extend(state.db.extras(channel_id, ts, crate::slack::ROLE_PART).await?);
    let mut targets = vec![*parts.last().expect("at least the primary part")];
    targets.extend(state.db.extras(channel_id, ts, crate::slack::ROLE_BROADCAST).await?.last());
    for target in targets {
        rewrite(state, &pair, target, &line).await?;
    }
    state.db.set_reaction_line(channel_id, ts, &line).await?;
    tracing::debug!(%channel_id, %ts, %line, "mirrored slack reactions to discord");
    Ok(())
}

async fn rewrite(state: &AppState, pair: &ChannelPair, msg: DiscordMsgRef, line: &str) -> Result<()> {
    let current = match crate::discord::webhook_message_content(state, pair, msg).await {
        Ok(c) => c,
        Err(e) if crate::discord::is_unknown_message(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    let updated = attach(&current, line);
    if updated != current {
        crate::discord::edit_on_discord(state, pair, msg.message_id, msg.thread_id, &updated).await?;
    }
    Ok(())
}

/// The reaction line for a Slack message as it stands now. The relay bot's
/// own reactions are left out: they mirror Discord reactions, which Discord
/// already shows.
async fn current_line(state: &AppState, channel_id: &str, ts: &str) -> Result<String> {
    let session = state.slack.open_session(&state.slack_bot_token);
    let req = SlackApiReactionsGetRequest::new()
        .with_channel(channel_id.into())
        .with_timestamp(ts.into())
        .with_full(true);
    let resp = session.reactions_get(&req).await.context("reactions.get")?;
    let SlackApiReactionsGetResponse::Message(m) = resp else { return Ok(String::new()) };
    let reactions = m.message.content.reactions.unwrap_or_default();

    let reacted: Vec<(String, Vec<String>)> = reactions
        .into_iter()
        .map(|r| {
            let users = r
                .users
                .iter()
                .map(|u| u.to_string())
                .filter(|u| *u != state.bot_user_id)
                .collect();
            (r.name.to_string(), users)
        })
        .collect();
    if reacted.iter().all(|(_, users)| users.is_empty()) {
        return Ok(String::new());
    }

    // Everyone the names must be told apart from: the channel, plus reactors
    // who have since left it.
    let mut ids = channel_members(state, channel_id).await;
    for (_, users) in &reacted {
        ids.extend(users.iter().cloned());
    }
    ids.sort();
    ids.dedup();
    let pool = people(state, &ids, &reacted).await;
    let short = short_names(&pool);

    let groups: Vec<Group> = reacted
        .into_iter()
        .map(|(emoji, users)| Group {
            emoji,
            names: users.iter().map(|u| short.get(u).cloned().unwrap_or_else(|| u.clone())).collect(),
        })
        .collect();
    Ok(render_line(&groups))
}

// ── who's who (cached) ───────────────────────────────────────────────────────

/// Workspace users from users.list, refreshed every `DIRECTORY_TTL`.
struct Directory {
    people: HashMap<String, Person>,
    fetched: Option<Instant>,
}

const DIRECTORY_TTL: Duration = Duration::from_secs(12 * 3600);
const MEMBERS_TTL: Duration = Duration::from_secs(3600);

static DIRECTORY: Lazy<Mutex<Directory>> =
    Lazy::new(|| Mutex::new(Directory { people: HashMap::new(), fetched: None }));
/// Channel id → (when fetched, member ids).
type MemberCache = HashMap<String, (Instant, Vec<String>)>;

static MEMBERS: Lazy<Mutex<MemberCache>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// `ids` as people. Reactors missing from the directory (new or external
/// users) are looked up one by one; other unknown channel members are left out.
async fn people(state: &AppState, ids: &[String], reacted: &[(String, Vec<String>)]) -> Vec<Person> {
    let mut dir = DIRECTORY.lock().await;
    if dir.fetched.map_or(true, |t| t.elapsed() > DIRECTORY_TTL) {
        match list_users(state).await {
            Ok(all) => dir.people = all,
            Err(e) => tracing::warn!(error = ?e, "users.list failed; names may be less precise"),
        }
        // Also on failure, so a missing scope doesn't mean a retry per reaction.
        dir.fetched = Some(Instant::now());
    }
    for (_, users) in reacted {
        for u in users {
            if !dir.people.contains_key(u) {
                if let Some(p) = lookup_user(state, u).await {
                    dir.people.insert(u.clone(), p);
                }
            }
        }
    }
    ids.iter().filter_map(|id| dir.people.get(id).cloned()).collect()
}

async fn list_users(state: &AppState) -> Result<HashMap<String, Person>> {
    let session = state.slack.open_session(&state.slack_bot_token);
    let mut out = HashMap::new();
    let mut cursor: Option<SlackCursorId> = None;
    loop {
        let mut req = SlackApiUsersListRequest::new().with_limit(200);
        if let Some(c) = cursor.take() {
            req = req.with_cursor(c);
        }
        let resp = session.users_list(&req).await.context("users.list")?;
        for u in &resp.members {
            out.insert(u.id.to_string(), person(u));
        }
        match resp.response_metadata.and_then(|m| m.next_cursor) {
            Some(c) if !c.to_string().is_empty() => cursor = Some(c),
            _ => break,
        }
    }
    Ok(out)
}

async fn lookup_user(state: &AppState, id: &str) -> Option<Person> {
    let session = state.slack.open_session(&state.slack_bot_token);
    match session.users_info(&SlackApiUsersInfoRequest::new(id.into())).await {
        Ok(resp) => Some(person(&resp.user)),
        Err(e) => {
            tracing::debug!(user = id, error = ?e, "users.info failed");
            None
        }
    }
}

/// Channel member ids, cached for `MEMBERS_TTL`. Empty if Slack refuses.
async fn channel_members(state: &AppState, channel_id: &str) -> Vec<String> {
    let mut cache = MEMBERS.lock().await;
    if let Some((at, ids)) = cache.get(channel_id) {
        if at.elapsed() < MEMBERS_TTL {
            return ids.clone();
        }
    }
    let ids = match fetch_members(state, channel_id).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(%channel_id, error = ?e, "conversations.members failed");
            Vec::new()
        }
    };
    cache.insert(channel_id.to_string(), (Instant::now(), ids.clone()));
    ids
}

async fn fetch_members(state: &AppState, channel_id: &str) -> Result<Vec<String>> {
    let session = state.slack.open_session(&state.slack_bot_token);
    let mut out = Vec::new();
    let mut cursor: Option<SlackCursorId> = None;
    loop {
        let mut req = SlackApiConversationsMembersRequest::new()
            .with_channel(channel_id.into())
            .with_limit(1000);
        if let Some(c) = cursor.take() {
            req = req.with_cursor(c);
        }
        let resp = session.conversations_members(&req).await.context("conversations.members")?;
        out.extend(resp.members.iter().map(|m| m.to_string()));
        match resp.response_metadata.and_then(|m| m.next_cursor) {
            Some(c) if !c.to_string().is_empty() => cursor = Some(c),
            _ => break,
        }
    }
    Ok(out)
}

/// First and last name from a Slack profile: the profile's own fields when
/// both are set, else the real name split at its first space, else the
/// display / user name as a lone first name.
fn person(u: &SlackUser) -> Person {
    let clean = |s: Option<&String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let profile = u.profile.as_ref();
    let first = clean(profile.and_then(|p| p.first_name.as_ref()));
    let last = clean(profile.and_then(|p| p.last_name.as_ref()));
    let (first, last) = match (first, last) {
        (Some(f), Some(l)) => (f, l),
        _ => {
            let real = clean(profile.and_then(|p| p.real_name.as_ref())).or_else(|| clean(u.real_name.as_ref()));
            match real {
                Some(r) => match r.split_once(char::is_whitespace) {
                    Some((f, l)) => (f.to_string(), l.trim().to_string()),
                    None => (r, String::new()),
                },
                None => (
                    clean(profile.and_then(|p| p.display_name.as_ref()))
                        .or_else(|| clean(u.name.as_ref()))
                        .unwrap_or_else(|| u.id.to_string()),
                    String::new(),
                ),
            }
        }
    };
    Person { id: u.id.to_string(), first, last }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str, first: &str, last: &str) -> Person {
        Person { id: id.into(), first: first.into(), last: last.into() }
    }

    #[test]
    fn first_name_alone_when_unique() {
        let pool = [p("1", "Antonin", "Duthil"), p("2", "Marie", "Curie")];
        let s = short_names(&pool);
        assert_eq!(s["1"], "Antonin");
        assert_eq!(s["2"], "Marie");
    }

    #[test]
    fn shared_first_names_get_last_name_letters() {
        let pool = [
            p("1", "Antoine", "Dupont"),
            p("2", "Antoine", "Lefebvre"),
            p("3", "antoine", "Durand"),
            p("4", "Antonin", "Duthil"),
        ];
        let s = short_names(&pool);
        assert_eq!(s["1"], "Antoine Dup");
        assert_eq!(s["2"], "Antoine L");
        assert_eq!(s["3"], "antoine Dur");
        assert_eq!(s["4"], "Antonin");
    }

    #[test]
    fn prefix_and_identical_names_fall_back_to_full() {
        let pool = [
            p("1", "Léa", "Du"),
            p("2", "Léa", "Dupont"),
            p("3", "Paul", "Martin"),
            p("4", "Paul", "Martin"),
            p("5", "Zoé", ""),
            p("6", "Zoé", "Roux"),
        ];
        let s = short_names(&pool);
        assert_eq!(s["1"], "Léa Du");
        assert_eq!(s["2"], "Léa Dup");
        assert_eq!(s["3"], "Paul Martin");
        assert_eq!(s["5"], "Zoé");
        assert_eq!(s["6"], "Zoé R");
    }

    #[test]
    fn line_lists_every_reactor() {
        let groups = [
            Group { emoji: "+1".into(), names: vec!["Antonin".into(), "Antoine D".into(), "Antoine L".into()] },
            Group { emoji: "tada".into(), names: vec!["Marie".into()] },
            Group { emoji: "partyparrot".into(), names: vec!["Jo_e".into()] },
            Group { emoji: "wave::skin-tone-3".into(), names: vec!["Zoé".into()] },
            Group { emoji: "eyes".into(), names: vec![] },
        ];
        assert_eq!(
            render_line(&groups),
            "-# \u{200b}👍 3 Antonin, Antoine D, Antoine L · 🎉 Marie · :partyparrot: Jo\\_e · 👋🏼 Zoé"
        );
        assert_eq!(render_line(&[]), "");
    }

    #[test]
    fn line_is_replaced_not_stacked() {
        let line1 = format!("{MARK}👍 Antonin");
        let line2 = format!("{MARK}👍 2 Antonin, Marie");
        let with1 = attach("hello\n-# not ours", &line1);
        assert_eq!(with1, format!("hello\n-# not ours\n{line1}"));
        assert_eq!(attach(&with1, &line2), format!("hello\n-# not ours\n{line2}"));
        assert_eq!(attach(&with1, ""), "hello\n-# not ours");
        // A file-only message (zero-width placeholder) gets just the line.
        assert_eq!(attach("\u{200b}", &line1), line1);
        assert_eq!(attach(&line1, ""), "");
    }

    #[test]
    fn long_lines_are_cut_to_fit() {
        let base = "x".repeat(1990);
        let line = format!("{MARK}👍 {}", "Name, ".repeat(10));
        let out = attach(&base, &line);
        assert_eq!(out.chars().count(), DISCORD_MAX_CHARS);
        assert!(out.ends_with('…'));
        assert_eq!(detach(&out), base);
        // No room at all: the message stays as it is.
        let full = "x".repeat(1998);
        assert_eq!(attach(&full, &line), full);
    }
}

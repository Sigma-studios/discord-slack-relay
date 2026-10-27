//! Conversion between Slack `mrkdwn` and Discord markdown.
//!
//! Only text-level markup lives here (bold / italic / strike / links / emoji /
//! spoilers / headers / entities). User and channel mentions need async
//! lookups, so the relay layer rewrites those around these functions. Code
//! blocks and inline code are passed through untouched in both directions.

use once_cell::sync::Lazy;
use regex::{Captures, Regex};

static CODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)```.*?```|`[^`\n]+`").unwrap());

/// Apply `f` to the parts of `input` outside code blocks / inline code.
fn outside_code(input: &str, f: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last = 0;
    for m in CODE.find_iter(input) {
        out.push_str(&f(&input[last..m.start()]));
        out.push_str(m.as_str());
        last = m.end();
    }
    out.push_str(&f(&input[last..]));
    out
}

static SLACK_LINK_LABELED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<(https?://[^|>]+)\|([^>]+)>").unwrap());
static SLACK_LINK_BARE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<(https?://[^>]+)>").unwrap());
static SLACK_USERGROUP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<!subteam\^[A-Z0-9]+(?:\|([^>]+))?>").unwrap());
static SLACK_EMOJI: Lazy<Regex> =
    Lazy::new(|| Regex::new(r":([a-z0-9_+\-']+):(?::skin-tone-[2-6]:)?").unwrap());
static SLACK_BOLD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*([^*\n]+)\*").unwrap());
static SLACK_STRIKE: Lazy<Regex> = Lazy::new(|| Regex::new(r"~([^~\n]+)~").unwrap());

/// Slack mrkdwn → Discord markdown.
pub fn slack_to_discord(input: &str) -> String {
    let s = outside_code(input, |seg| {
        // Links first, before `*`/`~`/emoji run over the label text.
        let s = SLACK_LINK_LABELED.replace_all(seg, "[$2]($1)");
        let s = SLACK_LINK_BARE.replace_all(&s, "$1");
        let s = SLACK_USERGROUP.replace_all(&s, |c: &Captures| {
            let name = c.get(1).map_or("group", |m| m.as_str());
            format!("@{}", name.trim_start_matches('@'))
        });
        // Standard shortcodes (`:tada:`) → the emoji; custom ones stay as text.
        let s = SLACK_EMOJI.replace_all(&s, |c: &Captures| {
            emojis::get_by_shortcode(&c[1])
                .map(|e| e.as_str().to_string())
                .unwrap_or_else(|| c[0].to_string())
        });
        // Slack bold `*x*` → Discord `**x**`, via a placeholder so the doubled
        // asterisks are not re-matched.
        let s = SLACK_BOLD.replace_all(&s, "\u{1}$1\u{1}");
        let s = SLACK_STRIKE.replace_all(&s, "~~$1~~");
        s.replace('\u{1}', "**")
    });
    decode_entities(&s)
}

static D_CUSTOM_EMOJI: Lazy<Regex> = Lazy::new(|| Regex::new(r"<a?:(\w+):\d+>").unwrap());
static D_SPOILER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)\|\|(.+?)\|\|").unwrap());
static D_SUBTEXT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^-# (.+)$").unwrap());
static D_HEADER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^#{1,3} (.+)$").unwrap());
static D_UNDERLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"__([^_\n]+)__").unwrap());
static D_LINK: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]+)\]\((https?://[^)]+)\)").unwrap());
static D_BOLD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*\*([^*\n]+)\*\*").unwrap());
static D_STRIKE: Lazy<Regex> = Lazy::new(|| Regex::new(r"~~([^~\n]+)~~").unwrap());
static D_ITALIC_STAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*([^*\n]+)\*").unwrap());

/// Discord markdown → Slack mrkdwn.
pub fn discord_to_slack(input: &str) -> String {
    outside_code(input, |seg| {
        // Custom emoji `<:name:123>` → `:name:` (renders if Slack has one).
        let s = D_CUSTOM_EMOJI.replace_all(seg, ":$1:");
        // Slack has no spoilers; don't reveal the hidden text.
        let s = D_SPOILER.replace_all(&s, "[spoiler hidden]");
        // `-# subtext` → italic; `# header` → bold (placeholder, see below).
        let s = D_SUBTEXT.replace_all(&s, "_${1}_");
        let s = D_HEADER.replace_all(&s, "\u{1}${1}\u{1}");
        // Slack has no underline: keep the text.
        let s = D_UNDERLINE.replace_all(&s, "$1");
        let s = D_LINK.replace_all(&s, "<$2|$1>");
        // Bold `**x**` → placeholder, so the single-`*` italic pass does not eat
        // the markers.
        let s = D_BOLD.replace_all(&s, "\u{1}$1\u{1}");
        let s = D_STRIKE.replace_all(&s, "~$1~");
        // `${1}` not `$1`: `_` is a word char, so `$1_` would be read as a group
        // named `1_` and drop the capture.
        let s = D_ITALIC_STAR.replace_all(&s, "_${1}_");
        s.replace('\u{1}', "*")
    })
}

/// Discord's per-message character limit.
pub const DISCORD_MAX_CHARS: usize = 2000;

/// Split text into chunks of at most `max` characters, preferring to break at a
/// newline, then a space, so long Slack posts (up to 40k chars) fit Discord.
pub fn split_for_discord(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.chars().count() > max {
        // Byte index of the `max`-th char boundary.
        let hard = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let window = &rest[..hard];
        let cut = window
            .rfind('\n')
            .or_else(|| window.rfind(' '))
            .filter(|&i| i > 0)
            .unwrap_or(hard);
        chunks.push(rest[..cut].to_string());
        rest = rest[cut..].trim_start_matches(['\n', ' ']);
    }
    if !rest.is_empty() || chunks.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

/// Truncate to Discord's limit (for edits, which can't be split).
pub fn truncate_for_discord(text: &str) -> String {
    if text.chars().count() <= DISCORD_MAX_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(DISCORD_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

/// Decode the handful of HTML entities Slack escapes in message text.
pub fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Encode the characters Slack requires escaped in outgoing text.
/// Not applied blanket to relayed text (that would escape intentional `<@U…>`
/// mentions); kept for escaping user-literal fragments.
#[allow(dead_code)]
pub fn encode_entities(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Slack reaction names for a Unicode emoji, best guess first (`👍` → `+1`,
/// `thumbsup`; `👍🏽` → `+1::skin-tone-4`, …). Empty when it has no shortcode
/// (e.g. mixed skin tones).
pub fn slack_reaction_names(emoji: &str) -> Vec<String> {
    use emojis::SkinTone;
    let Some(e) = emojis::get(emoji).or_else(|| emojis::get(emoji.trim_end_matches('\u{fe0f}'))) else {
        return Vec::new();
    };
    let tone = match e.skin_tone() {
        Some(SkinTone::Light) => Some(2),
        Some(SkinTone::MediumLight) => Some(3),
        Some(SkinTone::Medium) => Some(4),
        Some(SkinTone::MediumDark) => Some(5),
        Some(SkinTone::Dark) => Some(6),
        _ => None,
    };
    let base = match tone {
        Some(_) => e.with_skin_tone(SkinTone::Default).unwrap_or(e),
        None => e,
    };
    base.shortcodes()
        .map(|name| match tone {
            Some(n) => format!("{name}::skin-tone-{n}"),
            None => name.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_bold_to_discord() {
        assert_eq!(slack_to_discord("a *bold* word"), "a **bold** word");
    }

    #[test]
    fn slack_strike_to_discord() {
        assert_eq!(slack_to_discord("~nope~"), "~~nope~~");
    }

    #[test]
    fn slack_labeled_link() {
        assert_eq!(
            slack_to_discord("see <https://x.com|the site>"),
            "see [the site](https://x.com)"
        );
    }

    #[test]
    fn slack_bare_link() {
        assert_eq!(slack_to_discord("<https://x.com>"), "https://x.com");
    }

    #[test]
    fn slack_entities() {
        assert_eq!(slack_to_discord("a &amp;&lt;b&gt;"), "a &<b>");
    }

    #[test]
    fn discord_bold_to_slack() {
        assert_eq!(discord_to_slack("a **bold** word"), "a *bold* word");
    }

    #[test]
    fn discord_italic_to_slack() {
        assert_eq!(discord_to_slack("an *italic* word"), "an _italic_ word");
    }

    #[test]
    fn discord_bold_and_italic_mix() {
        assert_eq!(
            discord_to_slack("**b** and *i*"),
            "*b* and _i_"
        );
    }

    #[test]
    fn code_is_left_alone() {
        assert_eq!(slack_to_discord("`*x*` and *y*"), "`*x*` and **y**");
        assert_eq!(discord_to_slack("```\n**x** ||s||\n``` **y**"), "```\n**x** ||s||\n``` *y*");
    }

    #[test]
    fn slack_emoji_shortcodes() {
        assert_eq!(slack_to_discord(":tada: done"), "🎉 done");
        assert_eq!(slack_to_discord(":wave::skin-tone-3: hi"), "👋 hi");
        assert_eq!(slack_to_discord(":partyparrot: at 10:30:45"), ":partyparrot: at 10:30:45");
    }

    #[test]
    fn reaction_names() {
        assert_eq!(slack_reaction_names("👍").first().map(String::as_str), Some("+1"));
        assert_eq!(slack_reaction_names("❤️").first().map(String::as_str), Some("heart"));
        assert_eq!(slack_reaction_names("😂").first().map(String::as_str), Some("joy"));
        assert_eq!(
            slack_reaction_names("👍🏽").first().map(String::as_str),
            Some("+1::skin-tone-4")
        );
        assert!(slack_reaction_names("not an emoji").is_empty());
    }

    #[test]
    fn slack_usergroups() {
        assert_eq!(slack_to_discord("<!subteam^S0123|@devs> look"), "@devs look");
        assert_eq!(slack_to_discord("<!subteam^S0123> look"), "@group look");
    }

    #[test]
    fn discord_extras_to_slack() {
        assert_eq!(discord_to_slack("hi <:pog:123> <a:dance:456>"), "hi :pog: :dance:");
        assert_eq!(discord_to_slack("the end: ||he dies||"), "the end: [spoiler hidden]");
        assert_eq!(discord_to_slack("__under__ line"), "under line");
        assert_eq!(discord_to_slack("# Title\nbody\n-# small"), "*Title*\nbody\n_small_");
    }

    #[test]
    fn splits_long_text_at_line_breaks() {
        let text = format!("{}\n{}", "a".repeat(1500), "b".repeat(1500));
        let parts = split_for_discord(&text, DISCORD_MAX_CHARS);
        assert_eq!(parts, vec!["a".repeat(1500), "b".repeat(1500)]);
        assert_eq!(split_for_discord("short", DISCORD_MAX_CHARS), vec!["short"]);
        let unbroken = "x".repeat(4500);
        let parts = split_for_discord(&unbroken, DISCORD_MAX_CHARS);
        assert_eq!(parts.iter().map(|p| p.chars().count()).collect::<Vec<_>>(), vec![2000, 2000, 500]);
        assert_eq!(truncate_for_discord(&unbroken).chars().count(), DISCORD_MAX_CHARS);
    }

    #[test]
    fn discord_link_to_slack() {
        assert_eq!(
            discord_to_slack("[the site](https://x.com)"),
            "<https://x.com|the site>"
        );
    }
}

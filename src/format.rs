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
    fn discord_link_to_slack() {
        assert_eq!(
            discord_to_slack("[the site](https://x.com)"),
            "<https://x.com|the site>"
        );
    }
}

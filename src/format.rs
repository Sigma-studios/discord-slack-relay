//! Conversion between Slack `mrkdwn` and Discord markdown.
//!
//! Only inline markup lives here (bold / italic / strike / links / entities).
//! User and channel mentions (`<@U…>`, `<#C…>`, `@d:alias`) need async lookups,
//! so the relay layer rewrites those around these functions.

use once_cell::sync::Lazy;
use regex::Regex;

static SLACK_LINK_LABELED: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<(https?://[^|>]+)\|([^>]+)>").unwrap());
static SLACK_LINK_BARE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<(https?://[^>]+)>").unwrap());
static SLACK_BOLD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*([^*\n]+)\*").unwrap());
static SLACK_STRIKE: Lazy<Regex> = Lazy::new(|| Regex::new(r"~([^~\n]+)~").unwrap());

/// Slack mrkdwn → Discord markdown.
pub fn slack_to_discord(input: &str) -> String {
    // Links first, before `*`/`~` run over the label text.
    let s = SLACK_LINK_LABELED.replace_all(input, "[$2]($1)");
    let s = SLACK_LINK_BARE.replace_all(&s, "$1");
    // Slack bold `*x*` → Discord `**x**`. Use a placeholder so the doubled
    // asterisks are not re-matched.
    let s = SLACK_BOLD.replace_all(&s, "\u{1}$1\u{1}");
    let s = SLACK_STRIKE.replace_all(&s, "~~$1~~");
    let s = s.replace('\u{1}', "**");
    decode_entities(&s)
}

static D_LINK: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]+)\]\((https?://[^)]+)\)").unwrap());
static D_BOLD: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*\*([^*\n]+)\*\*").unwrap());
static D_STRIKE: Lazy<Regex> = Lazy::new(|| Regex::new(r"~~([^~\n]+)~~").unwrap());
static D_ITALIC_STAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*([^*\n]+)\*").unwrap());

/// Discord markdown → Slack mrkdwn.
pub fn discord_to_slack(input: &str) -> String {
    let s = D_LINK.replace_all(input, "<$2|$1>");
    // Bold `**x**` → placeholder, so the remaining single-`*` italic pass
    // does not eat the markers.
    let s = D_BOLD.replace_all(&s, "\u{1}$1\u{1}");
    let s = D_STRIKE.replace_all(&s, "~$1~");
    // `${1}` not `$1`: `_` is a word char, so `$1_` would be read as a group
    // named `1_` and drop the capture.
    let s = D_ITALIC_STAR.replace_all(&s, "_${1}_");
    s.replace('\u{1}', "*")
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
    fn discord_link_to_slack() {
        assert_eq!(
            discord_to_slack("[the site](https://x.com)"),
            "<https://x.com|the site>"
        );
    }
}

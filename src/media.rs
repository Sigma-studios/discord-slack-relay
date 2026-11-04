//! Media transfer helpers.
//!
//! Slack→Discord: download `url_private` with the bot token as a bearer header
//! (re-attached across redirects), then hand the bytes to a Discord webhook as an attachment.
//! Discord→Slack: download the Discord CDN url (public, no auth) and push it
//! through Slack's external upload flow.
//!
//! Downloads are capped at `INLINE_LIMIT` and abort early past it: the relay
//! runs in a 256 MB container, so a large video must never be buffered whole.

use anyhow::{Context, Result};
use reqwest::Client;

/// Max bytes we re-upload inline; larger files are posted as a link instead.
/// Discord's free limit is 20 MB (2026); keep a margin for request overhead.
pub const INLINE_LIMIT: u64 = 19 * 1024 * 1024;

#[derive(Clone)]
pub struct DownloadedFile {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub mimetype: Option<String>,
}

pub enum Download {
    File(DownloadedFile),
    /// Over `INLINE_LIMIT`; carries the size when the server reported it.
    TooLarge(Option<u64>),
}

/// Download a Slack private file. `token` is the bot token (xoxb-).
///
/// Redirects are followed by hand: reqwest drops the `Authorization` header
/// when a redirect changes host, and Slack's file URLs can bounce between
/// hosts, so the request would end up anonymous. Slack answers an anonymous
/// or unauthorized file request with `200` and its HTML sign-in page, not an
/// error. Re-uploaded under the file's `.png` name, that shows up on Discord as
/// a broken image. So the token is re-attached on every Slack-owned hop, and
/// an HTML answer to a non-HTML file counts as a failed download.
pub async fn download_slack_file(url_private: &str, filename: &str, token: &str) -> Result<Download> {
    let http = slack_file_client();
    let mut url = reqwest::Url::parse(url_private).context("bad slack file url")?;
    for _ in 0..MAX_REDIRECTS {
        let mut req = http.get(url.clone());
        if is_slack_host(&url) {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.context("slack file download request failed")?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("slack file redirect without a location")?;
            url = url.join(location).context("bad slack file redirect location")?;
            continue;
        }
        let resp = resp.error_for_status()?;
        let html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/html"));
        if html && !is_html_name(filename) {
            anyhow::bail!(
                "slack served an HTML page instead of the file (final host {:?}); \
                 the bot token lacks access (files:read?) or was dropped",
                url.host_str()
            );
        }
        return read_capped(resp, filename).await;
    }
    anyhow::bail!("slack file download: too many redirects")
}

const MAX_REDIRECTS: usize = 5;

/// A client that doesn't follow redirects, so the token can be re-attached.
fn slack_file_client() -> &'static Client {
    static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("building the slack file http client")
    })
}

/// Hosts that may receive the bot token: Slack's own domains only.
fn is_slack_host(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|h| {
            ["slack.com", "slack-edge.com", "slack-files.com"]
                .iter()
                .any(|d| h == *d || h.ends_with(&format!(".{d}")))
        })
}

fn is_html_name(filename: &str) -> bool {
    let lower = filename.to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".htm")
}

/// Download a public Discord attachment by CDN url.
pub async fn download_discord_attachment(
    http: &Client,
    url: &str,
    filename: &str,
) -> Result<Download> {
    let resp = http.get(url).send().await?.error_for_status()?;
    read_capped(resp, filename).await
}

async fn read_capped(mut resp: reqwest::Response, filename: &str) -> Result<Download> {
    if let Some(len) = resp.content_length() {
        if len > INLINE_LIMIT {
            return Ok(Download::TooLarge(Some(len)));
        }
    }
    let mimetype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > INLINE_LIMIT {
            return Ok(Download::TooLarge(None));
        }
    }
    Ok(Download::File(DownloadedFile {
        filename: filename.to_string(),
        bytes,
        mimetype,
    }))
}

/// "12.3 MB" style size for notices.
pub fn human_size(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        None => format!("over {} MB", INLINE_LIMIT / (1024 * 1024)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_only_goes_to_slack_hosts() {
        let ok = |u: &str| is_slack_host(&reqwest::Url::parse(u).unwrap());
        assert!(ok("https://files.slack.com/files-pri/T1-F1/x.png"));
        assert!(ok("https://acme.slack.com/files-pri/T1-F1/x.png"));
        assert!(ok("https://a.slack-edge.com/x"));
        assert!(!ok("http://files.slack.com/x"));
        assert!(!ok("https://evilslack.com/x"));
        assert!(!ok("https://slack.com.evil.io/x"));
        assert!(!ok("https://s3.amazonaws.com/x"));
    }
}

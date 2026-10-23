//! Media transfer helpers.
//!
//! Slack→Discord: download `url_private` with the bot token as a bearer header,
//! then hand the bytes to a Discord webhook as an attachment.
//! Discord→Slack: download the Discord CDN url (public, no auth) and push it
//! through Slack's external upload flow.

use anyhow::{Context, Result};
use reqwest::Client;

/// Max bytes we attempt to re-upload inline. Above this we fall back to posting
/// the original link instead. Discord's free limit is 20 MB (2026); keep a
/// margin for webhook overhead.
pub const INLINE_LIMIT: u64 = 19 * 1024 * 1024;

pub struct DownloadedFile {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub mimetype: Option<String>,
}

/// Download a Slack private file. `token` is the bot token (xoxb-).
pub async fn download_slack_file(
    http: &Client,
    url_private: &str,
    filename: &str,
    token: &str,
) -> Result<DownloadedFile> {
    let resp = http
        .get(url_private)
        .bearer_auth(token)
        .send()
        .await
        .context("slack file download request failed")?
        .error_for_status()?;
    let mimetype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp.bytes().await?.to_vec();
    Ok(DownloadedFile {
        filename: filename.to_string(),
        bytes,
        mimetype,
    })
}

/// Download a public Discord attachment by CDN url.
pub async fn download_discord_attachment(
    http: &Client,
    url: &str,
    filename: &str,
) -> Result<DownloadedFile> {
    let resp = http.get(url).send().await?.error_for_status()?;
    let mimetype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp.bytes().await?.to_vec();
    Ok(DownloadedFile {
        filename: filename.to_string(),
        bytes,
        mimetype,
    })
}

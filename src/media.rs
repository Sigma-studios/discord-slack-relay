//! Media transfer helpers.
//!
//! Slack→Discord: download `url_private` with the bot token as a bearer header,
//! then hand the bytes to a Discord webhook as an attachment.
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
pub async fn download_slack_file(
    http: &Client,
    url_private: &str,
    filename: &str,
    token: &str,
) -> Result<Download> {
    let resp = http
        .get(url_private)
        .bearer_auth(token)
        .send()
        .await
        .context("slack file download request failed")?
        .error_for_status()?;
    read_capped(resp, filename).await
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

//! Optional server-side content-addressing.
//!
//! The board's default contract is CID-only: a client adds content to IPFS itself and hands
//! the board a bare CID, which the board stores verbatim and never resolves. That keeps the
//! board a coordination/metadata layer, not a blob store. But a client with no local IPFS
//! (e.g. an off-LAN fleet agent) then can't author a document at all.
//!
//! When the deployment configures `ipfs_api_url` (typically the loopback Kubo API on the host
//! running the board), the board can accept raw document `content`, pin it via the IPFS HTTP
//! API, and store the returned CID — so those clients can author documents without local IPFS.
//! Without a configured backend the board stays strictly CID-only.

use anyhow::Context;
use serde::Deserialize;

/// The one field we need from an IPFS `/api/v0/add` response object.
#[derive(Deserialize)]
struct AddResponse {
    #[serde(rename = "Hash")]
    hash: String,
}

/// Pin `bytes` via the IPFS HTTP API at `api_url` (e.g. "http://127.0.0.1:5001") and return
/// the resulting CID. Uses `/api/v0/add` with a multipart file part, as Kubo expects.
pub async fn add(api_url: &str, bytes: Vec<u8>) -> anyhow::Result<String> {
    let url = format!("{}/api/v0/add?pin=true", api_url.trim_end_matches('/'));
    let part = reqwest::multipart::Part::bytes(bytes).file_name("content");
    let form = reqwest::multipart::Form::new().part("file", part);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building ipfs http client")?;
    let resp = client
        .post(&url)
        .multipart(form)
        .send()
        .await
        .context("posting to ipfs /api/v0/add")?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("ipfs add returned {code}: {body}");
    }
    // Kubo streams one JSON object per added object, newline-delimited; for a single file
    // that's one line. Take the last non-empty line (the top-level object).
    let text = resp.text().await.context("reading ipfs add response")?;
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .context("empty ipfs add response")?;
    let parsed: AddResponse =
        serde_json::from_str(line).with_context(|| format!("parsing ipfs add response: {line}"))?;
    Ok(parsed.hash)
}

/// Resolve the CID to store for a document version. Prefer an explicit precomputed `cid`;
/// otherwise content-address raw `content` server-side (which requires a configured backend).
/// The error messages start with "give " so the REST layer maps them to 400 (client input),
/// while a genuine add failure surfaces as a 500 (server fault).
pub async fn resolve_cid(
    cid: Option<&str>,
    content: Option<&str>,
    ipfs_api_url: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(cid) = cid.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(cid.to_string());
    }
    match content {
        Some(content) => match ipfs_api_url {
            Some(url) => add(url, content.as_bytes().to_vec()).await,
            None => anyhow::bail!(
                "give a `cid`: this board has no IPFS backend configured (set ipfs_api_url), so it cannot content-address raw `content`"
            ),
        },
        None => anyhow::bail!(
            "give either a `cid` (a precomputed content id) or `content` (raw bytes to content-address server-side)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_cid_prefers_explicit_cid() -> anyhow::Result<()> {
        // An explicit CID passes through verbatim, even with content + a backend present.
        let cid = resolve_cid(Some("  bafyexplicit "), Some("ignored"), Some("http://x")).await?;
        assert_eq!(cid, "bafyexplicit");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_cid_content_without_backend_is_a_client_error() {
        // Content but no configured backend: a 400-mapped "give a `cid`" error, no network.
        let err = resolve_cid(None, Some("hello"), None).await.unwrap_err().to_string();
        assert!(err.starts_with("give a `cid`"), "got: {err}");
    }

    #[tokio::test]
    async fn resolve_cid_requires_cid_or_content() {
        let err = resolve_cid(None, None, Some("http://x")).await.unwrap_err().to_string();
        assert!(err.starts_with("give either"), "got: {err}");
        // A blank cid is treated as absent.
        let err = resolve_cid(Some("   "), None, None).await.unwrap_err().to_string();
        assert!(err.starts_with("give either"), "got: {err}");
    }
}

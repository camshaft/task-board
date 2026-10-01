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

/// A permissive sanity check that `cid` looks like a bare content id, so the read gateway
/// never forwards junk (or a path-traversal attempt) to the IPFS node. Real CIDs are a single
/// token of base32/base58/base36 characters — so we require a non-empty string of ASCII
/// alphanumerics only. This is a guardrail, not a full multibase/multihash validation (the
/// IPFS node is the real authority and rejects a malformed CID itself).
pub fn is_probable_cid(cid: &str) -> bool {
    !cid.is_empty() && cid.len() <= 256 && cid.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Send an `/api/v0/cat` request for `cid` and return the response for STREAMING its body, so the
/// board never buffers a whole blob (task_757). This is the READ half of the CID-only exception
/// (see `add`): it lets the same-origin web app fetch a document's content to render it, without a
/// separate gateway. Deliberately scoped — it only cats content by CID; it never exposes the node's
/// RPC (pin management, config, ...). Status is checked here so callers only handle a 2xx body; the
/// caller either streams `.bytes_stream()` straight through (the pass-through gateway, bounded
/// memory regardless of blob size) or accumulates it under a per-call-site bound via `cat`.
pub async fn fetch(api_url: &str, cid: &str) -> anyhow::Result<reqwest::Response> {
    let url = format!("{}/api/v0/cat", api_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building ipfs http client")?;
    // Kubo's cat is a POST with the CID as the `arg` query param (reqwest url-encodes it).
    let resp = client
        .post(&url)
        .query(&[("arg", cid)])
        .send()
        .await
        .context("posting to ipfs /api/v0/cat")?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("ipfs cat returned {code}: {body}");
    }
    Ok(resp)
}

/// Read the content behind `cid` into memory for an in-process consumer, STREAMING it under an
/// incremental per-call-site `max_bytes` ceiling: it accumulates chunks and aborts the moment the
/// total would exceed `max_bytes`, so a runaway blob stops early instead of being buffered whole
/// and only then rejected (task_757). Each call site passes the bound appropriate to what it reads.
/// The pass-through gateway has no reason to hold the whole blob and streams `fetch` directly.
pub async fn cat(api_url: &str, cid: &str, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let stream = Box::pin(fetch(api_url, cid).await?.bytes_stream());
    collect_capped(stream, cid, max_bytes).await
}

/// Accumulate a byte stream into a Vec, aborting as soon as it would exceed `max_bytes`. Generic
/// over the chunk + error types so it is unit-testable without a live IPFS backend.
async fn collect_capped<S, B, E>(
    mut stream: S,
    cid: &str,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    use futures_util::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading ipfs cat response")?;
        let bytes = chunk.as_ref();
        if buf.len() + bytes.len() > max_bytes {
            anyhow::bail!(
                "content behind {cid} exceeds the {max_bytes}-byte read limit for this consumer"
            );
        }
        buf.extend_from_slice(bytes);
    }
    Ok(buf)
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
    async fn collect_capped_aborts_over_limit() {
        // Under the limit: the whole stream is accumulated.
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            vec![Ok(vec![1u8; 10]), Ok(vec![2u8; 10])];
        let got = collect_capped(tokio_stream::iter(chunks), "cidA", 100)
            .await
            .expect("under the limit should accumulate");
        assert_eq!(got.len(), 20);
        // Over the limit: aborts early with a clear error naming the limit.
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            vec![Ok(vec![1u8; 10]), Ok(vec![2u8; 10])];
        let err = collect_capped(tokio_stream::iter(chunks), "cidA", 15)
            .await
            .expect_err("over the limit must abort")
            .to_string();
        assert!(err.contains("15") && err.contains("exceeds"), "got: {err}");
    }

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
        let err = resolve_cid(None, Some("hello"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("give a `cid`"), "got: {err}");
    }

    #[test]
    fn is_probable_cid_guards_junk() {
        assert!(is_probable_cid(
            "QmVnKtNdzF7NEr7wQVjy8oUjRs9co9DbURhB5y1Q6ByEve"
        )); // base58 v0
        assert!(is_probable_cid(
            "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi"
        )); // base32 v1
        assert!(!is_probable_cid("")); // empty
        assert!(!is_probable_cid("../../etc/passwd")); // path traversal
        assert!(!is_probable_cid("bafy with space"));
        assert!(!is_probable_cid("bafy/sub")); // no slashes
    }

    #[tokio::test]
    async fn resolve_cid_requires_cid_or_content() {
        let err = resolve_cid(None, None, Some("http://x"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("give either"), "got: {err}");
        // A blank cid is treated as absent.
        let err = resolve_cid(Some("   "), None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("give either"), "got: {err}");
    }
}

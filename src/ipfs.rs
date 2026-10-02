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

/// Backoff before each retry of a transient `add` failure (task_851): a few retries over a few
/// seconds so a brief Kubo restart/blip is ridden out instead of hard-failing a document write.
/// One entry per retry (attempts = len + 1); the value is the pause BEFORE that retry.
const ADD_RETRY_BACKOFF: &[std::time::Duration] = &[
    std::time::Duration::from_millis(250),
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(2),
];

/// Pin `bytes` via the IPFS HTTP API at `api_url` (e.g. "http://127.0.0.1:5001") and return the
/// resulting CID. Uses `/api/v0/add` with a multipart file part, as Kubo expects. A transient
/// failure (a connection/send error or a 5xx from the node) is retried with backoff (task_851) so a
/// brief Kubo restart does not hard-fail the write; a 4xx (or an unparseable 2xx) is permanent and
/// returned at once.
pub async fn add(api_url: &str, bytes: Vec<u8>) -> anyhow::Result<String> {
    add_with_backoff(api_url, bytes, ADD_RETRY_BACKOFF).await
}

/// `add`, parameterized on the retry backoff so tests can drive it with zero delays. After the last
/// attempt a still-transient failure surfaces as an "ipfs backend unavailable" error, which the
/// HTTP layer maps to a retryable 503 rather than a hard 500.
async fn add_with_backoff(
    api_url: &str,
    bytes: Vec<u8>,
    backoff: &[std::time::Duration],
) -> anyhow::Result<String> {
    let url = format!("{}/api/v0/add?pin=true", api_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building ipfs http client")?;
    let attempts = backoff.len() + 1;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(backoff[attempt - 1]).await;
        }
        // Rebuild the multipart form each attempt: reqwest's Form is consumed by send().
        let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name("content");
        let form = reqwest::multipart::Form::new().part("file", part);
        match client.post(&url).multipart(form).send().await {
            Ok(resp) if resp.status().is_success() => {
                // Kubo streams one JSON object per added object, newline-delimited; for a single
                // file that's one line. Take the last non-empty line (the top-level object). A parse
                // failure here is a protocol mismatch (permanent), not transient -> return it.
                let text = resp.text().await.context("reading ipfs add response")?;
                let line = text
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .context("empty ipfs add response")?;
                let parsed: AddResponse = serde_json::from_str(line)
                    .with_context(|| format!("parsing ipfs add response: {line}"))?;
                return Ok(parsed.hash);
            }
            Ok(resp) => {
                let code = resp.status();
                let body = resp.text().await.unwrap_or_default();
                // A 4xx is a permanent client-side problem -> fail now, no retry. A 5xx is a
                // transient node problem -> remember it and fall through to the next attempt.
                if !code.is_server_error() {
                    anyhow::bail!("ipfs add returned {code}: {body}");
                }
                last_err = Some(anyhow::anyhow!("ipfs add returned {code}: {body}"));
            }
            Err(e) => {
                // Connection/send/timeout error: transient, retry.
                last_err = Some(anyhow::Error::new(e).context("posting to ipfs /api/v0/add"));
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("ipfs add failed"))
        .context(format!(
            "ipfs backend unavailable after {attempts} attempts (transient add failure); retry shortly"
        )))
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
        // task_1272: a bare content id is alphanumeric (base58/base32), never a path or phrase. A
        // path-shaped `cid` (e.g. a wiki path "designs/foo" stuffed into `cid`) was the silent
        // content-loss trap: create_document / publish_version called with a path in `cid` PLUS a
        // `content` body stored the path verbatim as the version cid and never persisted the body
        // (the doc then read empty). Reject a non-content-id `cid` loudly so a provided body is
        // never silently dropped; the legitimate real-CID-plus-content case (cid for storage,
        // content for link indexing) is unaffected since a real CID is alphanumeric.
        if !is_probable_cid(cid) {
            anyhow::bail!(
                "`cid` must be a bare content id (alphanumeric base58/base32), not a path or phrase (got '{cid}'): to store a body pass `content` (no `cid`), and set a wiki path separately with set_document_path"
            );
        }
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
    async fn resolve_cid_rejects_a_path_shaped_cid() -> anyhow::Result<()> {
        // task_1272: a wiki path stuffed into `cid` (alongside a body) was silently stored as the
        // version cid, dropping the body and yielding an empty doc. It is now a loud error, so the
        // body is never lost; a real bare CID + content (content for link indexing) still passes.
        let err = resolve_cid(
            Some("designs/task-close"),
            Some("the body"),
            Some("http://x"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("bare content id") && err.contains("designs/task-close"),
            "got: {err}"
        );
        let cid = resolve_cid(
            Some("QmVnKtNdzF7NEr7wQVjy8oUjRs9co9DbURhB5y1Q6ByEve"),
            Some("body for link indexing"),
            Some("http://x"),
        )
        .await?;
        assert_eq!(cid, "QmVnKtNdzF7NEr7wQVjy8oUjRs9co9DbURhB5y1Q6ByEve");
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

    /// Stand up a fake Kubo `/api/v0/add` on an ephemeral port that counts calls: returns a 503 for
    /// the first `fail_first` requests then a Kubo-shaped OK, or always `permanent` when set.
    async fn spawn_fake_add(
        fail_first: usize,
        permanent: Option<axum::http::StatusCode>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let app = axum::Router::new().route(
            "/api/v0/add",
            axum::routing::post(move || {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    if let Some(code) = permanent {
                        return (code, String::from("bad request"));
                    }
                    if n <= fail_first {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            String::from("node restarting"),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            String::from("{\"Hash\":\"QmFakeCid\"}\n"),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), count)
    }

    /// A transient 5xx is retried with backoff and the add eventually succeeds (task_851).
    #[tokio::test]
    async fn add_retries_transient_5xx_then_succeeds() {
        let (url, count) = spawn_fake_add(2, None).await;
        let zero = [std::time::Duration::ZERO; 3];
        let cid = add_with_backoff(&url, b"hi".to_vec(), &zero).await.unwrap();
        assert_eq!(cid, "QmFakeCid");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "2 failures + 1 success"
        );
    }

    /// A persistent 5xx gives up after the bounded attempts with a retryable "ipfs backend
    /// unavailable" error (mapped to 503), not an unbounded hang.
    #[tokio::test]
    async fn add_gives_up_after_bounded_attempts() {
        let (url, count) = spawn_fake_add(usize::MAX, None).await;
        let zero = [std::time::Duration::ZERO; 2]; // 3 attempts total
        let err = add_with_backoff(&url, b"x".to_vec(), &zero)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("ipfs backend unavailable"), "got: {err}");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// A 4xx is permanent: fail immediately, no retry.
    #[tokio::test]
    async fn add_does_not_retry_a_4xx() {
        let (url, count) = spawn_fake_add(0, Some(axum::http::StatusCode::BAD_REQUEST)).await;
        let zero = [std::time::Duration::ZERO; 2];
        let err = add_with_backoff(&url, b"x".to_vec(), &zero)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("ipfs add returned 400"), "got: {err}");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no retry on a permanent 4xx"
        );
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

//! OSS signed S3 transfers (docs/specs/aps-endpoints.md).
//!
//! Download: GET /oss/v2/buckets/{bucket}/objects/{urlenc key}/signeds3download
//!           ?minutesExpiration=60[&scopes=..]
//!           -> {"url": "...", "size": n, "status": "complete"}. Then GET url (NO Authorization
//!           header: it is a presigned S3 URL) with `Range: bytes=<offset>-` when offset > 0.
//!           Use the byte-preserving reqwest client (api.transfer_client()); never decode gzip.
//! Upload:   GET  .../signeds3upload?parts=N&firstPart=1[&uploadKey=..][&scopes=..]&minutesExpiration=60
//!           -> {"uploadKey","urls":[..]} ; request at most 25 part URLs per call.
//!           PUT each part (8 MiB, last smaller; min part 5 MiB except last) to its URL with
//!           the raw client, PARALLEL_PARTS at a time (one S3 stream is far below the link
//!           speed for large models). Each part is read once into a refcounted buffer that
//!           retries reuse without copying, so memory peaks at PARALLEL_PARTS × 8 MiB.
//!           POST .../signeds3upload[?scopes=] Body::Json {"uploadKey": k, "size": total}
//!           Retry a failed part up to 3 times (re-request its URL if 403 = expired).

use super::dm::pct;
use crate::http::{error_body, Api, ApiError, Body};
use crate::model::*;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Method;
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const PART_SIZE: usize = 8 * 1024 * 1024;
/// Parts in flight per upload.
const PARALLEL_PARTS: usize = 3;
/// S3 limit on the number of parts in one multipart upload.
const MAX_PARTS: u64 = 10_000;
/// Part URLs requested per signeds3upload GET.
const URL_BATCH: u64 = 25;
const MAX_RETRIES: u32 = 3;
/// Overall cap for one body transfer (the connect timeout is set on the shared client).
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

fn decode(msg: impl Into<String>) -> ApiError {
    ApiError::Decode(msg.into())
}

/// The `Api` error enum has no I/O variant; local file errors are reported as Decode.
fn io_err(e: std::io::Error) -> ApiError {
    decode(format!("local file: {e}"))
}

fn bucket_key(storage_urn: &str) -> Result<(&str, &str), ApiError> {
    parse_storage_urn(storage_urn).ok_or_else(|| decode("not an OSS storage urn"))
}

/// Append `scopes=` (with the right separator) when the project has OSS scopes.
fn add_scopes(mut url: String, p: &Project) -> String {
    if let Some(s) = &p.oss_scopes {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("scopes=");
        url.push_str(&pct(s));
    }
    url
}

/// Number of parts for a file of `total` bytes (an empty file is one empty part).
fn part_count(total: u64) -> Result<u64, ApiError> {
    let part = PART_SIZE as u64;
    let parts = total.div_ceil(part).max(1);
    if parts > MAX_PARTS {
        return Err(decode(format!(
            "file too large for multipart upload: {total} bytes"
        )));
    }
    Ok(parts)
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << (attempt.saturating_sub(1)).min(5))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DownloadResp {
    url: String,
    size: u64,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct UploadUrls {
    upload_key: String,
    urls: Vec<String>,
}

/// Presigned download URL and object size.
pub async fn download_url(
    api: &Api,
    p: &Project,
    storage_urn: &str,
) -> Result<(String, u64), ApiError> {
    let (bucket, key) = bucket_key(storage_urn)?;
    let url = add_scopes(
        format!(
            "/oss/v2/buckets/{}/objects/{}/signeds3download?minutesExpiration=60",
            pct(bucket),
            pct(key)
        ),
        p,
    );
    let r: DownloadResp = api.json(Method::GET, &url, None).await?;
    if r.url.is_empty() {
        return Err(decode("signeds3download: no url in response"));
    }
    Ok((r.url, r.size))
}

/// Start streaming `url` from `offset`. Caller reads with `resp.chunk().await`.
pub async fn open_download(
    api: &Api,
    url: &str,
    offset: u64,
) -> Result<reqwest::Response, ApiError> {
    // Presigned URL: deliberately no Authorization header.
    let mut req = api.transfer_client().get(url).timeout(TRANSFER_TIMEOUT);
    if offset > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = error_body(resp).await;
        return Err(ApiError::Status {
            status: status.as_u16(),
            body,
        });
    }
    if offset > 0 && status.as_u16() != 206 {
        return Err(decode("download: server ignored Range request"));
    }
    if status.as_u16() == 206 {
        validate_range_response(&resp, offset, None)?;
    }
    Ok(resp)
}

/// A 206 alone does not prove the server sent the requested bytes. Accepting a
/// different offset silently writes those bytes into the wrong part of the file.
fn validate_range_response(
    resp: &reqwest::Response,
    expected_start: u64,
    expected_end: Option<u64>,
) -> Result<(), ApiError> {
    let invalid = || decode("download: invalid or mismatched Content-Range");
    let value = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes "))
        .ok_or_else(invalid)?;
    let (range, total) = value.split_once('/').ok_or_else(invalid)?;
    let (start, last) = range.split_once('-').ok_or_else(invalid)?;
    let number = |s: &str| {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<u64>().ok()
    };
    let start = number(start).ok_or_else(invalid)?;
    let end = number(last)
        .and_then(|v| v.checked_add(1))
        .ok_or_else(invalid)?;
    let total = if total == "*" {
        None
    } else {
        Some(number(total).ok_or_else(invalid)?)
    };
    if start != expected_start || end <= start || total.is_some_and(|total| end > total) {
        return Err(invalid());
    }
    match expected_end {
        Some(expected) if end != expected => return Err(invalid()),
        // An open-ended request must include the entire remaining object. With
        // an unknown total we cannot verify that and must not accept a partial file.
        None if total != Some(end) => return Err(invalid()),
        _ => {}
    }
    if resp
        .content_length()
        .is_some_and(|length| length != end - start)
    {
        return Err(decode(
            "download: Content-Length does not match Content-Range",
        ));
    }
    Ok(())
}

/// Stream the byte range `[start, end)` of a presigned URL (used for parallel segments).
pub async fn open_range(
    api: &Api,
    url: &str,
    start: u64,
    end: u64,
) -> Result<reqwest::Response, ApiError> {
    if start >= end {
        return Err(decode("download: range must have start < end"));
    }
    let resp = api
        .transfer_client()
        .get(url)
        .timeout(TRANSFER_TIMEOUT)
        .header(reqwest::header::RANGE, format!("bytes={start}-{}", end - 1))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = error_body(resp).await;
        return Err(ApiError::Status {
            status: status.as_u16(),
            body,
        });
    }
    if status.as_u16() != 206 {
        return Err(decode("download: server ignored Range request"));
    }
    validate_range_response(&resp, start, Some(end))?;
    Ok(resp)
}

/// GET signeds3upload: a batch of part URLs starting at `first` (1-based).
async fn fetch_urls(
    api: &Api,
    p: &Project,
    bucket: &str,
    key: &str,
    first: u64,
    parts: u64,
    upload_key: Option<&str>,
) -> Result<(String, Vec<String>), ApiError> {
    let mut url = format!(
        "/oss/v2/buckets/{}/objects/{}/signeds3upload?parts={parts}&firstPart={first}&minutesExpiration=60",
        pct(bucket),
        pct(key)
    );
    if let Some(k) = upload_key {
        url.push_str("&uploadKey=");
        url.push_str(&pct(k));
    }
    let url = add_scopes(url, p);
    let r: UploadUrls = api.json(Method::GET, &url, None).await?;
    if r.urls.is_empty() {
        return Err(decode("signeds3upload: no part urls"));
    }
    Ok((r.upload_key, r.urls))
}

/// POST signeds3upload: finalize. Confirm the server committed the entire object.
/// A failure body can carry `size` as an object; partial or invalid sizes also fail.
async fn complete(
    api: &Api,
    p: &Project,
    bucket: &str,
    key: &str,
    upload_key: &str,
    total: u64,
    content_type: Option<&str>,
) -> Result<(), ApiError> {
    let url = add_scopes(
        format!(
            "/oss/v2/buckets/{}/objects/{}/signeds3upload",
            pct(bucket),
            pct(key)
        ),
        p,
    );
    let body = Body::Json(serde_json::json!({ "uploadKey": upload_key, "size": total }));
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(content_type) = content_type {
        let value = reqwest::header::HeaderValue::from_str(content_type)
            .map_err(|_| decode("upload target has an invalid MIME type"))?;
        headers.insert("x-ads-meta-content-type", value);
    }
    let bytes = api
        .request_with_headers(Method::POST, &url, Some(body), headers)
        .await?
        .bytes()
        .await?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| decode(error.to_string()))?;
    match v.get("size") {
        Some(s) if s.as_u64() == Some(total) => Ok(()),
        _ => {
            let reason = v
                .get("reason")
                .or_else(|| v.get("status"))
                .map(|r| r.to_string())
                .unwrap_or_default();
            Err(decode(format!("signeds3upload complete failed: {reason}")))
        }
    }
}

/// Upload a local file to the storage object. `progress(bytes_done, total)`.
pub async fn upload_file(
    api: &Api,
    p: &Project,
    storage_urn: &str,
    path: &Path,
    content_type: Option<&str>,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), ApiError> {
    // Validate metadata before allocating storage transfer work. The header is
    // recorded by OSS completion; it must not be added to presigned S3 part PUTs.
    if let Some(content_type) = content_type {
        reqwest::header::HeaderValue::from_str(content_type)
            .map_err(|_| decode("upload target has an invalid MIME type"))?;
    }
    let (bucket, key) = bucket_key(storage_urn)?;
    let file = Arc::new(std::fs::File::open(path).map_err(io_err)?);
    let total = file.metadata().map_err(io_err)?.len();
    let parts = part_count(total)?;
    let sent = AtomicU64::new(0);
    let mut upload_key: Option<String> = None;

    let mut first = 1;
    while first <= parts {
        let want = (parts - first + 1).min(URL_BATCH);
        let (k, urls) = fetch_urls(api, p, bucket, key, first, want, upload_key.as_deref()).await?;
        if urls.len() as u64 != want {
            return Err(decode(format!(
                "signeds3upload: asked for {want} part urls, got {}",
                urls.len()
            )));
        }
        let k = upload_key.get_or_insert(k).clone();
        futures_util::stream::iter(urls.into_iter().enumerate())
            .map(|(i, url)| {
                let part = first + i as u64;
                let (file, k, sent) = (file.clone(), k.clone(), &sent);
                async move {
                    let n = put_part(api, p, bucket, key, &k, part, url, &file, total).await?;
                    progress(sent.fetch_add(n, Ordering::Relaxed) + n, total);
                    Ok::<(), ApiError>(())
                }
            })
            .buffer_unordered(PARALLEL_PARTS)
            .try_collect::<()>()
            .await?;
        first += want;
    }

    let upload_key = upload_key.ok_or_else(|| decode("upload: no uploadKey"))?;
    complete(api, p, bucket, key, &upload_key, total, content_type).await
}

/// Read part `part` (1-based) of the file at its offset. Positional reads, so parts in flight
/// share one handle without seeking.
async fn read_part(file: Arc<std::fs::File>, part: u64, total: u64) -> Result<Bytes, ApiError> {
    let offset = (part - 1) * PART_SIZE as u64;
    let n = (total - offset).min(PART_SIZE as u64) as usize;
    tokio::task::spawn_blocking(move || {
        use std::os::windows::fs::FileExt;
        let mut buf = vec![0u8; n];
        let mut done = 0;
        while done < n {
            match file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(decode("local file: shorter than when the upload started")),
                Ok(k) => done += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        Ok(Bytes::from(buf))
    })
    .await
    .map_err(|e| decode(format!("read task: {e}")))?
}

/// PUT one part, with retries. Returns the bytes sent.
#[allow(clippy::too_many_arguments)]
async fn put_part(
    api: &Api,
    p: &Project,
    bucket: &str,
    key: &str,
    upload_key: &str,
    part: u64,
    mut url: String,
    file: &Arc<std::fs::File>,
    total: u64,
) -> Result<u64, ApiError> {
    let body = read_part(file.clone(), part, total).await?;
    let mut attempt: u32 = 0;
    let mut refreshed = false;
    loop {
        // Bytes clones share the buffer: a retry does not copy the part.
        let reason = match api
            .transfer_client()
            .put(&url)
            .timeout(TRANSFER_TIMEOUT)
            .body(body.clone())
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(body.len() as u64),
            Ok(r) if r.status().as_u16() == 403 && !refreshed => {
                // Presigned URL expired: fetch a fresh one for this part and retry.
                refreshed = true;
                let (_, fresh) = fetch_urls(api, p, bucket, key, part, 1, Some(upload_key)).await?;
                url = fresh.into_iter().next().unwrap_or_default();
                continue;
            }
            Ok(r) if r.status().is_client_error() && !matches!(r.status().as_u16(), 408 | 429) => {
                let status = r.status().as_u16();
                let body = error_body(r).await;
                return Err(ApiError::Status { status, body });
            }
            Ok(r) => format!("HTTP {}", r.status().as_u16()),
            Err(e) => e.to_string(),
        };
        attempt += 1;
        if attempt > MAX_RETRIES {
            return Err(decode(format!(
                "part {part} failed after retries: {reason}"
            )));
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Fault, Server};

    fn loopback_api(server: &Server) -> Api {
        Api::for_test(&server.origin, Duration::from_secs(2))
    }

    struct TestFile(std::path::PathBuf);

    impl TestFile {
        fn new(bytes: &[u8]) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "synka-resilience-upload-{}-{}.bin",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn proj(scopes: Option<&str>) -> Project {
        Project {
            pid: 1,
            hub_id: "b.h".into(),
            hub_name: "H".into(),
            project_id: "b.p".into(),
            name: "P".into(),
            root_folder_urn: "urn:adsk.wipemea:fs.folder:co.R".into(),
            urn_ns: "wipemea".into(),
            tops: vec![],
            oss_scopes: scopes.map(String::from),
            change_source: ChangeSourceKind::Unknown,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: Provider::Acc,
            meta: None,
        }
    }

    #[test]
    fn part_counts() {
        let part = PART_SIZE as u64;
        assert_eq!(part_count(0).unwrap(), 1);
        assert_eq!(part_count(1).unwrap(), 1);
        assert_eq!(part_count(part).unwrap(), 1);
        assert_eq!(part_count(part + 1).unwrap(), 2);
        assert_eq!(part_count(part * MAX_PARTS).unwrap(), MAX_PARTS);
        assert!(part_count(part * MAX_PARTS + 1).is_err());
    }

    #[test]
    fn key_encoding_and_scopes() {
        assert_eq!(pct("2232e616-ab.rvt"), "2232e616-ab.rvt");
        assert_eq!(pct("a/b c.rvt"), "a%2Fb%20c.rvt");
        let p = proj(Some("data:read,data:write"));
        assert_eq!(
            add_scopes("/x/signeds3upload?minutesExpiration=60".into(), &p),
            "/x/signeds3upload?minutesExpiration=60&scopes=data%3Aread%2Cdata%3Awrite"
        );
        assert_eq!(
            add_scopes("/x/y".into(), &p),
            "/x/y?scopes=data%3Aread%2Cdata%3Awrite"
        );
        assert_eq!(add_scopes("/x/y".into(), &proj(None)), "/x/y");
    }

    #[test]
    fn storage_urn_parse_and_backoff() {
        let (b, k) = bucket_key("urn:adsk.objects:os.object:wip.dm.emea.2/abc.rvt").unwrap();
        assert_eq!((b, k), ("wip.dm.emea.2", "abc.rvt"));
        assert!(bucket_key("urn:bogus").is_err());
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(3), Duration::from_millis(2000));
    }

    #[tokio::test]
    async fn parts_are_read_at_their_offsets() {
        let path = std::env::temp_dir().join(format!("synka-parts-{}.bin", std::process::id()));
        let total = PART_SIZE as u64 * 2 + 123;
        let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let file = Arc::new(std::fs::File::open(&path).unwrap());
        assert_eq!(part_count(total).unwrap(), 3);
        let mut joined = Vec::new();
        for part in 1..=3 {
            let b = read_part(file.clone(), part, total).await.unwrap();
            assert_eq!(b.len(), if part < 3 { PART_SIZE } else { 123 });
            joined.extend_from_slice(&b);
        }
        assert_eq!(joined, data);
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upload_url_batch_json() {
        let r: UploadUrls = serde_json::from_str(
            r#"{"uploadKey":"K1","uploadExpiration":"x","urlExpiration":"y","urls":["https://s3/a","https://s3/b"]}"#,
        )
        .unwrap();
        assert_eq!(r.upload_key, "K1");
        assert_eq!(r.urls.len(), 2);
    }

    #[tokio::test]
    async fn resilience_download_range_and_presigned_credentials() {
        let server = Server::scripted(vec![
            Fault::reply(200, "all bytes"),
            Fault::reply(206, "tail").header("Content-Range", "bytes 5-8/9"),
            Fault::reply(206, "by").header("Content-Range", "bytes 4-5/9"),
        ])
        .await;
        let api = loopback_api(&server);
        let url = format!("{}/signed?fake-signature=local-only", server.origin);
        assert_eq!(
            open_download(&api, &url, 0)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "all bytes"
        );
        assert_eq!(
            open_download(&api, &url, 5)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "tail"
        );
        assert_eq!(
            open_range(&api, &url, 4, 6)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "by"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].header("range"), None);
        assert_eq!(requests[1].header("range"), Some("bytes=5-"));
        assert_eq!(requests[2].header("range"), Some("bytes=4-5"));
        for request in requests {
            assert_eq!(
                request.header("authorization"),
                None,
                "presigned URLs must never receive account credentials"
            );
        }
    }

    #[tokio::test]
    async fn resilience_ignored_download_range_is_rejected() {
        let server = Server::new(|_, _| Fault::reply(200, "wrong full body")).await;
        let api = loopback_api(&server);
        let url = format!("{}/signed", server.origin);
        assert!(matches!(
            open_download(&api, &url, 5).await,
            Err(ApiError::Decode(_))
        ));
        assert!(matches!(
            open_range(&api, &url, 4, 6).await,
            Err(ApiError::Decode(_))
        ));
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn resilience_wrong_content_range_cannot_corrupt_download_segments() {
        for (range, body) in [
            (None, "by"),
            (Some("bytes 0-1/9"), "by"),
            (Some("bytes 4-6/9"), "byt"),
            (Some("bytes 4-5/5"), "by"),
            (Some("bytes 4-3/9"), "by"),
            (Some("bytes 4-5/9"), "wrong length"),
            (Some("bytes 4-18446744073709551615/*"), "by"),
            (Some("items 4-5/9"), "by"),
            (Some("bytes +4-5/9"), "by"),
        ] {
            let mut reply = Fault::reply(206, body);
            if let Some(range) = range {
                reply = reply.header("Content-Range", range);
            }
            let server = Server::scripted(vec![reply]).await;
            let api = loopback_api(&server);
            let result = open_range(&api, &format!("{}/signed", server.origin), 4, 6).await;
            assert!(
                matches!(result, Err(ApiError::Decode(_))),
                "range {range:?}"
            );
            assert_eq!(server.count(), 1);
        }
    }

    #[tokio::test]
    async fn resilience_resumed_and_initial_partial_downloads_require_complete_remainder() {
        let server = Server::scripted(vec![
            Fault::reply(206, "tail").header("Content-Range", "bytes 0-3/9"),
            Fault::reply(206, "ta").header("Content-Range", "bytes 5-6/9"),
            Fault::reply(206, "tail").header("Content-Range", "bytes 5-8/*"),
            Fault::reply(206, "head").header("Content-Range", "bytes 0-3/9"),
            Fault::reply(206, "all bytes").header("Content-Range", "bytes 0-8/9"),
        ])
        .await;
        let api = loopback_api(&server);
        let url = format!("{}/signed", server.origin);
        for offset in [5, 5, 5, 0] {
            assert!(matches!(
                open_download(&api, &url, offset).await,
                Err(ApiError::Decode(_))
            ));
        }
        assert_eq!(
            open_download(&api, &url, 0)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "all bytes"
        );
        assert_eq!(server.count(), 5);
    }

    #[tokio::test]
    async fn resilience_invalid_download_range_fails_before_network_request() {
        let server = Server::scripted(vec![]).await;
        let api = loopback_api(&server);
        let url = format!("{}/signed", server.origin);
        for (start, end) in [(0, 0), (4, 4), (5, 4)] {
            assert!(matches!(
                open_range(&api, &url, start, end).await,
                Err(ApiError::Decode(_))
            ));
        }
        assert_eq!(server.count(), 0);
    }

    #[tokio::test]
    async fn resilience_transfer_error_body_is_bounded() {
        let mut response = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 1000000000\r\n\r\n".to_vec();
        response.extend_from_slice("🧱".repeat(2048).as_bytes());
        let server = Server::new(move |_, _| Fault::OpenBody(response.clone())).await;
        let api = loopback_api(&server);
        let url = format!("{}/signed", server.origin);
        let error = tokio::time::timeout(Duration::from_secs(1), open_download(&api, &url, 0))
            .await
            .expect("transfer diagnostics must not consume an unfinished body")
            .unwrap_err();
        match error {
            ApiError::Status { status, body } => {
                assert_eq!(status, 403);
                assert_eq!(body.len(), 2048);
                assert!(body.chars().all(|c| c == '🧱'));
            }
            error => panic!("expected status error, got {error}"),
        }
    }

    #[tokio::test]
    async fn resilience_gzip_encoded_object_keeps_original_stored_bytes() {
        // An S3 object can carry Content-Encoding:gzip as stored metadata. It
        // remains an opaque file: cloud byte offsets and sizes describe these
        // compressed bytes, not the transparently decoded text inside them.
        let stored = vec![
            31, 139, 8, 0, 0, 0, 0, 0, 0, 10, 203, 47, 72, 44, 44, 77, 85, 72, 206, 207, 45, 40,
            74, 45, 46, 78, 77, 81, 200, 79, 202, 74, 77, 46, 81, 72, 170, 44, 73, 45, 6, 0, 0,
            203, 253, 103, 30, 0, 0, 0,
        ];
        let server = Server::scripted(vec![
            Fault::reply(200, stored.clone()).header("Content-Encoding", "gzip"),
            Fault::reply(206, stored.clone())
                .header("Content-Encoding", "gzip")
                .header("Content-Range", "bytes 0-49/50"),
            Fault::reply(200, stored.clone()).header("Content-Encoding", "gzip"),
        ])
        .await;
        let client = crate::http::build_client();
        let api = Api::new(
            client.clone(),
            Arc::new(crate::auth::Auth::for_test(client)),
        );
        let url = format!("{}/opaque-model.ifc.gz", server.origin);
        let bytes = open_download(&api, &url, 0)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        eprintln!(
            "OPAQUE_GZIP_OBJECT: stored_bytes={}, downloaded_bytes={}",
            stored.len(),
            bytes.len()
        );
        assert_eq!(
            bytes.as_ref(),
            stored.as_slice(),
            "file content and resume offsets must stay byte-exact"
        );
        let bytes = open_range(&api, &url, 0, 50)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(
            bytes.as_ref(),
            stored.as_slice(),
            "range responses must not be decoded either"
        );
        let metadata_decoded = api
            .client()
            .get(&url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(
            metadata_decoded.as_ref(),
            b"opaque compressed object bytes",
            "the metadata client must retain automatic gzip decoding"
        );
    }

    #[tokio::test]
    async fn resilience_expired_upload_url_refreshes_once_and_preserves_bytes() {
        let server = Server::new(|request, _| {
            if request.target == "/expired" {
                Fault::reply(403, "expired")
            } else if request.target.starts_with("/oss/") {
                let host = request.header("host").unwrap();
                Fault::reply(
                    200,
                    serde_json::json!({
                        "uploadKey": "synthetic-key", "urls": [format!("http://{host}/fresh")]
                    })
                    .to_string(),
                )
            } else {
                Fault::reply(200, "")
            }
        })
        .await;
        let payload = b"synthetic upload data, never a user file";
        let temp = TestFile::new(payload);
        let file = Arc::new(std::fs::File::open(&temp.0).unwrap());
        let count = put_part(
            &loopback_api(&server),
            &proj(None),
            "bucket",
            "key",
            "synthetic-key",
            1,
            format!("{}/expired", server.origin),
            &file,
            payload.len() as u64,
        )
        .await
        .unwrap();
        assert_eq!(count, payload.len() as u64);
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].body, payload);
        assert_eq!(requests[2].body, payload);
        assert_eq!(requests[0].header("authorization"), None);
        assert_eq!(requests[2].header("authorization"), None);
        assert!(requests[1].header("authorization").is_some());
    }

    #[tokio::test]
    async fn resilience_full_upload_recovers_part_failure_and_reports_exact_progress() {
        let parts = Arc::new(AtomicU64::new(0));
        let attempts = parts.clone();
        let server = Server::new(move |request, _| {
            if request.target == "/part" {
                if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    Fault::reply(503, "try again")
                } else {
                    Fault::reply(200, "")
                }
            } else if request.method == "GET" {
                let host = request.header("host").unwrap();
                Fault::reply(
                    200,
                    serde_json::json!({
                        "uploadKey": "synthetic-key", "urls": [format!("http://{host}/part")]
                    })
                    .to_string(),
                )
            } else {
                let completed: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                Fault::reply(
                    200,
                    serde_json::json!({"size": completed["size"]}).to_string(),
                )
            }
        })
        .await;
        let payload = b"synthetic upload used by a local test only!";
        let temp = TestFile::new(payload);
        let progress = std::sync::Mutex::new(Vec::new());
        upload_file(
            &loopback_api(&server),
            &proj(None),
            "urn:adsk.objects:os.object:bucket/key",
            &temp.0,
            Some("application/vnd.autodesk.revit"),
            &|done, total| progress.lock().unwrap().push((done, total)),
        )
        .await
        .unwrap();
        assert_eq!(
            *progress.lock().unwrap(),
            vec![(payload.len() as u64, payload.len() as u64)]
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1].body, payload);
        assert_eq!(requests[2].body, payload);
        let complete: serde_json::Value = serde_json::from_slice(&requests[3].body).unwrap();
        assert_eq!(complete["size"], payload.len());
        assert_eq!(complete["uploadKey"], "synthetic-key");
        assert_eq!(
            requests[3].header("x-ads-meta-content-type"),
            Some("application/vnd.autodesk.revit")
        );
        for part in &requests[1..3] {
            assert_eq!(part.header("x-ads-meta-content-type"), None);
            assert_eq!(part.header("authorization"), None);
        }
    }

    #[tokio::test]
    async fn resilience_upload_completion_malformed_json_remains_a_decode_failure() {
        let server = Server::scripted(vec![Fault::reply(200, r#"{"size":"#)]).await;
        let error = complete(
            &loopback_api(&server),
            &proj(None),
            "bucket",
            "key",
            "synthetic-key",
            17,
            Some("application/vnd.autodesk.revit"),
        )
        .await
        .expect_err("malformed completion was accepted");
        assert!(
            matches!(error, ApiError::Decode(_)),
            "malformed JSON must consume bounded retries, not postpone indefinitely as offline"
        );
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn resilience_upload_completion_rejects_wrong_or_invalid_committed_size() {
        for size in [
            serde_json::json!(0),
            serde_json::json!(42),
            serde_json::json!(-1),
            serde_json::json!(43.0),
            serde_json::json!(43.5),
            serde_json::json!("43"),
            serde_json::json!({"error": "not committed"}),
            serde_json::Value::Null,
        ] {
            let server = Server::scripted(vec![Fault::reply(
                200,
                serde_json::json!({
                    "size": size, "reason": "injected incomplete upload"
                })
                .to_string(),
            )])
            .await;
            let result = complete(
                &loopback_api(&server),
                &proj(None),
                "bucket",
                "key",
                "synthetic-key",
                43,
                None,
            )
            .await;
            assert!(matches!(result, Err(ApiError::Decode(_))), "size {size}");
            assert_eq!(
                server.count(),
                1,
                "completion must not repeat a successful POST"
            );
        }
        let server = Server::scripted(vec![Fault::reply(200, r#"{"size":0}"#)]).await;
        complete(
            &loopback_api(&server),
            &proj(None),
            "bucket",
            "key",
            "synthetic-key",
            0,
            None,
        )
        .await
        .unwrap();
    }
}

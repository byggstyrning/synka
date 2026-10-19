//! Shared metadata/byte-preserving transfer clients + the APS request helper.
//!
//! - reqwest with rustls, gzip, HTTP/2, pool_idle_timeout 90 s, connect timeout 15 s,
//!   per-request timeout 100 s for metadata (transfers set their own, see api::oss).
//! - Bearer token per request (never shared mutable default headers).
//! - Retry: 429/408/500/502/503/504 and transport errors: honour Retry-After (seconds or
//!   HTTP date, capped 60 s), else exponential backoff 1,2,4 s with ±20 % jitter, max 3 retries.
//!   Non-idempotent writes retry only explicit 429 rejection or a connection failure
//!   before delivery. Ambiguous transport/408/5xx failures return to outbox reconciliation.
//!   401: `auth.invalidate()` then retry once. 403/404/400/409: no retry.
//! - User-Agent: "synka/<CARGO_PKG_VERSION>".
//! - Log at debug: method, path (no query), status, elapsed ms. Never log bodies or tokens.

use crate::auth::Auth;
use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::debug;

pub const BASE: &str = "https://developer.api.autodesk.com";

const MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(100);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_ERROR_BODY: usize = 2048;
const JSON_API: &str = "application/vnd.api+json";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("auth: {0}")]
    Auth(anyhow::Error),
    #[error("decode: {0}")]
    Decode(String),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Body variants for `Api::request`.
pub enum Body {
    /// application/json
    Json(serde_json::Value),
    /// application/vnd.api+json (Data Management JSON:API writes)
    JsonApi(serde_json::Value),
}

#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    transfer_client: reqwest::Client,
    auth: Arc<Auth>,
    #[cfg(test)]
    test_policy: Option<TestPolicy>,
}

#[cfg(test)]
#[derive(Clone)]
struct TestPolicy {
    origin: String,
    timeout: Duration,
    retry_delay: Duration,
}

/// Build the shared reqwest client (also handed to Auth for token calls).
pub fn build_client() -> reqwest::Client {
    client_builder()
        .gzip(true)
        .build()
        .expect("reqwest-klienten kan inte byggas (cannot build reqwest client)")
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(concat!("synka/", env!("CARGO_PKG_VERSION")))
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(15))
}

/// Cloud object bytes and Range offsets must be preserved even when the stored
/// object advertises Content-Encoding:gzip. Metadata JSON keeps its usual decoder.
fn build_transfer_client() -> reqwest::Client {
    client_builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()
        .expect("cannot build byte-preserving transfer client")
}

impl Api {
    pub fn new(client: reqwest::Client, auth: Arc<Auth>) -> Api {
        Api {
            client,
            transfer_client: build_transfer_client(),
            auth,
            #[cfg(test)]
            test_policy: None,
        }
    }

    /// The production HTTP path with synthetic credentials and short, deterministic waits.
    /// Compiled only into tests; the origin must be a loopback address.
    #[cfg(test)]
    pub(crate) fn for_test(origin: &str, timeout: Duration) -> Self {
        let parsed = url::Url::parse(origin).expect("valid test origin");
        assert_eq!(parsed.scheme(), "http");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        let builder = || {
            reqwest::Client::builder()
                .no_proxy()
                .http1_only()
                .pool_max_idle_per_host(0)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(timeout)
        };
        let client = builder().gzip(true).build().unwrap();
        let transfer_client = builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .unwrap();
        let auth = Arc::new(Auth::for_test(client.clone()));
        Self {
            client,
            transfer_client,
            auth,
            test_policy: Some(TestPolicy {
                origin: origin.to_string(),
                timeout,
                retry_delay: Duration::from_millis(2),
            }),
        }
    }

    fn request_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(policy) = &self.test_policy {
            return policy.timeout;
        }
        REQUEST_TIMEOUT
    }

    fn retry_delay(&self, attempt: u32) -> Duration {
        #[cfg(test)]
        if let Some(policy) = &self.test_policy {
            return policy.retry_delay;
        }
        backoff(attempt)
    }
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
    pub(crate) fn transfer_client(&self) -> &reqwest::Client {
        &self.transfer_client
    }
    /// Authenticated request with the retry policy above. `url` may be absolute or a path
    /// starting with '/' (then BASE is prepended). Returns the response only for 2xx;
    /// non-2xx becomes ApiError::Status with the body truncated to 2 KB.
    /// JSON:API requests also send `Accept: application/vnd.api+json`.
    pub async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Body>,
    ) -> Result<reqwest::Response, ApiError> {
        self.request_with_headers(method, url, body, reqwest::header::HeaderMap::new())
            .await
    }

    /// Extra request metadata, rebuilt with the body on every authenticated retry.
    pub(crate) async fn request_with_headers(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Body>,
        headers: reqwest::header::HeaderMap,
    ) -> Result<reqwest::Response, ApiError> {
        let base = BASE;
        #[cfg(test)]
        let base = self
            .test_policy
            .as_ref()
            .map(|p| p.origin.as_str())
            .unwrap_or(base);
        let full = if url.starts_with('/') {
            format!("{base}{url}")
        } else {
            url.to_string()
        };
        let log_path = full.split('?').next().unwrap_or(&full).to_string();
        let json_api = matches!(body, Some(Body::JsonApi(_)));

        let mut retries = 0u32;
        let mut reauthed = false;
        loop {
            let token = self.auth.access_token().await.map_err(ApiError::Auth)?;
            let mut rb = self
                .client
                .request(method.clone(), &full)
                .headers(headers.clone())
                .bearer_auth(&token)
                .timeout(self.request_timeout());
            if json_api {
                rb = rb.header(ACCEPT, JSON_API);
            }
            // Rebuilt on every attempt from owned data, so retries resend the same body.
            match &body {
                None => {}
                Some(Body::Json(v)) => rb = rb.json(v),
                Some(Body::JsonApi(v)) => {
                    let bytes =
                        serde_json::to_vec(v).map_err(|e| ApiError::Decode(e.to_string()))?;
                    rb = rb.header(CONTENT_TYPE, JSON_API).body(bytes);
                }
            }

            let started = Instant::now();
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => {
                    debug!(method = %method, path = %log_path, error = %e, "transport error");
                    // A lost response does not mean a write failed: ACC may have
                    // committed it already. Replaying POST/PATCH can create another
                    // version. The outbox must reconcile its persisted intent first.
                    if retries < MAX_RETRIES && (method.is_idempotent() || e.is_connect()) {
                        tokio::time::sleep(self.retry_delay(retries)).await;
                        retries += 1;
                        continue;
                    }
                    return Err(ApiError::Transport(e));
                }
            };

            let status = resp.status();
            debug!(
                method = %method,
                path = %log_path,
                status = status.as_u16(),
                ms = started.elapsed().as_millis() as u64,
                "api"
            );

            if status.is_success() {
                return Ok(resp);
            }
            if status == StatusCode::UNAUTHORIZED && !reauthed {
                reauthed = true;
                self.auth.invalidate().await;
                continue;
            }
            if retryable(status)
                && retries < MAX_RETRIES
                && (method.is_idempotent() || status == StatusCode::TOO_MANY_REQUESTS)
            {
                let wait = resp
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| parse_retry_after(s, Utc::now()))
                    .unwrap_or_else(|| self.retry_delay(retries));
                retries += 1;
                drop(resp);
                tokio::time::sleep(wait).await;
                continue;
            }

            return Err(ApiError::Status {
                status: status.as_u16(),
                body: error_body(resp).await,
            });
        }
    }

    /// `request` + deserialize JSON (any content type).
    pub async fn json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Body>,
    ) -> Result<T, ApiError> {
        let resp = self.request(method, url, body).await?;
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
    }
}

/// Stop consuming an error response once the diagnostic limit is reached. Truncating
/// after `Response::text()` still buffers an arbitrarily large (or endless) server body.
pub(crate) async fn error_body(mut resp: reqwest::Response) -> String {
    let mut bytes = Vec::with_capacity(MAX_ERROR_BODY);
    while bytes.len() < MAX_ERROR_BODY {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(MAX_ERROR_BODY - bytes.len());
                bytes.extend_from_slice(&chunk[..take]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    truncate(&text, MAX_ERROR_BODY).to_string()
}

fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

/// Exponential backoff 1, 2, 4 s for attempt 0, 1, 2, with ±20 % jitter.
fn backoff(attempt: u32) -> Duration {
    let base = (1u64 << attempt.min(6)) as f64;
    let mut b = [0u8; 4];
    let unit = match getrandom::getrandom(&mut b) {
        Ok(()) => u32::from_le_bytes(b) as f64 / u32::MAX as f64,
        Err(_) => 0.5,
    };
    let jitter = 0.8 + 0.4 * unit;
    Duration::from_secs_f64(base * jitter)
}

/// Retry-After as delta-seconds or an HTTP date (RFC 2822 form), capped at 60 s.
/// Unparseable values give None (the caller falls back to backoff).
fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER));
    }
    let when = DateTime::parse_from_rfc2822(v).ok()?;
    let delta = when.signed_duration_since(now).num_seconds();
    Some(Duration::from_secs(delta.max(0) as u64).min(MAX_RETRY_AFTER))
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Fault, Server};
    use reqwest::Method;

    fn test_api(server: &Server) -> Api {
        Api::for_test(&server.origin, Duration::from_secs(2))
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc2822("Wed, 21 Oct 2015 07:28:00 +0000")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn retry_after_seconds() {
        assert_eq!(parse_retry_after("5", now()), Some(Duration::from_secs(5)));
        assert_eq!(parse_retry_after(" 0 ", now()), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("120", now()),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn retry_after_http_date() {
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:30 GMT", now()),
            Some(Duration::from_secs(30))
        );
        // Date in the past: no wait.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now()),
            Some(Duration::ZERO)
        );
        // Far future: capped.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 09:28:00 GMT", now()),
            Some(MAX_RETRY_AFTER)
        );
    }

    #[test]
    fn retry_after_garbage() {
        assert_eq!(parse_retry_after("soon", now()), None);
        assert_eq!(parse_retry_after("", now()), None);
    }

    #[test]
    fn backoff_ranges() {
        for _ in 0..50 {
            let b0 = backoff(0).as_secs_f64();
            assert!((0.8..=1.2).contains(&b0), "{b0}");
            let b2 = backoff(2).as_secs_f64();
            assert!((3.2..=4.8).contains(&b2), "{b2}");
        }
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 2), "ab");
        let s = "åäö";
        assert_eq!(truncate(s, 3), "å");
    }

    #[tokio::test]
    async fn resilience_transient_status_matrix_recovers() {
        for status in [408, 429, 500, 502, 503, 504] {
            let server = Server::scripted(vec![
                Fault::reply(status, "temporary").header("Retry-After", "0"),
                Fault::reply(200, r#"{"healthy":true}"#),
            ])
            .await;
            let value: serde_json::Value = test_api(&server)
                .json(Method::GET, "/status?secret=never-logged", None)
                .await
                .unwrap();
            assert_eq!(value["healthy"], true, "status {status}");
            assert_eq!(server.count(), 2, "status {status}");
            assert_eq!(server.requests()[0].target, "/status?secret=never-logged");
        }
    }

    #[tokio::test]
    async fn resilience_permanent_status_matrix_never_retries() {
        for status in [400, 403, 404, 409, 413, 422] {
            let server = Server::scripted(vec![Fault::reply(status, "permanent")]).await;
            let error = test_api(&server)
                .request(
                    Method::POST,
                    "/write",
                    Some(Body::Json(serde_json::json!({"x": 1}))),
                )
                .await
                .unwrap_err();
            assert_eq!(error.status(), Some(status));
            assert_eq!(server.count(), 1, "status {status}");
        }
    }

    #[tokio::test]
    async fn resilience_persistent_outage_has_exact_retry_budget() {
        let server =
            Server::new(|_, _| Fault::reply(503, "outage").header("Retry-After", "0")).await;
        let error = test_api(&server)
            .request(Method::GET, "/", None)
            .await
            .unwrap_err();
        assert_eq!(error.status(), Some(503));
        assert_eq!(server.count(), 1 + MAX_RETRIES as usize);
    }

    #[tokio::test]
    async fn resilience_unauthorized_reauth_attempt_is_bounded() {
        // Synthetic credentials deliberately stay available after invalidate(): this
        // verifies the HTTP 401 retry budget, not an external OAuth refresh exchange.
        let server = Server::new(|_, _| Fault::reply(401, "expired")).await;
        let error = test_api(&server)
            .request(Method::GET, "/", None)
            .await
            .unwrap_err();
        assert_eq!(error.status(), Some(401));
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn resilience_write_retry_preserves_body_and_json_api_headers() {
        let server = Server::scripted(vec![
            Fault::reply(429, "throttled").header("Retry-After", "0"),
            Fault::reply(200, "{}"),
        ])
        .await;
        let body = serde_json::json!({"data": {"name": "åäö 🏗", "items": [1, 2, 3]}});
        test_api(&server)
            .request(Method::POST, "/write", Some(Body::JsonApi(body.clone())))
            .await
            .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.method, "POST");
            assert_eq!(request.header("content-type"), Some(JSON_API));
            assert_eq!(request.header("accept"), Some(JSON_API));
            assert_eq!(
                request.header("authorization"),
                Some("Bearer synka-loopback-test-token")
            );
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
                body
            );
        }
        assert_eq!(requests[0].body, requests[1].body);
    }

    #[tokio::test]
    async fn resilience_disconnect_recovers_and_persistent_disconnect_stops() {
        let server = Server::scripted(vec![Fault::Disconnect, Fault::reply(200, "{}")]).await;
        test_api(&server)
            .json::<serde_json::Value>(Method::GET, "/", None)
            .await
            .unwrap();
        assert_eq!(server.count(), 2);

        let server = Server::new(|_, _| Fault::Disconnect).await;
        let error = test_api(&server)
            .request(Method::GET, "/", None)
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Transport(_)));
        assert_eq!(server.count(), 4);
    }

    #[tokio::test]
    async fn resilience_refused_connection_returns_without_hanging() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let api = Api::for_test(&origin, Duration::from_millis(100));
        let result =
            tokio::time::timeout(Duration::from_secs(3), api.request(Method::GET, "/", None))
                .await
                .expect("refused connection must be bounded");
        assert!(matches!(result, Err(ApiError::Transport(_))));
    }

    #[tokio::test]
    async fn resilience_timeout_retries_then_stops() {
        let server = Server::new(|_, _| Fault::Stall).await;
        let api = Api::for_test(&server.origin, Duration::from_millis(80));
        let error =
            tokio::time::timeout(Duration::from_secs(3), api.request(Method::GET, "/", None))
                .await
                .expect("stall must be bounded")
                .unwrap_err();
        assert!(matches!(error, ApiError::Transport(e) if e.is_timeout()));
        assert_eq!(server.count(), 4);
    }

    #[tokio::test]
    async fn resilience_slow_first_response_recovers() {
        let server = Server::scripted(vec![
            Fault::Delay(
                Duration::from_millis(300),
                Box::new(Fault::reply(200, "{}")),
            ),
            Fault::reply(200, r#"{"recovered":true}"#),
        ])
        .await;
        let api = Api::for_test(&server.origin, Duration::from_millis(80));
        let result: serde_json::Value = api.json(Method::GET, "/", None).await.unwrap();
        assert_eq!(result["recovered"], true);
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn resilience_malformed_and_truncated_json_are_errors() {
        let server = Server::scripted(vec![Fault::reply(200, r#"{"broken": ]"#)]).await;
        let error = test_api(&server)
            .json::<serde_json::Value>(Method::GET, "/", None)
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Decode(_)));
        assert_eq!(
            server.count(),
            1,
            "invalid JSON must not repeat a successful write"
        );

        let server = Server::scripted(vec![Fault::Raw(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"cut\":"
                .to_vec(),
        )])
        .await;
        let error = test_api(&server)
            .json::<serde_json::Value>(Method::GET, "/", None)
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Transport(_)));
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn resilience_oversized_unfinished_error_body_is_bounded() {
        let mut response =
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 1000000000\r\n\r\n".to_vec();
        response.extend_from_slice("å🧱".repeat(2000).as_bytes());
        let server = Server::scripted(vec![Fault::OpenBody(response)]).await;
        let api = test_api(&server);
        let error =
            tokio::time::timeout(Duration::from_secs(1), api.request(Method::GET, "/", None))
                .await
                .expect("diagnostic reader must stop before the unfinished body times out")
                .unwrap_err();
        match error {
            ApiError::Status { status, body } => {
                assert_eq!(status, 400);
                assert!(body.len() <= MAX_ERROR_BODY);
                assert!(!body.is_empty());
                assert!(body.starts_with("å🧱"));
            }
            error => panic!("expected bounded status error, got {error}"),
        }
    }

    #[tokio::test]
    async fn resilience_cancelled_request_stops_retrying() {
        let server = Server::new(|_, _| Fault::Stall).await;
        let api = test_api(&server);
        let task = tokio::spawn(async move { api.request(Method::GET, "/", None).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.count() == 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn resilience_lost_committed_version_reply_does_not_create_duplicate_version() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let committed = Arc::new(AtomicUsize::new(0));
        let remote = committed.clone();
        let server = Server::new(move |request, _| {
            assert_eq!(request.method, "POST");
            assert!(request.target.ends_with("/versions"));
            let version = 8 + remote.fetch_add(1, Ordering::SeqCst);
            if version == 8 {
                // ACC committed version 8. A proxy/socket failure loses the response.
                Fault::Disconnect
            } else {
                Fault::reply(201, serde_json::json!({
                    "data": {"type":"versions", "id": format!("urn:adsk.wipemea:fs.file:vf.synthetic?version={version}"),
                        "attributes": {"versionNumber": version}}
                }).to_string())
            }
        }).await;
        let result = crate::api::dm::create_version(
            &test_api(&server),
            &crate::test_support::project(),
            "synthetic-lineage",
            "model.rvt",
            "urn:adsk.objects:os.object:synthetic-bucket/uploaded-model",
        )
        .await;
        let commits = committed.load(Ordering::SeqCst);
        eprintln!(
            "LOST_VERSION_REPLY: committed_versions={commits}, HTTP_POSTs={}, result={result:?}",
            server.count()
        );
        assert_eq!(
            commits, 1,
            "a committed version with a lost response must be reconciled before another POST"
        );
        assert!(matches!(result, Err(ApiError::Transport(_))));
    }

    #[tokio::test]
    async fn resilience_lost_committed_item_reply_does_not_resend_create() {
        let server = Server::new(|request, n| {
            assert_eq!(request.method, "POST");
            assert!(request.target.ends_with("/items"));
            if n == 0 {
                // The item exists remotely, although the caller never receives its ID.
                Fault::Disconnect
            } else {
                Fault::reply(
                    409,
                    r#"{"errors":[{"code":"CONFLICT","detail":"Name already exists"}]}"#,
                )
            }
        })
        .await;
        let result = crate::api::dm::create_item(
            &test_api(&server),
            &crate::test_support::project(),
            "co.ROOT",
            "new-model.rvt",
            "urn:adsk.objects:os.object:synthetic-bucket/uploaded-model",
        )
        .await;
        eprintln!(
            "LOST_ITEM_REPLY: committed_items=1, HTTP_POSTs={}, result={result:?}",
            server.count()
        );
        assert_eq!(server.count(), 1, "retrying an ambiguously committed create incorrectly turns success into a name conflict");
        assert!(matches!(result, Err(ApiError::Transport(_))));
    }

    #[tokio::test]
    async fn resilience_committed_version_proxy_errors_do_not_replay_mutation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for status in [408, 500, 502, 503, 504] {
            let committed = Arc::new(AtomicUsize::new(0));
            let remote = committed.clone();
            let server = Server::new(move |request, _| {
                assert_eq!(request.method, "POST");
                remote.fetch_add(1, Ordering::SeqCst);
                // The origin committed, then an intermediary emitted an error.
                Fault::reply(status, "proxy lost the successful upstream reply")
                    .header("Retry-After", "0")
            })
            .await;
            let result = crate::api::dm::create_version(
                &test_api(&server),
                &crate::test_support::project(),
                "synthetic-lineage",
                "model.rvt",
                "urn:adsk.objects:os.object:synthetic-bucket/uploaded-model",
            )
            .await;
            assert_eq!(result.unwrap_err().status(), Some(status));
            assert_eq!(committed.load(Ordering::SeqCst), 1, "status {status}");
            assert_eq!(server.count(), 1);
        }
    }

    async fn spam_requests(operations: usize) {
        use std::collections::HashMap;
        use std::sync::Mutex;

        let seed = crate::test_support::test_seed();
        let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let recorded = attempts.clone();
        let server = Server::new(move |request, _| {
            let id: u32 = request.target.trim_start_matches("/spam/").parse().unwrap();
            let mut seen = recorded.lock().unwrap();
            let count = seen.entry(request.target.clone()).or_default();
            *count += 1;
            let roll = id
                .wrapping_mul(1664527)
                .wrapping_add(seed)
                .wrapping_add(1013904223);
            match roll % 5 {
                0 => Fault::reply(503, "persistent").header("Retry-After", "0"),
                1 => Fault::reply(403, "permanent"),
                2 if *count == 1 => Fault::Disconnect,
                3 if *count == 1 => Fault::reply(429, "throttled").header("Retry-After", "0"),
                _ => Fault::reply(200, r#"{"ok":true}"#),
            }
        })
        .await;
        let api = test_api(&server);
        let work = futures_util::stream::iter(0..operations).map(|id| {
            let api = api.clone();
            async move {
                let result = api
                    .json::<serde_json::Value>(Method::GET, &format!("/spam/{id}"), None)
                    .await;
                let roll = (id as u32)
                    .wrapping_mul(1664527)
                    .wrapping_add(seed)
                    .wrapping_add(1013904223);
                match roll % 5 {
                    0 => assert_eq!(result.unwrap_err().status(), Some(503)),
                    1 => assert_eq!(result.unwrap_err().status(), Some(403)),
                    _ => assert_eq!(result.unwrap()["ok"], true),
                }
            }
        });
        use futures_util::StreamExt;
        tokio::time::timeout(
            Duration::from_secs(60),
            work.buffer_unordered(24).collect::<Vec<_>>(),
        )
        .await
        .expect("bounded concurrent spam must complete");
        let attempts = attempts.lock().unwrap();
        assert_eq!(attempts.len(), operations);
        for (target, count) in attempts.iter() {
            let id: u32 = target.trim_start_matches("/spam/").parse().unwrap();
            let roll = id
                .wrapping_mul(1664527)
                .wrapping_add(seed)
                .wrapping_add(1013904223);
            let expected = match roll % 5 {
                0 => 4,
                1 | 4 => 1,
                _ => 2,
            };
            assert_eq!(*count, expected, "{target}, seed {seed}");
        }
        assert!(server.count() <= operations * 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resilience_concurrent_request_spam_has_bounded_retries() {
        spam_requests(100).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "extended stress tier; run via the resilience test runner"]
    async fn resilience_stress_seeded_request_storm() {
        spam_requests(crate::test_support::stress_operations()).await;
    }
}

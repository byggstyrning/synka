//! StreamBIM sign-in: region + e-mail + password, optional TOTP code (MFA). See
//! docs/PROVIDERS.md "StreamBIM" and docs/specs/streambim-endpoints.md section 2.
//!
//! Files in the state directory: `token-streambim.bin` (DPAPI-protected JSON with the refresh
//! material) and `user-streambim.json` (plain JSON profile with the region and base URL). The
//! password, the tokens and the MFA session are never logged or put in an error message; only
//! HTTP status codes are logged.

// `login`, `verify_mfa`, `sign_out` and `LoginOutcome` are called by the sign-in commands of
// the IPC layer, which is wired in a later step; until then only the tests call them.

use crate::auth::{dpapi_protect, dpapi_unprotect, UserInfo};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

const TOKEN_FILE: &str = "token-streambim.bin";
const PROFILE_FILE: &str = "user-streambim.json";
/// An MFA challenge is accepted this long after the password step.
const CHALLENGE_TTL_SECS: i64 = 5 * 60;
/// The id token is refreshed when less than this is left of its lifetime.
const REFRESH_MARGIN_SECS: i64 = 5 * 60;
/// Used when a reply has no `expiresIn` (undocumented; docs/specs/streambim-endpoints.md 2.2).
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const JSON_API: &str = "application/vnd.api+json";
const NOT_SIGNED_IN: &str = "Inte inloggad på StreamBIM – logga in igen.";
const SESSION_EXPIRED: &str = "StreamBIM-sessionen har gått ut – logga in igen. (Session expired)";
const CODE_EXPIRED: &str = "Koden har gått ut – logga in igen.";
const REFRESH_RETRY: &str =
    "StreamBIM-sessionen kunde inte förnyas: inget giltigt svar. Försök igen.";

/// What a password login leads to.
#[derive(Debug)]
pub enum LoginOutcome {
    /// The engine reads the cached profile (`user()`); the value is for callers that want it.
    SignedIn(#[allow(dead_code)] UserInfo),
    /// The account has MFA: call `verify_mfa` with the TOTP code.
    MfaRequired,
}

/// Holds the StreamBIM session (refresh material and the signed-in user).
pub struct StreamBimAuth {
    http: reqwest::Client,
    state_dir: PathBuf,
    state: Mutex<State>,
    /// One refresh request at a time. Callers that wait re-check the token before refreshing.
    refresh: tokio::sync::Mutex<()>,
    /// Test only: every region resolves to this origin (a loopback server).
    #[cfg(test)]
    base_override: Option<String>,
}

#[derive(Default)]
struct State {
    profile: Option<Profile>,
    tokens: Option<Tokens>,
    /// The token file has been read (or there is nothing to read).
    tokens_loaded: bool,
    /// Between the password step and the TOTP step. Memory only.
    pending: Option<Challenge>,
}

/// `user-streambim.json`: the user plus where the account lives.
#[derive(Serialize, Deserialize, Clone)]
struct Profile {
    user_id: String,
    name: String,
    email: String,
    region: String,
    base: String,
}

/// `token-streambim.bin` after DPAPI: no Debug, so the tokens cannot end up in a log line.
#[derive(Serialize, Deserialize, Clone)]
struct Tokens {
    refresh_token: Option<String>,
    id_token: String,
    /// Unix seconds.
    expires_at: i64,
}

#[derive(Clone)]
struct Challenge {
    region: String,
    base: String,
    username: String,
    session: String,
    /// Unix seconds.
    issued_at: i64,
}

/// The reply of `/auth/v1/login`, `/auth/v1/mfa/verify` and `/auth/v1/refresh`. Only the fields
/// used; the access token is not read.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct AuthReply {
    result: String,
    id_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    session: Option<String>,
}

/// What an auth endpoint answered.
enum Answer {
    Reply(AuthReply),
    /// A non-2xx status.
    Status(u16),
    /// No answer, or an answer that is not JSON.
    Failed,
}

impl StreamBimAuth {
    /// `state_dir` is where the token and profile files live. The profile is read now; the
    /// tokens are read on first use.
    pub fn new(http: reqwest::Client, state_dir: &Path) -> StreamBimAuth {
        let profile = std::fs::read(state_dir.join(PROFILE_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Profile>(&bytes).ok());
        StreamBimAuth {
            http,
            state_dir: state_dir.to_path_buf(),
            state: Mutex::new(State {
                profile,
                ..State::default()
            }),
            refresh: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            base_override: None,
        }
    }

    /// Test constructor: every request goes to `base_override` (a loopback origin).
    #[cfg(test)]
    pub(crate) fn for_test(
        http: reqwest::Client,
        state_dir: &Path,
        base_override: &str,
    ) -> StreamBimAuth {
        StreamBimAuth {
            base_override: Some(base_override.to_string()),
            ..StreamBimAuth::new(http, state_dir)
        }
    }

    /// Signed in: a profile with a user id, and tokens (on disk or in memory). The tokens are
    /// loaded here on first use, so a token file that cannot be read is dropped right away.
    pub fn is_signed_in(&self) -> bool {
        let has_tokens = self.cached_tokens().is_some();
        let st = self.st();
        let has_user = st
            .profile
            .as_ref()
            .is_some_and(|p| !p.user_id.trim().is_empty());
        has_user && has_tokens
    }

    pub fn user(&self) -> Option<UserInfo> {
        self.signed_in_profile().map(|p| profile_user(&p))
    }

    /// The region host once signed in, e.g. "app.streambim.com".
    pub fn host(&self) -> Option<String> {
        let base = self.signed_in_profile()?.base;
        url::Url::parse(&base).ok()?.host_str().map(str::to_string)
    }

    /// The region subdomain once signed in, e.g. "app".
    pub fn region(&self) -> Option<String> {
        self.signed_in_profile().map(|p| p.region)
    }

    /// Password step. `SignedIn` when the account has no MFA; `MfaRequired` keeps the challenge
    /// in memory for `verify_mfa`. A new login replaces an unfinished challenge.
    pub async fn login(
        &self,
        region: &str,
        username: &str,
        password: &str,
    ) -> Result<LoginOutcome> {
        let base = self.base_for_region(region)?;
        self.st().pending = None;
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            bail!("Ange e-post och lösenord för StreamBIM.");
        }
        let body = json!({ "username": username, "password": password });
        match self
            .call("login", &format!("{base}/auth/v1/login"), &body)
            .await
        {
            Answer::Reply(mut reply) => match std::mem::take(&mut reply.result).as_str() {
                "SUCCESS" => {
                    let tokens = tokens_from(reply, None, now_secs()).ok_or_else(|| {
                        anyhow!("StreamBIM-inloggningen misslyckades: svaret saknar token.")
                    })?;
                    let name = self.profile_name(&base, &tokens.id_token).await;
                    let profile = Profile {
                        user_id: username.to_string(),
                        name: name.unwrap_or_else(|| username.to_string()),
                        email: username.to_string(),
                        region: region.to_string(),
                        base,
                    };
                    self.save_session(tokens, &profile)?;
                    info!("StreamBIM: inloggad (region {region})");
                    Ok(LoginOutcome::SignedIn(profile_user(&profile)))
                }
                "CHALLENGE_REQUESTED" => {
                    let session = reply.session.filter(|s| !s.is_empty()).ok_or_else(|| {
                        anyhow!("StreamBIM-inloggningen misslyckades: MFA-sessionen saknas.")
                    })?;
                    self.st().pending = Some(Challenge {
                        region: region.to_string(),
                        base,
                        username: username.to_string(),
                        session,
                        issued_at: now_secs(),
                    });
                    info!("StreamBIM: MFA-kod krävs (region {region})");
                    Ok(LoginOutcome::MfaRequired)
                }
                _ => bail!("StreamBIM-inloggningen misslyckades: oväntat svar från servern."),
            },
            Answer::Status(400 | 401 | 403) => {
                bail!("Fel e-post eller lösenord för StreamBIM (region {region}).")
            }
            Answer::Status(status) => {
                bail!("StreamBIM-inloggningen misslyckades: HTTP {status}")
            }
            Answer::Failed => {
                bail!("StreamBIM-inloggningen misslyckades: inget giltigt svar från servern.")
            }
        }
    }

    /// TOTP step after `login` returned `MfaRequired`. The code must be six ASCII digits and the
    /// challenge at most five minutes old. Any definitive answer from the server (success or a
    /// 400/401/403 rejection) ends the challenge, so a code cannot be guessed over many tries
    /// without a new password step. A transient failure keeps it for a retry.
    pub async fn verify_mfa(&self, code: &str) -> Result<UserInfo> {
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            bail!("Ange de sex siffrorna från din autentiseringsapp.");
        }
        let challenge = match self.st().pending.take() {
            Some(c) if now_secs() - c.issued_at < CHALLENGE_TTL_SECS => c,
            _ => bail!(CODE_EXPIRED),
        };
        let body = json!({
            "username": challenge.username,
            "session": challenge.session,
            "code": code,
        });
        let url = format!("{}/auth/v1/mfa/verify", challenge.base);
        match self.call("mfa/verify", &url, &body).await {
            Answer::Reply(reply) if reply.result == "SUCCESS" => {
                let tokens = tokens_from(reply, None, now_secs()).ok_or_else(|| {
                    anyhow!("StreamBIM-verifieringen misslyckades: svaret saknar token.")
                })?;
                let name = self.profile_name(&challenge.base, &tokens.id_token).await;
                let profile = Profile {
                    user_id: challenge.username.clone(),
                    name: name.unwrap_or_else(|| challenge.username.clone()),
                    email: challenge.username.clone(),
                    region: challenge.region.clone(),
                    base: challenge.base.clone(),
                };
                self.save_session(tokens, &profile)?;
                info!("StreamBIM: MFA godkänd (region {})", challenge.region);
                Ok(profile_user(&profile))
            }
            Answer::Reply(_) => bail!("Fel kod för StreamBIM – logga in igen."),
            Answer::Status(400 | 401 | 403) => bail!("Fel kod för StreamBIM – logga in igen."),
            Answer::Status(status) => {
                self.st().pending = Some(challenge);
                bail!("StreamBIM-verifieringen misslyckades: HTTP {status}")
            }
            Answer::Failed => {
                self.st().pending = Some(challenge);
                bail!("StreamBIM-verifieringen misslyckades: inget giltigt svar från servern.")
            }
        }
    }

    /// Forget the session: memory, token file and profile. The server has no logout endpoint.
    pub async fn sign_out(&self) -> Result<()> {
        // A refresh in flight must not write its tokens back after they are deleted.
        let _turn = self.refresh.lock().await;
        *self.st() = State {
            tokens_loaded: true,
            ..State::default()
        };
        remove_if_exists(&self.token_path())?;
        remove_if_exists(&self.profile_path())?;
        info!("StreamBIM: utloggad");
        Ok(())
    }

    /// A bearer token (the id token), refreshed when less than five minutes are left. A
    /// rejected refresh ends the session and returns the "logga in igen" error.
    pub async fn access_token(&self) -> Result<String> {
        if let Some(id_token) = self.fresh_id_token() {
            return Ok(id_token);
        }
        let _turn = self.refresh.lock().await;
        if let Some(id_token) = self.fresh_id_token() {
            return Ok(id_token);
        }
        self.refresh_session().await
    }

    /// Make the next `access_token` refresh, the server having rejected `failed`: the id token
    /// that the failing request sent. Only when `failed` is still the current id token. A refresh
    /// that already replaced it makes this a no-op, so a burst of 401s causes one refresh. The
    /// current id token is kept, since the refresh sends it.
    pub async fn invalidate_token(&self, failed: &str) {
        let _ = self.cached_tokens();
        if let Some(tokens) = self
            .st()
            .tokens
            .as_mut()
            .filter(|tokens| tokens.id_token == failed)
        {
            tokens.expires_at = 0;
        }
    }

    /// Make the next `access_token` refresh whatever the current id token is. Prefer
    /// `invalidate_token`, which ignores a failure of a token that was already replaced.
    pub async fn invalidate(&self) {
        let _ = self.cached_tokens();
        if let Some(tokens) = self.st().tokens.as_mut() {
            tokens.expires_at = 0;
        }
    }

    // ---- internals -------------------------------------------------------------------------

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token_path(&self) -> PathBuf {
        self.state_dir.join(TOKEN_FILE)
    }

    fn profile_path(&self) -> PathBuf {
        self.state_dir.join(PROFILE_FILE)
    }

    fn signed_in_profile(&self) -> Option<Profile> {
        if !self.is_signed_in() {
            return None;
        }
        self.st().profile.clone()
    }

    /// The base URL for a region. Validates the region first, also in tests.
    fn base_for_region(&self, region: &str) -> Result<String> {
        let base = region_base(region)?;
        #[cfg(test)]
        if let Some(origin) = &self.base_override {
            return Ok(origin.clone());
        }
        Ok(base)
    }

    /// The tokens, read from disk on first use.
    fn cached_tokens(&self) -> Option<Tokens> {
        let mut st = self.st();
        if !st.tokens_loaded {
            st.tokens_loaded = true;
            st.tokens = self.read_token_file();
        }
        st.tokens.clone()
    }

    /// The tokens in the token file. A file that is there but cannot be read, decrypted or parsed
    /// is removed: the account must not stay signed in on a session that can never be used. The
    /// log line names no contents. Called with the state lock held, so it does not take it.
    fn read_token_file(&self) -> Option<Tokens> {
        let path = self.token_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                warn!(kind = ?e.kind(), "StreamBIM: token-filen kunde inte läsas; den tas bort");
                drop_token_file(&path);
                return None;
            }
        };
        let tokens = dpapi_unprotect(&bytes)
            .ok()
            .and_then(|plain| serde_json::from_slice::<Tokens>(&plain).ok());
        if tokens.is_none() {
            warn!("StreamBIM: token-filen kunde inte tolkas; den tas bort");
            drop_token_file(&path);
        }
        tokens
    }

    fn fresh_id_token(&self) -> Option<String> {
        let tokens = self.cached_tokens()?;
        (tokens.expires_at - now_secs() > REFRESH_MARGIN_SECS).then_some(tokens.id_token)
    }

    /// Exchange the last id token and the refresh token for new tokens. Caller holds `refresh`.
    async fn refresh_session(&self) -> Result<String> {
        let tokens = self.cached_tokens().ok_or_else(|| anyhow!(NOT_SIGNED_IN))?;
        let base = self
            .st()
            .profile
            .as_ref()
            .map(|p| p.base.clone())
            .ok_or_else(|| anyhow!(NOT_SIGNED_IN))?;
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            self.clear_session();
            bail!(SESSION_EXPIRED);
        };
        let body = json!({ "token": tokens.id_token, "refreshToken": refresh_token });
        match self
            .call("refresh", &format!("{base}/auth/v1/refresh"), &body)
            .await
        {
            Answer::Reply(reply) if reply.result == "SUCCESS" => {
                match tokens_from(reply, Some(refresh_token), now_secs()) {
                    Some(fresh) => {
                        let id_token = fresh.id_token.clone();
                        // Memory first: the new id token is usable even if the file write fails.
                        self.st().tokens = Some(fresh.clone());
                        if let Err(e) = self.persist_tokens(&fresh) {
                            warn!("StreamBIM: kunde inte spara sessionen: {e:#}");
                        }
                        Ok(id_token)
                    }
                    // A success without a token is not a sign-out: the next call tries again.
                    None => bail!(REFRESH_RETRY),
                }
            }
            // CHALLENGE_REQUESTED: the server wants MFA again, so the user signs in again.
            Answer::Reply(reply) if reply.result == "CHALLENGE_REQUESTED" => {
                self.clear_session();
                bail!(SESSION_EXPIRED)
            }
            // The server refuses the refresh material: the session is over.
            Answer::Status(status @ (400 | 401 | 403)) => {
                warn!(status, "StreamBIM: sessionen avvisades");
                self.clear_session();
                bail!(SESSION_EXPIRED)
            }
            // Everything else is temporary (throttling, timeouts, server errors, no answer, an
            // unknown reply). The refresh material may still be good: keep it.
            Answer::Status(status) => {
                warn!(status, "StreamBIM: sessionen kunde inte förnyas");
                bail!("StreamBIM-sessionen kunde inte förnyas: HTTP {status}. Försök igen.")
            }
            Answer::Reply(_) | Answer::Failed => bail!(REFRESH_RETRY),
        }
    }

    /// Forget the tokens in memory and on disk. The profile stays, so the UI can name the user.
    fn clear_session(&self) {
        {
            let mut st = self.st();
            st.tokens = None;
            st.tokens_loaded = true;
        }
        drop_token_file(&self.token_path());
    }

    /// Persist tokens and profile, then make them the in-memory session.
    fn save_session(&self, tokens: Tokens, profile: &Profile) -> Result<()> {
        self.persist_tokens(&tokens)?;
        write_atomic(&self.profile_path(), &serde_json::to_vec(profile)?)?;
        let mut st = self.st();
        st.profile = Some(profile.clone());
        st.tokens = Some(tokens);
        st.tokens_loaded = true;
        st.pending = None;
        Ok(())
    }

    fn persist_tokens(&self, tokens: &Tokens) -> Result<()> {
        let plain = serde_json::to_vec(tokens)?;
        write_atomic(&self.token_path(), &dpapi_protect(&plain)?)
    }

    /// POST a JSON body. Any 2xx with a JSON body is a `Reply`; other statuses are returned as
    /// they are. The body is never logged, only the endpoint and the status.
    async fn call(&self, endpoint: &str, url: &str, body: &Value) -> Answer {
        let Ok(bytes) = serde_json::to_vec(body) else {
            return Answer::Failed;
        };
        let sent = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, JSON_CONTENT_TYPE)
            .timeout(REQUEST_TIMEOUT)
            .body(bytes)
            .send()
            .await;
        let resp = match sent {
            Ok(resp) => resp,
            Err(e) => {
                debug!(endpoint, error = %e.without_url(), "StreamBIM: anropet misslyckades");
                return Answer::Failed;
            }
        };
        let status = resp.status();
        debug!(endpoint, status = status.as_u16(), "StreamBIM: auth");
        if !status.is_success() {
            return Answer::Status(status.as_u16());
        }
        match resp.json::<AuthReply>().await {
            Ok(reply) => Answer::Reply(reply),
            Err(_) => Answer::Failed,
        }
    }

    /// The display name from the user-profile document; None when it cannot be read. The
    /// username is shown instead.
    async fn profile_name(&self, base: &str, id_token: &str) -> Option<String> {
        let url = format!("{base}/mgw/api/v2/user-profiles?filter%5Bprofile%5D=true");
        let sent = self
            .http
            .get(&url)
            .bearer_auth(id_token)
            .header(reqwest::header::ACCEPT, JSON_API)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await;
        let resp = match sent {
            Ok(resp) => resp,
            Err(_) => {
                warn!("StreamBIM: profilen kunde inte hämtas (nätverksfel)");
                return None;
            }
        };
        if !resp.status().is_success() {
            warn!(
                status = resp.status().as_u16(),
                "StreamBIM: profilen kunde inte hämtas"
            );
            return None;
        }
        let doc: Value = resp.json().await.ok()?;
        name_from_profile(&doc)
    }
}

fn profile_user(p: &Profile) -> UserInfo {
    UserInfo {
        user_id: p.user_id.clone(),
        name: p.name.clone(),
        email: p.email.clone(),
    }
}

/// The display name of a user-profile document. The schema is undocumented, so the first
/// resource's `attributes.name` is tried, then first + last name.
fn name_from_profile(doc: &Value) -> Option<String> {
    let data = match doc.get("data")? {
        Value::Array(items) => items.first()?,
        other => other,
    };
    let attrs = data.get("attributes")?;
    let text = |key: &str| {
        attrs
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if let Some(name) = text("name") {
        return Some(name.to_string());
    }
    let parts: Vec<&str> = ["first-name", "last-name"]
        .into_iter()
        .filter_map(text)
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Tokens from a SUCCESS reply. `previous_refresh` is kept when the reply has no new refresh
/// token. None when the reply has no id token.
fn tokens_from(reply: AuthReply, previous_refresh: Option<String>, now: i64) -> Option<Tokens> {
    if reply.id_token.is_empty() {
        return None;
    }
    let expires_in = reply
        .expires_in
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_EXPIRES_IN_SECS);
    Some(Tokens {
        refresh_token: reply
            .refresh_token
            .filter(|t| !t.is_empty())
            .or(previous_refresh),
        id_token: reply.id_token,
        expires_at: now + expires_in,
    })
}

/// True for a StreamBIM region subdomain (`^[a-z0-9-]{1,32}$`).
pub fn valid_region(region: &str) -> bool {
    (1..=32).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `https://<region>.streambim.com`. Anything but a plain subdomain is rejected.
pub fn region_base(region: &str) -> Result<String> {
    if !valid_region(region) {
        bail!("Ogiltig StreamBIM-region.");
    }
    Ok(format!("https://{region}.streambim.com"))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Write via `<path>.tmp` then rename over the target (atomic on the same volume). The same as
/// `auth::write_atomic`, which is private to that module.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, data).with_context(|| format!("kunde inte skriva {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("kunde inte byta ut {}", path.display()))?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("kunde inte ta bort {}", path.display())),
    }
}

/// Remove the token file; a failure is logged (the caller has already cleared the memory).
fn drop_token_file(path: &Path) {
    if let Err(e) = remove_if_exists(path) {
        warn!("StreamBIM: kunde inte ta bort token-filen: {e:#}");
    }
}

#[cfg(test)]
impl StreamBimAuth {
    /// Make the pending MFA challenge older than its five minutes.
    pub(crate) fn expire_challenge(&self) {
        if let Some(challenge) = self.st().pending.as_mut() {
            challenge.issued_at -= CHALLENGE_TTL_SECS + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Fault, Request, Server};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A state directory that is removed when dropped.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "synka-streambim-auth-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn reply_json(status: u16, value: Value) -> Fault {
        Fault::reply(status, value.to_string()).header("Content-Type", "application/json")
    }

    fn profile_reply() -> Fault {
        reply_json(
            200,
            json!({"data": {"type": "user-profiles", "attributes": {"first-name": "Ada", "last-name": "Lovelace"}}}),
        )
    }

    fn body_json(req: &Request) -> Value {
        serde_json::from_slice(&req.body).unwrap()
    }

    /// Server for the common paths: login SUCCESS with tokens, and the profile.
    async fn success_server() -> Server {
        Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-1", "refreshToken": "rt-1",
                       "accessToken": "at-1", "expiresIn": 3600}),
            ),
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await
    }

    #[test]
    fn region_validation() {
        assert_eq!(
            region_base("sweden").unwrap(),
            "https://sweden.streambim.com"
        );
        assert_eq!(region_base("app").unwrap(), "https://app.streambim.com");
        assert!(valid_region(&"a".repeat(32)));
        assert!(!valid_region(&"a".repeat(33)));
        for bad in [
            "",
            "App",
            "a.b",
            "a/b",
            "a b",
            "ä",
            "evil.example.com/x",
            "a?b",
        ] {
            assert!(!valid_region(bad), "{bad:?} must be rejected");
            assert!(region_base(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn region_is_validated_before_any_request() {
        let server = success_server().await;
        let dir = TestDir::new("region");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        for bad in ["", "App", "evil.example.com/x"] {
            assert!(auth.login(bad, "ada@example.com", "pw").await.is_err());
        }
        assert_eq!(server.count(), 0);
    }

    #[tokio::test]
    async fn login_success_persists_files_and_signs_in() {
        let server = success_server().await;
        let dir = TestDir::new("login");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);

        let outcome = auth
            .login("app", "ada@example.com", "pw-secret")
            .await
            .unwrap();
        match outcome {
            LoginOutcome::SignedIn(user) => {
                assert_eq!(user.name, "Ada Lovelace");
                assert_eq!(user.email, "ada@example.com");
                assert_eq!(user.user_id, "ada@example.com");
            }
            LoginOutcome::MfaRequired => panic!("no MFA expected"),
        }
        assert!(auth.is_signed_in());
        assert_eq!(auth.region().as_deref(), Some("app"));
        assert_eq!(auth.host().as_deref(), Some("127.0.0.1"));

        // The login request: JSON body and the charset content type.
        let requests = server.requests();
        let login = &requests[0];
        assert_eq!(login.method, "POST");
        assert_eq!(
            login.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(
            body_json(login),
            json!({"username": "ada@example.com", "password": "pw-secret"})
        );
        // The profile request carries the id token and asks for JSON:API.
        let profile = &requests[1];
        assert_eq!(profile.header("authorization"), Some("Bearer id-1"));
        assert_eq!(profile.header("accept"), Some(JSON_API));

        // Files: the refresh token is protected, the profile has no secrets.
        let token_bytes = std::fs::read(dir.path().join(TOKEN_FILE)).unwrap();
        assert!(!token_bytes.windows(4).any(|w| w == b"rt-1"));
        let profile_text = std::fs::read_to_string(dir.path().join(PROFILE_FILE)).unwrap();
        assert!(profile_text.contains("\"region\":\"app\""));
        assert!(profile_text.contains("\"base\""));
        for secret in ["pw-secret", "rt-1", "id-1", "at-1"] {
            assert!(!profile_text.contains(secret));
        }

        // A new instance (after a restart) is signed in and uses the stored id token, with no
        // request, while the token is fresh.
        let again = StreamBimAuth::new(http(), dir.path());
        assert!(again.is_signed_in());
        assert_eq!(again.user().unwrap().email, "ada@example.com");
        assert_eq!(again.access_token().await.unwrap(), "id-1");
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn wrong_password_is_a_swedish_error_without_the_password() {
        let server = Server::new(|_, _| reply_json(401, json!({"error": "bad"}))).await;
        let dir = TestDir::new("wrongpw");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        let err = auth
            .login("sweden", "ada@example.com", "hunter2-secret")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Fel e-post eller lösenord för StreamBIM (region sweden)."
        );
        assert!(!err.contains("hunter2"));
        assert!(!auth.is_signed_in());
    }

    #[tokio::test]
    async fn other_login_failures_report_the_status_only() {
        let server = Server::new(|_, _| reply_json(500, json!({"detail": "boom"}))).await;
        let dir = TestDir::new("status");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        let err = auth
            .login("app", "a@b.se", "pw")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "StreamBIM-inloggningen misslyckades: HTTP 500");
    }

    #[tokio::test]
    async fn challenge_then_mfa_verify_signs_in() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-1"}),
            ),
            "/auth/v1/mfa/verify" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-2", "expiresIn": 3600}),
            ),
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("mfa");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);

        let outcome = auth.login("app", "ada@example.com", "pw").await.unwrap();
        assert!(matches!(outcome, LoginOutcome::MfaRequired));
        assert!(!auth.is_signed_in());
        assert!(!dir.path().join(TOKEN_FILE).exists());

        let user = auth.verify_mfa("123456").await.unwrap();
        assert_eq!(user.name, "Ada Lovelace");
        assert!(auth.is_signed_in());

        let mfa = server
            .requests()
            .into_iter()
            .find(|r| r.target == "/auth/v1/mfa/verify")
            .unwrap();
        assert_eq!(
            body_json(&mfa),
            json!({"username": "ada@example.com", "session": "sess-1", "code": "123456"})
        );
        // The verify reply had no refresh token: the session still works until the id token ends.
        assert_eq!(auth.access_token().await.unwrap(), "id-2");
    }

    #[tokio::test]
    async fn verify_mfa_rejects_bad_codes_and_expired_challenges() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-1"}),
            ),
            "/auth/v1/mfa/verify" => reply_json(401, json!({"error": "code"})),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("badcode");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);

        // Malformed codes never reach the server.
        auth.login("app", "ada@example.com", "pw").await.unwrap();
        for bad in [
            "12345",
            "1234567",
            "12345a",
            " 12345",
            "",
            "12 456",
            "١٢٣٤٥٦",
        ] {
            let err = auth.verify_mfa(bad).await.unwrap_err().to_string();
            assert!(err.contains("sex siffrorna"), "{bad:?}: {err}");
        }
        assert_eq!(server.count(), 1);

        // An expired challenge is refused without a request.
        auth.expire_challenge();
        let err = auth.verify_mfa("123456").await.unwrap_err().to_string();
        assert_eq!(err, CODE_EXPIRED);
        assert_eq!(server.count(), 1);

        // A wrong code ends the challenge: the next try needs a new password step.
        auth.login("app", "ada@example.com", "pw").await.unwrap();
        let err = auth.verify_mfa("000000").await.unwrap_err().to_string();
        assert_eq!(err, "Fel kod för StreamBIM – logga in igen.");
        assert_eq!(server.count(), 3);
        let err = auth.verify_mfa("000000").await.unwrap_err().to_string();
        assert_eq!(err, CODE_EXPIRED);
        assert_eq!(server.count(), 3);
    }

    #[tokio::test]
    async fn access_token_refreshes_when_invalidated() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-1", "refreshToken": "rt-1", "expiresIn": 3600}),
            ),
            "/auth/v1/refresh" => {
                reply_json(200, json!({"result": "SUCCESS", "idToken": "id-2", "expiresIn": 3600}))
            }
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("refresh");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();

        auth.invalidate().await;
        assert_eq!(auth.access_token().await.unwrap(), "id-2");
        let refresh = server
            .requests()
            .into_iter()
            .find(|r| r.target == "/auth/v1/refresh")
            .unwrap();
        assert_eq!(
            body_json(&refresh),
            json!({"token": "id-1", "refreshToken": "rt-1"})
        );
        // The new id token is fresh: no further request, and the refresh token is kept.
        assert_eq!(auth.access_token().await.unwrap(), "id-2");
        assert_eq!(server.count(), 3);
        auth.invalidate().await;
        auth.access_token().await.unwrap();
        let second = server.requests().last().unwrap().clone();
        assert_eq!(body_json(&second)["refreshToken"], "rt-1");
    }

    #[tokio::test]
    async fn rejected_refresh_signs_out() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-1", "refreshToken": "rt-1", "expiresIn": 3600}),
            ),
            "/auth/v1/refresh" => reply_json(401, json!({"error": "expired"})),
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("rejected");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();
        assert!(dir.path().join(TOKEN_FILE).exists());

        auth.invalidate().await;
        let err = auth.access_token().await.unwrap_err().to_string();
        assert_eq!(
            err,
            "StreamBIM-sessionen har gått ut – logga in igen. (Session expired)"
        );
        assert!(!auth.is_signed_in());
        assert!(auth.user().is_none());
        assert!(!dir.path().join(TOKEN_FILE).exists());
        // Signed out: no more refresh attempts.
        let before = server.count();
        assert!(auth.access_token().await.is_err());
        assert_eq!(server.count(), before);
    }

    #[tokio::test]
    async fn refresh_without_refresh_token_expires_the_session() {
        // MFA reply without a refresh token: the session lasts until the id token ends.
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "CHALLENGE_REQUESTED", "session": "s"}),
            ),
            "/auth/v1/mfa/verify" => {
                reply_json(200, json!({"result": "SUCCESS", "idToken": "id-3"}))
            }
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("norefresh");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();
        auth.verify_mfa("654321").await.unwrap();
        let count = server.count();

        auth.invalidate().await;
        let err = auth.access_token().await.unwrap_err().to_string();
        assert!(err.contains("logga in igen"), "{err}");
        assert_eq!(server.count(), count, "no refresh without a refresh token");
        assert!(!auth.is_signed_in());
        assert!(!dir.path().join(TOKEN_FILE).exists());
    }

    #[tokio::test]
    async fn server_error_on_refresh_keeps_the_session() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-1", "refreshToken": "rt-1", "expiresIn": 3600}),
            ),
            "/auth/v1/refresh" => reply_json(503, json!({"error": "busy"})),
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("outage");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();
        auth.invalidate().await;

        let err = auth.access_token().await.unwrap_err().to_string();
        assert_eq!(
            err,
            "StreamBIM-sessionen kunde inte förnyas: HTTP 503. Försök igen."
        );
        assert!(auth.is_signed_in());
        assert!(dir.path().join(TOKEN_FILE).exists());
    }

    #[tokio::test]
    async fn sign_out_removes_the_session_and_the_files() {
        let server = success_server().await;
        let dir = TestDir::new("signout");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();
        auth.sign_out().await.unwrap();
        assert!(!auth.is_signed_in());
        assert!(auth.user().is_none());
        assert!(!dir.path().join(TOKEN_FILE).exists());
        assert!(!dir.path().join(PROFILE_FILE).exists());
        assert!(!StreamBimAuth::new(http(), dir.path()).is_signed_in());
    }

    /// Server: login with `id-1`/`rt-1`, the profile, and a refresh answered by `refresh`.
    async fn refresh_server(refresh: impl Fn() -> Fault + Send + Sync + 'static) -> Server {
        Server::new(move |req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-1", "refreshToken": "rt-1", "expiresIn": 3600}),
            ),
            "/auth/v1/refresh" => refresh(),
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await
    }

    #[tokio::test]
    async fn a_stale_failure_does_not_refresh_again() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-1", "refreshToken": "rt-1", "expiresIn": 3600}),
            ),
            "/auth/v1/refresh" => {
                reply_json(200, json!({"result": "SUCCESS", "idToken": "id-2", "expiresIn": 3600}))
            }
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("stale");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();

        // A 401 for a token that is not the current one changes nothing.
        auth.invalidate_token("id-0").await;
        assert_eq!(auth.access_token().await.unwrap(), "id-1");
        assert_eq!(server.count(), 2, "no refresh for a stale failure");

        // The failure of the current token: one refresh, which keeps the refresh token.
        auth.invalidate_token("id-1").await;
        assert_eq!(auth.access_token().await.unwrap(), "id-2");
        assert_eq!(server.count(), 3);

        // A burst of 401s for the token that was already replaced causes no more refreshes.
        for _ in 0..5 {
            auth.invalidate_token("id-1").await;
        }
        assert_eq!(auth.access_token().await.unwrap(), "id-2");
        assert_eq!(server.count(), 3);
        let refresh = server
            .requests()
            .into_iter()
            .find(|r| r.target == "/auth/v1/refresh")
            .unwrap();
        assert_eq!(
            body_json(&refresh),
            json!({"token": "id-1", "refreshToken": "rt-1"})
        );
    }

    #[tokio::test]
    async fn temporary_refresh_failures_keep_the_session() {
        for status in [404u16, 408, 429, 500, 502, 503] {
            let server =
                refresh_server(move || reply_json(status, json!({"error": "later"}))).await;
            let dir = TestDir::new("temporary");
            let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
            auth.login("app", "ada@example.com", "pw").await.unwrap();
            auth.invalidate_token("id-1").await;

            let err = auth.access_token().await.unwrap_err().to_string();
            assert_eq!(
                err,
                format!("StreamBIM-sessionen kunde inte förnyas: HTTP {status}. Försök igen.")
            );
            assert!(auth.is_signed_in(), "{status}: the session stays");
            assert!(dir.path().join(TOKEN_FILE).exists(), "{status}");
            // The next call tries the refresh again with the same tokens.
            let before = server.count();
            assert!(auth.access_token().await.is_err());
            assert_eq!(server.count(), before + 1, "{status}");
        }
    }

    #[tokio::test]
    async fn a_refresh_without_an_answer_keeps_the_session() {
        let server = refresh_server(|| Fault::Disconnect).await;
        let dir = TestDir::new("noanswer");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();
        auth.invalidate_token("id-1").await;

        assert_eq!(
            auth.access_token().await.unwrap_err().to_string(),
            REFRESH_RETRY
        );
        assert!(auth.is_signed_in());
        assert!(dir.path().join(TOKEN_FILE).exists());
    }

    #[tokio::test]
    async fn refusals_and_a_new_mfa_challenge_end_the_session() {
        let answers: [(&str, Fault); 4] = [
            ("400", Fault::reply(400, "bad")),
            ("401", Fault::reply(401, "bad")),
            ("403", Fault::reply(403, "bad")),
            (
                "challenge",
                reply_json(
                    200,
                    json!({"result": "CHALLENGE_REQUESTED", "session": "s"}),
                ),
            ),
        ];
        for (name, answer) in answers {
            let server = refresh_server(move || answer.clone()).await;
            let dir = TestDir::new("refused");
            let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
            auth.login("app", "ada@example.com", "pw").await.unwrap();
            auth.invalidate_token("id-1").await;

            assert_eq!(
                auth.access_token().await.unwrap_err().to_string(),
                SESSION_EXPIRED,
                "{name}"
            );
            assert!(!auth.is_signed_in(), "{name}");
            assert!(!dir.path().join(TOKEN_FILE).exists(), "{name}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_token_file_ends_the_sign_in() {
        let server = success_server().await;
        let dir = TestDir::new("unreadable");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("app", "ada@example.com", "pw").await.unwrap();

        // Bytes that are not DPAPI data, and DPAPI data that is not a token document.
        let garbage = [
            b"not protected".to_vec(),
            dpapi_protect(b"{\"refresh_").unwrap(),
        ];
        for bytes in garbage {
            std::fs::write(dir.path().join(TOKEN_FILE), &bytes).unwrap();
            let again = StreamBimAuth::new(http(), dir.path());
            assert!(!again.is_signed_in());
            assert!(again.user().is_none());
            assert!(!dir.path().join(TOKEN_FILE).exists(), "the file is removed");
            let err = again.access_token().await.unwrap_err().to_string();
            assert!(err.contains("logga in igen"), "{err}");
        }
        assert_eq!(server.count(), 2, "no request without a readable session");
    }

    #[test]
    fn profile_name_shapes() {
        assert_eq!(
            name_from_profile(&json!({"data": {"attributes": {"name": " Ada "}}})).as_deref(),
            Some("Ada")
        );
        assert_eq!(
            name_from_profile(
                &json!({"data": [{"attributes": {"first-name": "Ada", "last-name": "L"}}]})
            )
            .as_deref(),
            Some("Ada L")
        );
        assert_eq!(
            name_from_profile(&json!({"data": {"attributes": {"first-name": "Ada"}}})).as_deref(),
            Some("Ada")
        );
        assert_eq!(name_from_profile(&json!({"data": []})), None);
        assert_eq!(name_from_profile(&json!({"errors": []})), None);
    }
}

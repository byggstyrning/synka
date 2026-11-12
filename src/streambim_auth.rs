//! StreamBIM sign-in: a server (a region or a custom https address) + e-mail + password, optional
//! TOTP code (MFA). See docs/PROVIDERS.md "StreamBIM" and docs/specs/streambim-endpoints.md
//! section 2.
//!
//! One `StreamBimAuth` per server account (`StreamBimAccount`), held by `StreamBimAccounts`. The
//! account key is the region subdomain for `*.streambim.com` (`app`, `japan`) and the lowercase
//! host otherwise (docs/IPC.md "Providers and accounts").
//!
//! Files in the state directory, per key: `token-streambim-<key>.bin` (DPAPI-protected JSON with
//! the refresh material) and `user-streambim-<key>.json` (plain JSON profile with the user, the
//! key and the base URL). The password, the tokens and the MFA session are never logged or put in
//! an error message. A refused call is logged with the server's reason, but only after the
//! submitted secrets are removed from it (see `clean`).

use crate::auth::{dpapi_protect, dpapi_unprotect, remove_if_exists, write_atomic, UserInfo};
use crate::http::{Api, TokenSource};
use crate::model::{HubInfo, Project};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

const TOKEN_PREFIX: &str = "token-streambim-";
const PROFILE_PREFIX: &str = "user-streambim-";
/// The files of a single-account install, renamed to the keyed names once (`migrate_legacy`).
const LEGACY_TOKEN_FILE: &str = "token-streambim.bin";
const LEGACY_PROFILE_FILE: &str = "user-streambim.json";

/// The token file of the account `key`.
fn token_file(key: &str) -> String {
    format!("{TOKEN_PREFIX}{key}.bin")
}

/// The profile file of the account `key`.
fn profile_file(key: &str) -> String {
    format!("{PROFILE_PREFIX}{key}.json")
}
/// An MFA challenge is accepted this long after the password step.
const CHALLENGE_TTL_SECS: i64 = 5 * 60;
/// Wrong codes one challenge takes. The next wrong code ends it: a new password step is needed.
const MAX_CODE_TRIES: u32 = 3;
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
const WRONG_CODE: &str = "Fel kod. Ange koden som autentiseringsappen visar nu.";
const WRONG_CODE_LAST: &str = "Fel kod för många gånger – logga in igen.";
const MFA_SESSION_ENDED: &str = "Inloggningen hos StreamBIM har gått ut – logga in igen.";
const REFRESH_RETRY: &str =
    "StreamBIM-sessionen kunde inte förnyas: inget giltigt svar. Försök igen.";
const TOO_MANY_LOGINS: &str =
    "StreamBIM: för många inloggningsförsök. Vänta en stund och försök igen.";
/// The most bytes of an error reply that are read. The rest is not read.
const ERROR_BODY_BYTES: usize = 8 * 1024;
/// Caps for the cleaned texts of a refused call (characters).
const REASON_CHARS: usize = 200;
const TEXT_REASON_CHARS: usize = 120;
const CODE_CHARS: usize = 100;
const RESULT_CHARS: usize = 60;

/// What a password login leads to.
#[derive(Debug)]
pub enum LoginOutcome {
    /// The engine reads the cached profile (`user()`); the value is for callers that want it.
    SignedIn(#[allow(dead_code)] UserInfo),
    /// The account has MFA: call `verify_mfa` with the TOTP code.
    MfaRequired,
}

/// The session of one StreamBIM server account: refresh material and the signed-in user.
pub struct StreamBimAuth {
    http: reqwest::Client,
    state_dir: PathBuf,
    /// The account key (`app`, `japan`, or the lowercase host of a custom server).
    key: String,
    /// The server's origin, `https://<host>` (a loopback origin in tests).
    base: String,
    state: Mutex<State>,
    /// One refresh request at a time. Callers that wait re-check the token before refreshing.
    refresh: tokio::sync::Mutex<()>,
}

/// One StreamBIM account: the server's key, its session and the API client that sends the
/// session's bearer token (`TokenSource::StreamBim`).
#[derive(Clone)]
pub struct StreamBimAccount {
    pub key: String,
    pub auth: Arc<StreamBimAuth>,
    pub api: Api,
}

/// Every StreamBIM account of this install, by key. Accounts are loaded from the state directory
/// at start and added on their first sign-in.
pub struct StreamBimAccounts {
    http: reqwest::Client,
    state_dir: PathBuf,
    accounts: RwLock<BTreeMap<String, StreamBimAccount>>,
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

/// `user-streambim-<key>.json`: the user plus where the account lives. `region` holds the account
/// key (for a custom server, its host); `base` is the origin.
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
    username: String,
    session: String,
    /// Unix seconds.
    issued_at: i64,
    /// Wrong codes so far.
    tries: u32,
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
    /// A non-2xx status, with the server's code and reason. Both are cleaned of the secrets
    /// sent in the request (see `clean`), so they may be shown and logged.
    Status {
        status: u16,
        code: String,
        reason: String,
    },
    /// No answer, or an answer that is not JSON.
    Failed,
}

impl StreamBimAuth {
    /// `state_dir` is where the token and profile files live. The profile is read now; the
    /// tokens are read on first use. `key` names the account, `base` is its server's origin.
    pub fn new(http: reqwest::Client, state_dir: &Path, key: &str, base: &str) -> StreamBimAuth {
        let profile = std::fs::read(state_dir.join(profile_file(key)))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Profile>(&bytes).ok());
        StreamBimAuth {
            http,
            state_dir: state_dir.to_path_buf(),
            key: key.to_string(),
            base: base.trim_end_matches('/').to_string(),
            state: Mutex::new(State {
                profile,
                ..State::default()
            }),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    /// Test constructor for the account "app": every request goes to `base` (a loopback origin).
    #[cfg(test)]
    pub(crate) fn for_test(http: reqwest::Client, state_dir: &Path, base: &str) -> StreamBimAuth {
        StreamBimAuth::new(http, state_dir, "app", base)
    }

    /// Test constructor for an account with any key.
    #[cfg(test)]
    pub(crate) fn for_test_key(
        http: reqwest::Client,
        state_dir: &Path,
        key: &str,
        base: &str,
    ) -> StreamBimAuth {
        StreamBimAuth::new(http, state_dir, key, base)
    }

    /// The server's origin, `https://<host>` (a loopback origin in tests).
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The server's host, e.g. "app.streambim.com".
    pub fn host(&self) -> String {
        url::Url::parse(&self.base)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| canonical_host(&self.key))
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

    /// True while an MFA code is awaited (the password step was accepted).
    pub fn has_pending_mfa(&self) -> bool {
        self.st().pending.is_some()
    }

    /// Password step. `SignedIn` when the account has no MFA; `MfaRequired` keeps the challenge
    /// in memory for `verify_mfa`. A new login replaces an unfinished challenge.
    pub async fn login(&self, username: &str, password: &str) -> Result<LoginOutcome> {
        self.st().pending = None;
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            bail!("Ange e-post och lösenord för StreamBIM.");
        }
        let base = self.base.clone();
        let body = json!({ "username": username, "password": password });
        match self
            .call(
                "login",
                &format!("{base}/auth/v1/login"),
                &body,
                &[username, password],
            )
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
                        region: self.key.clone(),
                        base,
                    };
                    self.save_session(tokens, &profile)?;
                    info!("StreamBIM: inloggad (server {})", self.key);
                    Ok(LoginOutcome::SignedIn(profile_user(&profile)))
                }
                "CHALLENGE_REQUESTED" => {
                    let session = reply.session.filter(|s| !s.is_empty()).ok_or_else(|| {
                        anyhow!("StreamBIM-inloggningen misslyckades: MFA-sessionen saknas.")
                    })?;
                    self.st().pending = Some(Challenge {
                        username: username.to_string(),
                        session,
                        issued_at: now_secs(),
                        tries: 0,
                    });
                    info!("StreamBIM: MFA-kod krävs (server {})", self.key);
                    Ok(LoginOutcome::MfaRequired)
                }
                "NEW_PASSWORD_REQUIRED" => bail!(password_reset_text(&self.key)),
                _ => bail!("StreamBIM-inloggningen misslyckades: oväntat svar från servern."),
            },
            Answer::Status {
                status,
                code,
                reason,
            } => bail!(login_refusal(&self.key, status, &code, &reason)),
            Answer::Failed => {
                bail!("StreamBIM-inloggningen misslyckades: inget giltigt svar från servern.")
            }
        }
    }

    /// TOTP step after `login` returned `MfaRequired`. The code must be six ASCII digits and the
    /// challenge at most five minutes old. A wrong code keeps the challenge for another code, as
    /// StreamBIM's web app does, but only `MAX_CODE_TRIES` times: a code cannot be guessed over
    /// many tries without a new password step. Any other definitive answer (success or a
    /// 400/401/403 refusal) ends the challenge. A transient failure keeps it for a retry.
    pub async fn verify_mfa(&self, code: &str) -> Result<UserInfo> {
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            bail!("Ange de sex siffrorna från din autentiseringsapp.");
        }
        let mut challenge = match self.st().pending.take() {
            Some(c) if now_secs() - c.issued_at < CHALLENGE_TTL_SECS => c,
            _ => bail!(CODE_EXPIRED),
        };
        let body = json!({
            "username": challenge.username,
            "session": challenge.session,
            "code": code,
        });
        let base = self.base.clone();
        let url = format!("{base}/auth/v1/mfa/verify");
        let secrets = [
            challenge.username.as_str(),
            challenge.session.as_str(),
            code,
        ];
        // (the text, whether the code was wrong, whether the answer ends the challenge)
        let (message, wrong, definitive) =
            match self.call("mfa/verify", &url, &body, &secrets).await {
                Answer::Reply(reply) if reply.result == "SUCCESS" => {
                    let tokens = tokens_from(reply, None, now_secs()).ok_or_else(|| {
                        anyhow!("StreamBIM-verifieringen misslyckades: svaret saknar token.")
                    })?;
                    let name = self.profile_name(&base, &tokens.id_token).await;
                    let profile = Profile {
                        user_id: challenge.username.clone(),
                        name: name.unwrap_or_else(|| challenge.username.clone()),
                        email: challenge.username.clone(),
                        region: self.key.clone(),
                        base,
                    };
                    self.save_session(tokens, &profile)?;
                    info!("StreamBIM: MFA godkänd (server {})", self.key);
                    return Ok(profile_user(&profile));
                }
                // The server asks for a code again: the code was not accepted. A new session
                // string, when there is one, is the one the next code goes with.
                Answer::Reply(reply) if reply.result == "CHALLENGE_REQUESTED" => {
                    if let Some(session) = reply.session.filter(|s| !s.is_empty()) {
                        challenge.session = session;
                    }
                    (WRONG_CODE.to_string(), true, true)
                }
                Answer::Reply(reply) => {
                    let result = clean(&reply.result, &secret_forms(&secrets), RESULT_CHARS);
                    (mfa_refusal(200, "", &result), wrong_code("", &result), true)
                }
                Answer::Status {
                    status,
                    code: error_code,
                    reason,
                } => (
                    mfa_refusal(status, &error_code, &reason),
                    wrong_code(&error_code, &reason),
                    matches!(status, 400 | 401 | 403),
                ),
                Answer::Failed => (
                    "StreamBIM-verifieringen misslyckades: inget giltigt svar från servern."
                        .to_string(),
                    false,
                    false,
                ),
            };
        if wrong {
            challenge.tries += 1;
            if challenge.tries >= MAX_CODE_TRIES {
                info!(
                    "StreamBIM: fel MFA-kod för många gånger (server {})",
                    self.key
                );
                bail!(WRONG_CODE_LAST);
            }
            info!(
                tries = challenge.tries,
                "StreamBIM: fel MFA-kod (server {})", self.key
            );
            self.st().pending = Some(challenge);
            bail!(WRONG_CODE);
        }
        // A transient failure keeps the challenge for another try.
        if !definitive {
            self.st().pending = Some(challenge);
        }
        bail!(message)
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
    #[cfg(test)]
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
        self.state_dir.join(token_file(&self.key))
    }

    fn profile_path(&self) -> PathBuf {
        self.state_dir.join(profile_file(&self.key))
    }

    fn signed_in_profile(&self) -> Option<Profile> {
        if !self.is_signed_in() {
            return None;
        }
        self.st().profile.clone()
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
        if self.st().profile.is_none() {
            bail!(NOT_SIGNED_IN);
        }
        let base = self.base.clone();
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            self.clear_session();
            bail!(SESSION_EXPIRED);
        };
        let body = json!({ "token": tokens.id_token, "refreshToken": refresh_token });
        let secrets = [tokens.id_token.as_str(), refresh_token.as_str()];
        match self
            .call(
                "refresh",
                &format!("{base}/auth/v1/refresh"),
                &body,
                &secrets,
            )
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
            // The server refuses the refresh material: the session is over. The reason is in the
            // WARN line that `call` logged.
            Answer::Status {
                status: status @ (400 | 401 | 403),
                ..
            } => {
                warn!(status, "StreamBIM: sessionen avvisades");
                self.clear_session();
                bail!(SESSION_EXPIRED)
            }
            // Everything else is temporary (throttling, timeouts, server errors, no answer, an
            // unknown reply). The refresh material may still be good: keep it.
            Answer::Status { status, .. } => {
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

    /// POST a JSON body. A 2xx with a JSON body is a `Reply`, and so is a refusal whose body says
    /// `CHALLENGE_REQUESTED`: StreamBIM asks for the MFA code with HTTP 401 (its web app reads
    /// `result` and `session` from the 401 body). Any other status is a `Status` with the
    /// server's code and reason. `secrets` are the values sent in this request; they are removed
    /// from everything that is kept or logged. Each failed call is logged at WARN with the
    /// endpoint, the status, the code and the reason.
    async fn call(&self, endpoint: &str, url: &str, body: &Value, secrets: &[&str]) -> Answer {
        let Ok(bytes) = serde_json::to_vec(body) else {
            return Answer::Failed;
        };
        let forms = secret_forms(secrets);
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
                warn!(endpoint, error = %e.without_url(), "StreamBIM: anropet misslyckades");
                return Answer::Failed;
            }
        };
        let status = resp.status().as_u16();
        debug!(endpoint, status, "StreamBIM: auth");
        if !(200..300).contains(&status) {
            let bytes = read_error_body(resp).await;
            if let Some(reply) = serde_json::from_slice::<AuthReply>(&bytes)
                .ok()
                .filter(|reply| reply.result == "CHALLENGE_REQUESTED")
            {
                info!(endpoint, status, "StreamBIM: servern begär en MFA-kod");
                return Answer::Reply(reply);
            }
            let (code, reason) = refusal_details(&bytes, &forms);
            warn!(endpoint, status, code = %code, reason = %reason, "StreamBIM: anropet avvisades");
            return Answer::Status {
                status,
                code,
                reason,
            };
        }
        match resp.json::<AuthReply>().await {
            Ok(mut reply) => {
                // Token data without a `result` is a success: the web app reads only the tokens.
                if reply.result.is_empty() && !reply.id_token.is_empty() {
                    reply.result = "SUCCESS".to_string();
                }
                if !matches!(reply.result.as_str(), "SUCCESS" | "CHALLENGE_REQUESTED") {
                    let result = clean(&reply.result, &forms, RESULT_CHARS);
                    warn!(endpoint, status, result = %result, "StreamBIM: oväntat resultat i svaret");
                }
                Answer::Reply(reply)
            }
            Err(_) => {
                warn!(endpoint, status, "StreamBIM: svaret kunde inte läsas");
                Answer::Failed
            }
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

/// The start of an error reply, at most `ERROR_BODY_BYTES`. Reading stops there.
async fn read_error_body(mut resp: reqwest::Response) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let room = ERROR_BODY_BYTES - bytes.len();
        if chunk.len() >= room {
            bytes.extend_from_slice(&chunk[..room]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    bytes
}

/// The code and the reason of a refused call, both cleaned with `forms`. A JSON reply gives them
/// from the fields that StreamBIM and its AWS-style backends use. Other text gives its first
/// characters, and HTML is only named.
fn refusal_details(body: &[u8], forms: &[String]) -> (String, String) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        let text = String::from_utf8_lossy(body);
        if text.contains('<') {
            return (String::new(), "(HTML)".to_string());
        }
        return (String::new(), clean(&text, forms, TEXT_REASON_CHARS));
    };
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let first = value.get("errors").and_then(|errors| errors.get(0));
    let nested = |key: &str| first.and_then(|e| e.get(key)).and_then(Value::as_str);
    let reason = first_clean(
        [
            text("message"),
            text("errorMessage"),
            text("error_description"),
            text("error"),
            text("detail"),
            nested("detail"),
            nested("title"),
            text("__type"),
            text("code"),
            text("errorCode"),
            text("type"),
        ],
        forms,
        REASON_CHARS,
    );
    let code = first_clean(
        [
            text("code"),
            text("errorCode"),
            text("__type"),
            text("error"),
            nested("code"),
        ],
        forms,
        CODE_CHARS,
    );
    (code, reason)
}

/// The first candidate that is not empty once cleaned. The secrets are removed before an AWS
/// error is reordered (`aws_error_first`), so no reordering can split one.
fn first_clean<'a>(
    candidates: impl IntoIterator<Item = Option<&'a str>>,
    forms: &[String],
    cap: usize,
) -> String {
    candidates
        .into_iter()
        .flatten()
        .map(|raw| clean(&aws_error_first(&redact(raw, forms)), forms, cap))
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

/// An AWS SDK error as StreamBIM passes it on, "operation error Cognito Identity Provider:
/// InitiateAuth, https response error StatusCode: 400, RequestID: <id>, NotAuthorizedException:
/// Incorrect username or password.", becomes "NotAuthorizedException: Incorrect username or
/// password. (RequestID: <id>)". What went wrong then comes first and is kept by the length caps.
/// Other text is returned as it is.
fn aws_error_first(text: &str) -> String {
    const ID: &str = "RequestID: ";
    let reordered = text
        .trim_start()
        .starts_with("operation error ")
        .then(|| text.find(ID))
        .flatten()
        .and_then(|at| text[at + ID.len()..].split_once(", "))
        .map(|(id, tail)| (id.trim(), tail.trim()))
        .map(|(id, tail)| (id, tail.strip_prefix("api error ").unwrap_or(tail)))
        .filter(|(_, tail)| !tail.is_empty())
        .map(|(id, tail)| format!("{tail} (RequestID: {id})"));
    reordered.unwrap_or_else(|| text.to_string())
}

/// Each secret as it can appear in a reply: as typed, and as JSON text (quotes and backslashes
/// escaped), since a server may repeat the request body. Empty secrets are skipped. Longest first,
/// so that a secret which contains another one is removed whole.
fn secret_forms(secrets: &[&str]) -> Vec<String> {
    let mut forms: Vec<String> = Vec::new();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        forms.push(secret.to_string());
        let json = serde_json::to_string(secret).unwrap_or_default();
        if let Some(escaped) = json.get(1..json.len().saturating_sub(1)) {
            if escaped != *secret {
                forms.push(escaped.to_string());
            }
        }
    }
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    forms
}

/// `text` with every occurrence of each form replaced by "***". ASCII letters match in any case.
fn redact(text: &str, forms: &[String]) -> String {
    forms
        .iter()
        .fold(text.to_string(), |out, form| replace_secret(&out, form))
}

fn replace_secret(text: &str, secret: &str) -> String {
    // ASCII lowercasing keeps every byte offset, so a match in `lower` is a match in `text`.
    let lower = text.to_ascii_lowercase();
    let secret = secret.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, found) in lower.match_indices(secret.as_str()) {
        out.push_str(&text[last..start]);
        out.push_str("***");
        last = start + found.len();
    }
    out.push_str(&text[last..]);
    out
}

/// A reply's text made safe to keep or log: the secrets become "***" (before anything is cut, so
/// a cut never leaves part of one), control characters are dropped, whitespace is collapsed, and
/// the result is at most `cap` characters.
fn clean(text: &str, forms: &[String], cap: usize) -> String {
    let plain: String = redact(text, forms)
        .chars()
        .filter_map(|c| {
            if c.is_whitespace() {
                Some(' ')
            } else if c.is_control() {
                None
            } else {
                Some(c)
            }
        })
        .collect();
    let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .chars()
        .take(cap)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Letters and digits only, lowercase. Matching on this makes "Password reset required" match
/// "passwordresetrequired" and "NEW_PASSWORD_REQUIRED" match "new_password_required".
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// True when the code or the reason contains one of the needles (case and punctuation aside).
fn mentions(code: &str, reason: &str, needles: &[&str]) -> bool {
    let hay = squash(&format!("{code} {reason}"));
    needles.iter().any(|needle| hay.contains(&squash(needle)))
}

/// The Swedish text for a refused password step. `key` is the account key of the server.
fn login_refusal(key: &str, status: u16, code: &str, reason: &str) -> String {
    let host = canonical_host(key);
    if mentions(
        code,
        reason,
        &["usernotfound", "user does not exist", "not found"],
    ) {
        if valid_region(key) {
            return format!(
                "Det finns inget StreamBIM-konto med den e-postadressen i regionen {key}. \
                 Välj en annan region (de flesta svenska konton finns i Europe eller Sweden)."
            );
        }
        return format!("Det finns inget StreamBIM-konto med den e-postadressen på {host}.");
    }
    if mentions(
        code,
        reason,
        &[
            "passwordresetrequired",
            "new_password_required",
            "force_change_password",
        ],
    ) {
        return password_reset_text(key);
    }
    if mentions(code, reason, &["usernotconfirmed"]) {
        return "StreamBIM-kontot är inte bekräftat ännu. Bekräfta det via e-posten från StreamBIM."
            .to_string();
    }
    // Not "identity provider": every Cognito error starts "operation error Cognito Identity
    // Provider: …", a wrong password too.
    if mentions(code, reason, &["federat", "sso", "external"]) {
        return "Kontot loggar in via SSO (t.ex. Microsoft). Synka kan bara logga in StreamBIM-konton med lösenord ännu."
            .to_string();
    }
    if status == 401 || mentions(code, reason, &["notauthorized", "incorrect"]) {
        if valid_region(key) {
            return format!(
                "Fel e-post eller lösenord för StreamBIM (region {key}). \
                 Kontrollera också att kontot finns i den regionen."
            );
        }
        return format!(
            "Fel e-post eller lösenord för StreamBIM ({host}). \
             Kontrollera också att kontot finns på servern."
        );
    }
    if status == 429 {
        return TOO_MANY_LOGINS.to_string();
    }
    refused_text(status, reason)
}

/// The Swedish text for a login that needs a password change in the web app.
fn password_reset_text(key: &str) -> String {
    format!(
        "StreamBIM kräver att du byter lösenord. Logga in på {} i webbläsaren först.",
        canonical_host(key)
    )
}

/// True when a refused verify_mfa call says the code was wrong. StreamBIM's web app reads
/// `{"error": "invalidCode"}`; Cognito says `CodeMismatchException`.
fn wrong_code(code: &str, reason: &str) -> bool {
    mentions(code, reason, &["codemismatch", "invalid code", "incorrect"])
}

/// The Swedish text for a refused verify_mfa call.
fn mfa_refusal(status: u16, code: &str, reason: &str) -> String {
    if wrong_code(code, reason) {
        return WRONG_CODE.to_string();
    }
    if mentions(
        code,
        reason,
        &[
            "invalid session",
            "session is expired",
            "session expired",
            "session can only be used once",
        ],
    ) {
        return MFA_SESSION_ENDED.to_string();
    }
    if status == 429 {
        return TOO_MANY_LOGINS.to_string();
    }
    refused_text(status, reason)
}

/// The generic text: the status, and the reason when there is one.
fn refused_text(status: u16, reason: &str) -> String {
    if reason.is_empty() {
        format!("StreamBIM avvisade inloggningen (HTTP {status})")
    } else {
        format!("StreamBIM avvisade inloggningen (HTTP {status}): {reason}")
    }
}

/// True for a StreamBIM region subdomain (`^[a-z0-9-]{1,32}$`).
pub fn valid_region(region: &str) -> bool {
    (1..=32).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The account key and the origin of a region: `app` gives `https://app.streambim.com`.
pub fn server_from_region(region: &str) -> Result<(String, String), String> {
    if !valid_region(region) {
        return Err("Ogiltig StreamBIM-region.".into());
    }
    Ok((
        region.to_string(),
        format!("https://{region}.streambim.com"),
    ))
}

/// The account key and the origin of a custom StreamBIM address `https://<host>`. Only https, a
/// DNS name with at least one dot, no port, path (other than "/"), query, fragment or user info,
/// and no IP address. A host `<region>.streambim.com` is the region itself (key `<region>`). The
/// host is lowercased, so the key is the lowercase host.
pub fn server_from_url(url: &str) -> Result<(String, String), String> {
    const SCHEME: &str = "https://";
    let url = url.trim();
    let rest =
        match url.get(..SCHEME.len()) {
            Some(scheme) if scheme.eq_ignore_ascii_case(SCHEME) => &url[SCHEME.len()..],
            _ => return Err(
                "Adressen måste börja med https:// (till exempel https://streambim.example.com)."
                    .into(),
            ),
        };
    if rest.contains(['?', '#']) {
        return Err("Adressen får inte innehålla frågetecken (?) eller fragment (#).".into());
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if !path.is_empty() && path != "/" {
        return Err("Adressen får inte innehålla någon sökväg. Ange bara https://värdnamn.".into());
    }
    if authority.contains('@') {
        return Err("Adressen får inte innehålla användarnamn eller lösenord.".into());
    }
    if authority.contains(':') {
        return Err("Adressen får inte innehålla någon port.".into());
    }
    let host = authority.to_ascii_lowercase();
    if host.is_empty() {
        return Err("Ange adressen till StreamBIM-servern.".into());
    }
    if host.contains('[') {
        return Err("Ange ett värdnamn, inte en IP-adress.".into());
    }
    if host
        .split('.')
        .all(|label| label.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("Ange ett värdnamn, inte en IP-adress.".into());
    }
    if !host.contains('.') {
        return Err(
            "Värdnamnet måste ha minst en punkt, till exempel streambim.example.com.".into(),
        );
    }
    if !valid_host(&host) {
        return Err("Ogiltigt värdnamn. Använd bokstäver, siffror och bindestreck, till exempel streambim.example.com.".into());
    }
    let key = match host.strip_suffix(".streambim.com") {
        Some(label) if valid_region(label) => label.to_string(),
        _ => host.clone(),
    };
    Ok((key, format!("https://{host}")))
}

/// A DNS name: at most 253 characters, labels of 1 to 63 letters, digits and '-', none starting or
/// ending with '-'. The host is already lowercase.
fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// The account key of a server origin, when it is a StreamBIM server this app can sign in to.
/// `https://app.streambim.com` gives "app"; a loopback test origin gives None.
pub fn key_for_base(base: &str) -> Option<String> {
    server_from_url(base).ok().map(|(key, _)| key)
}

/// True for an account key: a region (`app`) or the lowercase host of a custom server.
pub fn valid_key(key: &str) -> bool {
    valid_region(key)
        || (key.contains('.')
            && server_from_url(&format!("https://{key}")).is_ok_and(|(found, _)| found == key))
}

/// The host of an account's server: `<key>.streambim.com` for a region, the key itself otherwise.
pub fn canonical_host(key: &str) -> String {
    if valid_region(key) {
        format!("{key}.streambim.com")
    } else {
        key.to_string()
    }
}

/// The account key of a project: from the origin of its `meta.base`, else its `meta.region`
/// (the key of a project made before a custom server could be chosen).
pub fn project_key(p: &Project) -> Option<String> {
    let meta: Value = serde_json::from_str(p.meta.as_deref()?).ok()?;
    meta.get("base")
        .and_then(Value::as_str)
        .and_then(key_for_base)
        .or_else(|| {
            meta.get("region")
                .and_then(Value::as_str)
                .filter(|region| valid_key(region))
                .map(str::to_string)
        })
}

/// What the user signs in to for a project: "StreamBIM (<host>)".
pub fn sign_in_label(p: &Project) -> String {
    match project_key(p) {
        Some(key) => format!("StreamBIM ({})", canonical_host(&key)),
        None => "StreamBIM".to_string(),
    }
}

/// The base URL of an account key: `https://<key>.streambim.com`, or `https://<host>`.
fn base_for_key(key: &str) -> String {
    format!("https://{}", canonical_host(key))
}

/// The keys of the profiles in the state directory (`user-streambim-<key>.json`), sorted.
fn keys_on_disk(state_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let key = name
                .strip_prefix(PROFILE_PREFIX)?
                .strip_suffix(".json")?
                .to_string();
            valid_key(&key).then_some(key)
        })
        .collect();
    keys.sort();
    keys
}

/// A single-account install keeps `token-streambim.bin` and `user-streambim.json`. Their server
/// gives the key (from the profile's `base`, else its `region`), and each file is renamed to the
/// keyed name, unless that file already exists: then the legacy one is deleted. A profile with no
/// usable server is removed with its token, since nothing can use them.
fn migrate_legacy(state_dir: &Path) {
    let legacy_profile = state_dir.join(LEGACY_PROFILE_FILE);
    if !legacy_profile.exists() {
        return;
    }
    let key = std::fs::read(&legacy_profile)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|profile| {
            profile
                .get("base")
                .and_then(Value::as_str)
                .and_then(key_for_base)
                .or_else(|| {
                    profile
                        .get("region")
                        .and_then(Value::as_str)
                        .filter(|region| valid_region(region))
                        .map(str::to_string)
                })
        });
    let Some(key) = key else {
        warn!("StreamBIM: den gamla kontofilen saknar server och tas bort");
        drop_file(&legacy_profile);
        drop_file(&state_dir.join(LEGACY_TOKEN_FILE));
        return;
    };
    move_or_drop(&legacy_profile, &state_dir.join(profile_file(&key)));
    move_or_drop(
        &state_dir.join(LEGACY_TOKEN_FILE),
        &state_dir.join(token_file(&key)),
    );
    info!("StreamBIM: kontofilerna flyttades till kontot {key}");
}

/// Renames `from` to `to`, unless `to` exists: then `from` is deleted.
fn move_or_drop(from: &Path, to: &Path) {
    if !from.exists() {
        return;
    }
    if to.exists() {
        drop_file(from);
    } else if let Err(e) = std::fs::rename(from, to) {
        warn!("StreamBIM: kunde inte flytta {}: {e}", from.display());
    }
}

/// Removes a file, logging a failure (no contents are named).
fn drop_file(path: &Path) {
    if let Err(e) = remove_if_exists(path) {
        warn!("StreamBIM: kunde inte ta bort en kontofil: {e:#}");
    }
}

impl StreamBimAccounts {
    /// The accounts whose profiles are in `state_dir`. A single-account install is migrated first.
    pub fn load(http: reqwest::Client, state_dir: &Path) -> StreamBimAccounts {
        migrate_legacy(state_dir);
        let accounts = StreamBimAccounts {
            http,
            state_dir: state_dir.to_path_buf(),
            accounts: RwLock::new(BTreeMap::new()),
        };
        for key in keys_on_disk(state_dir) {
            let account = accounts.make(&key, &base_for_key(&key));
            accounts.write().insert(key, account);
        }
        accounts
    }

    /// The account with this key, if it exists (signed in or not).
    pub fn get(&self, key: &str) -> Option<StreamBimAccount> {
        self.read().get(key).cloned()
    }

    /// The account with this key, added (signed out) on first use with its server `base`.
    pub fn get_or_add(&self, key: &str, base: &str) -> StreamBimAccount {
        if let Some(account) = self.get(key) {
            return account;
        }
        let mut accounts = self.write();
        accounts
            .entry(key.to_string())
            .or_insert_with(|| self.make(key, base))
            .clone()
    }

    /// Every account, sorted by key.
    pub fn all(&self) -> Vec<StreamBimAccount> {
        self.read().values().cloned().collect()
    }

    /// The accounts that are signed in, sorted by key.
    pub fn signed_in(&self) -> Vec<StreamBimAccount> {
        self.all()
            .into_iter()
            .filter(|account| account.auth.is_signed_in())
            .collect()
    }

    /// True when at least one account is signed in.
    pub fn any_signed_in(&self) -> bool {
        self.all().iter().any(|account| account.auth.is_signed_in())
    }

    /// The account a project belongs to (its key, see `project_key`).
    pub fn for_project(&self, p: &Project) -> Option<StreamBimAccount> {
        self.get(&project_key(p)?)
    }

    /// The account of a StreamBIM hub (its region is the key).
    pub fn for_hub(&self, hub: &HubInfo) -> Option<StreamBimAccount> {
        self.get(hub.region.as_deref()?)
    }

    /// Drops the in-memory account when it has no session and waits for no MFA code. Its files
    /// are already gone after a sign-out; a failed sign-in leaves none.
    pub fn forget_if_signed_out(&self, key: &str) {
        let mut accounts = self.write();
        let signed_out = accounts
            .get(key)
            .is_some_and(|account| !account.auth.is_signed_in() && !account.auth.has_pending_mfa());
        if signed_out {
            accounts.remove(key);
        }
    }

    fn make(&self, key: &str, base: &str) -> StreamBimAccount {
        let auth = Arc::new(StreamBimAuth::new(
            self.http.clone(),
            &self.state_dir,
            key,
            base,
        ));
        let api = Api::with_source(self.http.clone(), TokenSource::StreamBim(auth.clone()));
        StreamBimAccount {
            key: key.to_string(),
            auth,
            api,
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, StreamBimAccount>> {
        self.accounts.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, StreamBimAccount>> {
        self.accounts.write().unwrap_or_else(|e| e.into_inner())
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    use std::sync::Arc;

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
            server_from_region("sweden").unwrap().1,
            "https://sweden.streambim.com"
        );
        assert_eq!(
            server_from_region("app").unwrap().1,
            "https://app.streambim.com"
        );
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
            assert!(server_from_region(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn login_success_persists_files_and_signs_in() {
        let server = success_server().await;
        let dir = TestDir::new("login");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);

        let outcome = auth.login("ada@example.com", "pw-secret").await.unwrap();
        match outcome {
            LoginOutcome::SignedIn(user) => {
                assert_eq!(user.name, "Ada Lovelace");
                assert_eq!(user.email, "ada@example.com");
                assert_eq!(user.user_id, "ada@example.com");
            }
            LoginOutcome::MfaRequired => panic!("no MFA expected"),
        }
        assert!(auth.is_signed_in());
        assert_eq!(auth.host(), "127.0.0.1");

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
        let token_bytes = std::fs::read(dir.path().join(token_file("app"))).unwrap();
        assert!(!token_bytes.windows(4).any(|w| w == b"rt-1"));
        let profile_text = std::fs::read_to_string(dir.path().join(profile_file("app"))).unwrap();
        assert!(profile_text.contains("\"region\":\"app\""));
        assert!(profile_text.contains("\"base\""));
        for secret in ["pw-secret", "rt-1", "id-1", "at-1"] {
            assert!(!profile_text.contains(secret));
        }

        // A new instance (after a restart) is signed in and uses the stored id token, with no
        // request, while the token is fresh.
        let again = StreamBimAuth::new(http(), dir.path(), "app", &server.origin);
        assert!(again.is_signed_in());
        assert_eq!(again.user().unwrap().email, "ada@example.com");
        assert_eq!(again.access_token().await.unwrap(), "id-1");
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn wrong_password_is_a_swedish_error_without_the_password() {
        let server = Server::new(|_, _| reply_json(401, json!({"error": "bad"}))).await;
        let dir = TestDir::new("wrongpw");
        let auth = StreamBimAuth::for_test_key(http(), dir.path(), "sweden", &server.origin);
        let err = auth
            .login("ada@example.com", "hunter2-secret")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "Fel e-post eller lösenord för StreamBIM (region sweden). Kontrollera också att kontot finns i den regionen."
        );
        assert!(!err.contains("hunter2"));
        assert!(!auth.is_signed_in());
    }

    #[tokio::test]
    async fn other_login_failures_report_the_status_and_the_reason() {
        let server = Server::new(|_, _| reply_json(500, json!({"detail": "boom"}))).await;
        let dir = TestDir::new("status");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        let err = auth.login("a@b.se", "pw").await.unwrap_err().to_string();
        assert_eq!(err, "StreamBIM avvisade inloggningen (HTTP 500): boom");
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

        let outcome = auth.login("ada@example.com", "pw").await.unwrap();
        assert!(matches!(outcome, LoginOutcome::MfaRequired));
        assert!(!auth.is_signed_in());
        assert!(!dir.path().join(token_file("app")).exists());

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
            "/auth/v1/mfa/verify" => reply_json(
                401,
                json!({"__type": "CodeMismatchException", "message": "Invalid code received for user."}),
            ),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("badcode");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);

        // Malformed codes never reach the server.
        auth.login("ada@example.com", "pw").await.unwrap();
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

        // A wrong code keeps the challenge for another code, up to MAX_CODE_TRIES wrong codes.
        // Then the challenge ends: the next try needs a new password step.
        auth.login("ada@example.com", "pw").await.unwrap();
        for _ in 1..MAX_CODE_TRIES {
            let err = auth.verify_mfa("000000").await.unwrap_err().to_string();
            assert_eq!(err, WRONG_CODE);
            assert!(auth.has_pending_mfa());
        }
        let err = auth.verify_mfa("000000").await.unwrap_err().to_string();
        assert_eq!(err, WRONG_CODE_LAST);
        assert!(!auth.has_pending_mfa());
        assert_eq!(server.count(), 2 + MAX_CODE_TRIES as usize);
        let err = auth.verify_mfa("000000").await.unwrap_err().to_string();
        assert_eq!(err, CODE_EXPIRED);
        assert_eq!(server.count(), 2 + MAX_CODE_TRIES as usize);
    }

    /// StreamBIM's own shape: the challenge comes with HTTP 401, and the verify reply is token
    /// data (its web app reads no `result` from it).
    #[tokio::test]
    async fn a_challenge_sent_with_401_asks_for_the_code() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                401,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-secret-7"}),
            ),
            "/auth/v1/mfa/verify" => reply_json(
                200,
                json!({"idToken": "id-2", "refreshToken": "rt-2", "accessToken": "at-2",
                       "tokenType": "Bearer", "expiresIn": 3600}),
            ),
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("mfa401");
        let auth = StreamBimAuth::for_test_key(http(), dir.path(), "sweden", &server.origin);
        let logs = LogCapture::default();
        let _guard = logs.install();

        let outcome = auth.login("ada@example.com", "pw").await.unwrap();
        assert!(matches!(outcome, LoginOutcome::MfaRequired));
        assert!(auth.has_pending_mfa());
        assert!(!auth.is_signed_in());

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
            json!({"username": "ada@example.com", "session": "sess-secret-7", "code": "123456"})
        );
        let log = logs.text();
        assert!(!log.contains("anropet avvisades"), "{log}");
        assert!(!log.contains("sess-secret-7"), "{log}");

        // The verify reply's refresh token is the one the session refreshes with.
        auth.invalidate().await;
        let _ = auth.access_token().await;
        let refresh = server
            .requests()
            .into_iter()
            .find(|r| r.target == "/auth/v1/refresh")
            .unwrap();
        assert_eq!(
            body_json(&refresh),
            json!({"token": "id-2", "refreshToken": "rt-2"})
        );
    }

    #[tokio::test]
    async fn a_mistyped_code_can_be_typed_again_with_the_same_session() {
        let server = Server::scripted(vec![
            reply_json(
                401,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-1"}),
            ),
            reply_json(400, json!({"error": "invalidCode"})),
            reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-2", "refreshToken": "rt-2", "expiresIn": 3600}),
            ),
            profile_reply(),
        ])
        .await;
        let dir = TestDir::new("retrycode");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("ada@example.com", "pw").await.unwrap();

        let err = auth.verify_mfa("111111").await.unwrap_err().to_string();
        assert_eq!(err, WRONG_CODE);
        assert!(auth.has_pending_mfa());
        auth.verify_mfa("222222").await.unwrap();
        assert!(auth.is_signed_in());
        assert!(!auth.has_pending_mfa());

        let sessions: Vec<Value> = server
            .requests()
            .iter()
            .filter(|r| r.target == "/auth/v1/mfa/verify")
            .map(|r| body_json(r)["session"].clone())
            .collect();
        assert_eq!(sessions, vec![json!("sess-1"), json!("sess-1")]);
    }

    #[tokio::test]
    async fn a_new_challenge_from_the_verify_step_takes_the_next_code() {
        let server = Server::scripted(vec![
            reply_json(
                200,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-1"}),
            ),
            reply_json(
                401,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-2"}),
            ),
            reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-2", "expiresIn": 3600}),
            ),
            profile_reply(),
        ])
        .await;
        let dir = TestDir::new("rechallenge");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("ada@example.com", "pw").await.unwrap();

        let err = auth.verify_mfa("111111").await.unwrap_err().to_string();
        assert_eq!(err, WRONG_CODE);
        auth.verify_mfa("222222").await.unwrap();
        let last = server
            .requests()
            .into_iter()
            .rfind(|r| r.target == "/auth/v1/mfa/verify")
            .unwrap();
        assert_eq!(body_json(&last)["session"], json!("sess-2"));
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
        auth.login("ada@example.com", "pw").await.unwrap();

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
        auth.login("ada@example.com", "pw").await.unwrap();
        assert!(dir.path().join(token_file("app")).exists());

        auth.invalidate().await;
        let err = auth.access_token().await.unwrap_err().to_string();
        assert_eq!(
            err,
            "StreamBIM-sessionen har gått ut – logga in igen. (Session expired)"
        );
        assert!(!auth.is_signed_in());
        assert!(auth.user().is_none());
        assert!(!dir.path().join(token_file("app")).exists());
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
        auth.login("ada@example.com", "pw").await.unwrap();
        auth.verify_mfa("654321").await.unwrap();
        let count = server.count();

        auth.invalidate().await;
        let err = auth.access_token().await.unwrap_err().to_string();
        assert!(err.contains("logga in igen"), "{err}");
        assert_eq!(server.count(), count, "no refresh without a refresh token");
        assert!(!auth.is_signed_in());
        assert!(!dir.path().join(token_file("app")).exists());
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
        auth.login("ada@example.com", "pw").await.unwrap();
        auth.invalidate().await;

        let err = auth.access_token().await.unwrap_err().to_string();
        assert_eq!(
            err,
            "StreamBIM-sessionen kunde inte förnyas: HTTP 503. Försök igen."
        );
        assert!(auth.is_signed_in());
        assert!(dir.path().join(token_file("app")).exists());
    }

    #[tokio::test]
    async fn sign_out_removes_the_session_and_the_files() {
        let server = success_server().await;
        let dir = TestDir::new("signout");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("ada@example.com", "pw").await.unwrap();
        auth.sign_out().await.unwrap();
        assert!(!auth.is_signed_in());
        assert!(auth.user().is_none());
        assert!(!dir.path().join(token_file("app")).exists());
        assert!(!dir.path().join(profile_file("app")).exists());
        assert!(!StreamBimAuth::new(http(), dir.path(), "app", &server.origin).is_signed_in());
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
        auth.login("ada@example.com", "pw").await.unwrap();

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
            auth.login("ada@example.com", "pw").await.unwrap();
            auth.invalidate_token("id-1").await;

            let err = auth.access_token().await.unwrap_err().to_string();
            assert_eq!(
                err,
                format!("StreamBIM-sessionen kunde inte förnyas: HTTP {status}. Försök igen.")
            );
            assert!(auth.is_signed_in(), "{status}: the session stays");
            assert!(dir.path().join(token_file("app")).exists(), "{status}");
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
        auth.login("ada@example.com", "pw").await.unwrap();
        auth.invalidate_token("id-1").await;

        assert_eq!(
            auth.access_token().await.unwrap_err().to_string(),
            REFRESH_RETRY
        );
        assert!(auth.is_signed_in());
        assert!(dir.path().join(token_file("app")).exists());
    }

    #[tokio::test]
    async fn refusals_and_a_new_mfa_challenge_end_the_session() {
        let answers: [(&str, Fault); 5] = [
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
            (
                "challenge with 401",
                reply_json(
                    401,
                    json!({"result": "CHALLENGE_REQUESTED", "session": "s"}),
                ),
            ),
        ];
        for (name, answer) in answers {
            let server = refresh_server(move || answer.clone()).await;
            let dir = TestDir::new("refused");
            let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
            auth.login("ada@example.com", "pw").await.unwrap();
            auth.invalidate_token("id-1").await;

            assert_eq!(
                auth.access_token().await.unwrap_err().to_string(),
                SESSION_EXPIRED,
                "{name}"
            );
            assert!(!auth.is_signed_in(), "{name}");
            assert!(!dir.path().join(token_file("app")).exists(), "{name}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_token_file_ends_the_sign_in() {
        let server = success_server().await;
        let dir = TestDir::new("unreadable");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("ada@example.com", "pw").await.unwrap();

        // Bytes that are not DPAPI data, and DPAPI data that is not a token document.
        let garbage = [
            b"not protected".to_vec(),
            dpapi_protect(b"{\"refresh_").unwrap(),
        ];
        for bytes in garbage {
            std::fs::write(dir.path().join(token_file("app")), &bytes).unwrap();
            let again = StreamBimAuth::new(http(), dir.path(), "app", &server.origin);
            assert!(!again.is_signed_in());
            assert!(again.user().is_none());
            assert!(
                !dir.path().join(token_file("app")).exists(),
                "the file is removed"
            );
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

    /// Collects what the tracing subscriber writes while `install` is in force (this thread).
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl LogCapture {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let sink = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || sink.clone())
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The error text of a login that the server answers with `fault`. The password and the
    /// e-mail are asserted to be absent from it.
    async fn login_error_with(fault: Fault) -> String {
        let server = Server::new(move |_, _| fault.clone()).await;
        let dir = TestDir::new("loginerr");
        let auth = StreamBimAuth::for_test_key(http(), dir.path(), "sweden", &server.origin);
        let err = auth
            .login("ada@example.com", "pw-secret")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("pw-secret") && !err.contains("ada@example.com"),
            "{err}"
        );
        err
    }

    /// The error text of a verify_mfa call that follows a password step asking for MFA, when the
    /// server answers the verify step with `verify`.
    async fn mfa_error_with(verify: Fault) -> String {
        let server = Server::new(move |req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-1"}),
            ),
            "/auth/v1/mfa/verify" => verify.clone(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("mfaerr");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("ada@example.com", "pw").await.unwrap();
        let err = auth.verify_mfa("123456").await.unwrap_err().to_string();
        assert!(
            !err.contains("sess-1") && !err.contains("123456") && !err.contains("ada@example.com"),
            "{err}"
        );
        err
    }

    #[tokio::test]
    async fn an_unknown_account_names_the_region() {
        let err = login_error_with(reply_json(
            400,
            json!({"__type": "UserNotFoundException", "message": "User does not exist."}),
        ))
        .await;
        assert_eq!(
            err,
            "Det finns inget StreamBIM-konto med den e-postadressen i regionen sweden. Välj en annan region (de flesta svenska konton finns i Europe eller Sweden)."
        );
    }

    #[tokio::test]
    async fn a_cognito_wrong_password_is_a_password_error() {
        let err = login_error_with(reply_json(
            400,
            json!({"__type": "NotAuthorizedException", "message": "Incorrect username or password."}),
        ))
        .await;
        assert_eq!(
            err,
            "Fel e-post eller lösenord för StreamBIM (region sweden). Kontrollera också att kontot finns i den regionen."
        );
    }

    /// StreamBIM passes Cognito's error on in one string that names the "Cognito Identity
    /// Provider" (seen live on sweden.streambim.com). That is a wrong password, not SSO.
    #[tokio::test]
    async fn streambims_cognito_wrong_password_text_is_a_password_error() {
        let err = login_error_with(reply_json(
            401,
            json!({"error": "operation error Cognito Identity Provider: InitiateAuth, https response error StatusCode: 400, RequestID: 3d6e4fe6-7051-4e7a-b828-e4d05bd18a2b, NotAuthorizedException: Incorrect username or password."}),
        ))
        .await;
        assert_eq!(
            err,
            "Fel e-post eller lösenord för StreamBIM (region sweden). Kontrollera också att kontot finns i den regionen."
        );
    }

    #[tokio::test]
    async fn an_aws_error_shows_what_went_wrong_first() {
        let err = login_error_with(reply_json(
            400,
            json!({"error": "operation error Cognito Identity Provider: InitiateAuth, https response error StatusCode: 400, RequestID: 3d6e4fe6-7051-4e7a-b828-e4d05bd18a2b, InternalErrorException: Something odd."}),
        ))
        .await;
        assert_eq!(
            err,
            "StreamBIM avvisade inloggningen (HTTP 400): InternalErrorException: Something odd. (RequestID: 3d6e4fe6-7051-4e7a-b828-e4d05bd18a2b)"
        );
    }

    #[test]
    fn aws_error_first_shapes() {
        assert_eq!(
            aws_error_first("operation error X: Op, https response error StatusCode: 400, RequestID: r-1, api error CodeMismatchException: Invalid code received for user"),
            "CodeMismatchException: Invalid code received for user (RequestID: r-1)"
        );
        for kept in [
            "Incorrect username or password.",
            "operation error X: Op, no request id here",
            "operation error X: Op, RequestID: r-1",
            "operation error X: Op, RequestID: r-1, ",
            "Rejected, RequestID: r-1, something",
        ] {
            assert_eq!(aws_error_first(kept), kept);
        }
    }

    #[tokio::test]
    async fn a_password_reset_is_sent_to_the_web_app() {
        let text = "StreamBIM kräver att du byter lösenord. Logga in på sweden.streambim.com i webbläsaren först.";
        let err = login_error_with(reply_json(
            400,
            json!({"message": "Password reset required for the user"}),
        ))
        .await;
        assert_eq!(err, text);
        // Cognito's own result, in a 200 reply.
        let err =
            login_error_with(reply_json(200, json!({"result": "NEW_PASSWORD_REQUIRED"}))).await;
        assert_eq!(err, text);
    }

    #[tokio::test]
    async fn an_unconfirmed_account_is_named() {
        let err = login_error_with(reply_json(
            400,
            json!({"__type": "UserNotConfirmedException", "message": "User is not confirmed."}),
        ))
        .await;
        assert_eq!(
            err,
            "StreamBIM-kontot är inte bekräftat ännu. Bekräfta det via e-posten från StreamBIM."
        );
    }

    #[tokio::test]
    async fn a_federated_account_is_told_to_use_sso() {
        let err = login_error_with(reply_json(
            400,
            json!({"message": "Users from a federated identity provider must sign in with it."}),
        ))
        .await;
        assert_eq!(
            err,
            "Kontot loggar in via SSO (t.ex. Microsoft). Synka kan bara logga in StreamBIM-konton med lösenord ännu."
        );
    }

    #[tokio::test]
    async fn throttling_is_named() {
        let err = login_error_with(Fault::reply(429, "")).await;
        assert_eq!(
            err,
            "StreamBIM: för många inloggningsförsök. Vänta en stund och försök igen."
        );
    }

    #[tokio::test]
    async fn an_html_refusal_is_named_and_not_shown() {
        let html = Fault::reply(403, "<html><body><h1>Forbidden</h1></body></html>")
            .header("Content-Type", "text/html");
        assert_eq!(
            login_error_with(html).await,
            "StreamBIM avvisade inloggningen (HTTP 403): (HTML)"
        );
    }

    #[tokio::test]
    async fn a_json_api_error_gives_its_detail() {
        let err = login_error_with(reply_json(
            400,
            json!({"errors": [{"status": "400", "code": "badRequest", "detail": "The request body is malformed."}]}),
        ))
        .await;
        assert_eq!(
            err,
            "StreamBIM avvisade inloggningen (HTTP 400): The request body is malformed."
        );
    }

    #[tokio::test]
    async fn a_refusal_without_a_reason_has_the_status_only() {
        assert_eq!(
            login_error_with(Fault::reply(404, "")).await,
            "StreamBIM avvisade inloggningen (HTTP 404)"
        );
    }

    #[tokio::test]
    async fn an_error_reply_is_read_up_to_8_kib_only() {
        // The '<' comes after the first 8 KiB, so it is never read and HTML is not named.
        let mut body = "a".repeat(ERROR_BODY_BYTES + 100);
        body.push('<');
        let err = login_error_with(Fault::reply(400, body)).await;
        assert_eq!(
            err,
            format!(
                "StreamBIM avvisade inloggningen (HTTP 400): {}",
                "a".repeat(TEXT_REASON_CHARS)
            )
        );
    }

    #[tokio::test]
    async fn an_echoed_login_keeps_the_password_and_the_e_mail_out() {
        // The server repeats the request body in its error. The password has a quote and a
        // backslash, so its JSON-escaped form is echoed too.
        let server = Server::new(|req, _| {
            let echoed = String::from_utf8_lossy(&req.body).into_owned();
            reply_json(400, json!({"message": format!("Rejected: {echoed}")}))
        })
        .await;
        let dir = TestDir::new("echo");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        let logs = LogCapture::default();
        let _guard = logs.install();

        let err = auth
            .login("ada@example.com", "hunter\"2\\secret")
            .await
            .unwrap_err()
            .to_string();
        let log = logs.text();
        assert_eq!(
            err,
            r#"StreamBIM avvisade inloggningen (HTTP 400): Rejected: {"password":"***","username":"***"}"#
        );
        assert!(log.contains("StreamBIM: anropet avvisades"), "{log}");
        for secret in ["hunter", "secret", "ada@example.com"] {
            assert!(
                !err.contains(secret) && !log.contains(secret),
                "{secret}: {log}"
            );
        }
    }

    #[tokio::test]
    async fn an_echoed_mfa_step_keeps_the_session_and_the_code_out() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-secret-9"}),
            ),
            "/auth/v1/mfa/verify" => {
                let echoed = String::from_utf8_lossy(&req.body).into_owned();
                reply_json(400, json!({"message": format!("Rejected: {echoed}")}))
            }
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("mfaecho");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        let logs = LogCapture::default();
        let _guard = logs.install();
        auth.login("ada@example.com", "pw").await.unwrap();

        let err = auth.verify_mfa("482915").await.unwrap_err().to_string();
        let log = logs.text();
        assert_eq!(
            err,
            r#"StreamBIM avvisade inloggningen (HTTP 400): Rejected: {"code":"***","session":"***","username":"***"}"#
        );
        for secret in ["sess-secret-9", "482915", "ada@example.com"] {
            assert!(
                !err.contains(secret) && !log.contains(secret),
                "{secret}: {log}"
            );
        }
    }

    #[tokio::test]
    async fn an_echoed_refresh_keeps_the_tokens_out_of_the_log() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-secret-token", "refreshToken": "rt-secret-token", "expiresIn": 3600}),
            ),
            "/auth/v1/refresh" => {
                let echoed = String::from_utf8_lossy(&req.body).into_owned();
                reply_json(400, json!({"message": format!("Expired: {echoed}")}))
            }
            t if t.starts_with("/mgw/api/v2/user-profiles") => profile_reply(),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let dir = TestDir::new("refreshecho");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("ada@example.com", "pw").await.unwrap();
        auth.invalidate().await;
        let logs = LogCapture::default();
        let _guard = logs.install();

        assert_eq!(
            auth.access_token().await.unwrap_err().to_string(),
            SESSION_EXPIRED
        );
        let log = logs.text();
        assert!(log.contains("Expired: "), "{log}");
        assert!(log.contains("***"), "{log}");
        for secret in ["id-secret-token", "rt-secret-token"] {
            assert!(!log.contains(secret), "{log}");
        }
    }

    #[tokio::test]
    async fn a_failed_call_is_logged_at_warn_with_the_reason() {
        let server = Server::new(|_, _| {
            reply_json(
                400,
                json!({"__type": "UserNotFoundException", "message": "User does not exist."}),
            )
        })
        .await;
        let dir = TestDir::new("warnlog");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        let logs = LogCapture::default();
        let _guard = logs.install();

        auth.login("ada@example.com", "pw").await.unwrap_err();
        let log = logs.text();
        let line = log
            .lines()
            .find(|line| line.contains("anropet avvisades"))
            .unwrap_or_default();
        for part in [
            "WARN",
            "login",
            "400",
            "UserNotFoundException",
            "User does not exist.",
        ] {
            assert!(line.contains(part), "{part}: {log}");
        }
    }

    #[tokio::test]
    async fn an_unexpected_result_is_logged_at_warn() {
        let server =
            Server::new(|_, _| reply_json(200, json!({"result": "NEW_PASSWORD_REQUIRED"}))).await;
        let dir = TestDir::new("resultlog");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        let logs = LogCapture::default();
        let _guard = logs.install();

        auth.login("ada@example.com", "pw").await.unwrap_err();
        let log = logs.text();
        assert!(
            log.lines()
                .any(|line| line.contains("WARN") && line.contains("NEW_PASSWORD_REQUIRED")),
            "{log}"
        );
    }

    #[tokio::test]
    async fn a_wrong_mfa_code_is_named_and_other_refusals_give_their_reason() {
        assert_eq!(
            mfa_error_with(reply_json(
                400,
                json!({"__type": "CodeMismatchException", "message": "Invalid code received for user."}),
            ))
            .await,
            WRONG_CODE
        );
        assert_eq!(
            mfa_error_with(reply_json(200, json!({"result": "CODE_MISMATCH"}),)).await,
            WRONG_CODE
        );
        // StreamBIM's web app reads this one.
        assert_eq!(
            mfa_error_with(reply_json(401, json!({"error": "invalidCode"}))).await,
            WRONG_CODE
        );
        assert_eq!(
            mfa_error_with(reply_json(
                400,
                json!({"error": "operation error Cognito Identity Provider: RespondToAuthChallenge, https response error StatusCode: 400, RequestID: 3d6e4fe6-7051-4e7a-b828-e4d05bd18a2b, NotAuthorizedException: Invalid session for the user, session is expired."}),
            ))
            .await,
            MFA_SESSION_ENDED
        );
        assert_eq!(
            mfa_error_with(reply_json(
                400,
                json!({"__type": "ExpiredCodeException", "message": "Code has expired."}),
            ))
            .await,
            "StreamBIM avvisade inloggningen (HTTP 400): Code has expired."
        );
        assert_eq!(
            mfa_error_with(Fault::reply(429, "")).await,
            "StreamBIM: för många inloggningsförsök. Vänta en stund och försök igen."
        );
    }

    #[tokio::test]
    async fn a_transient_mfa_failure_keeps_the_challenge() {
        let server = Server::scripted(vec![
            reply_json(
                200,
                json!({"result": "CHALLENGE_REQUESTED", "session": "sess-1"}),
            ),
            Fault::reply(503, "busy"),
            reply_json(
                200,
                json!({"result": "SUCCESS", "idToken": "id-2", "expiresIn": 3600}),
            ),
            profile_reply(),
        ])
        .await;
        let dir = TestDir::new("transient");
        let auth = StreamBimAuth::for_test(http(), dir.path(), &server.origin);
        auth.login("ada@example.com", "pw").await.unwrap();

        let err = auth.verify_mfa("123456").await.unwrap_err().to_string();
        assert_eq!(err, "StreamBIM avvisade inloggningen (HTTP 503): busy");
        // The same challenge is still there for the next code.
        assert!(auth.verify_mfa("123456").await.is_ok());
        assert!(auth.is_signed_in());
    }

    #[test]
    fn refusal_details_read_the_known_fields() {
        let details = |body: &[u8]| refusal_details(body, &[]);
        assert_eq!(
            details(br#"{"__type":"UserNotFoundException","message":"User does not exist."}"#),
            (
                "UserNotFoundException".to_string(),
                "User does not exist.".to_string()
            )
        );
        assert_eq!(
            details(br#"{"detail":"d","message":"m"}"#),
            (String::new(), "m".to_string())
        );
        assert_eq!(
            details(br#"{"error":{"x":1},"detail":"d"}"#),
            (String::new(), "d".to_string())
        );
        assert_eq!(
            details(br#"{"errors":[{"detail":"D","title":"T","code":"c"}]}"#),
            ("c".to_string(), "D".to_string())
        );
        assert_eq!(
            details(br#"{"errors":[{"title":"T"}]}"#),
            (String::new(), "T".to_string())
        );
        assert_eq!(
            details(br#"{"error":"invalid_grant"}"#),
            ("invalid_grant".to_string(), "invalid_grant".to_string())
        );
        assert_eq!(details(b""), (String::new(), String::new()));
        assert_eq!(details(b"<html>"), (String::new(), "(HTML)".to_string()));
        assert_eq!(
            details(b"Bad gateway"),
            (String::new(), "Bad gateway".to_string())
        );
    }

    #[test]
    fn clean_removes_secrets_controls_and_extra_space() {
        let forms = secret_forms(&["ada@example.com", "hunter2"]);
        // Case does not matter for the secrets.
        assert_eq!(
            clean("Hej ADA@example.com, hunter2!", &forms, 200),
            "Hej ***, ***!"
        );
        assert_eq!(clean("a\u{0}b\tc\n\n d\u{7f}e", &[], 200), "ab c de");
        assert_eq!(clean(" \n ", &[], 200), "");
        // The cap counts characters. A secret is removed before the cut, so none of it survives.
        assert_eq!(clean(&"å".repeat(300), &[], 200).chars().count(), 200);
        assert_eq!(clean("pw=hunter2", &forms, 5), "pw=**");
        assert_eq!(clean("abc def", &[], 4), "abc");
    }

    #[test]
    fn secrets_are_removed_in_json_form_longest_first_and_never_empty() {
        let forms = secret_forms(&["hunter\"2\\secret"]);
        assert_eq!(
            clean(r#"x "hunter\"2\\secret" y"#, &forms, 200),
            r#"x "***" y"#
        );
        assert_eq!(
            clean(
                "ada@example.com",
                &secret_forms(&["ada", "ada@example.com"]),
                200
            ),
            "***"
        );
        assert_eq!(
            clean("banana", &secret_forms(&["", "n"]), 200),
            "ba***a***a"
        );
    }

    #[test]
    fn a_region_is_a_server_under_its_own_key() {
        assert_eq!(
            server_from_region("app").unwrap(),
            ("app".to_string(), "https://app.streambim.com".to_string())
        );
        assert_eq!(
            server_from_region("japan").unwrap().1,
            "https://japan.streambim.com"
        );
        for bad in [
            "",
            "App",
            "a.b",
            "a/b",
            "a b",
            "evil.example.com/x",
            "https://app.streambim.com",
        ] {
            assert!(server_from_region(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(server_from_region(&"a".repeat(33)).is_err());
    }

    #[test]
    fn a_custom_address_is_an_https_host_and_nothing_else() {
        let ok = |url: &str| server_from_url(url).unwrap();
        let example = (
            "streambim.example.com".to_string(),
            "https://streambim.example.com".to_string(),
        );
        assert_eq!(ok("https://streambim.example.com"), example);
        assert_eq!(
            ok("https://streambim.example.com/"),
            example,
            "a trailing slash is fine"
        );
        assert_eq!(
            ok("https://StreamBIM.Example.COM"),
            example,
            "the host is lowercased"
        );
        assert_eq!(
            ok("  https://streambim.example.com \n"),
            example,
            "the ends are trimmed"
        );
        // A host under streambim.com with a region label is that region.
        assert_eq!(
            ok("https://japan.streambim.com"),
            (
                "japan".to_string(),
                "https://japan.streambim.com".to_string()
            )
        );
        assert_eq!(ok("https://JAPAN.streambim.com/").0, "japan");
        // A label that is not a region keeps the whole host as its key.
        assert_eq!(ok("https://a.b.streambim.com").0, "a.b.streambim.com");

        for bad in [
            "http://streambim.example.com",
            "ftp://streambim.example.com",
            "streambim.example.com",
            "https://",
            "https://streambim.example.com:443",
            "https://streambim.example.com:8443",
            "https://streambim.example.com/x",
            "https://streambim.example.com/?q=1",
            "https://streambim.example.com?q=1",
            "https://streambim.example.com#frag",
            "https://user@streambim.example.com",
            "https://user:pw@streambim.example.com",
            "https://192.168.0.1",
            "https://[::1]",
            "https://localhost",
            "https://streambim_example.com",
            "https://-bad.example.com",
            "https://bad-.example.com",
            "https://a..b.com",
            "https://example.com.",
            "https://ex ample.com",
            "https://ä.example.com",
        ] {
            assert!(server_from_url(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn a_base_gives_its_key_and_only_a_streambim_server_has_one() {
        assert_eq!(
            key_for_base("https://app.streambim.com").as_deref(),
            Some("app")
        );
        assert_eq!(
            key_for_base("https://app.streambim.com/").as_deref(),
            Some("app")
        );
        assert_eq!(
            key_for_base("https://streambim.example.com").as_deref(),
            Some("streambim.example.com")
        );
        // The loopback origins of the tests have no key: the project's region names it then.
        assert_eq!(key_for_base("http://127.0.0.1:9000"), None);
        assert_eq!(key_for_base("garbage"), None);
        assert!(valid_key("app"));
        assert!(valid_key("streambim.example.com"));
        assert!(!valid_key("app.streambim.com"), "a region key has no dot");
        assert!(!valid_key("App"));
        assert!(!valid_key("1.2.3.4"));
        assert_eq!(canonical_host("sweden"), "sweden.streambim.com");
        assert_eq!(
            canonical_host("streambim.example.com"),
            "streambim.example.com"
        );
    }

    fn project_with_meta(meta: &str) -> Project {
        Project {
            pid: 1,
            hub_id: "streambim:app".into(),
            hub_name: "StreamBIM".into(),
            project_id: "streambim:app:13588".into(),
            name: "Demo".into(),
            root_folder_urn: "0".into(),
            urn_ns: String::new(),
            tops: vec![crate::model::TopFolder {
                id: "0".into(),
                name: "Dokument".into(),
            }],
            oss_scopes: None,
            change_source: crate::model::ChangeSourceKind::PublicWalk,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: crate::model::Provider::StreamBim,
            meta: Some(meta.into()),
        }
    }

    #[test]
    fn a_project_belongs_to_the_account_of_its_server() {
        // The meta of a project made by the single-account version.
        let legacy = project_with_meta(
            r#"{"base":"https://app.streambim.com","project":"13588","region":"app"}"#,
        );
        assert_eq!(project_key(&legacy).as_deref(), Some("app"));
        assert_eq!(sign_in_label(&legacy), "StreamBIM (app.streambim.com)");
        let custom = project_with_meta(
            r#"{"region":"streambim.example.com","base":"https://streambim.example.com","project":"7"}"#,
        );
        assert_eq!(
            project_key(&custom).as_deref(),
            Some("streambim.example.com")
        );
        // A test origin has no key, so the region of the meta names the account.
        let loopback =
            project_with_meta(r#"{"region":"app","base":"http://127.0.0.1:9","project":"1"}"#);
        assert_eq!(project_key(&loopback).as_deref(), Some("app"));
        assert_eq!(project_key(&project_with_meta("{}")), None);
        assert_eq!(sign_in_label(&project_with_meta("{}")), "StreamBIM");
    }

    /// Writes a signed-in account (its token and profile) the way a sign-in leaves them.
    fn write_account(dir: &Path, key: &str, base: &str) {
        let tokens = json!({
            "refresh_token": format!("rt-{key}"),
            "id_token": format!("id-{key}"),
            "expires_at": 4_102_444_800i64,
        });
        std::fs::write(
            dir.join(token_file(key)),
            dpapi_protect(tokens.to_string().as_bytes()).unwrap(),
        )
        .unwrap();
        let profile = json!({
            "user_id": format!("{key}@example.com"),
            "name": "Ada",
            "email": format!("{key}@example.com"),
            "region": key,
            "base": base,
        });
        std::fs::write(dir.join(profile_file(key)), profile.to_string()).unwrap();
    }

    #[test]
    fn a_single_account_install_is_migrated_to_its_key() {
        let dir = TestDir::new("migrate");
        // The exact profile shape of the single-account version.
        std::fs::write(
            dir.path().join(LEGACY_PROFILE_FILE),
            r#"{"user_id":"ada@example.com","name":"Ada","email":"ada@example.com","region":"app","base":"https://app.streambim.com"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(LEGACY_TOKEN_FILE),
            dpapi_protect(
                br#"{"refresh_token":"rt-legacy","id_token":"id-legacy","expires_at":4102444800}"#,
            )
            .unwrap(),
        )
        .unwrap();

        let accounts = StreamBimAccounts::load(http(), dir.path());
        assert!(!dir.path().join(LEGACY_PROFILE_FILE).exists());
        assert!(!dir.path().join(LEGACY_TOKEN_FILE).exists());
        assert!(dir.path().join("user-streambim-app.json").exists());
        assert!(dir.path().join("token-streambim-app.bin").exists());
        let app = accounts.get("app").expect("the account is loaded");
        assert!(app.auth.is_signed_in());
        assert_eq!(app.auth.base_url(), "https://app.streambim.com");
        assert_eq!(app.auth.user().unwrap().email, "ada@example.com");
    }

    #[test]
    fn a_keyed_file_wins_over_the_legacy_copy() {
        let dir = TestDir::new("migrate-keep");
        write_account(dir.path(), "app", "https://app.streambim.com");
        std::fs::write(
            dir.path().join(LEGACY_PROFILE_FILE),
            r#"{"user_id":"old@example.com","name":"Old","email":"old@example.com","region":"app","base":"https://app.streambim.com"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join(LEGACY_TOKEN_FILE), b"legacy").unwrap();

        let accounts = StreamBimAccounts::load(http(), dir.path());
        assert!(!dir.path().join(LEGACY_PROFILE_FILE).exists());
        assert!(!dir.path().join(LEGACY_TOKEN_FILE).exists());
        let app = accounts.get("app").unwrap();
        assert_eq!(app.auth.user().unwrap().user_id, "app@example.com");
        assert!(app.auth.is_signed_in());
    }

    #[test]
    fn a_legacy_profile_without_a_server_is_dropped() {
        let dir = TestDir::new("migrate-none");
        std::fs::write(dir.path().join(LEGACY_PROFILE_FILE), r#"{"user_id":"x"}"#).unwrap();
        std::fs::write(dir.path().join(LEGACY_TOKEN_FILE), b"legacy").unwrap();
        let accounts = StreamBimAccounts::load(http(), dir.path());
        assert!(accounts.all().is_empty());
        assert!(!dir.path().join(LEGACY_PROFILE_FILE).exists());
        assert!(!dir.path().join(LEGACY_TOKEN_FILE).exists());
    }

    #[tokio::test]
    async fn two_accounts_are_independent_and_signing_one_out_keeps_the_other() {
        let dir = TestDir::new("two");
        write_account(dir.path(), "app", "https://app.streambim.com");
        write_account(dir.path(), "japan", "https://japan.streambim.com");
        write_account(
            dir.path(),
            "streambim.example.com",
            "https://streambim.example.com",
        );
        let accounts = StreamBimAccounts::load(http(), dir.path());
        let keys: Vec<String> = accounts.all().into_iter().map(|a| a.key).collect();
        assert_eq!(
            keys,
            ["app", "japan", "streambim.example.com"],
            "sorted by key"
        );
        assert_eq!(accounts.signed_in().len(), 3);

        let japan = accounts.get("japan").unwrap();
        japan.auth.sign_out().await.unwrap();
        accounts.forget_if_signed_out("japan");
        assert!(!dir.path().join("token-streambim-japan.bin").exists());
        assert!(!dir.path().join("user-streambim-japan.json").exists());
        assert!(dir.path().join("token-streambim-app.bin").exists());
        assert!(dir.path().join("user-streambim-app.json").exists());
        assert!(accounts.get("japan").is_none());
        assert!(accounts.get("app").unwrap().auth.is_signed_in());
        assert_eq!(accounts.signed_in().len(), 2);
        assert!(accounts.any_signed_in());
    }

    #[test]
    fn an_account_is_added_once_and_keeps_its_first_server() {
        let dir = TestDir::new("add");
        let accounts = StreamBimAccounts::load(http(), dir.path());
        assert!(accounts.all().is_empty());
        let japan = accounts.get_or_add("japan", "https://japan.streambim.com");
        assert!(!japan.auth.is_signed_in(), "a new account is signed out");
        assert_eq!(
            accounts
                .get_or_add("japan", "https://elsewhere.invalid")
                .auth
                .base_url(),
            "https://japan.streambim.com"
        );
        assert_eq!(accounts.all().len(), 1);
        accounts.forget_if_signed_out("japan");
        assert!(accounts.get("japan").is_none());
    }
}

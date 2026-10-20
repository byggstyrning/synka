//! APS 3-legged OAuth: Authorization Code + PKCE (public client, no secret) over a loopback
//! redirect. Refresh token persisted DPAPI-protected (CryptProtectData, current user) in
//! `<state_dir>\token.bin`. Access token kept in memory only.
//!
//! Endpoints:
//!   authorize: https://developer.api.autodesk.com/authentication/v2/authorize
//!     ?response_type=code&client_id=..&redirect_uri=..&scope=..&code_challenge=..
//!      &code_challenge_method=S256&state=..&prompt=login (prompt only on explicit re-login)
//!   token:     POST https://developer.api.autodesk.com/authentication/v2/token
//!     form: grant_type=authorization_code&code=..&redirect_uri=..&client_id=..&code_verifier=..
//!     form: grant_type=refresh_token&refresh_token=..&client_id=..&scope=..
//!     (refresh tokens rotate: always store the new one)
//!   user:      GET https://api.userprofile.autodesk.com/userinfo  (OIDC: sub, name, email)
//!
//! Loopback: bind 127.0.0.1:REDIRECT_PORT with tokio TcpListener, open the browser with
//! ShellExecuteW, accept requests until one to "/?code=..&state=.." (ignore /favicon.ico etc.),
//! check `state`, reply with a tiny HTML page
//! ("Inloggad – du kan stänga fliken."), time out after 5 minutes.
//!
//! Never log tokens. Concurrent `access_token()` calls must share one refresh (tokio Mutex).

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Registered APS callback: http://localhost:47863/ (host, port and slash must match).
/// Do not fall back to another port: Autodesk rejects an unregistered redirect before login.
pub const REDIRECT_PORT: u16 = 47863;

fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/")
}
// Only what the endpoints in docs/specs/aps-endpoints.md need: data:read (project/v1, data/v1
// reads, OSS download), data:write + data:create (folders, storage, items, versions, PATCH),
// user-profile:read (userinfo). No data:search, no account:read.
pub const SCOPES: &str = "data:read data:write data:create user-profile:read";

const AUTHORIZE_URL: &str = "https://developer.api.autodesk.com/authentication/v2/authorize";
const TOKEN_URL: &str = "https://developer.api.autodesk.com/authentication/v2/token";
const USERINFO_URL: &str = "https://api.userprofile.autodesk.com/userinfo";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OAUTH_ERROR_BYTES: usize = 16 * 1024;
const PUBLIC_APP_GUIDANCE: &str = "Välj apptypen Desktop, Mobile, Single-Page App i Autodesk APS och registrera http://localhost:47863/. Traditional Web App kräver en client secret och stöds inte. (Use Desktop, Mobile, Single-Page App with PKCE; Traditional Web App is not supported.)";
/// Refresh the access token when less than this is left.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
const PORT_BUSY_MSG: &str = "Kan inte öppna inloggningsport 47863 – porten används redan eller är blockerad. Stäng programmet som använder porten och försök igen. (Sign-in port 47863 is unavailable)";

/// Default loopback redirect of the Interaxo client (what Interaxo support registers).
pub const INTERAXO_DEFAULT_REDIRECT: &str = "http://localhost:47864/";

/// Interaxo OIDC client ("bring your own client id").
#[derive(Clone, PartialEq, Eq, Default)]
pub struct InteraxoClient {
    pub client_id: String,
    /// Only for a confidential client. Never printed (see the `Debug` impl).
    pub client_secret: Option<String>,
    pub redirect_uri: String,
}

impl std::fmt::Debug for InteraxoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteraxoClient")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

/// Ok when `uri` is a loopback redirect Synka can listen on: `http://localhost:<port>/<path>`
/// or `http://127.0.0.1:<port>/<path>` with an explicit port from 1 to 65535, and no user
/// info, query or fragment. The Err text is shown to the user (Swedish).
pub fn check_loopback_redirect(uri: &str) -> Result<(), String> {
    const HOST: &str = "Omdirigerings-URI:n måste börja med http://localhost eller http://127.0.0.1 (redirect URI must be http://localhost or http://127.0.0.1)";
    const PORT: &str = "Omdirigerings-URI:n måste ange en port från 1 till 65535, till exempel http://localhost:47864/ (redirect URI needs an explicit port)";
    if uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(HOST.into());
    }
    let url = url::Url::parse(uri).map_err(|_| HOST.to_string())?;
    if url.scheme() != "http" || !matches!(url.host_str(), Some("localhost" | "127.0.0.1")) {
        return Err(HOST.into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Omdirigerings-URI:n får inte innehålla användarnamn eller lösenord (redirect URI must not contain user info)".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("Omdirigerings-URI:n får inte innehålla frågesträng eller fragment (redirect URI must not contain a query or fragment)".into());
    }
    // `url` drops a port that equals the scheme default (":80"), so read the port from the text.
    let authority = uri
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .unwrap_or_default();
    let port = authority
        .rsplit_once(':')
        .map(|(_, port)| port)
        .filter(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    match port.map(str::parse::<u16>) {
        Some(Ok(port)) if port >= 1 => Ok(()),
        _ => Err(PORT.into()),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct UserInfo {
    /// OIDC `sub` (Autodesk user id / oxygen id).
    pub user_id: String,
    pub name: String,
    pub email: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

struct VerifiedSignIn {
    tokens: TokenResponse,
    user: UserInfo,
}

fn default_expires_in() -> u64 {
    3600
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UserinfoResponse {
    sub: String,
    name: String,
    email: String,
}

/// Token endpoint failure. `Rejected` = HTTP 400/401 (invalid_grant): the refresh token is dead.
enum TokenErr {
    Rejected(String),
    Other(anyhow::Error),
}

impl TokenErr {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            TokenErr::Rejected(m) => anyhow!(m),
            TokenErr::Other(e) => e,
        }
    }
}

#[derive(Default)]
struct TokenState {
    access: Option<String>,
    expires_at: Option<Instant>,
    refresh: Option<String>,
}

pub struct Auth {
    client_id: String,
    http: reqwest::Client,
    state_dir: PathBuf,
    /// File names in `state_dir` (ACC: token.bin / user.json; other OAuth providers use their own).
    token_file: &'static str,
    user_file: &'static str,
    /// The Interaxo client, for the Interaxo account only (None for ACC). Its client id
    /// overrides `client_id`.
    interaxo: RwLock<Option<InteraxoClient>>,
    token: Mutex<TokenState>,
    user: RwLock<Option<UserInfo>>,
    #[cfg(test)]
    test_access_token: Option<String>,
    #[cfg(test)]
    test_endpoints: Option<(String, String)>,
}

impl Auth {
    /// `client_id` empty => `is_configured()` false; sign_in returns an error explaining the
    /// APS app must be configured.
    pub fn new(client_id: String, http: reqwest::Client, state_dir: &Path) -> Auth {
        Auth::with_files(client_id, http, state_dir, "token.bin", "user.json")
    }

    /// Like `new`, with the token and profile files named by the caller (one pair per OAuth
    /// account). The endpoints and scopes are the APS ones; the Interaxo spec is wired later.
    pub fn with_files(
        client_id: String,
        http: reqwest::Client,
        state_dir: &Path,
        token_file: &'static str,
        user_file: &'static str,
    ) -> Auth {
        let user = std::fs::read(state_dir.join(user_file))
            .ok()
            .and_then(|b| serde_json::from_slice::<UserInfo>(&b).ok());
        Auth {
            client_id,
            http,
            state_dir: state_dir.to_path_buf(),
            token_file,
            user_file,
            interaxo: RwLock::new(None),
            token: Mutex::new(TokenState::default()),
            user: RwLock::new(user),
            #[cfg(test)]
            test_access_token: None,
            #[cfg(test)]
            test_endpoints: None,
        }
    }

    /// In-memory credentials for loopback HTTP tests. Never reads or writes account state.
    #[cfg(test)]
    pub(crate) fn for_test(http: reqwest::Client) -> Self {
        Self {
            client_id: String::new(),
            http,
            state_dir: PathBuf::new(),
            token_file: "token.bin",
            user_file: "user.json",
            interaxo: RwLock::new(None),
            token: Mutex::new(TokenState::default()),
            user: RwLock::new(None),
            test_access_token: Some("synka-loopback-test-token".to_string()),
            test_endpoints: None,
        }
    }

    /// The Interaxo account: an `Auth` for Interaxo's OIDC client, with its own token and
    /// profile files. An empty client id means not configured yet.
    pub fn interaxo(client: InteraxoClient, http: reqwest::Client, state_dir: &Path) -> Auth {
        let base = Auth::with_files(
            client.client_id.clone(),
            http,
            state_dir,
            "token-interaxo.bin",
            "user-interaxo.json",
        );
        Auth {
            interaxo: RwLock::new(Some(client)),
            ..base
        }
    }

    /// Replace the stored Interaxo client (the caller makes sure the account is signed out).
    #[allow(dead_code)] // the settings step calls it; tests use it now
    pub fn set_interaxo_client(&self, client: InteraxoClient) {
        *self.interaxo.write().unwrap_or_else(|e| e.into_inner()) = Some(client);
    }

    /// The stored Interaxo client; None for the ACC account.
    #[allow(dead_code)] // the settings step reads it; tests use it now
    pub fn interaxo_client(&self) -> Option<InteraxoClient> {
        self.interaxo
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The client id in use: the stored Interaxo client's, else the constructor's.
    fn current_client_id(&self) -> String {
        let interaxo = self.interaxo.read().unwrap_or_else(|e| e.into_inner());
        match interaxo.as_ref() {
            Some(client) => client.client_id.clone(),
            None => self.client_id.clone(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.current_client_id().trim().is_empty()
    }

    /// A complete local session needs credentials and a validated user profile.
    /// The stored refresh token can still turn out to be expired or revoked.
    /// A token left by an interrupted older sign-in must still allow a new browser attempt.
    pub fn is_signed_in(&self) -> bool {
        let has_user = self
            .user
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|user| !user.user_id.trim().is_empty());
        if !has_user {
            return false;
        }
        self.token_path().exists()
            || self
                .token
                .try_lock()
                .map(|st| st.refresh.is_some())
                .unwrap_or(false)
    }

    /// Interactive browser login. Persists the refresh token and caches the user.
    pub async fn sign_in(&self) -> Result<UserInfo> {
        self.browser_sign_in(false).await
    }

    /// Validate a candidate public APS client through a fresh PKCE exchange. Candidate
    /// credentials and profile are discarded: no account or configuration files are read
    /// or written. The caller commits the setting only after this succeeds.
    pub async fn validate_client_id(client_id: String, http: reqwest::Client) -> Result<()> {
        let candidate = Self {
            client_id,
            http,
            state_dir: PathBuf::new(),
            token_file: "token.bin",
            user_file: "user.json",
            interaxo: RwLock::new(None),
            token: Mutex::new(TokenState::default()),
            user: RwLock::new(None),
            #[cfg(test)]
            test_access_token: None,
            #[cfg(test)]
            test_endpoints: None,
        };
        candidate.browser_sign_in(true).await?;
        Ok(())
    }

    async fn browser_sign_in(&self, validation_only: bool) -> Result<UserInfo> {
        if !self.is_configured() {
            bail!(
                "APS-appen är inte konfigurerad: client id saknas (APS app is not configured: \
                 missing client id). Sätt aps_client_id i settings.json."
            );
        }
        // Signing in while already signed in is an explicit re-login: ask Autodesk to show
        // the login form again instead of silently reusing the browser session.
        let relogin = !validation_only && self.is_signed_in();

        let verifier = random_b64(32)?;
        let challenge = pkce_challenge(&verifier);
        let state = random_b64(16)?;

        // Bind BEFORE opening the browser so the redirect cannot arrive at nothing.
        let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
            .await
            .context(PORT_BUSY_MSG)?;
        let redirect = redirect_uri(REDIRECT_PORT);

        let client_id = self.current_client_id();
        let mut authorize = url::Url::parse(AUTHORIZE_URL).context("authorize-URL")?;
        {
            let mut q = authorize.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect)
                .append_pair("scope", SCOPES)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state);
            if relogin {
                q.append_pair("prompt", "login");
            }
        }
        open_browser(authorize.as_str())?;
        info!(redirect_uri = %redirect, timeout_secs = CALLBACK_TIMEOUT.as_secs(),
            "inloggning startad – väntar på webbläsaren");

        let (code, mut sock) = match tokio::time::timeout(
            CALLBACK_TIMEOUT,
            accept_callback(&listener, &state),
        )
        .await
        {
            Ok(r) => r?,
            Err(_) => bail!(
                "Inloggningen tog för lång tid (5 minuter) – försök igen. (Sign-in timed out)"
            ),
        };

        let result = self
            .complete_browser_sign_in(&code, &verifier, &redirect, validation_only)
            .await;
        match result {
            Ok(user) => {
                respond(
                    &mut sock,
                    "200 OK",
                    &page(if validation_only {
                        "APS-appen har verifierats. Återgå till Synka. (APS app verified. Return to Synka.)"
                    } else {
                        "Inloggad – du kan stänga fliken."
                    }),
                )
                .await;
                Ok(user)
            }
            Err(e) => {
                respond(
                    &mut sock,
                    "500 Internal Server Error",
                    &page("Inloggningen misslyckades – se Synka för detaljer. (Sign-in failed)"),
                )
                .await;
                Err(e)
            }
        }
    }

    /// Forget tokens (delete token.bin) and the cached user.
    pub async fn sign_out(&self) -> Result<()> {
        {
            let mut st = self.token.lock().await;
            st.access = None;
            st.expires_at = None;
            st.refresh = None;
        }
        remove_if_exists(&self.token_path())?;
        remove_if_exists(&self.user_path())?;
        *self.user.write().unwrap_or_else(|e| e.into_inner()) = None;
        info!("utloggad");
        Ok(())
    }

    /// Valid access token, refreshing when < 5 min left. Err if not signed in or the refresh
    /// token was rejected (then the stored token is deleted so the UI shows "Logga in").
    pub async fn access_token(&self) -> Result<String> {
        #[cfg(test)]
        if let Some(token) = &self.test_access_token {
            return Ok(token.clone());
        }
        let mut st = self.token.lock().await;
        if let (Some(access), Some(exp)) = (&st.access, st.expires_at) {
            if exp.saturating_duration_since(Instant::now()) > REFRESH_MARGIN {
                return Ok(access.clone());
            }
        }
        if st.refresh.is_none() {
            st.refresh = self.load_refresh();
        }
        let rt = match st.refresh.clone() {
            Some(rt) => rt,
            None => bail!("Inte inloggad – logga in igen. (Not signed in)"),
        };

        let client_id = self.current_client_id();
        let result = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", rt.as_str()),
                ("client_id", client_id.as_str()),
                ("scope", SCOPES),
            ])
            .await;
        match result {
            Ok(tr) => {
                if let Some(new_rt) = tr.refresh_token {
                    // Refresh tokens rotate: the old one is dead, keep the new one even if
                    // persisting fails (then it lives in memory until restart).
                    if let Err(e) =
                        write_atomic(&self.token_path(), &dpapi_protect(new_rt.as_bytes())?)
                    {
                        warn!("kunde inte spara token.bin: {e:#}");
                    }
                    st.refresh = Some(new_rt);
                }
                st.expires_at = Some(Instant::now() + Duration::from_secs(tr.expires_in));
                st.access = Some(tr.access_token.clone());
                Ok(tr.access_token)
            }
            Err(TokenErr::Rejected(msg)) => {
                warn!("refresh avvisad: {msg}");
                st.access = None;
                st.expires_at = None;
                st.refresh = None;
                let _ = remove_if_exists(&self.token_path());
                bail!("Sessionen har gått ut – logga in igen. (Session expired)")
            }
            Err(TokenErr::Other(e)) => Err(e.context("token refresh")),
        }
    }

    /// Force a refresh on the next `access_token()` (used after a 401).
    pub async fn invalidate(&self) {
        self.token.lock().await.access = None;
    }

    /// Cached user info (persisted in `<state_dir>\user.json`), fetched after sign-in.
    pub fn user(&self) -> Option<UserInfo> {
        self.user.read().ok().and_then(|g| g.clone())
    }

    // ---- internals -------------------------------------------------------------------------

    fn token_path(&self) -> PathBuf {
        self.state_dir.join(self.token_file)
    }

    fn user_path(&self) -> PathBuf {
        self.state_dir.join(self.user_file)
    }

    /// Read and decrypt the stored refresh token. An unreadable blob (e.g. another Windows
    /// user's file) is deleted so the UI falls back to "Logga in".
    fn load_refresh(&self) -> Option<String> {
        let blob = match std::fs::read(self.token_path()) {
            Ok(b) => b,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("kunde inte läsa token.bin: {e}");
                }
                return None;
            }
        };
        match dpapi_unprotect(&blob).and_then(|p| String::from_utf8(p).map_err(anyhow::Error::from))
        {
            Ok(s) if !s.is_empty() => Some(s),
            Ok(_) | Err(_) => {
                warn!("token.bin kan inte dekrypteras – tar bort den");
                let _ = remove_if_exists(&self.token_path());
                None
            }
        }
    }

    async fn finish_sign_in(&self, code: &str, verifier: &str, redirect: &str) -> Result<UserInfo> {
        self.persist_sign_in(self.exchange_sign_in(code, verifier, redirect).await?)
            .await
    }

    async fn complete_browser_sign_in(
        &self,
        code: &str,
        verifier: &str,
        redirect: &str,
        validation_only: bool,
    ) -> Result<UserInfo> {
        if validation_only {
            Ok(self.exchange_sign_in(code, verifier, redirect).await?.user)
        } else {
            self.finish_sign_in(code, verifier, redirect).await
        }
    }

    async fn exchange_sign_in(
        &self,
        code: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<VerifiedSignIn> {
        let client_id = self.current_client_id();
        let tr = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("client_id", client_id.as_str()),
                ("code_verifier", verifier),
            ])
            .await
            .map_err(|e| e.into_anyhow().context(PUBLIC_APP_GUIDANCE))?;

        if tr.access_token.trim().is_empty() {
            bail!("Autodesk returnerade ingen access token (no access token returned)");
        }
        tr.refresh_token
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("Autodesk returnerade ingen refresh token (no refresh token returned)")
            })?;
        // A failed profile request must not leave a token-only session that prevents retry.
        let user = self.fetch_user(&tr.access_token).await?;
        Ok(VerifiedSignIn { tokens: tr, user })
    }

    async fn persist_sign_in(&self, verified: VerifiedSignIn) -> Result<UserInfo> {
        let VerifiedSignIn { tokens: tr, user } = verified;
        let refresh = tr
            .refresh_token
            .as_deref()
            .context("verified refresh token missing")?;
        // Acquire the last async lock before committing credentials. Cancellation followed
        // by awaiting this task then guarantees sign-out can remove every committed file.
        let mut st = self.token.lock().await;
        write_atomic(&self.token_path(), &dpapi_protect(refresh.as_bytes())?)
            .context("kunde inte spara token.bin")?;
        st.access = Some(tr.access_token.clone());
        st.expires_at = Some(Instant::now() + Duration::from_secs(tr.expires_in));
        st.refresh = Some(refresh.to_owned());
        match serde_json::to_vec_pretty(&user) {
            Ok(bytes) => {
                if let Err(e) = write_atomic(&self.user_path(), &bytes) {
                    warn!("kunde inte spara user.json: {e:#}");
                }
            }
            Err(e) => warn!("kunde inte serialisera användaruppgifter: {e}"),
        }
        *self.user.write().unwrap_or_else(|e| e.into_inner()) = Some(user.clone());
        info!("inloggad");
        Ok(user)
    }

    async fn fetch_user(&self, access: &str) -> Result<UserInfo> {
        let resp = self
            .http
            .get(self.userinfo_endpoint())
            .bearer_auth(access)
            .timeout(TOKEN_TIMEOUT)
            .send()
            .await
            .context("userinfo-anrop")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("userinfo misslyckades: HTTP {status}");
        }
        let u: UserinfoResponse = resp.json().await.context("userinfo-svar")?;
        if u.sub.trim().is_empty() {
            bail!(
                "Autodesk returnerade ingen användaridentitet (userinfo response has no user id)"
            );
        }
        Ok(UserInfo {
            user_id: u.sub,
            name: u.name,
            email: u.email,
        })
    }

    /// POST a form to the token endpoint. Public client: client_id goes in the body, no
    /// Authorization header.
    async fn token_request(
        &self,
        pairs: &[(&str, &str)],
    ) -> std::result::Result<TokenResponse, TokenErr> {
        let body = {
            let mut s = url::form_urlencoded::Serializer::new(String::new());
            s.extend_pairs(pairs);
            s.finish()
        };
        let resp = self
            .http
            .post(self.token_endpoint())
            .timeout(TOKEN_TIMEOUT)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|e| TokenErr::Other(e.into()))?;
        let status = resp.status();
        if !status.is_success() {
            // Only the OAuth error code is read; never the body, which may echo credentials.
            let bytes = bounded_oauth_error(resp).await;
            let code = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str().map(str::to_owned)))
                .unwrap_or_default();
            let code = safe_oauth_error(&code);
            let msg =
                format!("token-anrop misslyckades: HTTP {status} {code} (token request failed)");
            return Err(if matches!(status.as_u16(), 400 | 401) {
                TokenErr::Rejected(msg)
            } else {
                TokenErr::Other(anyhow!(msg))
            });
        }
        resp.json::<TokenResponse>()
            .await
            .map_err(|e| TokenErr::Other(anyhow!("token-svar kunde inte läsas: {e}")))
    }

    fn token_endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some((token, _)) = &self.test_endpoints {
            return token;
        }
        TOKEN_URL
    }

    fn userinfo_endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some((_, userinfo)) = &self.test_endpoints {
            return userinfo;
        }
        USERINFO_URL
    }
}

/// Error bodies can be arbitrarily large and can echo secrets. Retain only a bounded
/// JSON document long enough to select an allowlisted OAuth error code.
async fn bounded_oauth_error(mut response: reqwest::Response) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len().saturating_add(chunk.len()) > MAX_OAUTH_ERROR_BYTES {
            return Vec::new();
        }
        bytes.extend_from_slice(&chunk);
    }
    bytes
}

/// Remote error values are untrusted and may echo credentials; log only known OAuth codes.
fn safe_oauth_error(error: &str) -> &str {
    match error {
        "access_denied"
        | "invalid_request"
        | "invalid_client"
        | "invalid_grant"
        | "unauthorized_client"
        | "invalid_scope"
        | "unsupported_response_type"
        | "unsupported_grant_type"
        | "server_error"
        | "temporarily_unavailable"
        | "login_required"
        | "interaction_required"
        | "consent_required" => error,
        _ => "oauth_error",
    }
}

// ---- loopback callback ----------------------------------------------------------------------

/// Accept connections until a valid `/?code=..&state=..` arrives. Returns the code and the
/// open socket so the caller can answer once the token exchange has finished.
async fn accept_callback(
    listener: &TcpListener,
    expected_state: &str,
) -> Result<(String, TcpStream)> {
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                debug!("accept misslyckades: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Some(target) = read_request_target(&mut sock).await else {
            respond(&mut sock, "400 Bad Request", "").await;
            continue;
        };
        let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else {
            respond(&mut sock, "400 Bad Request", "").await;
            continue;
        };
        if url.path() != "/" {
            respond(&mut sock, "404 Not Found", "").await;
            continue;
        }
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        if !params.contains_key("code") && !params.contains_key("error") {
            respond(&mut sock, "404 Not Found", "").await;
            continue;
        }
        if params.get("state").map(String::as_str) != Some(expected_state) {
            // An error from a replaced browser attempt cannot cancel the current attempt.
            warn!("callback med ogiltig state ignorerades");
            respond(&mut sock, "400 Bad Request", "").await;
            continue;
        }
        if let Some(err) = params.get("error") {
            respond(
                &mut sock,
                "200 OK",
                &page("Inloggningen avbröts. (Sign-in cancelled)"),
            )
            .await;
            bail!(
                "Inloggningen avbröts: {} (sign-in cancelled)",
                safe_oauth_error(err)
            );
        }
        let Some(code) = params.get("code") else {
            respond(&mut sock, "404 Not Found", "").await;
            continue;
        };
        return Ok((code.clone(), sock));
    }
}

/// Read the request head (up to 8 KB, 5 s) and return the request target of a GET.
async fn read_request_target(sock: &mut TcpStream) -> Option<String> {
    let mut buf = vec![0u8; 8192];
    let mut n = 0usize;
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while n < buf.len() {
            match sock.read(&mut buf[n..]).await {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
    })
    .await;
    let head = String::from_utf8_lossy(&buf[..n]);
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    parts.next().map(str::to_string)
}

async fn respond(sock: &mut TcpStream, status: &str, html: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        html.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(html.as_bytes()).await;
    let _ = sock.shutdown().await;
}

fn page(message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"sv\"><meta charset=\"utf-8\"><title>Synka</title>\
         <body style=\"font-family:sans-serif;padding:2em\"><p>{message}</p></body></html>"
    )
}

fn open_browser(url: &str) -> Result<()> {
    let op = wide("open");
    let file = wide(url);
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = r.0 as isize;
    if code <= 32 {
        bail!("Kunde inte öppna webbläsaren (could not open the browser, code {code})");
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- PKCE / random --------------------------------------------------------------------------

fn random_b64(n: usize) -> Result<String> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).map_err(|e| anyhow!("slumpgenerator misslyckades: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(b))
}

/// S256 code challenge: base64url(sha256(verifier)) without padding.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

// ---- files and DPAPI ------------------------------------------------------------------------

/// Write via `<path>.tmp` then rename over the target (atomic on the same volume).
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

/// Protect `data` for the current Windows user (DPAPI).
pub(crate) fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|e| anyhow!("DPAPI-kryptering misslyckades: {e}"))?;
    Ok(take_blob(output))
}

/// Reverse of `dpapi_protect`; fails for a blob another Windows user protected.
pub(crate) fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|e| anyhow!("DPAPI-upplåsning misslyckades: {e}"))?;
    Ok(take_blob(output))
}

/// Copy a DPAPI-allocated blob into a Vec and free the original with LocalFree.
fn take_blob(b: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if b.pbData.is_null() {
        return Vec::new();
    }
    let v = unsafe { std::slice::from_raw_parts(b.pbData, b.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(b.pbData as *mut core::ffi::c_void)));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestStateDir {
        root: PathBuf,
        path: PathBuf,
    }

    impl TestStateDir {
        fn new() -> Self {
            let root = std::env::temp_dir().canonicalize().unwrap();
            let path = root.join(format!("synka-auth-test-{}", random_b64(16).unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self { root, path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestStateDir {
        fn drop(&mut self) {
            // Delete only the exact private directory created directly under the temp root.
            if self.path.parent() == Some(self.root.as_path())
                && self
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("synka-auth-test-"))
            {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    #[test]
    fn pkce_rfc7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_lengths() {
        // 32 bytes -> 43 chars unpadded base64url; 16 bytes -> 22 chars.
        assert_eq!(random_b64(32).unwrap().len(), 43);
        assert_eq!(random_b64(16).unwrap().len(), 22);
    }

    #[test]
    fn callback_matches_the_registered_aps_address() {
        assert_eq!(redirect_uri(REDIRECT_PORT), "http://localhost:47863/");
    }

    async fn callback_request(address: std::net::SocketAddr, target: &str) -> String {
        let mut sock = TcpStream::connect(address).await.unwrap();
        sock.write_all(
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), sock.read_to_end(&mut response))
            .await
            .expect("callback response deadline")
            .unwrap();
        String::from_utf8(response).unwrap()
    }

    #[tokio::test]
    async fn resilience_callback_ignores_stale_errors_before_accepting_current_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (code, mut sock) = accept_callback(&listener, "current-state").await?;
            respond(&mut sock, "200 OK", "done").await;
            Ok::<_, anyhow::Error>(code)
        });
        for target in [
            "/?error=access_denied&state=old-state",
            "/?error=access_denied",
            "/?code=old-code&state=old-state",
        ] {
            assert!(callback_request(address, target)
                .await
                .starts_with("HTTP/1.1 400"));
            assert!(
                !task.is_finished(),
                "stale callback ended the current attempt"
            );
        }
        assert!(
            callback_request(address, "/?code=current-code&state=current-state")
                .await
                .starts_with("HTTP/1.1 200")
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "current-code"
        );
    }

    #[tokio::test]
    async fn resilience_callback_accepts_only_current_errors_and_redacts_unknown_values() {
        for error in ["access_denied", "synthetic-sensitive-error-value"] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                accept_callback(&listener, "current-state")
                    .await
                    .map(|_| ())
            });
            let response =
                callback_request(address, &format!("/?error={error}&state=current-state")).await;
            assert!(response.starts_with("HTTP/1.1 200"));
            assert!(!response.contains(error));
            let message = task.await.unwrap().unwrap_err().to_string();
            if error == "access_denied" {
                assert!(message.contains("access_denied"));
            } else {
                assert!(!message.contains(error));
                assert!(message.contains("oauth_error"));
            }
        }
    }

    fn json_reply(value: serde_json::Value) -> crate::test_support::Fault {
        crate::test_support::Fault::reply(200, serde_json::to_vec(&value).unwrap())
            .header("Content-Type", "application/json")
    }

    fn token_reply(refresh: &str) -> crate::test_support::Fault {
        json_reply(serde_json::json!({
            "access_token": "loopback-access-only",
            "refresh_token": refresh,
            "expires_in": 3600
        }))
    }

    fn loopback_auth(server: &crate::test_support::Server, directory: &Path) -> Auth {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut auth = Auth::new("loopback-client".into(), http, directory);
        auth.test_endpoints = Some((
            format!("{}/token", server.origin),
            format!("{}/userinfo", server.origin),
        ));
        auth
    }

    #[tokio::test]
    async fn resilience_profile_failure_does_not_persist_partial_login_and_retry_succeeds() {
        use crate::test_support::{Fault, Server};
        let server = Server::scripted(vec![
            token_reply("first-refresh-must-not-persist"),
            Fault::reply(503, "temporary profile failure"),
            token_reply("second-refresh-for-test"),
            json_reply(serde_json::json!({"sub":"test-user", "name":"Test", "email":"test@example.invalid"})),
        ])
        .await;
        let directory = TestStateDir::new();
        let auth = loopback_auth(&server, directory.path());
        let result = auth
            .finish_sign_in("first-code", "test-verifier", "http://localhost:47863/")
            .await;
        assert!(result.unwrap_err().to_string().contains("503"));
        assert!(!auth.is_signed_in());
        assert!(auth.user().is_none());
        assert!(!auth.token_path().exists());
        assert!(!auth.user_path().exists());
        assert!(auth.token.lock().await.refresh.is_none());

        let user = auth
            .finish_sign_in("second-code", "test-verifier", "http://localhost:47863/")
            .await
            .unwrap();
        assert_eq!(user.user_id, "test-user");
        assert!(auth.is_signed_in());
        assert_eq!(
            dpapi_unprotect(&std::fs::read(auth.token_path()).unwrap()).unwrap(),
            b"second-refresh-for-test"
        );
        let requests = server.requests();
        assert_eq!(
            requests
                .iter()
                .map(|r| r.target.as_str())
                .collect::<Vec<_>>(),
            ["/token", "/userinfo", "/token", "/userinfo"]
        );
        for request in requests.iter().filter(|r| r.target == "/token") {
            assert!(request.header("authorization").is_none());
            assert!(!String::from_utf8_lossy(&request.body).contains("client_secret"));
        }
        auth.sign_out().await.unwrap();
        assert!(!auth.is_signed_in());
        assert!(!auth.token_path().exists());
    }

    #[tokio::test]
    async fn resilience_profile_without_user_id_cannot_commit_a_session() {
        use crate::test_support::Server;
        let server = Server::scripted(vec![
            token_reply("test-refresh"),
            json_reply(serde_json::json!({})),
        ])
        .await;
        let directory = TestStateDir::new();
        let auth = loopback_auth(&server, directory.path());
        assert!(auth
            .finish_sign_in("test-code", "test-verifier", "http://localhost:47863/")
            .await
            .is_err());
        assert!(!auth.is_signed_in());
        assert!(!auth.token_path().exists());
        assert!(auth.user().is_none());
    }

    #[test]
    fn legacy_token_without_profile_still_allows_sign_in() {
        let directory = TestStateDir::new();
        std::fs::write(directory.path().join("token.bin"), b"legacy-partial-token").unwrap();
        let auth = Auth::new(
            "test-client".into(),
            reqwest::Client::new(),
            directory.path(),
        );
        assert!(!auth.is_signed_in());
    }

    #[tokio::test]
    async fn resilience_candidate_validation_uses_public_pkce_without_persisting_credentials() {
        use crate::test_support::Server;
        let server = Server::scripted(vec![
            token_reply("candidate-refresh-never-persisted"),
            json_reply(serde_json::json!({"sub":"candidate-user"})),
        ])
        .await;
        let directory = TestStateDir::new();
        // Existing account/configuration files must survive validation byte-for-byte.
        std::fs::write(directory.path().join("token.bin"), b"existing-token").unwrap();
        std::fs::write(directory.path().join("user.json"), b"existing-profile").unwrap();
        std::fs::write(directory.path().join("settings.json"), b"existing-config").unwrap();
        let auth = loopback_auth(&server, directory.path());
        let user = auth
            .complete_browser_sign_in(
                "fresh-test-code",
                "fresh-test-verifier",
                "http://localhost:47863/",
                true,
            )
            .await
            .unwrap();
        assert_eq!(user.user_id, "candidate-user");
        for (file, expected) in [
            ("token.bin", b"existing-token".as_slice()),
            ("user.json", b"existing-profile".as_slice()),
            ("settings.json", b"existing-config".as_slice()),
        ] {
            assert_eq!(
                std::fs::read(directory.path().join(file)).unwrap(),
                expected
            );
        }
        assert!(auth.user().is_none());
        assert!(auth.token.lock().await.refresh.is_none());
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let request = &requests[0];
        assert!(request.header("authorization").is_none());
        let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        assert_eq!(form.get("client_id").unwrap(), "loopback-client");
        assert_eq!(form.get("code_verifier").unwrap(), "fresh-test-verifier");
        assert_eq!(form.get("redirect_uri").unwrap(), "http://localhost:47863/");
        assert!(!form.contains_key("client_secret"));
    }

    #[tokio::test]
    async fn resilience_candidate_validation_rejects_confidential_client_without_leaking_remote_body(
    ) {
        use crate::test_support::{Fault, Server};
        let server = Server::scripted(vec![Fault::reply(401,
            r#"{"error":"invalid_client","error_description":"client_secret=synthetic-private-value","developerMessage":"test-code-do-not-echo"}"#)])
            .await;
        let directory = TestStateDir::new();
        let auth = loopback_auth(&server, directory.path());
        let error = auth
            .complete_browser_sign_in(
                "test-code",
                "test-verifier",
                "http://localhost:47863/",
                true,
            )
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("401"));
        assert!(message.contains("invalid_client"));
        assert!(message.contains("Desktop, Mobile, Single-Page App"));
        assert!(!message.contains("synthetic-private-value"));
        assert!(!message.contains("test-code-do-not-echo"));
        assert_eq!(server.requests().len(), 1);
        assert!(!auth.token_path().exists());
        assert!(!auth.user_path().exists());
    }

    #[tokio::test]
    async fn resilience_candidate_validation_requires_both_tokens_and_a_valid_profile() {
        use crate::test_support::Server;
        for tokens in [
            serde_json::json!({"access_token":"valid-access"}),
            serde_json::json!({"access_token":" ", "refresh_token":"valid-refresh"}),
        ] {
            let server = Server::scripted(vec![json_reply(tokens)]).await;
            let directory = TestStateDir::new();
            let auth = loopback_auth(&server, directory.path());
            assert!(auth
                .complete_browser_sign_in(
                    "test-code",
                    "test-verifier",
                    "http://localhost:47863/",
                    true
                )
                .await
                .is_err());
            assert_eq!(server.requests().len(), 1);
            assert!(!auth.token_path().exists());
        }
        let server = Server::scripted(vec![
            token_reply("candidate-refresh"),
            json_reply(serde_json::json!({})),
        ])
        .await;
        let directory = TestStateDir::new();
        let auth = loopback_auth(&server, directory.path());
        assert!(auth
            .complete_browser_sign_in(
                "test-code",
                "test-verifier",
                "http://localhost:47863/",
                true
            )
            .await
            .is_err());
        assert!(!auth.token_path().exists());
        assert!(!auth.user_path().exists());
    }

    #[tokio::test]
    async fn resilience_oversized_oauth_error_does_not_escape_or_require_unbounded_buffering() {
        use crate::test_support::{Fault, Server};
        let server = Server::scripted(vec![Fault::reply(
            401,
            "sensitive".repeat(MAX_OAUTH_ERROR_BYTES),
        )])
        .await;
        let directory = TestStateDir::new();
        let auth = loopback_auth(&server, directory.path());
        let error = auth
            .complete_browser_sign_in(
                "test-code",
                "test-verifier",
                "http://localhost:47863/",
                true,
            )
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("oauth_error"));
        assert!(!message.contains("sensitive"));
        assert!(!auth.token_path().exists());
    }
}

#[cfg(test)]
mod interaxo_client_tests {
    use super::*;

    /// A private state directory, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let path = std::env::temp_dir().join(format!(
                "synka-interaxo-{tag}-{}-{}",
                std::process::id(),
                random_b64(8).unwrap()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[test]
    fn loopback_redirects_with_an_explicit_port_are_accepted() {
        for uri in [
            "http://localhost:47864/",
            "http://127.0.0.1:1/",
            "http://localhost:65535/callback/interaxo",
            "http://localhost:80/",
            "http://LOCALHOST:47864/",
            INTERAXO_DEFAULT_REDIRECT,
        ] {
            assert_eq!(check_loopback_redirect(uri), Ok(()), "{uri}");
        }
    }

    #[test]
    fn redirects_outside_the_loopback_rules_are_rejected() {
        for uri in [
            "",
            "https://localhost:47864/",
            "http://example.com:47864/",
            "http://localhost.example:47864/",
            "http://[::1]:47864/",
            "http://localhost/",
            "http://localhost:/",
            "http://localhost:0/",
            "http://localhost:65536/",
            "http://localhost:+80/",
            "http://localhost:47864/?x=1",
            "http://localhost:47864/#fragment",
            "http://user@localhost:47864/",
            "http://user:pw@localhost:47864/",
            "http://localhost:47864/a b",
            "http://localhost:47864/\n",
            "localhost:47864",
        ] {
            assert!(
                check_loopback_redirect(uri).is_err(),
                "{uri:?} must be rejected"
            );
        }
    }

    #[test]
    fn interaxo_account_is_configured_by_its_stored_client_id() {
        let dir = Dir::new("configured");
        let auth = Auth::interaxo(InteraxoClient::default(), http(), &dir.0);
        assert!(!auth.is_configured());
        assert!(auth.interaxo_client().is_some());

        let client = InteraxoClient {
            client_id: "my-client".into(),
            client_secret: None,
            redirect_uri: INTERAXO_DEFAULT_REDIRECT.into(),
        };
        auth.set_interaxo_client(client.clone());
        assert!(auth.is_configured());
        assert_eq!(auth.current_client_id(), "my-client");
        assert_eq!(auth.interaxo_client(), Some(client));

        auth.set_interaxo_client(InteraxoClient {
            client_id: "  ".into(),
            ..Default::default()
        });
        assert!(!auth.is_configured());
    }

    #[test]
    fn acc_account_has_no_interaxo_client() {
        let dir = Dir::new("acc");
        let acc = Auth::new("aps-client".into(), http(), &dir.0);
        assert!(acc.is_configured());
        assert!(acc.interaxo_client().is_none());
        assert!(!Auth::new(String::new(), http(), &dir.0).is_configured());
    }

    #[test]
    fn interaxo_account_keeps_its_own_token_and_profile_files() {
        let dir = Dir::new("files");
        let auth = Auth::interaxo(InteraxoClient::default(), http(), &dir.0);
        assert_eq!(auth.token_path(), dir.0.join("token-interaxo.bin"));
        assert_eq!(auth.user_path(), dir.0.join("user-interaxo.json"));
    }

    #[test]
    fn debug_output_never_shows_the_client_secret() {
        let client = InteraxoClient {
            client_id: "id".into(),
            client_secret: Some("very-secret-value".into()),
            redirect_uri: INTERAXO_DEFAULT_REDIRECT.into(),
        };
        let shown = format!("{client:?}");
        assert!(!shown.contains("very-secret-value"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }
}

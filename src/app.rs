//! Shared application context and the engine <-> UI messages.

use crate::api::streambim::Region;
use crate::auth::{Auth, UserInfo};
use crate::config::Config;
use crate::http::Api;
use crate::ipc::IpcHub;
use crate::model::{now_ms, HubInfo, Project, ProjectInfo, Provider};
use crate::paths::Paths;
use crate::store::Store;
use crate::streambim_auth::{sign_in_label, StreamBimAccounts};
use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// One sign-in attempt or pending wait: the provider, and for StreamBIM the key of its account.
/// ACC and Interaxo have one account each, so their key has no account.
pub type SignInKey = (Provider, Option<String>);

pub struct Ctx {
    /// Startup snapshot (paths, client id, update channel). Runtime-changeable settings live in
    /// `settings`; read them through `cfg_now()`.
    pub cfg: Config,
    pub settings: RwLock<Config>,
    pub paths: Paths,
    pub store: Arc<Store>,
    /// ACC account (APS OAuth).
    pub auth: Arc<Auth>,
    pub api: Api,
    /// StreamBIM accounts, one per server (see `crate::streambim_auth`). Each account has its own
    /// API client; StreamBIM requests go only through them.
    pub streambim: Arc<StreamBimAccounts>,
    /// Interaxo account: an `Auth` with the Interaxo OIDC spec (empty client id until configured).
    pub interaxo: Arc<Auth>,
    pub interaxo_api: Api,
    pub ui: UiSender,
    /// Providers paused after HTTP 429 (see `crate::sync::throttle`).
    pub throttle: crate::sync::throttle::Throttle,
}

impl Ctx {
    /// Current settings (a cheap copy of the small Config struct).
    pub fn cfg_now(&self) -> Config {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Interaxo is turned on: `interaxo_enabled` in settings.json, read at start (not an IPC setting),
    /// and never in the ACC-only build.
    pub fn interaxo_enabled(&self) -> bool {
        self.cfg.interaxo_on()
    }

    /// Whether the engine offers a provider: it signs in, lists, polls and publishes it. ACC always;
    /// StreamBIM unless this is the ACC-only build (`Config::acc_only`); Interaxo while
    /// `interaxo_enabled`. A session file of a provider that is not offered is not deleted.
    pub fn offers(&self, p: Provider) -> bool {
        match p {
            Provider::Acc => true,
            Provider::StreamBim => !self.cfg.acc_only,
            Provider::Interaxo => self.interaxo_enabled(),
        }
    }

    /// The offered providers, in account order (`settings.providers`).
    pub fn offered_providers(&self) -> Vec<Provider> {
        Provider::ALL
            .into_iter()
            .filter(|p| self.offers(*p))
            .collect()
    }

    /// True when that provider's account has a usable session. StreamBIM: any account. A session
    /// counts only while the engine offers its provider.
    pub fn signed_in(&self, p: Provider) -> bool {
        self.offers(p)
            && match p {
                Provider::Acc => self.auth.is_signed_in(),
                Provider::StreamBim => self.streambim.any_signed_in(),
                Provider::Interaxo => self.interaxo.is_signed_in(),
            }
    }

    /// True when at least one provider is signed in.
    pub fn any_signed_in(&self) -> bool {
        Provider::ALL.into_iter().any(|p| self.signed_in(p))
    }

    /// The signed-in user of one provider, if any. StreamBIM: the first signed-in account's user.
    pub fn user_for(&self, p: Provider) -> Option<UserInfo> {
        if !self.offers(p) {
            return None;
        }
        match p {
            Provider::Acc => self.auth.user(),
            Provider::StreamBim => self
                .streambim
                .signed_in()
                .first()
                .and_then(|account| account.auth.user()),
            Provider::Interaxo => self.interaxo.user(),
        }
    }

    /// True when the provider can serve this project now. For StreamBIM that is the project's own
    /// account, signed in; a project of another server (or of a signed-out account) is paused like
    /// one of a signed-out provider.
    pub fn project_connected(&self, p: &Project) -> bool {
        match p.provider {
            Provider::StreamBim => {
                self.offers(Provider::StreamBim)
                    && self
                        .streambim
                        .for_project(p)
                        .is_some_and(|account| account.auth.is_signed_in())
            }
            other => self.signed_in(other),
        }
    }

    /// True when the StreamBIM account of a hub is signed in (other providers: as `signed_in`).
    pub fn hub_connected(&self, hub: &HubInfo) -> bool {
        match hub.provider {
            Provider::StreamBim => {
                self.offers(Provider::StreamBim)
                    && self
                        .streambim
                        .for_hub(hub)
                        .is_some_and(|account| account.auth.is_signed_in())
            }
            other => self.signed_in(other),
        }
    }

    /// What the user signs in to for this project, in the words of the sign-in messages: the
    /// provider, or "StreamBIM (<host>)" for a StreamBIM project.
    pub fn sign_in_target(&self, p: &Project) -> String {
        match p.provider {
            Provider::StreamBim => sign_in_label(p),
            other => other.label().to_string(),
        }
    }
}

/// Snapshot shown in the tray tooltip and sent as the `status` event.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub user: Option<UserInfo>,
    /// Swedish one-liner: "Synkad", "Synkar…", "1 ändring väntar", "Offline", "Logga in", "Fel: …".
    pub text: String,
    /// signed_out | synced | syncing | waiting | attention | offline | error
    pub state: &'static str,
    /// Last engine error, when any.
    pub error: Option<String>,
    pub pending_jobs: i64,
    pub failed_jobs: i64,
    pub transfers: Vec<Transfer>,
    pub sync_root: String,
    /// Large delete bursts waiting for the user, per project.
    pub held_deletes: Vec<HeldDeletes>,
    /// File data on disk across projects (populated folders only). None until first counted.
    pub local_bytes: Option<u64>,
    pub engine_version: String,
    /// One entry per provider, in the order acc, streambim, interaxo.
    pub accounts: Vec<AccountStatus>,
}

/// One account as shown in `status.accounts`: ACC, Interaxo, or one StreamBIM account.
#[derive(Debug, Clone)]
pub struct AccountStatus {
    pub provider: Provider,
    /// The StreamBIM account key; None for ACC and Interaxo.
    pub account: Option<String>,
    pub label: &'static str,
    /// The provider can sign in (ACC has a client id, Interaxo has `interaxo_client_id`).
    pub configured: bool,
    pub signed_in: bool,
    pub signing_in: bool,
    pub user: Option<UserInfo>,
    /// StreamBIM: the server's host.
    pub host: Option<String>,
}

impl AccountStatus {
    /// The entry for ACC or Interaxo that is not signed in (used before the first status).
    pub fn signed_out(provider: Provider) -> AccountStatus {
        AccountStatus {
            provider,
            account: None,
            label: provider.label(),
            configured: false,
            signed_in: false,
            signing_in: false,
            user: None,
            host: None,
        }
    }
}

/// A secret text (StreamBIM password, MFA code). Debug never shows it.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for Secret {
    /// Only text is accepted, and the error never repeats the input (serde would echo it).
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Secret, D::Error> {
        match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(s) => Ok(Secret(s)),
            _ => Err(serde::de::Error::custom("lösenord och kod måste vara text")),
        }
    }
}

/// The StreamBIM server a sign-in is for: a region subdomain, or a custom https address. Both are
/// checked again by the engine (`streambim_auth::server_from_region` / `server_from_url`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamBimServer {
    Region(String),
    Url(String),
}

/// StreamBIM password sign-in input. The password is used once and never stored.
#[derive(Clone)]
pub struct StreamBimLogin {
    pub server: StreamBimServer,
    pub username: String,
    pub password: Secret,
}

impl std::fmt::Debug for StreamBimLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamBimLogin")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

/// Deletes held for confirmation in one project; `count` weighs a folder by its contents.
#[derive(Debug, Clone, Serialize)]
pub struct HeldDeletes {
    pub project_id: String,
    pub project: String,
    pub count: i64,
    /// StreamBIM model and floor plan layers among the held deletes, by name.
    pub linked: Vec<String>,
}

/// One active download or upload. `direction` is "down" or "up"; sizes in bytes.
#[derive(Debug, Clone, Serialize)]
pub struct Transfer {
    pub name: String,
    pub project: String,
    pub direction: &'static str,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// The `settings` event payload (keys as in IPC.md).
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub propagate_deletes: bool,
    pub max_downloads: usize,
    pub max_uploads: usize,
    pub poll_active_secs: u64,
    pub log_level: String,
    pub start_at_login: bool,
    pub auto_update: bool,
    pub update_channel_configured: bool,
    pub sync_root: String,
    pub client_id_configured: bool,
    /// Explicit saved override only; None means use the built-in/build-time fallback.
    pub aps_client_id: Option<String>,
    pub aps_client_id_override_supported: bool,
    pub aps_client_id_validation_supported: bool,
    /// The providers the engine offers, in account order (Interaxo only while `interaxo_enabled`).
    pub providers: Vec<&'static str>,
    /// The providers this build contains, in account order: `["acc"]` in the ACC-only build. The
    /// window shows nothing of a provider outside it.
    pub build_providers: Vec<&'static str>,
    /// Saved Interaxo client id (trimmed), or None.
    pub interaxo_client_id: Option<String>,
    /// The redirect URI in use (saved, or the default).
    pub interaxo_redirect_uri: String,
    pub interaxo_redirect_uri_default: &'static str,
    /// Whether a client secret is saved. The secret itself is never sent.
    pub interaxo_client_secret_set: bool,
}

/// One entry of the activity feed. `kind` is one of the IPC.md kinds.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityItem {
    pub ts_ms: i64,
    pub kind: &'static str,
    pub name: String,
    pub project: String,
    pub detail: String,
}

/// Hubs with their projects; the bool is "subscribed".
pub type HubList = Vec<(HubInfo, Vec<(ProjectInfo, bool)>)>;

/// Messages from the engine/tokio side to the tray (main thread) and the IPC hub.
#[derive(Debug, Clone)]
pub enum UiMsg {
    Status(Status),
    Projects {
        hubs: HubList,
        loading: bool,
    },
    /// Notification for the user (title is always "Synka").
    Notify {
        title: String,
        body: String,
        severity: Severity,
    },
    /// `installing` is true while a downloaded update is being handed to Windows.
    Update {
        available: bool,
        version: Option<String>,
        error: Option<String>,
        installing: bool,
    },
    Settings(SettingsView),
    Activity(ActivityItem),
    /// Sign-in progress of one provider (of one StreamBIM account, when `account` is set):
    /// browser | mfa_required | signed_in | failed | cancelled.
    SignInProgress {
        provider: Provider,
        account: Option<String>,
        state: &'static str,
        error: Option<String>,
    },
    /// The StreamBIM regions (live list or the built-in fallback).
    StreamBimRegions(Vec<Region>),
    /// Correlated outcome of a staged public-client PKCE test. No credentials are sent.
    ApsClientIdValidation {
        request_id: String,
        state: &'static str,
        error: Option<String>,
    },
}

impl UiMsg {
    pub fn notify(severity: Severity, body: impl Into<String>) -> UiMsg {
        UiMsg::Notify {
            title: "Synka".into(),
            body: body.into(),
            severity,
        }
    }
}

/// Commands from the tray and the IPC server to the engine.
#[derive(Debug, Clone)]
pub enum TrayCmd {
    /// Sign in to one provider. ACC and Interaxo open the browser; StreamBIM needs `streambim`.
    SignIn {
        provider: Provider,
        streambim: Option<StreamBimLogin>,
    },
    /// The MFA code for a pending StreamBIM sign-in of the account `account`.
    SignInMfa {
        provider: Provider,
        account: String,
        code: Secret,
    },
    /// Cancel a pending sign-in (browser or MFA wait) of one provider. For StreamBIM, `account`
    /// names one account; without it every pending StreamBIM sign-in is cancelled.
    CancelSignIn {
        provider: Provider,
        account: Option<String>,
    },
    /// Sign out of one provider, or of every provider (`None`). `account` (StreamBIM only) signs
    /// out that account alone.
    SignOut {
        provider: Option<Provider>,
        account: Option<String>,
    },
    /// Load the StreamBIM region list and publish it as `streambim_regions`.
    StreamBimRegions,
    TestApsClientId {
        client_id: String,
        request_id: String,
    },
    CancelApsClientIdTest {
        request_id: String,
    },
    /// Open the sync root in Explorer.
    OpenFolder,
    /// Open an absolute path (file or folder) with the shell.
    OpenPath(PathBuf),
    OpenLog,
    SyncNow,
    /// Reload hubs/projects from ACC and publish `projects`.
    RefreshProjects,
    /// Subscribe/unsubscribe a project. Hub and project are looked up in the engine's last list.
    SetProject {
        hub_id: String,
        project_id: String,
        subscribe: bool,
    },
    /// Already validated by the IPC layer (see `config::check_setting`).
    SetSetting {
        key: String,
        value: serde_json::Value,
    },
    FreeUpSpace {
        project_id: Option<String>,
    },
    CheckUpdates,
    InstallUpdate,
    /// Revive failed (dead) outbox jobs.
    RetryFailed,
    /// Zip logs + a sanitized state summary to the Desktop for support.
    ExportDiagnostics,
    /// Rebuild a project (or one folder in it, `path`) from ACC; unsynced local changes are
    /// saved to a rescue folder first.
    Reset {
        project_id: Option<String>,
        path: Option<PathBuf>,
    },
    /// Rebuild every project from ACC where it is.
    ResetAll,
    /// Move the sync root to this (already checked) path and rebuild everything there; the
    /// engine restarts itself afterwards.
    MoveSyncRoot {
        path: PathBuf,
    },
    /// Answer held mass deletes: send them to ACC, or bring the files back.
    ConfirmDeletes {
        project_id: String,
        confirm: bool,
    },
    /// An Explorer context-menu verb on paths under the sync root (`crate::shell`).
    Shell {
        verb: crate::shell::ShellVerb,
        paths: Vec<PathBuf>,
    },
    /// A window connected. `hello` itself is answered by the IPC layer from its caches; the
    /// engine only refreshes what the window shows.
    Hello,
    Quit,
}

/// Cross-thread sender to the tray: pushes the message to the IPC hub and to the tray, and
/// wakes the Win32 message loop with PostThreadMessageW(main_thread_id, WM_APP_UI, 0, 0).
#[derive(Clone)]
pub struct UiSender {
    pub tx: std::sync::mpsc::Sender<UiMsg>,
    pub main_thread_id: u32,
    pub hub: Arc<IpcHub>,
}

pub const WM_APP_UI: u32 = 0x8000 + 1; // WM_APP + 1

impl UiSender {
    pub fn send(&self, m: UiMsg) {
        self.hub.on_msg(&m);
        if self.tx.send(m).is_ok() {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    self.main_thread_id,
                    WM_APP_UI,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
    }

    /// Append an activity item (cheap: one message, ring buffer kept by the hub).
    pub fn activity(&self, kind: &'static str, name: &str, project: &str, detail: &str) {
        self.send(UiMsg::Activity(ActivityItem {
            ts_ms: now_ms(),
            kind,
            name: name.to_string(),
            project: project.to_string(),
            detail: detail.to_string(),
        }));
    }
}

pub type CtxRef = Arc<Ctx>;

static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Start the engine again once this process has stopped (after a sync root move).
pub fn request_restart() {
    RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub fn restart_requested() -> bool {
    RESTART.load(std::sync::atomic::Ordering::SeqCst)
}

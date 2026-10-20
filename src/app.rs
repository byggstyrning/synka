//! Shared application context and the engine <-> UI messages.

use crate::api::streambim::Region;
use crate::auth::{Auth, UserInfo};
use crate::config::Config;
use crate::http::Api;
use crate::ipc::IpcHub;
use crate::model::{now_ms, HubInfo, Project, ProjectInfo, Provider};
use crate::paths::Paths;
use crate::store::Store;
use crate::streambim_auth::StreamBimAuth;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

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
    /// StreamBIM account (its own login, see `crate::streambim_auth`).
    pub streambim: Arc<StreamBimAuth>,
    pub streambim_api: Api,
    /// Interaxo account: an `Auth` with the Interaxo OIDC spec (empty client id until configured).
    pub interaxo: Arc<Auth>,
    pub interaxo_api: Api,
    pub ui: UiSender,
}

impl Ctx {
    /// Current settings (a cheap copy of the small Config struct).
    pub fn cfg_now(&self) -> Config {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The API client of one provider's account.
    pub fn api_for(&self, p: Provider) -> &Api {
        match p {
            Provider::Acc => &self.api,
            Provider::StreamBim => &self.streambim_api,
            Provider::Interaxo => &self.interaxo_api,
        }
    }

    /// True when that provider's account has a usable session.
    pub fn signed_in(&self, p: Provider) -> bool {
        match p {
            Provider::Acc => self.auth.is_signed_in(),
            Provider::StreamBim => self.streambim.is_signed_in(),
            Provider::Interaxo => self.interaxo.is_signed_in(),
        }
    }

    /// True when at least one provider is signed in.
    pub fn any_signed_in(&self) -> bool {
        Provider::ALL.into_iter().any(|p| self.signed_in(p))
    }

    /// The signed-in user of one provider, if any.
    pub fn user_for(&self, p: Provider) -> Option<UserInfo> {
        match p {
            Provider::Acc => self.auth.user(),
            Provider::StreamBim => self.streambim.user(),
            Provider::Interaxo => self.interaxo.user(),
        }
    }

    /// True when the provider can serve this project now: its account is signed in and, for
    /// StreamBIM, the signed-in account is in the project's own region. A project of another
    /// region (or of an account that was signed out) is paused like one of a signed-out provider.
    pub fn project_connected(&self, p: &Project) -> bool {
        if !self.signed_in(p.provider) {
            return false;
        }
        match p.provider {
            Provider::StreamBim => {
                project_region(p).is_some() && self.streambim.region() == project_region(p)
            }
            Provider::Acc | Provider::Interaxo => true,
        }
    }

    /// What the user signs in to for this project, in the words of the sign-in messages: the
    /// provider, or "StreamBIM (<region>)" when the signed-in StreamBIM account is elsewhere.
    pub fn sign_in_target(&self, p: &Project) -> String {
        match project_region(p) {
            Some(region)
                if p.provider == Provider::StreamBim
                    && self.signed_in(p.provider)
                    && self.streambim.region().as_deref() != Some(region.as_str()) =>
            {
                format!("StreamBIM ({region})")
            }
            _ => p.provider.label().to_string(),
        }
    }
}

/// The StreamBIM region a project was made under: `meta` is `{"region":..}` (docs/PROVIDERS.md).
pub fn project_region(p: &Project) -> Option<String> {
    let meta: serde_json::Value = serde_json::from_str(p.meta.as_deref()?).ok()?;
    meta.get("region")?.as_str().map(str::to_string)
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

/// One provider's account as shown in `status.accounts`.
#[derive(Debug, Clone)]
pub struct AccountStatus {
    pub provider: Provider,
    pub label: &'static str,
    /// The provider can sign in (ACC has a client id, Interaxo has `interaxo_client_id`).
    pub configured: bool,
    pub signed_in: bool,
    pub signing_in: bool,
    pub user: Option<UserInfo>,
    /// StreamBIM: the region host once signed in.
    pub host: Option<String>,
}

impl AccountStatus {
    /// The entry for a provider that is not signed in (used before the first status).
    pub fn signed_out(provider: Provider) -> AccountStatus {
        AccountStatus {
            provider,
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

/// StreamBIM password sign-in input. The password is used once and never stored.
#[derive(Clone)]
pub struct StreamBimLogin {
    pub region: String,
    pub username: String,
    pub password: Secret,
}

impl std::fmt::Debug for StreamBimLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamBimLogin")
            .field("region", &self.region)
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
    /// Every provider the engine knows, in account order.
    pub providers: Vec<&'static str>,
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
    /// Sign-in progress of one provider: browser | mfa_required | signed_in | failed | cancelled.
    SignInProgress {
        provider: Provider,
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
    /// The MFA code for a pending StreamBIM sign-in.
    SignInMfa {
        provider: Provider,
        code: Secret,
    },
    /// Cancel a pending sign-in (browser or MFA wait) of one provider.
    CancelSignIn {
        provider: Provider,
    },
    /// Sign out of one provider, or of every provider (`None`).
    SignOut {
        provider: Option<Provider>,
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

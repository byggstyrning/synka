//! The sync engine. See DESIGN.md.
//!
//! Local layout: <sync_root>\<hub name>\<project name>\<top folder>\<folders...>, as in the ACC
//! web UI: the project directory holds "Project Files", or, for a user who
//! only reaches some folders inside it, each of those (`Project::tops`, from topFolders).
//! Hub dirs are Kind::Hub placeholders (identity pid = 0, id = hub id); project dirs are
//! Kind::ProjectRoot with the fixed id PROJECT_ROOT_ID (ours, not an ACC folder: nothing can
//! be created, renamed or deleted directly in it, and its top folders cannot be renamed,
//! moved or deleted from here). Hub dirs are created with on_demand = false (they only contain
//! subscribed projects); project dirs and folders are on_demand = true and answer
//! FETCH_PLACEHOLDERS from SQLite (populated on first enumeration). Top folders are refreshed
//! every TOPS_EVERY (permissions change); a project whose rows predate top folders (the
//! project directory was "Project Files") is rebuilt once at startup.
//! Names are sanitized for Windows (replace <>:"/\|?* and control chars with '_', trim
//! trailing dots/spaces, avoid reserved names CON, PRN, AUX, NUL, COM1-9, LPT1-9 by inserting
//! '_' before the first dot).
//!
//! Engine::run loop (tokio::select!):
//!   - cf events (FetchData, FetchPlaceholders, Delete, Rename, cancels)
//!   - tray commands (TrayCmd)
//!   - local watcher batches (watcher::LocalChange)
//!   - scheduler (next due project poll, outbox tick, daily update check)
//!
//! Remote poll per project (by Project.change_source):
//!   PublicWalk (always when Config::desktop_sync_feed is false, the default) -> for every populated
//!     folder (store.populated_folders): remote::folder_contents, diff against store.children: build
//!     Delta (upserts for new/changed, deletes for missing). Never write when identical.
//!   DesktopSync/Unknown, only when desktop_sync_feed is true -> api::syncfeed::poll; on
//!     FeedError::NotSupported -> set PublicWalk. With the flag off, a project stored as
//!     DesktopSync/Unknown is switched to PublicWalk and its feed cursor dropped first.
//!   Then store.apply_delta -> Applied -> disk_apply (placeholders on disk).
//!
//! disk_apply (Applied) — only for items whose parent folder is populated (else DB only):
//!   created  -> create placeholder in parent dir. If a non-placeholder local file with the same
//!               name exists -> conflict: rename local to "name (konflikt <date>).ext" and
//!               enqueue it as a new upload.
//!   changed version (file) -> if local placeholder is in sync (clean): update_placeholder with
//!               dehydrate = hydrated; pinned files are re-hydrated. Otherwise conflict copy.
//!   renamed/moved -> std::fs::rename old path -> new path, then metadata update.
//!   deleted   -> if clean: remove (recursively for folders); if dirty: keep local.
//!   locked_by -> set_read_only(true/false).
//!
//! FETCH_PLACEHOLDERS(dir): hub dir -> projects of that hub. Folder/root -> if never listed
//!   fetch remote::folder_contents first, then answer with children and set populated = true.
//! FETCH_DATA(file): signeds3download -> stream into one reusable 1 MiB buffer ->
//!   CfExecute(TRANSFER_DATA) at 4 KiB-aligned offsets. Semaphore(cfg.max_downloads), cancel by
//!   AbortHandle keyed by file_id/transfer_key. On success set base_version = remote_version.
//! Delete / Rename: ack immediately, then enqueue outbox jobs ("delete", "rename", "move").
//!
//! Status: UiMsg::Status throttled to <= 2 per second, Swedish text.

pub mod ignore;
pub mod naming;
mod recovery;
mod relocate;
mod reset;
mod shell_cmds;
pub mod upload;
pub mod watcher;

#[cfg(test)]
mod change_source_tests;
#[cfg(test)]
mod local_inspection_tests;
#[cfg(test)]
mod provider_tests;
#[cfg(test)]
mod resilience_tests;

pub use naming::sanitize_name;
pub(crate) use relocate::check as check_new_root;
pub(crate) use reset::project_resetting;

use crate::api::syncfeed;
use crate::api::syncfeed::FeedError;
use crate::app::{
    AccountStatus, Ctx, CtxRef, HeldDeletes, HubList, SettingsView, Severity, Status,
    StreamBimLogin, Transfer, TrayCmd, UiMsg,
};
use crate::auth::{UserInfo, INTERAXO_DEFAULT_REDIRECT};
use crate::cf::callbacks as cb;
use crate::cf::placeholder::{self as ph, PinState, PlaceholderInfo, PlaceholderSpec};
use crate::cf::{
    self, CfEvent, DeleteReq, FetchDataReq, FetchPlaceholdersReq, OpKeys, ProviderStatus,
    RenameReq, STATUS_CLOUD_FILE_ACCESS_DENIED, STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
    STATUS_CLOUD_FILE_UNSUCCESSFUL,
};
use crate::http::ApiError;
use crate::model::*;
use crate::remote::{self, DownloadUrl};
use crate::store::Store;
use crate::streambim_auth::LoginOutcome;
use crate::sync::watcher::{LocalChange, WatchHandle};
use crate::update::{self, UpdateInfo};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, Semaphore};
use tokio::task::AbortHandle;
use tokio::time::{sleep_until, Instant};
use tracing::{debug, error, info, warn};

pub(crate) const APP_NAME: &str = "Synka";
/// Shown instead of an update check when `Config::update_channel` is empty (updates are off).
pub(crate) const UPDATES_OFF: &str = "Uppdateringskontrollen är avstängd. Nya versioner av Synka delas via den interna OneDrive-mappen.";
const ACTIVE_WINDOW: Duration = Duration::from_secs(10 * 60);
const RECENT_WINDOW: Duration = Duration::from_secs(60 * 60);
const IDLE_MS: u64 = 30 * 60 * 1000;
const STATUS_GAP: Duration = Duration::from_millis(500);
/// Collapse a double-click without blocking a deliberate retry of a failed browser tab.
const SIGN_IN_BURST: Duration = Duration::from_secs(1);
/// How long a StreamBIM sign-in waits for its MFA code.
const MFA_WAIT: Duration = Duration::from_secs(5 * 60);
/// Stored in kv: the id the sync root is registered under (see `root_uid`).
const SYNC_ROOT_UID_KEY: &str = "sync_root_uid";
const PROGRESS_GAP: Duration = Duration::from_millis(250);
/// Shortest pause between two outbox passes (jobs enqueued during a pass run in the next one).
const OUTBOX_MIN_GAP: Duration = Duration::from_millis(500);
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// A pending automatic install waits this long between checks for a quiet moment.
const AUTO_INSTALL_RETRY: Duration = Duration::from_secs(5 * 60);
const CHUNK: usize = 1024 * 1024;
/// Files at least this large are fetched as SEGMENTS parallel byte ranges.
const SEGMENT_MIN: u64 = 32 * 1024 * 1024;
const SEGMENTS: u64 = 4;
const ALIGN: u64 = 4096;
const SUPPRESS: Duration = Duration::from_secs(5);
/// A local delete waits this long before it is sent to ACC, so that a "safe save" (delete or
/// rename-to-temp, then a new file under the same name) becomes a new version instead of a
/// delete plus a new file with a lost version history.
const DELETE_GRACE_MS: i64 = 15_000;
/// While the user is idle the next poll may be 12 minutes away; check this often whether they
/// came back, so the first poll after a break is immediate.
const IDLE_CHECK: Duration = Duration::from_secs(60);
/// `local_bytes` is recomputed at most this often, and only while a window is connected.
const LOCAL_BYTES_EVERY: Duration = Duration::from_secs(120);
/// A remote change that could not be applied because the file was open is retried this often.
const REFRESH_RETRY: Duration = Duration::from_secs(60);

/// This many pending deletes (a folder counts its contents) in one project within the grace
/// period are held until the user confirms: one stray Shift+Delete must not empty ACC.
const MASS_DELETE: i64 = 25;

/// Cloud Files can still report IN_SYNC while bytes appended to a previously empty
/// placeholder remain in its local modified range. Those bytes are not safe to
/// discard or replace until an upload has validated them.
fn placeholder_is_clean(info: &PlaceholderInfo) -> bool {
    info.in_sync && info.modified_size == 0
}

fn placeholder_belongs_to(info: &PlaceholderInfo, pid: Pid, id: &str) -> bool {
    ph::decode_identity(&info.identity).is_some_and(|(kind, actual_pid, actual_id)| {
        kind == Kind::File && actual_pid == pid && actual_id == id
    })
}

fn can_replace_remote_content(item: &Item, info: &PlaceholderInfo) -> bool {
    // A previously interrupted upload can leave CFAPI reporting clean while
    // the database still owns local edits. The pending local state wins.
    item.local_state == LocalState::Clean
        && placeholder_is_clean(info)
        && placeholder_belongs_to(info, item.pid, &item.id)
}

/// A placeholder may still hold local ranges from an older remote version even
/// when it is clean and its DB row has already advanced to a newer tip. Unknown
/// A missing base is unsafe once a remote tip exists: older installations may
/// have left a partially hydrated placeholder without recording its version.
fn fetch_would_mix_conflict_versions(item: &Item) -> bool {
    item.base_version
        .is_some_and(|base| Some(base) != item.remote_version)
        || (item.base_version.is_none()
            && (item.remote_version.is_some()
                || matches!(item.local_state, LocalState::Dirty | LocalState::Error)))
}

/// A conflict copy receives a new local-only identity, so it must already have
/// every byte on disk. OnDiskDataSize is an aggregate; the recall flag also has
/// to say the whole file is local before we detach its remote backing identity.
fn conflict_copy_has_all_bytes(info: &PlaceholderInfo, size: u64) -> bool {
    size == 0 || (info.hydrated && info.on_disk_size >= size)
}

/// This conflict cannot be split safely until the original version's missing
/// ranges have been recovered. Uploads should fail visibly rather than retrying
/// a rename that cannot succeed without losing access to those ranges.
#[derive(Debug, thiserror::Error)]
#[error("kan inte skapa konfliktkopia av {path}: det är inte bekräftat att hela filen finns lokalt ({on_disk_size} av {size} byte); den ursprungliga filen ligger kvar")]
pub(crate) struct IncompleteConflictCopy {
    path: String,
    on_disk_size: u64,
    size: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("kan inte säkra konfliktkopians identitet för {path}: {source}")]
pub(crate) struct ConflictDetachFailed {
    path: String,
    pub(crate) rollback_failed: bool,
    #[source]
    source: anyhow::Error,
}
/// How often a project's top folders are asked for again (they change with permissions).
const TOPS_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Serializes every pass that changes the DB and the disk together (remote apply, local
/// changes, cfapi delete/rename). Never held across an `.await`.
static LOCAL_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn local_lock() -> MutexGuard<'static, ()> {
    LOCAL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Scheduler and session state shared with spawned tasks. Never held across an `.await`.
#[derive(Default)]
struct St {
    next_due: HashMap<Pid, Instant>,
    polling: HashSet<Pid>,
    /// Placeholder refresh workers; true means a newer change arrived while one was running.
    refreshing: HashMap<(Pid, String), bool>,
    /// Failed local placeholder queries awaiting one delayed inspection per path.
    pending_local_retries: HashSet<PathBuf>,
    fails: HashMap<Pid, u32>,
    activity: HashMap<Pid, Instant>,
    /// file_id -> (transfer_key, handle) of running FETCH_DATA tasks.
    downloads: HashMap<i64, Vec<(i64, AbortHandle)>>,
    /// transfer_key -> active download.
    transfers: HashMap<i64, Transfer>,
    last_error: Option<String>,
    offline: bool,
    outbox_running: bool,
    outbox_next: Option<Instant>,
    update_due: Option<Instant>,
    pending_update: Option<UpdateInfo>,
    /// Install the pending update automatically once the engine is quiet.
    auto_install_at: Option<Instant>,
    installing: bool,
    /// Providers with a sign-in attempt in progress (browser, login or MFA wait).
    signing_in: HashSet<Provider>,
    last_status: Option<Instant>,
    flush_scheduled: bool,
    provider: Option<u8>,
    /// Some project is on the idle tier: wake every IDLE_CHECK to notice the user's return.
    idle_parked: bool,
    /// Unix ms of the last successful remote poll in this process.
    last_ok_ms: Option<i64>,
    local_bytes: Option<u64>,
    local_bytes_at: Option<Instant>,
    counting_bytes: bool,
    /// When each project's top folders were last asked for.
    tops_at: HashMap<Pid, Instant>,
    /// The sync root is being moved (relocate.rs); the engine restarts when done.
    moving: bool,
    /// A reset/move is draining workers or swapping paths; defer new mutation work.
    recovering: bool,
    /// Commands already admitted before recovery began must finish first.
    commands_running: usize,
    /// Partial recovery keeps affected paths/jobs paused until restart.
    incomplete_recoveries: Vec<anyhow::Error>,
    /// Maintenance is counted before spawning, including time before its first poll.
    maintenance_running: usize,
}

/// Side effects of a local or remote batch, applied by the engine after the blocking work.
#[derive(Default)]
pub(crate) struct Work {
    touched: HashSet<Pid>,
    hydrate: Vec<PathBuf>,
    enqueued: bool,
    /// Remote changes that could not reach the disk yet (file open): retried later.
    refresh: Vec<(Pid, String)>,
    /// Remote renames or moves waiting for an open file or folder (see `pend_move`).
    moves: Vec<(Pid, String)>,
    /// Unknown Cloud Files state must be inspected again, never assumed to be a local edit.
    local_retry: Vec<PathBuf>,
}

enum Internal {
    /// A sign-in task of one provider finished (a StreamBIM login may answer "MFA required").
    SignedIn {
        provider: Provider,
        generation: u64,
        result: Result<LoginOutcome>,
    },
    /// The five minutes for a StreamBIM MFA code are over.
    SignInExpired { provider: Provider, generation: u64 },
    ClientIdValidated {
        generation: u64,
        request_id: String,
        result: Result<()>,
    },
    /// Stop the run loop (the process then exits, or restarts after a sync root move).
    Quit,
}

/// Candidate settings are kept in memory until the entire public-client flow succeeds.
struct ClientIdValidation {
    request_id: String,
    client_id: String,
    original_override: Option<String>,
    original_effective_id: String,
}

/// One pending sign-in of one provider. Owned by the command loop, so a retry or a sign-out can
/// wait for the cancellation.
struct SignInAttempt {
    /// The sign-in itself; for a StreamBIM MFA wait, the timer that ends the wait.
    task: tokio::task::JoinHandle<()>,
    /// Identifies this attempt: a completion for another generation is ignored.
    generation: u64,
    started_at: Instant,
    /// StreamBIM: the login asked for an MFA code and `task` is the wait's timer.
    mfa_pending: bool,
}

/// Connection-scoped handles, owned by the run loop.
struct Live {
    conn: Option<cf::Connection>,
    watch: Option<WatchHandle>,
    cf_tx: UnboundedSender<CfEvent>,
    w_tx: UnboundedSender<Vec<LocalChange>>,
    /// Pending sign-in attempts, one per provider.
    sign_ins: HashMap<Provider, SignInAttempt>,
    /// Last generation handed out (monotonic over all providers).
    sign_in_generation: u64,
    client_validation: Option<ClientIdValidation>,
    /// A saved APS identity change must stop before another command or timer runs.
    restart_pending: bool,
}

pub struct Engine {
    ctx: CtxRef,
    st: Mutex<St>,
    down_sem: Arc<Semaphore>,
    /// One free-up scan at a time, even when commands arrive from different windows/pages.
    free_up_sem: Arc<Semaphore>,
    wake: Notify,
    /// Last hub/project listing from ACC (for SetProject lookups and the projects event).
    projects: Mutex<HubList>,
}

struct MaintenanceGuard(Arc<Engine>);

impl Drop for MaintenanceGuard {
    fn drop(&mut self) {
        let mut st = self.0.st();
        st.maintenance_running = st.maintenance_running.saturating_sub(1);
    }
}

/// Name of the StartupTask declared in the MSIX manifest.
const STARTUP_TASK_ID: &str = "SynkaStartup";

impl Engine {
    pub fn new(ctx: CtxRef) -> Arc<Engine> {
        let permits = ctx.cfg.max_downloads.max(1);
        Arc::new(Engine {
            ctx,
            st: Mutex::new(St::default()),
            down_sem: Arc::new(Semaphore::new(permits)),
            free_up_sem: Arc::new(Semaphore::new(1)),
            wake: Notify::new(),
            projects: Mutex::new(Vec::new()),
        })
    }

    /// Runs until TrayCmd::Quit. Registers + connects the sync root when signed in
    /// (cf::register with the user's id), creates hub/project dirs for enabled projects,
    /// starts the watcher, then loops. Returns after cleanly disconnecting.
    pub async fn run(self: Arc<Self>, mut cmds: UnboundedReceiver<TrayCmd>) -> anyhow::Result<()> {
        let (cf_tx, mut cf_rx) = unbounded_channel::<CfEvent>();
        let (w_tx, mut w_rx) = unbounded_channel::<Vec<LocalChange>>();
        let (itx, mut irx) = unbounded_channel::<Internal>();
        let mut live = Live {
            conn: None,
            watch: None,
            cf_tx,
            w_tx,
            sign_ins: HashMap::new(),
            sign_in_generation: 0,
            client_validation: None,
            restart_pending: false,
        };

        if !self.ctx.cfg.update_channel.trim().is_empty() {
            self.st().update_due = Some(Instant::now() + Duration::from_secs(60));
        }
        if self.ctx.any_signed_in() {
            if let Err(e) = self.go_online(&mut live).await {
                error!("kunde inte starta synk: {e:#}");
                self.set_error(format!("{e:#}"));
            }
        } else {
            info!("ej inloggad");
        }
        self.publish();
        self.publish_settings();
        // Lists the hubs of every signed-in provider, and the subscriptions of the others.
        let me = self.clone();
        self.spawn_maintenance(async move { me.refresh_projects().await });

        loop {
            let wake_at = self.next_wake();
            tokio::select! {
                ev = cf_rx.recv() => {
                    if let Some(ev) = ev {
                        self.on_cf(ev);
                    }
                }
                cmd = cmds.recv() => match cmd {
                    None | Some(TrayCmd::Quit) => break,
                    Some(cmd) => self.on_cmd(cmd, &mut live, &itx).await,
                },
                batch = w_rx.recv() => {
                    if let Some(batch) = batch {
                        self.on_local(batch);
                    }
                }
                msg = irx.recv() => match msg {
                    Some(Internal::SignedIn { provider, generation, result }) => {
                        self.on_signed_in(provider, generation, result, &mut live, &itx).await;
                    }
                    Some(Internal::SignInExpired { provider, generation }) => {
                        self.on_sign_in_expired(provider, generation, &mut live);
                    }
                    Some(Internal::ClientIdValidated { generation, request_id, result }) => {
                        self.on_client_id_validated(generation, &request_id, result, &mut live).await;
                        if live.restart_pending {
                            crate::app::request_restart();
                        }
                    }
                    Some(Internal::Quit) => break,
                    None => {}
                },
                _ = sleep_until(wake_at) => self.on_timers(),
                _ = self.wake.notified() => {}
            }
            if live.restart_pending {
                break;
            }
            self.sync_provider(&live);
        }

        info!("avslutar synkmotorn");
        self.cancel_all_sign_ins(&mut live).await;
        self.abort_downloads();
        live.watch.take();
        live.conn.take();
        Ok(())
    }

    fn st(&self) -> MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn spawn_maintenance(
        self: &Arc<Self>,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let guard = self.maintenance_guard();
        tokio::spawn(async move {
            let _guard = guard;
            work.await;
        });
    }

    fn maintenance_guard(self: &Arc<Self>) -> MaintenanceGuard {
        self.st().maintenance_running += 1;
        MaintenanceGuard(self.clone())
    }

    /// Deferred moves/refreshes may sleep while signed out. Only their active
    /// mutation span blocks a client change; retry markers remain for next login.
    fn active_session_work(self: &Arc<Self>) -> Option<MaintenanceGuard> {
        let mut st = self.st();
        if !self.ctx.any_signed_in() || st.recovering || st.moving {
            return None;
        }
        st.maintenance_running += 1;
        Some(MaintenanceGuard(self.clone()))
    }

    // ------------------------------------------------------------------ session

    /// True when the project's provider is signed in (false when the project is unknown).
    fn project_signed_in(&self, pid: Pid) -> bool {
        self.ctx
            .store
            .project(pid)
            .ok()
            .flatten()
            .is_some_and(|p| self.ctx.signed_in(p.provider))
    }

    /// The user of a provider whose account is signed in.
    fn signed_user(&self, p: Provider) -> Option<UserInfo> {
        if self.ctx.signed_in(p) {
            self.ctx.user_for(p)
        } else {
            None
        }
    }

    /// The id the sync root is registered under. Stored once (kv `sync_root_uid`), so it
    /// stays the same while the root is registered whichever account signs in later.
    fn sync_root_uid(&self) -> Result<String> {
        let acc = self.signed_user(Provider::Acc);
        let first_other = Provider::ALL
            .into_iter()
            .filter(|p| *p != Provider::Acc)
            .find_map(|p| self.signed_user(p).map(|u| (p, u)));
        let stored = self.ctx.store.kv_get(SYNC_ROOT_UID_KEY)?;
        let uid = root_uid(
            stored.clone(),
            acc.as_ref(),
            first_other.as_ref().map(|(p, u)| (*p, u)),
        )
        .ok_or_else(|| anyhow!("ingen användarinformation"))?;
        if stored.as_deref() != Some(uid.as_str()) {
            self.ctx.store.kv_set(SYNC_ROOT_UID_KEY, Some(&uid))?;
        }
        Ok(uid)
    }

    async fn go_online(self: &Arc<Self>, live: &mut Live) -> Result<()> {
        if live.conn.is_some() {
            return Ok(());
        }
        let uid = self.sync_root_uid()?;
        let root = self.ctx.paths.sync_root.clone();
        let tx = live.cf_tx.clone();
        let root2 = root.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<cf::Connection> {
            cf::register(&root2, &uid, APP_NAME, env!("CARGO_PKG_VERSION"))?;
            cf::connect(&root2, tx)
        })
        .await
        .map_err(|e| anyhow!("uppgift avbröts: {e}"))??;
        live.conn = Some(conn);
        live.watch = Some(watcher::start(&root, live.w_tx.clone())?);
        let ctx = self.ctx.clone();
        if let Ok(Some(old)) =
            tokio::task::spawn_blocking(move || relocate::forget_old_root(&ctx)).await
        {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Info,
                format!("Den gamla synkmappen {} har filer som inte synkades och ligger kvar. Ta bort den när du har kollat den.", old.display()),
            ));
        }
        let ctx = self.ctx.clone();
        let (pids, relayout) = tokio::task::spawn_blocking(move || ensure_all_dirs(&ctx))
            .await
            .map_err(|e| anyhow!("uppgift avbröts: {e}"))??;
        // Deletes and renames made while no engine listened, then edits and new files (those
        // produce no watcher events either): compare the populated folders with the DB once.
        let ctx = self.ctx.clone();
        match tokio::task::spawn_blocking(move || reconcile(&ctx)).await {
            Ok(w) => self.after_work(w),
            Err(e) => warn!("avstämning avbröts: {e}"),
        }
        let _ = live.w_tx.send(vec![LocalChange::Rescan]);
        {
            let now = Instant::now();
            let mut st = self.st();
            for pid in pids {
                st.next_due.insert(pid, now);
            }
            // Jobs left from the previous session (or revived) must run without waiting
            // for a new local change to kick the queue.
            if st.outbox_next.is_none() {
                st.outbox_next = Some(now);
            }
        }
        info!("ansluten till {}", root.display());
        let me = self.clone();
        self.spawn_maintenance(async move {
            let ctx = me.ctx.clone();
            let found = tokio::task::spawn_blocking(move || reset::leftovers(&ctx))
                .await
                .unwrap_or_default();
            for (aside, name) in found {
                info!("avslutar avbruten återställning: {}", aside.display());
                let guard = reset::guard_aside(&aside);
                let set = reset::SetAside {
                    name: name.clone(),
                    aside: Some(aside),
                    guard,
                };
                me.finish_reset(set, name).await;
            }
            for (pid, id) in pending_moves(&me.ctx) {
                me.schedule_move(pid, id);
            }
            for (pid, id) in startup_refreshes(&me.ctx.store) {
                me.schedule_refresh(pid, id);
            }
            me.publish();
            // The project directory used to be "Project Files" itself; now it holds the top
            // folders. Rebuilding is the safe way over: unsynced work is saved first.
            for project_id in relayout {
                info!("bygger om {project_id} med toppmappar");
                me.clone().reset(Some(project_id), None).await;
            }
        });
        self.clear_error();
        self.publish();
        self.wake.notify_one();
        Ok(())
    }

    /// Stops one provider's pending sign-in and waits until its task is gone. Aborting first
    /// releases a browser callback socket before a replacement tries the same port, and awaiting
    /// the aborted task prevents a late token commit after logout. A completion that is already
    /// queued is dropped: its attempt is no longer in `sign_ins`.
    async fn cancel_sign_in(self: &Arc<Self>, live: &mut Live, provider: Provider) {
        if let Some(attempt) = live.sign_ins.remove(&provider) {
            attempt.task.abort();
            if let Err(error) = attempt.task.await {
                if !error.is_cancelled() {
                    warn!("inloggningstask avbröts: {error}");
                }
            }
        }
        self.st().signing_in.remove(&provider);
        if provider == Provider::Acc {
            if let Some(pending) = live.client_validation.take() {
                self.client_validation_event(&pending.request_id, "cancelled", None);
            }
        }
    }

    async fn cancel_all_sign_ins(self: &Arc<Self>, live: &mut Live) {
        for provider in Provider::ALL {
            self.cancel_sign_in(live, provider).await;
        }
    }

    /// `sign_in` for one provider. A repeat within SIGN_IN_BURST shares the pending attempt, so an
    /// accidental double click cannot open two browser tabs; a later one replaces it.
    async fn sign_in(
        self: &Arc<Self>,
        provider: Provider,
        login: Option<StreamBimLogin>,
        live: &mut Live,
        itx: &UnboundedSender<Internal>,
    ) {
        let shared = live.sign_ins.get(&provider).is_some_and(|attempt| {
            attempt.started_at.elapsed() < SIGN_IN_BURST
                && !attempt.task.is_finished()
                && !attempt.mfa_pending
        }) && !(provider == Provider::Acc && live.client_validation.is_some());
        if shared {
            self.publish();
            return;
        }
        // One more explicit click retries a closed browser tab or starts a new password login.
        self.cancel_sign_in(live, provider).await;
        if self.ctx.signed_in(provider) {
            if let Err(e) = self.go_online(live).await {
                self.set_error(format!("{e:#}"));
            }
            self.publish();
            return;
        }
        let generation = next_sign_in_generation(live);
        let me = self.clone();
        let tx = itx.clone();
        let task = match provider {
            Provider::Acc => tokio::spawn(async move {
                let result = me.ctx.auth.sign_in().await.map(LoginOutcome::SignedIn);
                let _ = tx.send(Internal::SignedIn {
                    provider,
                    generation,
                    result,
                });
                me.wake.notify_one();
            }),
            Provider::Interaxo => tokio::spawn(async move {
                let result = me.ctx.interaxo.sign_in().await.map(LoginOutcome::SignedIn);
                let _ = tx.send(Internal::SignedIn {
                    provider,
                    generation,
                    result,
                });
                me.wake.notify_one();
            }),
            Provider::StreamBim => {
                let Some(login) = login else {
                    self.ctx.ui.send(UiMsg::notify(
                        Severity::Warning,
                        "Ange region, e-postadress och lösenord för StreamBIM.",
                    ));
                    self.publish();
                    return;
                };
                tokio::spawn(async move {
                    let result = me
                        .ctx
                        .streambim
                        .login(&login.region, &login.username, &login.password.0)
                        .await;
                    drop(login);
                    let _ = tx.send(Internal::SignedIn {
                        provider,
                        generation,
                        result,
                    });
                    me.wake.notify_one();
                })
            }
        };
        live.sign_ins.insert(
            provider,
            SignInAttempt {
                task,
                generation,
                started_at: Instant::now(),
                mfa_pending: false,
            },
        );
        {
            let mut st = self.st();
            st.signing_in.insert(provider);
            if provider == Provider::Acc {
                st.last_error = None;
            }
        }
        if provider != Provider::StreamBim {
            self.ctx.ui.send(UiMsg::SignInProgress {
                provider,
                state: "browser",
                error: None,
            });
        }
        self.publish();
    }

    /// The result of a sign-in task. A result of an attempt that was replaced or cancelled is
    /// dropped. A StreamBIM login that needs an MFA code keeps its attempt for `MFA_WAIT`.
    async fn on_signed_in(
        self: &Arc<Self>,
        provider: Provider,
        generation: u64,
        r: Result<LoginOutcome>,
        live: &mut Live,
        itx: &UnboundedSender<Internal>,
    ) {
        let current = live
            .sign_ins
            .get(&provider)
            .is_some_and(|attempt| attempt.generation == generation);
        if live.restart_pending || !current {
            debug!("ignorerar svar från en ersatt inloggning");
            return;
        }
        match r {
            Ok(LoginOutcome::MfaRequired) => {
                // The login task is finished; the attempt now waits for the code.
                let timer = mfa_timer(provider, generation, itx);
                if let Some(attempt) = live.sign_ins.get_mut(&provider) {
                    attempt.task = timer;
                    attempt.mfa_pending = true;
                    attempt.started_at = Instant::now();
                }
                self.ctx.ui.send(UiMsg::SignInProgress {
                    provider,
                    state: "mfa_required",
                    error: None,
                });
            }
            Ok(LoginOutcome::SignedIn(_)) => {
                live.sign_ins.remove(&provider);
                self.st().signing_in.remove(&provider);
                info!("inloggad i {}", provider.label());
                self.ctx.ui.send(UiMsg::SignInProgress {
                    provider,
                    state: "signed_in",
                    error: None,
                });
                if live.conn.is_some() {
                    self.provider_connected(provider).await;
                } else if let Err(e) = self.go_online(live).await {
                    error!("kunde inte starta synk: {e:#}");
                    self.set_error(format!("{e:#}"));
                }
                let me = self.clone();
                self.spawn_maintenance(async move { me.refresh_projects().await });
            }
            Err(e) => {
                live.sign_ins.remove(&provider);
                self.st().signing_in.remove(&provider);
                warn!("inloggning misslyckades ({}): {e:#}", provider.label());
                let text = format!("{e:#}");
                if provider == Provider::Acc {
                    self.set_error(text.clone());
                    self.ctx.ui.send(UiMsg::notify(
                        Severity::Error,
                        format!("Inloggningen misslyckades: {text}"),
                    ));
                }
                self.ctx.ui.send(UiMsg::SignInProgress {
                    provider,
                    state: "failed",
                    error: Some(text),
                });
            }
        }
        self.publish();
    }

    /// The MFA wait ran out: the attempt ends, the user starts over.
    fn on_sign_in_expired(self: &Arc<Self>, provider: Provider, generation: u64, live: &mut Live) {
        let waiting = live
            .sign_ins
            .get(&provider)
            .is_some_and(|attempt| attempt.mfa_pending && attempt.generation == generation);
        if !waiting {
            return;
        }
        live.sign_ins.remove(&provider);
        self.st().signing_in.remove(&provider);
        self.ctx.ui.send(UiMsg::SignInProgress {
            provider,
            state: "cancelled",
            error: Some("Tiden för koden gick ut – logga in igen.".into()),
        });
        self.publish();
    }

    /// A provider signed in while the sync root is already registered: its projects get their
    /// folders and an immediate poll, and the outbox runs (its jobs waited for the sign-in).
    async fn provider_connected(self: &Arc<Self>, provider: Provider) {
        let ctx = self.ctx.clone();
        let pids =
            match tokio::task::spawn_blocking(move || ensure_provider_dirs(&ctx, provider)).await {
                Ok(Ok(pids)) => pids,
                Ok(Err(e)) => {
                    warn!("kunde inte skapa mappar för {}: {e:#}", provider.label());
                    Vec::new()
                }
                Err(e) => {
                    warn!("uppgift avbröts: {e}");
                    Vec::new()
                }
            };
        let now = Instant::now();
        let mut st = self.st();
        for pid in pids {
            st.next_due.insert(pid, now);
        }
        st.outbox_next = Some(now);
    }

    /// `sign_out` with `Some(provider)` or `None` (every provider). While another provider stays
    /// signed in, the sync root stays registered and only this provider's projects pause.
    async fn sign_out(self: &Arc<Self>, provider: Option<Provider>, live: &mut Live) {
        match provider {
            Some(p) => {
                self.cancel_sign_in(live, p).await;
                let others = Provider::ALL
                    .into_iter()
                    .any(|q| q != p && self.ctx.signed_in(q));
                if others {
                    self.sign_out_account(p).await;
                    let paused: Vec<Pid> = self
                        .ctx
                        .store
                        .projects()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|q| q.provider == p)
                        .map(|q| q.pid)
                        .collect();
                    {
                        let mut st = self.st();
                        for pid in paused {
                            st.next_due.remove(&pid);
                            st.fails.remove(&pid);
                            st.activity.remove(&pid);
                        }
                    }
                    info!("utloggad från {}; övriga konton synkar vidare", p.label());
                    self.publish();
                    return;
                }
            }
            None => self.cancel_all_sign_ins(live).await,
        }
        self.sign_out_everything(live).await;
    }

    async fn sign_out_account(&self, provider: Provider) {
        let result = match provider {
            Provider::Acc => self.ctx.auth.sign_out().await,
            Provider::StreamBim => self.ctx.streambim.sign_out().await,
            Provider::Interaxo => self.ctx.interaxo.sign_out().await,
        };
        if let Err(e) = result {
            warn!("utloggning ({}): {e:#}", provider.label());
        }
    }

    /// The full sign-out: unregister the sync root, sign out of every account, forget the root
    /// id and clear the engine state.
    async fn sign_out_everything(self: &Arc<Self>, live: &mut Live) {
        let stored = self.ctx.store.kv_get(SYNC_ROOT_UID_KEY).ok().flatten();
        let uid = stored
            .or_else(|| self.ctx.auth.user().map(|u| u.user_id))
            .or_else(|| {
                Provider::ALL
                    .into_iter()
                    .find_map(|p| self.ctx.user_for(p))
                    .map(|u| u.user_id)
            });
        live.watch.take();
        live.conn.take();
        self.abort_downloads();
        if let Some(uid) = uid {
            match tokio::task::spawn_blocking(move || cf::unregister(&uid)).await {
                Ok(Err(e)) => warn!("avregistrering av synkrot misslyckades: {e:#}"),
                Err(e) => warn!("avregistrering avbröts: {e}"),
                Ok(Ok(())) => {}
            }
        }
        for provider in Provider::ALL {
            self.sign_out_account(provider).await;
        }
        if let Err(e) = self.ctx.store.kv_set(SYNC_ROOT_UID_KEY, None) {
            warn!("kunde inte glömma synkrotens id: {e:#}");
        }
        {
            let mut st = self.st();
            st.next_due.clear();
            st.activity.clear();
            st.fails.clear();
            st.last_error = None;
            st.offline = false;
            st.provider = None;
        }
        info!("utloggad");
        self.publish();
    }

    /// Creates hub/project dirs for one project and marks it for an immediate poll.
    fn schedule_now(&self, pid: Pid) {
        self.st().next_due.insert(pid, Instant::now());
        self.wake.notify_one();
    }

    fn sync_provider(&self, live: &Live) {
        let Some(conn) = live.conn.as_ref() else {
            return;
        };
        let (code, status) = {
            let st = self.st();
            let busy = !st.polling.is_empty() || !st.downloads.is_empty() || st.outbox_running;
            if st.offline {
                (3u8, ProviderStatus::ConnectivityLost)
            } else if st.last_error.is_some() {
                (4, ProviderStatus::Error)
            } else if busy {
                (2, ProviderStatus::Syncing)
            } else {
                (1, ProviderStatus::Idle)
            }
        };
        let changed = {
            let mut st = self.st();
            if st.provider == Some(code) {
                false
            } else {
                st.provider = Some(code);
                true
            }
        };
        if changed {
            cf::set_provider_status(conn, status);
        }
    }

    // ------------------------------------------------------------------ scheduler

    fn next_wake(&self) -> Instant {
        let st = self.st();
        let mut t = Instant::now() + Duration::from_secs(3600);
        for (pid, due) in &st.next_due {
            if !st.recovering && !st.moving && !st.polling.contains(pid) && *due < t {
                t = *due;
            }
        }
        // While offline the queue waits for the next good poll (no wake-ups for it).
        if !st.recovering && !st.moving && !st.outbox_running && !st.offline {
            if let Some(o) = st.outbox_next {
                if o < t {
                    t = o;
                }
            }
        }
        for u in [st.update_due, st.auto_install_at].into_iter().flatten() {
            if u < t {
                t = u;
            }
        }
        let now = Instant::now();
        if st.idle_parked {
            t = t.min(now + IDLE_CHECK);
        }
        if self.ctx.ui.hub.client_count() > 0 {
            t = t.min(now + LOCAL_BYTES_EVERY);
        }
        t
    }

    fn on_timers(self: &Arc<Self>) {
        let now = Instant::now();
        self.wake_if_user_returned(now);
        if self.ctx.ui.hub.client_count() > 0 {
            self.refresh_local_bytes(false);
        }
        let due: Vec<Pid> = {
            let st = self.st();
            st.next_due
                .iter()
                .filter(|(pid, d)| {
                    !st.recovering && !st.moving && **d <= now && !st.polling.contains(*pid)
                })
                .map(|(pid, _)| *pid)
                .collect()
        };
        for pid in due {
            // A project of a signed-out provider does not poll. It leaves the schedule and comes
            // back when that provider signs in (provider_connected).
            let provider = self
                .ctx
                .store
                .project(pid)
                .ok()
                .flatten()
                .map(|p| p.provider);
            match provider {
                Some(provider) if !self.ctx.signed_in(provider) => {
                    self.st().next_due.remove(&pid);
                }
                _ => self.spawn_poll(pid),
            }
        }
        let signed_in = self.ctx.any_signed_in();
        let run_outbox = {
            let mut st = self.st();
            match st.outbox_next {
                Some(t)
                    if t <= now
                        && !st.recovering
                        && !st.moving
                        && !st.outbox_running
                        && !st.offline
                        && signed_in =>
                {
                    st.outbox_running = true;
                    st.outbox_next = None;
                    true
                }
                // Signed out: the jobs wait; signing in again starts the queue (go_online).
                Some(t) if t <= now && !signed_in => {
                    st.outbox_next = None;
                    false
                }
                _ => false,
            }
        };
        if run_outbox {
            self.spawn_outbox();
        }
        let run_update = {
            let mut st = self.st();
            match st.update_due {
                Some(t) if t <= now => {
                    st.update_due = Some(now + UPDATE_EVERY);
                    true
                }
                _ => false,
            }
        };
        if run_update {
            self.check_updates(false);
        }
        self.maybe_auto_install(now);
    }

    /// Idle-to-active transition: projects parked on the idle tier poll right away.
    fn wake_if_user_returned(&self, now: Instant) {
        let mut st = self.st();
        if !st.idle_parked || idle_ms() > IDLE_MS {
            return;
        }
        st.idle_parked = false;
        let polling = st.polling.clone();
        for (pid, due) in st.next_due.iter_mut() {
            if !polling.contains(pid) {
                *due = now;
            }
        }
        debug!("användaren är tillbaka, synkar nu");
    }

    /// Recompute `local_bytes` in the background (at most every LOCAL_BYTES_EVERY unless
    /// `force`). Only directories the user has opened are walked, with no file opened.
    fn refresh_local_bytes(self: &Arc<Self>, force: bool) {
        {
            let mut st = self.st();
            let fresh = st
                .local_bytes_at
                .is_some_and(|t| t.elapsed() < LOCAL_BYTES_EVERY);
            if st.counting_bytes || (fresh && !force) || !self.ctx.any_signed_in() {
                return;
            }
            st.counting_bytes = true;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let ctx = me.ctx.clone();
            let bytes = tokio::task::spawn_blocking(move || local_bytes(&ctx))
                .await
                .ok();
            {
                let mut st = me.st();
                st.counting_bytes = false;
                st.local_bytes_at = Some(Instant::now());
                if let Some(b) = bytes {
                    st.local_bytes = Some(b);
                }
            }
            me.publish();
        });
    }

    fn spawn_poll(self: &Arc<Self>, pid: Pid) {
        {
            let mut st = self.st();
            if st.recovering || st.moving || !st.polling.insert(pid) {
                return;
            }
        }
        let me = self.clone();
        tokio::spawn(async move {
            let r = me.poll_project(pid).await;
            me.finish_poll(pid, r);
        });
    }

    fn finish_poll(self: &Arc<Self>, pid: Pid, r: Result<()>) {
        let now = Instant::now();
        {
            let mut st = self.st();
            st.polling.remove(&pid);
            let delay = match r {
                Ok(()) => {
                    st.fails.remove(&pid);
                    st.offline = false;
                    st.last_error = None;
                    st.last_ok_ms = Some(now_ms());
                    let (d, idle) = interval_secs(self.ctx.cfg_now().poll_active_secs, &st, pid);
                    st.idle_parked |= idle;
                    d
                }
                Err(e) => {
                    let n = {
                        let c = st.fails.entry(pid).or_insert(0);
                        *c += 1;
                        *c
                    };
                    if is_transport(&e) {
                        st.offline = true;
                        debug!("nätverksfel, backoff: {e:#}");
                    } else {
                        warn!("synk misslyckades: {e:#}");
                        st.last_error = Some(short(format!("{e:#}")));
                    }
                    Duration::from_secs((30u64 << (n - 1).min(4)).min(600))
                }
            };
            if st.next_due.contains_key(&pid) {
                st.next_due.insert(pid, now + jitter(delay));
            }
        }
        self.publish();
        self.wake.notify_one();
    }

    // ------------------------------------------------------------------ remote poll

    async fn poll_project(self: &Arc<Self>, pid: Pid) -> Result<()> {
        let p = db(&self.ctx, move |s| s.project(pid))
            .await?
            .ok_or_else(|| anyhow!("projektet finns inte"))?;
        if !p.enabled || project_resetting(pid) {
            return Ok(());
        }
        let mut p = self.refresh_tops(p).await?;
        // The undocumented dm/v3 feed is off unless an admin enabled it in settings.json (requires
        // written Autodesk approval). Off: never call dm/v3; drop what the feed left behind and list.
        if !self.ctx.cfg.desktop_sync_feed && p.change_source != ChangeSourceKind::PublicWalk {
            db(&self.ctx, move |s| {
                s.forget_cursor(pid)?;
                s.set_change_source(pid, ChangeSourceKind::PublicWalk)
            })
            .await?;
            p.change_source = ChangeSourceKind::PublicWalk;
            p.cursor = None;
        }
        // The dm/v3 desktop-sync feed is ACC's alone: other providers always list.
        let mut delta =
            if p.change_source == ChangeSourceKind::PublicWalk || p.provider != Provider::Acc {
                self.walk_delta(&p).await?
            } else {
                match syncfeed::poll(&self.ctx.api, &p).await {
                    Ok(d) => {
                        if p.change_source == ChangeSourceKind::Unknown {
                            db(&self.ctx, move |s| {
                                s.set_change_source(pid, ChangeSourceKind::DesktopSync)
                            })
                            .await?;
                        }
                        d
                    }
                    Err(FeedError::NotSupported(code)) => {
                        info!(
                            "{}: desktop-sync ej tillgänglig (HTTP {code}), använder listning",
                            p.name
                        );
                        db(&self.ctx, move |s| {
                            s.set_change_source(pid, ChangeSourceKind::PublicWalk)
                        })
                        .await?;
                        self.walk_delta(&p).await?
                    }
                    Err(FeedError::Api(e)) => return Err(e.into()),
                }
            };
        for u in delta.upserts.iter_mut() {
            u.pid = pid;
        }
        let ctx = self.ctx.clone();
        let p2 = p.clone();
        let work = tokio::task::spawn_blocking(move || -> Result<(usize, Work)> {
            let _guard = local_lock();
            merge_tops(&ctx.store, &p2, &mut delta)?;
            if delta.is_empty() && delta.new_cursor.is_none() {
                return Ok((0, Work::default()));
            }
            keep_pending_local(&ctx.store, pid, &mut delta)?;
            let applied = ctx.store.apply_delta(pid, &delta)?;
            let count = applied.changes.len();
            if count == 0 {
                return Ok((0, Work::default()));
            }
            Ok((count, disk_apply(&ctx, &p2, &applied.changes)))
        })
        .await??;
        let (count, work) = work;
        if count > 0 {
            let from = p.provider.label();
            info!("{}: {count} ändringar från {from}", p.name);
            self.ctx.ui.activity(
                "remote_change",
                &format!("{count} ändringar från {from}"),
                &p.name,
                "",
            );
            self.after_work(work);
        }
        Ok(())
    }

    /// Ask ACC for the project's top folders again when they are unknown or TOPS_EVERY old.
    /// A new or lost top folder drops the feed cursor, so the next poll enumerates everything
    /// the user can now see. An answer with no folders at all keeps the old ones (a glitch must
    /// never empty a project on disk).
    async fn refresh_tops(self: &Arc<Self>, mut p: Project) -> Result<Project> {
        let due = p.tops.is_empty()
            || self
                .st()
                .tops_at
                .get(&p.pid)
                .is_none_or(|t| t.elapsed() >= TOPS_EVERY);
        if !due {
            return Ok(p);
        }
        let found = match remote::top_folders(&self.ctx, &p).await {
            Ok(found) => found,
            Err(e) if !p.tops.is_empty() => {
                debug!(
                    "{}: kunde inte läsa toppmapparna ({e}), behåller de gamla",
                    p.name
                );
                self.st().tops_at.insert(p.pid, Instant::now());
                return Ok(p);
            }
            Err(e) => return Err(e.into()),
        };
        self.st().tops_at.insert(p.pid, Instant::now());
        let tops = unique_tops(found);
        if tops.is_empty() {
            if p.tops.is_empty() {
                return Err(anyhow!("du har inte åtkomst till någon mapp i {}", p.name));
            }
            warn!(
                "{}: ACC visar inga toppmappar längre, behåller de gamla",
                p.name
            );
            return Ok(p);
        }
        if tops == p.tops {
            return Ok(p);
        }
        let ids = |t: &[TopFolder]| t.iter().map(|t| t.id.clone()).collect::<HashSet<String>>();
        let new_ids = ids(&tops) != ids(&p.tops);
        let pid = p.pid;
        let (tops2, had) = (tops.clone(), !p.tops.is_empty());
        db(&self.ctx, move |s| {
            s.set_tops(pid, &tops2)?;
            if new_ids && had {
                s.forget_cursor(pid)?;
            }
            Ok(())
        })
        .await?;
        info!(
            "{}: toppmappar {}",
            p.name,
            tops.iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        p.tops = tops;
        if new_ids && had {
            p.cursor = None;
        }
        Ok(p)
    }

    /// Public-API fallback: list every populated folder and diff against the DB.
    async fn walk_delta(self: &Arc<Self>, p: &Project) -> Result<Delta> {
        let pid = p.pid;
        // Local-only folders (temporary "local-" ids, not yet created in ACC) have nothing
        // to list remotely; asking would fail the whole walk with HTTP 400.
        let folders: Vec<Item> = db(&self.ctx, move |s| s.populated_folders(pid))
            .await?
            .into_iter()
            .filter(|f| !is_local_id(&f.id) && f.local_state != LocalState::New)
            .collect();
        let ctx = self.ctx.clone();
        let results: Vec<Result<Delta>> = futures_util::stream::iter(folders)
            .map(|f| {
                let ctx = ctx.clone();
                let p = p.clone();
                let me = self.clone();
                async move {
                    let mut remote = match remote::folder_contents(&ctx, &p, &f.id).await {
                        Ok(r) => r,
                        // Deleted or no longer visible to us: the parent's listing reports
                        // it missing, which removes it. A top folder goes when the top folders
                        // are asked for again.
                        Err(e) if matches!(e.status(), Some(403 | 404)) => {
                            debug!("{}: mappen {} finns inte längre ({e})", p.name, f.name);
                            if p.top(&f.id).is_some() {
                                me.st().tops_at.remove(&p.pid);
                            }
                            return Ok(Delta::default());
                        }
                        Err(e) => return Err(e.into()),
                    };
                    for r in remote.iter_mut() {
                        r.pid = pid;
                    }
                    let fid = f.id.clone();
                    let local = db(&ctx, move |s| s.children(pid, &fid)).await?;
                    let mut d = Delta::default();
                    diff_listing(&local, remote, &mut d);
                    Ok::<Delta, anyhow::Error>(d)
                }
            })
            .buffer_unordered(4)
            .collect()
            .await;
        let mut out = Delta::default();
        for r in results {
            let d = r?;
            out.upserts.extend(d.upserts);
            out.deletes.extend(d.deletes);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ CF callbacks

    fn on_cf(self: &Arc<Self>, ev: CfEvent) {
        match ev {
            CfEvent::FetchPlaceholders(req) => {
                if ph::decode_identity(&req.identity)
                    .is_some_and(|(_, pid, _)| project_resetting(pid))
                {
                    tokio::spawn(fail_placeholders(req.keys, STATUS_CLOUD_FILE_UNSUCCESSFUL));
                    return;
                }
                let Some(guard) = self.try_maintenance_guard() else {
                    tokio::spawn(fail_placeholders(req.keys, STATUS_CLOUD_FILE_UNSUCCESSFUL));
                    return;
                };
                if let Some((_, pid, _)) = ph::decode_identity(&req.identity) {
                    self.touch(pid);
                }
                let me = self.clone();
                tokio::spawn(async move {
                    let _guard = guard;
                    me.fetch_placeholders(req).await
                });
            }
            CfEvent::CancelFetchPlaceholders => {}
            CfEvent::FetchData(req) => self.start_download(req),
            CfEvent::CancelFetchData {
                file_id,
                transfer_key,
            } => self.cancel_download(file_id, transfer_key),
            CfEvent::Delete(req) => {
                if let Some((_, pid, _)) = ph::decode_identity(&req.identity) {
                    self.touch(pid);
                }
                let me = self.clone();
                self.spawn_maintenance(async move { me.on_delete(req).await });
            }
            CfEvent::Rename(req) => {
                if let Some((_, pid, _)) = ph::decode_identity(&req.identity) {
                    self.touch(pid);
                }
                let me = self.clone();
                self.spawn_maintenance(async move { me.on_rename(req).await });
            }
        }
    }

    fn touch(&self, pid: Pid) {
        self.st().activity.insert(pid, Instant::now());
    }

    async fn fetch_placeholders(self: Arc<Self>, req: FetchPlaceholdersReq) {
        let keys = req.keys;
        match self.build_listing(&req).await {
            Ok(listing) => {
                let Listing {
                    specs,
                    mark,
                    file_versions,
                } = listing;
                let res =
                    tokio::task::spawn_blocking(move || cb::transfer_placeholders(&keys, &specs))
                        .await;
                match res {
                    Ok(Ok((complete, created))) => {
                        // Only a newly created callback placeholder can inherit the listing's
                        // base. An already-existing name may contain old partial bytes.
                        let created_files: Vec<_> = created
                            .into_iter()
                            .filter_map(|index| file_versions.get(index).cloned().flatten())
                            .collect();
                        if !created_files.is_empty() {
                            if let Err(e) = db(&self.ctx, move |s| {
                                for (pid, id, version) in created_files {
                                    s.set_base_if_unset_at_tip(pid, &id, version)?;
                                }
                                Ok(())
                            })
                            .await
                            {
                                warn!("kunde inte spara basversion för ny platshållare: {e:#}");
                            }
                        }
                        if !complete {
                            warn!("vissa filer kunde inte visas; mappen förblir inte färdiglistad och kan hämtas igen");
                        } else {
                            if let Some((pid, id)) = mark {
                                if let Err(e) =
                                    db(&self.ctx, move |s| s.set_populated(pid, &id, true)).await
                                {
                                    warn!("kunde inte markera mapp som listad: {e:#}");
                                }
                            }
                            // A folder kept on this device gets its files, listed or not yet.
                            let dir = req.path.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                if ph::dir_pinned(&dir) {
                                    let files: Vec<PathBuf> = std::fs::read_dir(&dir)
                                        .map(|rd| {
                                            rd.flatten()
                                                .filter(|e| {
                                                    e.file_type().is_ok_and(|t| t.is_file())
                                                })
                                                .map(|e| e.path())
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    hydrate_later(files);
                                }
                            })
                            .await;
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("TRANSFER_PLACEHOLDERS misslyckades: {e:#}");
                        fail_placeholders(keys, STATUS_CLOUD_FILE_UNSUCCESSFUL).await;
                    }
                    Err(e) => {
                        warn!("TRANSFER_PLACEHOLDERS avbröts: {e}");
                        fail_placeholders(keys, STATUS_CLOUD_FILE_UNSUCCESSFUL).await;
                    }
                }
            }
            Err(e) => {
                let status = if is_transport(&e) {
                    self.st().offline = true;
                    STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE
                } else {
                    STATUS_CLOUD_FILE_UNSUCCESSFUL
                };
                warn!("kunde inte lista {}: {e:#}", req.path.display());
                fail_placeholders(keys, status).await;
            }
        }
        self.publish();
    }

    async fn build_listing(self: &Arc<Self>, req: &FetchPlaceholdersReq) -> Result<Listing> {
        if reset::listing_suppressed(&req.path) {
            return Ok(Listing {
                specs: Vec::new(),
                mark: None,
                file_versions: Vec::new(),
            });
        }
        if req.identity.is_empty() {
            // The sync root itself: one directory per hub that has an enabled project.
            let specs = db(&self.ctx, move |s| -> Result<Vec<PlaceholderSpec>> {
                let mut seen = HashSet::new();
                let mut v = Vec::new();
                for p in s.projects()? {
                    if p.enabled && seen.insert(p.hub_id.clone()) {
                        v.push(hub_dir_spec(&p));
                    }
                }
                Ok(v)
            })
            .await?;
            return Ok(Listing {
                specs,
                mark: None,
                file_versions: Vec::new(),
            });
        }
        let (kind, pid, id) =
            ph::decode_identity(&req.identity).ok_or_else(|| anyhow!("okänd identitet"))?;
        if kind == Kind::Hub {
            let hub_id = id.clone();
            let specs = db(&self.ctx, move |s| -> Result<Vec<PlaceholderSpec>> {
                let mut v = Vec::new();
                for p in s.projects()? {
                    if p.enabled && p.hub_id == hub_id {
                        v.push(project_dir_spec(&p));
                    }
                }
                Ok(v)
            })
            .await?;
            return Ok(Listing {
                specs,
                mark: None,
                file_versions: Vec::new(),
            });
        }

        let idc = id.clone();
        let (proj, item, kids_empty) = db(
            &self.ctx,
            move |s| -> Result<(Option<Project>, Option<Item>, bool)> {
                let it = s.item(pid, &idc)?;
                let empty = s.children(pid, &idc)?.is_empty();
                Ok((s.project(pid)?, it, empty))
            },
        )
        .await?;
        let proj = proj.ok_or_else(|| anyhow!("projektet finns inte"))?;
        if !proj.enabled {
            return Ok(Listing {
                specs: Vec::new(),
                mark: None,
                file_versions: Vec::new(),
            });
        }
        let item = item.ok_or_else(|| anyhow!("okänd mapp"))?;
        if item.kind == Kind::ProjectRoot {
            // Ours, not an ACC folder: its children are the top folders.
            let p2 = proj.clone();
            db(&self.ctx, move |s| {
                for row in top_rows(s, &p2)?.0 {
                    s.upsert_item(&row)?;
                }
                Ok(())
            })
            .await?;
        }
        let needs_fetch = item.kind != Kind::ProjectRoot
            && !item.populated
            && (proj.change_source != ChangeSourceKind::DesktopSync || kids_empty);
        if needs_fetch {
            let mut items = remote::folder_contents(&self.ctx, &proj, &id).await?;
            for it in items.iter_mut() {
                it.pid = pid;
            }
            let d = Delta {
                upserts: items,
                ..Default::default()
            };
            db(&self.ctx, move |s| s.apply_delta(pid, &d)).await?;
        }
        let id2 = id.clone();
        let kids = db(&self.ctx, move |s| s.children(pid, &id2)).await?;
        let me = me(&self.ctx);
        let mut seen = HashSet::new();
        let mut specs = Vec::new();
        let mut file_versions = Vec::new();
        for k in kids.iter().filter(|k| k.local_state != LocalState::Deleted) {
            let spec = spec_for(k, me.as_deref());
            if seen.insert(naming::fold_key(&spec.name)) {
                file_versions
                    .push((k.kind == Kind::File).then(|| (k.pid, k.id.clone(), k.remote_version)));
                specs.push(spec);
            }
        }
        Ok(Listing {
            specs,
            mark: Some((pid, id)),
            file_versions,
        })
    }

    fn start_download(self: &Arc<Self>, req: FetchDataReq) {
        if ph::decode_identity(&req.identity).is_some_and(|(_, pid, _)| project_resetting(pid)) {
            tokio::spawn(fail_data(
                req.keys,
                req.required_offset,
                req.required_length,
                STATUS_CLOUD_FILE_UNSUCCESSFUL,
            ));
            return;
        }
        let Some(guard) = self.try_maintenance_guard() else {
            tokio::spawn(fail_data(
                req.keys,
                req.required_offset,
                req.required_length,
                STATUS_CLOUD_FILE_UNSUCCESSFUL,
            ));
            return;
        };
        let file_id = req.keys.file_id;
        let tk = req.keys.transfer_key;
        if let Some((_, pid, _)) = ph::decode_identity(&req.identity) {
            self.touch(pid);
        }
        let me = self.clone();
        let me2 = self.clone();
        let handle = tokio::spawn(async move {
            let _guard = guard;
            me.download(req).await;
            me2.forget_download(file_id, tk);
        });
        self.st()
            .downloads
            .entry(file_id)
            .or_default()
            .push((tk, handle.abort_handle()));
    }

    fn cancel_download(self: &Arc<Self>, file_id: i64, tk: i64) {
        {
            let mut st = self.st();
            let empty = match st.downloads.get_mut(&file_id) {
                Some(v) => {
                    for (k, h) in v.iter() {
                        if *k == tk {
                            h.abort();
                        }
                    }
                    v.retain(|(k, _)| *k != tk);
                    v.is_empty()
                }
                None => false,
            };
            if empty {
                st.downloads.remove(&file_id);
            }
            st.transfers.remove(&tk);
        }
        self.publish();
    }

    fn forget_download(self: &Arc<Self>, file_id: i64, tk: i64) {
        {
            let mut st = self.st();
            let empty = match st.downloads.get_mut(&file_id) {
                Some(v) => {
                    v.retain(|(k, _)| *k != tk);
                    v.is_empty()
                }
                None => false,
            };
            if empty {
                st.downloads.remove(&file_id);
            }
            st.transfers.remove(&tk);
        }
        self.publish();
    }

    fn abort_downloads(&self) {
        let mut st = self.st();
        for (_, v) in st.downloads.drain() {
            for (_, h) in v {
                h.abort();
            }
        }
        st.transfers.clear();
    }

    async fn download(self: Arc<Self>, req: FetchDataReq) {
        let tk = req.keys.transfer_key;
        let Some((_, pid, id)) = ph::decode_identity(&req.identity) else {
            fail_data(
                req.keys,
                req.required_offset,
                req.required_length,
                STATUS_CLOUD_FILE_UNSUCCESSFUL,
            )
            .await;
            return;
        };
        let provider = db(&self.ctx, move |s| s.project(pid))
            .await
            .ok()
            .flatten()
            .map_or(Provider::Acc, |p| p.provider);
        if !self.ctx.signed_in(provider) {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                format!("Logga in på {} för att hämta filen.", provider.label()),
            ));
            fail_data(
                req.keys,
                req.required_offset,
                req.required_length,
                STATUS_CLOUD_FILE_UNSUCCESSFUL,
            )
            .await;
            return;
        }
        let permit = match self.down_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };
        let res = self.stream_file(&req, pid, &id).await;
        drop(permit);
        self.st().transfers.remove(&tk);
        match res {
            Ok(()) => debug!("hämtning klar (transfer {tk})"),
            Err(e) => {
                let status = if is_transport(&e) {
                    self.st().offline = true;
                    STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE
                } else {
                    STATUS_CLOUD_FILE_UNSUCCESSFUL
                };
                warn!("hämtning av {} misslyckades: {e:#}", req.path.display());
                fail_data(req.keys, req.required_offset, req.required_length, status).await;
            }
        }
        self.publish();
    }

    async fn stream_file(self: &Arc<Self>, req: &FetchDataReq, pid: Pid, id: &str) -> Result<()> {
        let idc = id.to_string();
        let refresh_marker = refresh_key(pid, id);
        let (proj, item, mut refresh_pending) = db(
            &self.ctx,
            move |s| -> Result<(Option<Project>, Option<Item>, bool)> {
                Ok((
                    s.project(pid)?,
                    s.item(pid, &idc)?,
                    s.kv_get(&refresh_marker)?.is_some(),
                ))
            },
        )
        .await?;
        let proj = proj.ok_or_else(|| anyhow!("projektet finns inte"))?;
        let mut item = item.ok_or_else(|| anyhow!("okänt objekt"))?;
        if !proj.enabled {
            return Err(anyhow!("projektet är avstängt"));
        }
        // A freshly listed file can be opened just before TRANSFER_PLACEHOLDERS records
        // its base version. Let that short DB stamp finish, without opening a file
        // handle from within this FETCH_DATA callback.
        if !refresh_pending
            && item.local_state == LocalState::Clean
            && item.base_version.is_none()
            && item.remote_version.is_some()
        {
            for wait_ms in [10, 20, 40, 80, 160, 250] {
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                let idc = id.to_string();
                let key = refresh_key(pid, id);
                let (latest, pending) = db(&self.ctx, move |s| {
                    Ok((s.item(pid, &idc)?, s.kv_get(&key)?.is_some()))
                })
                .await?;
                item = latest.ok_or_else(|| anyhow!("objektet togs bort under hämtningen"))?;
                refresh_pending = pending;
                if refresh_pending || item.base_version.is_some() {
                    break;
                }
            }
        }
        if refresh_pending || fetch_would_mix_conflict_versions(&item) {
            if item.local_state == LocalState::Clean {
                self.refresh_soon(pid, id.to_string());
            }
            return Err(anyhow!(
                "filens lagrade byte kan tillhöra en annan ACC-version; väntar på säker uppdatering av platshållaren"
            ));
        }
        let storage = item
            .storage_urn
            .clone()
            .ok_or_else(|| anyhow!("saknar lagringsobjekt"))?;
        let download = remote::download_url(&self.ctx, &proj, id, &storage).await?;
        if download.size > 0 && download.size != req.file_size {
            // The signed download describes a different byte length than the callback's
            // placeholder, even if the database has not caught up yet. Streaming those
            // bytes into the callback's range could corrupt the file. Refresh the
            // placeholder so the next open can use the current size.
            self.refresh_soon(pid, id.to_string());
            return Err(anyhow!(
                "filen har ändrats i ACC ({} → {} byte), öppna den igen",
                req.file_size,
                download.size
            ));
        }

        let keys = req.keys;
        let tk = keys.transfer_key;
        let start = req.required_offset / ALIGN * ALIGN;
        self.st().transfers.insert(
            tk,
            Transfer {
                name: item.name.clone(),
                project: proj.name.clone(),
                direction: "down",
                done: 0,
                total: req.file_size,
            },
        );

        let total = req.file_size;
        // Large files: several byte ranges in parallel (see `segment_plan`). Each segment holds
        // one CHUNK buffer, so memory stays at SEGMENTS × 1 MiB.
        let progress = std::sync::atomic::AtomicU64::new(start);
        let last = std::sync::Mutex::new(None::<Instant>);
        let mut parts = Vec::new();
        for (a, b) in segment_plan(start, total, download.ranges) {
            parts.push(self.stream_segment(&keys, &proj, &download, a, b, total, &progress, &last));
        }
        if parts.is_empty() {
            // Zero-length file (or nothing left): complete with an empty transfer.
            tokio::task::block_in_place(|| cb::transfer_data(&keys, start, &[]))?;
        }
        futures_util::future::try_join_all(parts).await?;
        cb::report_progress(&keys, total, total);

        let rv = item.remote_version;
        let id_owned = id.to_string();
        db(&self.ctx, move |s| {
            s.set_base_if_unset_at_tip(pid, &id_owned, rv)
        })
        .await?;
        self.ctx.ui.activity("download", &item.name, &proj.name, "");
        Ok(())
    }

    /// Stream `[a, b)` of the object into the placeholder. `a` is 4 KiB aligned and `b` is
    /// either aligned too or the end of the file, so every TRANSFER_DATA is valid.
    ///
    /// Without Range support (`!download.ranges`) the object is read from its start and the
    /// first `a` bytes are dropped before any data is transferred. `segment_plan` gives one
    /// segment in that case.
    #[allow(clippy::too_many_arguments)]
    async fn stream_segment(
        self: &Arc<Self>,
        keys: &OpKeys,
        proj: &Project,
        download: &DownloadUrl,
        a: u64,
        b: u64,
        total: u64,
        progress: &std::sync::atomic::AtomicU64,
        last: &std::sync::Mutex<Option<Instant>>,
    ) -> Result<()> {
        use std::sync::atomic::Ordering;
        let mut resp = if download.ranges && (a != 0 || b != total) {
            remote::open_range(&self.ctx, proj, download, a, b).await?
        } else {
            remote::open_download(&self.ctx, proj, download, 0).await?
        };
        let mut skip = if download.ranges { 0 } else { a };
        let tk = keys.transfer_key;
        let mut buf: Vec<u8> = Vec::with_capacity(CHUNK);
        let mut off = a;
        while let Some(chunk) = resp.chunk().await? {
            let mut rest: &[u8] = &chunk;
            if skip > 0 {
                let n = skip.min(rest.len() as u64) as usize;
                rest = &rest[n..];
                skip -= n as u64;
            }
            while !rest.is_empty() {
                let n = (CHUNK - buf.len()).min(rest.len());
                buf.extend_from_slice(&rest[..n]);
                rest = &rest[n..];
                if buf.len() == CHUNK {
                    tokio::task::block_in_place(|| cb::transfer_data(keys, off, &buf))?;
                    off += CHUNK as u64;
                    buf.clear();
                    let done = progress.fetch_add(CHUNK as u64, Ordering::Relaxed) + CHUNK as u64;
                    let mut l = last.lock().unwrap_or_else(|p| p.into_inner());
                    self.report(keys, tk, total, done.min(total), &mut l);
                }
            }
        }
        if !buf.is_empty() {
            tokio::task::block_in_place(|| cb::transfer_data(keys, off, &buf))?;
            off += buf.len() as u64;
            progress.fetch_add(buf.len() as u64, Ordering::Relaxed);
        }
        if off != b {
            return Err(anyhow!(
                "segmentet blev kort: {} av {} byte",
                off - a,
                b - a
            ));
        }
        Ok(())
    }

    fn report(
        self: &Arc<Self>,
        keys: &OpKeys,
        tk: i64,
        total: u64,
        done: u64,
        last: &mut Option<Instant>,
    ) {
        let now = Instant::now();
        if last.is_none_or(|l| now.duration_since(l) >= PROGRESS_GAP) {
            *last = Some(now);
            cb::report_progress(keys, total, done);
            if let Some(e) = self.st().transfers.get_mut(&tk) {
                e.done = done;
                e.total = total;
            }
            self.publish();
        }
    }

    async fn on_delete(self: Arc<Self>, req: DeleteReq) {
        let ctx = self.ctx.clone();
        match tokio::task::spawn_blocking(move || handle_delete(&ctx, &req)).await {
            Ok(w) => self.after_work(w),
            Err(e) => warn!("borttagning avbröts: {e}"),
        }
    }

    async fn on_rename(self: Arc<Self>, req: RenameReq) {
        let ctx = self.ctx.clone();
        match tokio::task::spawn_blocking(move || handle_rename(&ctx, &req)).await {
            Ok(w) => self.after_work(w),
            Err(e) => warn!("namnbyte avbröts: {e}"),
        }
    }

    // ------------------------------------------------------------------ local changes

    fn on_local(self: &Arc<Self>, batch: Vec<LocalChange>) {
        let Some(guard) = self.try_maintenance_guard() else {
            return; // RecoveryGuard schedules a fresh scan once the paths are stable.
        };
        let mut paths = Vec::new();
        let mut rescan = false;
        for c in batch {
            match c {
                LocalChange::Changed(p) => paths.push(p),
                LocalChange::Rescan => rescan = true,
            }
        }
        let ctx = self.ctx.clone();
        let me = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let res = tokio::task::spawn_blocking(move || {
                if rescan {
                    paths.extend(scan_populated(&ctx));
                }
                process_local(&ctx, paths)
            })
            .await;
            match res {
                Ok(w) => me.after_work(w),
                Err(e) => warn!("lokal genomgång avbröts: {e}"),
            }
        });
    }

    /// Apply the side effects of a batch: bump scheduling, kick the outbox, hydrate pinned files.
    fn after_work(self: &Arc<Self>, w: Work) {
        let now = Instant::now();
        {
            let mut st = self.st();
            for pid in &w.touched {
                st.activity.insert(*pid, now);
                st.next_due.insert(*pid, now);
            }
            if w.enqueued {
                st.outbox_next = Some(now);
            }
        }
        hydrate_later(w.hydrate);
        for (pid, id) in w.refresh {
            self.schedule_refresh(pid, id);
        }
        for (pid, id) in w.moves {
            self.schedule_move(pid, id);
        }
        for path in w.local_retry {
            self.schedule_local_retry(path, REFRESH_RETRY);
        }
        self.publish();
        self.wake.notify_one();
    }

    fn schedule_local_retry(self: &Arc<Self>, path: PathBuf, wait: Duration) {
        if !self.st().pending_local_retries.insert(path.clone()) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            // Sleeping inspections neither block recovery/client changes nor
            // accumulate another timer when watcher events repeat for this path.
            tokio::time::sleep(wait).await;
            me.st().pending_local_retries.remove(&path);
            let Some(_work) = me.active_session_work() else {
                // Login and recovery completion perform a fresh local scan.
                return;
            };
            me.on_local(vec![LocalChange::Changed(path)]);
        });
    }

    /// Retry a remote rename or move that an open file blocked, every REFRESH_RETRY, for as
    /// long as it is pending (it survives restarts; `go_online` schedules the stored ones).
    fn schedule_move(self: &Arc<Self>, pid: Pid, id: String) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REFRESH_RETRY).await;
                let Some(_work) = me.active_session_work() else {
                    if me.project_signed_in(pid) {
                        continue; // Recovery pauses persisted remote work; it does not cancel it.
                    }
                    break;
                };
                let (ctx, id2) = (me.ctx.clone(), id.clone());
                let done = tokio::task::spawn_blocking(move || {
                    let _guard = local_lock();
                    retry_move(&ctx, pid, &id2)
                })
                .await
                .unwrap_or(true);
                me.publish();
                if done {
                    break;
                }
            }
        });
    }

    /// Bring a placeholder up to date right away (after the open that found it stale has been
    /// failed), then keep retrying like `schedule_refresh`.
    fn refresh_soon(self: &Arc<Self>, pid: Pid, id: String) {
        self.schedule_refresh_after(pid, id, Duration::from_secs(1));
    }

    /// Retry bringing a placeholder up to the version in the DB once the file is closed
    /// (a remote update that arrived while e.g. Revit had the file open). Pending refreshes
    /// survive restarts and have no attempt limit: a file can be open for days.
    fn schedule_refresh(self: &Arc<Self>, pid: Pid, id: String) {
        self.schedule_refresh_after(pid, id, REFRESH_RETRY);
    }

    fn schedule_refresh_after(self: &Arc<Self>, pid: Pid, id: String, first_wait: Duration) {
        let key = refresh_key(pid, &id);
        {
            let mut st = self.st();
            if let Err(e) = self.ctx.store.kv_set(&key, Some("pending")) {
                warn!("kunde inte spara väntande uppdatering {id}: {e:#}");
            }
            if let Some(again) = st.refreshing.get_mut(&(pid, id.clone())) {
                *again = true;
                return;
            }
            st.refreshing.insert((pid, id.clone()), false);
        }
        let me = self.clone();
        tokio::spawn(async move {
            let mut wait = first_wait;
            loop {
                tokio::time::sleep(wait).await;
                let Some(_work) = me.active_session_work() else {
                    if me.project_signed_in(pid) {
                        continue;
                    }
                    break;
                };
                let ctx = me.ctx.clone();
                let id2 = id.clone();
                let done = tokio::task::spawn_blocking(move || {
                    let _guard = local_lock();
                    refresh_placeholder(&ctx, pid, &id2)
                })
                .await
                .unwrap_or(false);
                me.publish();
                if done {
                    let mut st = me.st();
                    if let Some(again) = st.refreshing.get_mut(&(pid, id.clone())) {
                        if *again {
                            *again = false;
                            wait = Duration::from_secs(1);
                            continue;
                        }
                    }
                    if let Err(e) = me.ctx.store.kv_set(&key, None) {
                        warn!("kunde inte rensa väntande uppdatering {id}: {e:#}");
                    }
                    st.refreshing.remove(&(pid, id.clone()));
                    return;
                }
                wait = REFRESH_RETRY;
            }
            // Sign-out pauses the worker; the stored key restarts it at go_online.
            me.st().refreshing.remove(&(pid, id));
        });
    }

    // ------------------------------------------------------------------ outbox

    fn spawn_outbox(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            // Upload progress lives in upload.rs: publish it while the queue runs (throttled).
            let ticker = {
                let ticking = me.clone();
                tokio::spawn(async move {
                    loop {
                        sleep_until(Instant::now() + Duration::from_millis(500)).await;
                        ticking.publish();
                    }
                })
            };
            let processed = upload::process_due(&me.ctx).await;
            ticker.abort();
            match processed {
                Ok(n) if n > 0 => debug!("kö: {n} jobb behandlade"),
                Ok(_) => {}
                Err(e) => warn!("kö: {e:#}"),
            }
            let dirty = upload::take_dirty_projects();
            // Sleep exactly until the next job is due (a retry, a deferred locked file, a delete
            // grace period); never tick while the queue is empty.
            let next = me.ctx.store.next_job_due().ok().flatten();
            {
                let now = Instant::now();
                let mut st = me.st();
                st.outbox_running = false;
                st.outbox_next = next.map(|due| {
                    let wait = (due - now_ms()).max(0) as u64;
                    now + Duration::from_millis(wait).max(OUTBOX_MIN_GAP)
                });
                for pid in dirty {
                    st.next_due.insert(pid, now);
                }
            }
            me.publish();
            me.wake.notify_one();
        });
    }

    // ------------------------------------------------------------------ tray commands

    async fn on_cmd(
        self: &Arc<Self>,
        cmd: TrayCmd,
        live: &mut Live,
        itx: &UnboundedSender<Internal>,
    ) {
        if live.restart_pending {
            return;
        }
        let Some(_command) = self.begin_command(&cmd) else {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "Vänta tills den pågående återställningen eller flytten är klar.",
            ));
            return;
        };
        match cmd {
            TrayCmd::SignIn {
                provider,
                streambim,
            } => {
                self.sign_in(provider, streambim, live, itx).await;
            }
            TrayCmd::SignInMfa { provider, code } => {
                let generation = live
                    .sign_ins
                    .get(&provider)
                    .filter(|attempt| attempt.mfa_pending)
                    .map(|attempt| attempt.generation);
                let Some(generation) = generation else {
                    self.ctx.ui.send(UiMsg::notify(
                        Severity::Warning,
                        "Ingen kod väntas. Logga in igen.",
                    ));
                    return;
                };
                // The MFA wait's timer gives way to the verification of the code.
                if let Some(wait) = live.sign_ins.remove(&provider) {
                    wait.task.abort();
                    let _ = wait.task.await;
                }
                let me = self.clone();
                let tx = itx.clone();
                let task = tokio::spawn(async move {
                    let result = me
                        .ctx
                        .streambim
                        .verify_mfa(&code.0)
                        .await
                        .map(LoginOutcome::SignedIn);
                    drop(code);
                    let _ = tx.send(Internal::SignedIn {
                        provider,
                        generation,
                        result,
                    });
                    me.wake.notify_one();
                });
                live.sign_ins.insert(
                    provider,
                    SignInAttempt {
                        task,
                        generation,
                        started_at: Instant::now(),
                        mfa_pending: false,
                    },
                );
                self.publish();
            }
            TrayCmd::CancelSignIn { provider } => {
                self.cancel_sign_in(live, provider).await;
                self.ctx.ui.send(UiMsg::SignInProgress {
                    provider,
                    state: "cancelled",
                    error: None,
                });
                self.publish();
            }
            TrayCmd::SignOut { provider } => self.sign_out(provider, live).await,
            TrayCmd::StreamBimRegions => {
                let client = self.ctx.api.client().clone();
                let ui = self.ctx.ui.clone();
                tokio::spawn(async move {
                    let regions = crate::api::streambim::regions(&client).await;
                    ui.send(UiMsg::StreamBimRegions(regions));
                });
            }
            TrayCmd::TestApsClientId {
                client_id,
                request_id,
            } => {
                self.test_client_id(client_id, request_id, live, itx).await;
            }
            TrayCmd::CancelApsClientIdTest { request_id } => {
                if live
                    .client_validation
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
                {
                    self.cancel_sign_in(live, Provider::Acc).await;
                    self.publish();
                } else if !self.ctx.ui.hub.replay_client_validation_result(&request_id) {
                    // A new engine has no live request after a crash/restart. Let the UI
                    // leave its pending state immediately while preserving other tests.
                    self.client_validation_event(&request_id, "cancelled", None);
                }
            }
            TrayCmd::OpenFolder => open_path(&self.ctx.paths.sync_root),
            TrayCmd::OpenPath(path) => open_path(&path),
            TrayCmd::OpenLog => open_path(&self.ctx.paths.log_dir),
            TrayCmd::SyncNow => {
                let now = Instant::now();
                let mut st = self.st();
                let keys: Vec<Pid> = st.next_due.keys().copied().collect();
                for pid in keys {
                    st.next_due.insert(pid, now);
                }
                drop(st);
                self.wake.notify_one();
            }
            TrayCmd::RefreshProjects => {
                let me = self.clone();
                self.spawn_maintenance(async move { me.refresh_projects().await });
            }
            TrayCmd::SetProject {
                hub_id,
                project_id,
                subscribe,
            } => {
                if live.conn.is_none() {
                    self.ctx
                        .ui
                        .send(UiMsg::notify(Severity::Warning, "Logga in först"));
                    return;
                }
                let found = {
                    let list = self.projects.lock().unwrap_or_else(|e| e.into_inner());
                    list.iter()
                        .find(|(h, _)| h.id == hub_id)
                        .and_then(|(h, ps)| {
                            ps.iter()
                                .find(|(p, _)| p.id == project_id)
                                .map(|(p, _)| (h.clone(), p.clone()))
                        })
                };
                match found {
                    // Unsubscribing needs no API, so a signed-out provider's project can be let go.
                    Some((hub, _)) if subscribe && !self.ctx.signed_in(hub.provider) => {
                        self.ctx.ui.send(UiMsg::notify(
                            Severity::Warning,
                            format!("Logga in på {} först.", hub.provider.label()),
                        ));
                    }
                    Some((hub, project)) => {
                        let me = self.clone();
                        self.spawn_maintenance(async move {
                            me.toggle_project(hub, project, subscribe).await
                        });
                    }
                    None => self.ctx.ui.send(UiMsg::notify(
                        Severity::Warning,
                        "Projektet finns inte i listan. Uppdatera listan och försök igen.",
                    )),
                }
            }
            TrayCmd::SetSetting { key, value } if key == "aps_client_id" => {
                if value.as_str().is_none_or(|id| !id.trim().is_empty()) {
                    self.ctx.ui.send(UiMsg::notify(
                        Severity::Warning,
                        "APS Client ID måste testas innan det sparas. Använd Testa och spara.",
                    ));
                    self.publish_settings();
                    return;
                }
                if live.client_validation.is_some() {
                    self.cancel_sign_in(live, Provider::Acc).await;
                }
                if self.set_client_id(&value, live).await {
                    crate::app::request_restart();
                    let _ = itx.send(Internal::Quit);
                }
            }
            TrayCmd::SetSetting { key, value } => self.set_setting(&key, &value).await,
            TrayCmd::FreeUpSpace { project_id } => self.free_up_space(project_id),
            TrayCmd::CheckUpdates => self.check_updates(true),
            TrayCmd::InstallUpdate => {
                let Some(guard) = self.try_maintenance_guard() else {
                    let info = self.st().pending_update.clone();
                    self.ctx.ui.send(UiMsg::Update {
                        available: info.is_some(),
                        version: info.map(|info| info.version),
                        error: Some("Vänta tills återställningen eller flytten är klar.".into()),
                        installing: false,
                    });
                    return;
                };
                let me = self.clone();
                tokio::spawn(async move {
                    let _guard = guard;
                    me.install_update(true).await
                });
            }
            // Answered by the IPC layer from its caches; a window just connected, so make the
            // numbers only it shows fresh.
            TrayCmd::Hello => self.refresh_local_bytes(false),
            TrayCmd::ExportDiagnostics => {
                let ctx = self.ctx.clone();
                tokio::spawn(async move {
                    let c2 = ctx.clone();
                    match tokio::task::spawn_blocking(move || crate::diag::export(&c2)).await {
                        Ok(Ok(path)) => {
                            info!("diagnostik sparad: {}", path.display());
                            crate::diag::reveal(&path);
                            ctx.ui.send(UiMsg::notify(
                                Severity::Info,
                                format!("Diagnostik sparad: {}", path.display()),
                            ));
                        }
                        Ok(Err(e)) => ctx.ui.send(UiMsg::notify(
                            Severity::Error,
                            format!("Kunde inte spara diagnostik: {e:#}"),
                        )),
                        Err(e) => warn!("diagnostik avbröts: {e}"),
                    }
                });
            }
            TrayCmd::Reset { project_id, path } => {
                let me = self.clone();
                self.spawn_maintenance(async move { me.reset(project_id, path).await });
            }
            TrayCmd::ResetAll => {
                let me = self.clone();
                self.spawn_maintenance(async move { me.reset_all().await });
            }
            TrayCmd::MoveSyncRoot { path } => {
                let me = self.clone();
                let quit = itx.clone();
                self.spawn_maintenance(async move { me.move_root(path, quit).await });
            }
            TrayCmd::ConfirmDeletes {
                project_id,
                confirm,
            } => {
                let me = self.clone();
                self.spawn_maintenance(
                    async move { me.confirm_deletes(&project_id, confirm).await },
                );
            }
            TrayCmd::RetryFailed => {
                match db(&self.ctx, |s| s.retry_dead_jobs()).await {
                    Ok(n) => info!("försöker igen med {n} misslyckade jobb"),
                    Err(e) => warn!("kunde inte starta om jobb: {e:#}"),
                }
                self.st().outbox_next = Some(Instant::now());
                self.publish();
                self.wake.notify_one();
            }
            TrayCmd::Shell { verb, paths } => self.on_shell(verb, paths),
            TrayCmd::Quit => {}
        }
    }

    /// Hubs and projects of every signed-in provider. A provider whose hubs cannot be listed
    /// keeps the hubs it had before (one notice); the other providers are listed as usual.
    async fn refresh_projects(self: &Arc<Self>) {
        let previous = self
            .projects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.ctx.ui.send(UiMsg::Projects {
            hubs: previous.clone(),
            loading: true,
        });
        let stored = db(&self.ctx, |s| s.projects()).await.unwrap_or_default();
        let subscribed: HashSet<String> = stored
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.project_id.clone())
            .collect();
        let lists = futures_util::future::join_all(
            Provider::ALL
                .into_iter()
                .filter(|provider| self.ctx.signed_in(*provider))
                .map(|provider| {
                    let subscribed = &subscribed;
                    async move { (provider, self.provider_hubs(provider, subscribed).await) }
                }),
        )
        .await;
        let mut out: HubList = Vec::new();
        for (provider, listed) in lists {
            match listed {
                Ok(hubs) => out.extend(hubs),
                Err(e) => {
                    warn!("kunde inte lista hubbar ({}): {e:#}", provider.label());
                    let text = if provider == Provider::Acc {
                        format!("Kunde inte hämta projekt: {e}")
                    } else {
                        format!("Kunde inte hämta projekt från {}: {e}", provider.label())
                    };
                    self.ctx.ui.send(UiMsg::notify(Severity::Error, text));
                    out.extend(
                        previous
                            .iter()
                            .filter(|(hub, _)| hub.provider == provider)
                            .cloned(),
                    );
                }
            }
        }
        // A signed-out provider cannot be listed. Its subscribed projects still show, so they
        // can be unsubscribed.
        for provider in Provider::ALL {
            if !self.ctx.signed_in(provider) {
                out.extend(offline_hubs(provider, &previous, &stored, &subscribed));
            }
        }
        *self.projects.lock().unwrap_or_else(|e| e.into_inner()) = out.clone();
        self.ctx.ui.send(UiMsg::Projects {
            hubs: out,
            loading: false,
        });
    }

    /// The hubs of one provider with their projects. A hub whose projects cannot be listed
    /// comes with none (logged). `subscribed` holds the ids of the projects in the store.
    async fn provider_hubs(
        &self,
        provider: Provider,
        subscribed: &HashSet<String>,
    ) -> Result<HubList, ApiError> {
        let hubs = remote::list_hubs(&self.ctx, provider).await?;
        let lists = futures_util::future::join_all(
            hubs.iter().map(|h| remote::list_projects(&self.ctx, h)),
        )
        .await;
        Ok(hubs
            .into_iter()
            .zip(lists)
            .map(|(h, l)| match l {
                Ok(ps) => {
                    let v: Vec<(ProjectInfo, bool)> = ps
                        .into_iter()
                        .map(|p| {
                            let s = subscribed.contains(&p.id);
                            (p, s)
                        })
                        .collect();
                    (h, v)
                }
                Err(e) => {
                    warn!("kunde inte lista projekt i hubben {}: {e:#}", h.name);
                    (h, Vec::new())
                }
            })
            .collect())
    }

    async fn toggle_project(self: Arc<Self>, hub: HubInfo, project: ProjectInfo, subscribe: bool) {
        let r = if subscribe {
            self.subscribe(&hub, &project).await
        } else {
            self.unsubscribe(&project.id).await
        };
        if let Err(e) = r {
            warn!("projektändring misslyckades: {e:#}");
            self.ctx.ui.send(UiMsg::notify(
                Severity::Error,
                format!("Kunde inte ändra projektet: {e:#}"),
            ));
        }
        self.refresh_projects().await;
    }

    async fn subscribe(self: &Arc<Self>, hub: &HubInfo, project: &ProjectInfo) -> Result<()> {
        let setup = remote::project_setup(&self.ctx, hub, project).await?;
        let p = Project {
            pid: 0,
            hub_id: hub.id.clone(),
            hub_name: hub.name.clone(),
            project_id: project.id.clone(),
            name: project.name.clone(),
            root_folder_urn: setup.root_folder_urn,
            urn_ns: setup.urn_ns,
            tops: unique_tops(setup.tops),
            oss_scopes: setup.oss_scopes,
            // Only ACC has the desktop-sync feed to probe; the other providers always list.
            change_source: if hub.provider == Provider::Acc {
                ChangeSourceKind::Unknown
            } else {
                ChangeSourceKind::PublicWalk
            },
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: hub.provider,
            meta: setup.meta,
        };
        let pid = db(&self.ctx, move |s| s.upsert_project(&p)).await?;
        self.st().tops_at.insert(pid, Instant::now());
        let p = db(&self.ctx, move |s| s.project(pid))
            .await?
            .ok_or_else(|| anyhow!("projektet försvann"))?;
        let ctx = self.ctx.clone();
        tokio::task::spawn_blocking(move || ensure_project_dirs(&ctx, &p))
            .await
            .map_err(|e| anyhow!("uppgift avbröts: {e}"))??;
        self.schedule_now(pid);
        self.touch(pid);
        info!("prenumererar på {}", project.name);
        self.publish();
        Ok(())
    }

    async fn unsubscribe(self: &Arc<Self>, project_id: &str) -> Result<()> {
        let pid_str = project_id.to_string();
        let found = db(&self.ctx, move |s| -> Result<Option<Project>> {
            Ok(s.projects()?.into_iter().find(|p| p.project_id == pid_str))
        })
        .await?;
        let Some(p) = found else { return Ok(()) };
        let pid = p.pid;
        let ctx = self.ctx.clone();
        let p2 = p.clone();
        let clean = tokio::task::spawn_blocking(move || -> bool {
            let dir = project_dir(&ctx.paths.sync_root, &p2);
            tree_is_clean(&ctx, p2.pid, p2.root_id(), &dir)
        })
        .await
        .map_err(|e| anyhow!("uppgift avbröts: {e}"))?;
        if !clean {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                format!(
                    "Kan inte avsluta synken för {}: det finns osparade lokala ändringar",
                    p.name
                ),
            ));
            return Ok(());
        }
        db(&self.ctx, move |s| s.set_project_enabled(pid, false)).await?;
        {
            let mut st = self.st();
            st.next_due.remove(&pid);
            st.activity.remove(&pid);
            st.fails.remove(&pid);
        }
        let ctx = self.ctx.clone();
        let p3 = p.clone();
        tokio::task::spawn_blocking(move || {
            let root_id = p3.root_id().to_string();
            let dir = project_dir(&ctx.paths.sync_root, &p3);
            let populated = ctx
                .store
                .item(p3.pid, &root_id)
                .ok()
                .flatten()
                .is_some_and(|i| i.populated);
            if dir.symlink_metadata().is_ok() {
                // tree_is_clean() held above, so nothing is expected to be kept.
                let mut kept = Vec::new();
                remove_clean(&dir, populated, &mut kept);
            }
            if let Some(hub_dir) = dir.parent() {
                let _ = std::fs::remove_dir(hub_dir);
            }
        })
        .await
        .map_err(|e| anyhow!("uppgift avbröts: {e}"))?;
        db(&self.ctx, move |s| s.delete_project(pid)).await?;
        info!("avslutade synk för {}", p.name);
        self.publish();
        Ok(())
    }

    /// `manual`: the user asked (always report the result). Automatic checks stay quiet and, with
    /// `auto_update`, schedule an install for the next quiet moment.
    async fn project_by_id(&self, project_id: &str) -> Option<Project> {
        let id = project_id.to_string();
        db(&self.ctx, move |s| {
            Ok(s.projects()?
                .into_iter()
                .find(|p| p.project_id == id && p.enabled))
        })
        .await
        .ok()
        .flatten()
    }

    /// Rebuild a project or one of its folders from ACC (see sync/reset.rs).
    async fn reset(self: Arc<Self>, project_id: Option<String>, path: Option<PathBuf>) {
        let found = match (&project_id, &path) {
            (Some(id), _) => self.project_by_id(id).await,
            (None, Some(path)) => {
                let (ctx, path) = (self.ctx.clone(), path.clone());
                db(&self.ctx, move |s| {
                    let comps = rel_components(&ctx.paths.sync_root, &path).unwrap_or_default();
                    Ok(s.projects()?.into_iter().find(|p| {
                        p.enabled && comps.len() >= 2 && project_matches(p, &comps[0], &comps[1])
                    }))
                })
                .await
                .ok()
                .flatten()
            }
            (None, None) => None,
        };
        let Some(p) = found else {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "Hittar inte projektet att återställa",
            ));
            return;
        };
        if !self.ctx.signed_in(p.provider) || self.st().offline {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "Återställning kräver att du är inloggad och online",
            ));
            return;
        }
        let Some(recovery) = self.begin_recovery() else {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "En återställning eller flytt pågår redan. Försök igen när den är klar.",
            ));
            return;
        };
        self.publish();
        if !self.wait_for_recovery_idle(Duration::from_secs(60)).await {
            self.ctx.ui.send(UiMsg::notify(Severity::Warning, "Kunde inte återställa: synkarbetet blev inte klart i tid. Lokala filer är kvar. Försök igen när överföringarna är klara."));
            return;
        }
        if !self.ctx.signed_in(p.provider) || self.st().offline {
            self.ctx.ui.send(UiMsg::notify(Severity::Warning, "Återställningen avbröts eftersom anslutningen eller inloggningen ändrades. Dina lokala filer finns kvar."));
            return;
        }
        let ctx = self.ctx.clone();
        let (p2, path2) = (p.clone(), path.clone());
        let begun =
            tokio::task::spawn_blocking(move || reset::begin(&ctx, &p2, path2.as_deref())).await;
        let set = match begun {
            Ok(Ok(set)) => set,
            Ok(Err(e)) => {
                warn!("återställning av {} misslyckades: {e:#}", p.name);
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Warning,
                    format!("Kunde inte återställa: {e:#}"),
                ));
                if e.is::<reset::IncompleteReset>() {
                    self.st().incomplete_recoveries.push(e);
                }
                return;
            }
            Err(e) => {
                warn!("återställning avbröts: {e}");
                return;
            }
        };
        drop(recovery);
        info!("{}: {} byggs om från ACC", p.name, set.name);
        self.ctx.ui.activity("reset", &set.name, &p.name, "");
        self.schedule_now(p.pid);
        self.touch(p.pid);
        self.publish();
        self.finish_reset(set, p.name.clone()).await;
    }

    /// Phase 2 of a reset (or a leftover from an interrupted one): save what was not synced,
    /// then remove the set-aside copy.
    async fn finish_reset(self: &Arc<Self>, set: reset::SetAside, project: String) {
        let Some(aside) = set.aside.clone() else {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Info,
                format!("{} byggs om från ACC.", set.name),
            ));
            return;
        };
        let ctx = self.ctx.clone();
        let name = set.name.clone();
        let (saved, dest) = tokio::task::spawn_blocking(move || {
            let r = reset::finish(&ctx, &aside, &name);
            drop(set.guard);
            r
        })
        .await
        .unwrap_or((0, None));
        let msg = match &dest {
            Some(d) if saved > 0 => {
                crate::diag::reveal(d);
                format!("{} byggs om från ACC. {saved} lokala ändringar som inte var synkade sparades i {}.", set.name, d.display())
            }
            _ => format!("{} byggs om från ACC.", set.name),
        };
        info!("{project}: {msg}");
        self.ctx.ui.send(UiMsg::notify(Severity::Info, msg));
    }

    /// Rebuild every project from ACC where it is (the reset tool for everything).
    async fn reset_all(self: Arc<Self>) {
        let ids: Vec<String> = db(&self.ctx, |s| {
            Ok(s.projects()?
                .into_iter()
                .filter(|p| p.enabled)
                .map(|p| p.project_id)
                .collect())
        })
        .await
        .unwrap_or_default();
        if ids.is_empty() {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Info,
                "Det finns inga projekt att bygga om",
            ));
        }
        for id in ids {
            self.clone().reset(Some(id), None).await;
        }
    }

    /// Move the sync root to `to` and rebuild everything there (see relocate.rs). Ends with
    /// an engine restart; a failure preserves local work in the original or set-aside tree.
    async fn move_root(self: Arc<Self>, to: PathBuf, quit: UnboundedSender<Internal>) {
        let Some(_recovery) = self.begin_recovery() else {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "En återställning eller flytt pågår redan. Försök igen när den är klar.",
            ));
            return;
        };
        let current = self.ctx.paths.sync_root.clone();
        let to2 = to.clone();
        let checked = tokio::task::spawn_blocking(move || relocate::check(&current, &to2)).await;
        let check_error = match checked {
            Ok(Ok(())) => None,
            Ok(Err(message)) => Some(message),
            Err(error) => Some(format!("kontrollen av den nya platsen avbröts: {error}")),
        };
        if let Some(message) = check_error {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                format!("Synkmappen flyttades inte: {message}"),
            ));
            return;
        }
        {
            let mut st = self.st();
            if st.moving {
                return;
            }
            st.moving = true;
        }
        info!("flyttar synkmappen till {}", to.display());
        self.publish();
        if !self.wait_for_recovery_idle(Duration::from_secs(60)).await {
            self.st().moving = false;
            self.ctx.ui.send(UiMsg::notify(Severity::Warning, "Synkmappen flyttades inte: synkarbetet blev inte klart i tid. Lokala filer är kvar. Försök igen när överföringarna är klara."));
            return;
        }
        self.abort_downloads();
        let ctx = self.ctx.clone();
        let to2 = to.clone();
        let moved = match tokio::task::spawn_blocking(move || relocate::set_aside(&ctx, &to2)).await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                warn!("synkmappen flyttades inte: {e:#}");
                self.st().moving = false;
                self.publish();
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Warning,
                    format!("Synkmappen flyttades inte: {e:#}"),
                ));
                if e.is::<reset::IncompleteReset>() {
                    self.st().incomplete_recoveries.push(e);
                }
                return;
            }
            Err(e) => {
                warn!("flytten avbröts: {e}");
                self.st().moving = false;
                self.publish();
                return;
            }
        };
        let ctx = self.ctx.clone();
        let (saved, dests) = tokio::task::spawn_blocking(move || {
            let mut saved = 0;
            let mut dests = Vec::new();
            for (aside, hub) in &moved.asides {
                let (n, dest) = reset::finish(&ctx, aside, hub);
                saved += n;
                dests.extend(dest);
            }
            // Projects stay stopped until the process exits: their rows are gone here.
            std::mem::forget(moved.guard);
            (saved, dests)
        })
        .await
        .unwrap_or((0, Vec::new()));
        let mut msg = format!(
            "Synkmappen flyttas till {}. Alla projekt byggs om där från ACC.",
            to.display()
        );
        if saved > 0 {
            if let Some(d) = dests.first() {
                crate::diag::reveal(d);
                msg.push_str(&format!(
                    " {saved} lokala ändringar som inte var synkade sparades i {}.",
                    d.display()
                ));
            }
        }
        info!("{msg}");
        self.ctx.ui.send(UiMsg::notify(Severity::Info, msg));
        crate::app::request_restart();
        let _ = quit.send(Internal::Quit);
    }

    async fn confirm_deletes(self: Arc<Self>, project_id: &str, confirm: bool) {
        let Some(p) = self.project_by_id(project_id).await else {
            return;
        };
        if confirm {
            match db(&self.ctx, move |s| s.release_deletes(p.pid)).await {
                Ok(n) => info!("{}: {n} borttagningar bekräftade", p.name),
                Err(e) => warn!("kunde inte bekräfta borttagningar: {e:#}"),
            }
            self.st().outbox_next = Some(Instant::now());
        } else {
            let ctx = self.ctx.clone();
            let p2 = p.clone();
            match tokio::task::spawn_blocking(move || restore_held(&ctx, &p2)).await {
                Ok(Ok(n)) => {
                    info!("{}: {n} borttagna objekt återställda", p.name);
                    self.ctx
                        .ui
                        .activity("restore", &format!("{n} objekt"), &p.name, "");
                }
                Ok(Err(e)) => warn!("kunde inte återställa borttagningar: {e:#}"),
                Err(e) => warn!("återställning avbröts: {e}"),
            }
        }
        self.publish();
        self.wake.notify_one();
    }

    fn check_updates(self: &Arc<Self>, manual: bool) {
        if self.ctx.cfg.update_channel.trim().is_empty() {
            if manual {
                self.ctx.ui.send(UiMsg::Update {
                    available: false,
                    version: None,
                    error: Some(UPDATES_OFF.into()),
                    installing: false,
                });
            }
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let r = update::check(me.ctx.api.client(), &me.ctx.cfg.update_channel).await;
            match r {
                Ok(Some(info)) => {
                    info!("uppdatering {} finns", info.version);
                    me.ctx.ui.send(UiMsg::Update {
                        available: true,
                        version: Some(info.version.clone()),
                        error: None,
                        installing: false,
                    });
                    let mut st = me.st();
                    st.pending_update = Some(info);
                    if me.ctx.cfg_now().auto_update && st.auto_install_at.is_none() {
                        st.auto_install_at = Some(Instant::now());
                    }
                }
                Ok(None) => {
                    let mut st = me.st();
                    st.pending_update = None;
                    st.auto_install_at = None;
                    drop(st);
                    me.ctx.ui.send(UiMsg::Update {
                        available: false,
                        version: None,
                        error: None,
                        installing: false,
                    });
                    if manual {
                        me.ctx
                            .ui
                            .send(UiMsg::notify(Severity::Info, "Du har senaste versionen"));
                    }
                }
                Err(e) => {
                    warn!("uppdateringskontroll misslyckades: {e:#}");
                    let mut st = me.st();
                    st.pending_update = None;
                    st.auto_install_at = None;
                    drop(st);
                    let msg = short(format!("{e:#}"));
                    me.ctx.ui.send(UiMsg::Update {
                        available: false,
                        version: None,
                        error: Some(msg),
                        installing: false,
                    });
                    if manual {
                        me.ctx.ui.send(UiMsg::notify(
                            Severity::Error,
                            "Kunde inte kontrollera uppdateringar",
                        ));
                    }
                }
            }
            me.wake.notify_one();
        });
    }

    /// Nothing in flight and no window open: an update (which restarts the engine and closes
    /// the window) interrupts nobody.
    fn is_quiet(&self) -> bool {
        let pending = self.ctx.store.job_counts().map(|c| c.0).unwrap_or(1);
        let st = self.st();
        st.polling.is_empty()
            && st.downloads.is_empty()
            && !st.outbox_running
            && !st.recovering
            && !st.moving
            && st.maintenance_running == 0
            && !st.installing
            && pending == 0
            && upload::active_transfers().is_empty()
            && self.ctx.ui.hub.client_count() == 0
    }

    /// Automatic install, once the engine is quiet (checked every AUTO_INSTALL_RETRY).
    fn maybe_auto_install(self: &Arc<Self>, now: Instant) {
        let due = matches!(self.st().auto_install_at, Some(t) if t <= now);
        if !due {
            return;
        }
        if !self.ctx.cfg_now().auto_update || self.st().pending_update.is_none() {
            self.st().auto_install_at = None;
            return;
        }
        if !self.is_quiet() {
            self.st().auto_install_at = Some(now + AUTO_INSTALL_RETRY);
            return;
        }
        let Some(guard) = self.try_maintenance_guard() else {
            self.st().auto_install_at = Some(now + AUTO_INSTALL_RETRY);
            return;
        };
        self.st().auto_install_at = None;
        let me = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            me.install_update(false).await
        });
    }

    async fn install_update(self: Arc<Self>, manual: bool) {
        if self.ctx.cfg.update_channel.trim().is_empty() {
            let mut st = self.st();
            st.pending_update = None;
            st.auto_install_at = None;
            drop(st);
            self.ctx.ui.send(UiMsg::Update {
                available: false,
                version: None,
                error: Some(UPDATES_OFF.into()),
                installing: false,
            });
            return;
        }
        // Re-read the channel just before installation. A release can be withdrawn
        // after the daily check, and a cached URL must not install it later.
        let info = match update::check(self.ctx.api.client(), &self.ctx.cfg.update_channel).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                let mut st = self.st();
                st.pending_update = None;
                st.auto_install_at = None;
                drop(st);
                self.ctx.ui.send(UiMsg::Update {
                    available: false,
                    version: None,
                    error: None,
                    installing: false,
                });
                if manual {
                    self.ctx
                        .ui
                        .send(UiMsg::notify(Severity::Info, "Ingen uppdatering finns"));
                }
                return;
            }
            Err(e) => {
                warn!("uppdateringskontroll före installation misslyckades: {e:#}");
                if !manual {
                    self.st().auto_install_at = Some(Instant::now() + AUTO_INSTALL_RETRY);
                } else {
                    self.ctx.ui.send(UiMsg::notify(
                        Severity::Error,
                        "Kunde inte kontrollera uppdateringen",
                    ));
                }
                return;
            }
        };
        {
            let mut st = self.st();
            if st.installing {
                return;
            }
            st.installing = true;
            st.pending_update = Some(info.clone());
        }
        info!("installerar uppdatering {}", info.version);
        let version = Some(info.version.clone());
        self.ctx.ui.send(UiMsg::Update {
            available: true,
            version: version.clone(),
            error: None,
            installing: true,
        });
        // On success Windows stops this process and restarts it on the new version (see
        // main.rs, RegisterApplicationRestart), so this only returns on failure.
        let r = update::install(self.ctx.api.client(), &info).await;
        self.st().installing = false;
        if let Err(e) = r {
            warn!("uppdatering misslyckades: {e:#}");
            let msg = short(format!("{e:#}"));
            self.ctx.ui.send(UiMsg::Update {
                available: true,
                version,
                error: Some(msg),
                installing: false,
            });
            if manual {
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Error,
                    format!("Uppdateringen misslyckades: {e:#}"),
                ));
            }
        }
    }

    // ------------------------------------------------------------------ status

    fn set_error(self: &Arc<Self>, msg: String) {
        self.st().last_error = Some(short(msg));
        self.publish();
    }

    fn clear_error(&self) {
        self.st().last_error = None;
    }

    /// Throttled status push (<= 2 per second). A trailing push is scheduled when throttled.
    fn publish(self: &Arc<Self>) {
        let now = Instant::now();
        let delay = {
            let mut st = self.st();
            match st.last_status {
                Some(last) if now.duration_since(last) < STATUS_GAP => {
                    if st.flush_scheduled {
                        return;
                    }
                    st.flush_scheduled = true;
                    Some(last + STATUS_GAP)
                }
                _ => None,
            }
        };
        match delay {
            Some(at) => {
                let me = self.clone();
                tokio::spawn(async move {
                    sleep_until(at).await;
                    me.st().flush_scheduled = false;
                    me.publish_now();
                });
            }
            None => self.publish_now(),
        }
    }

    fn publish_now(&self) {
        let (pending, failed) = self.ctx.store.job_counts().unwrap_or((0, 0));
        let deferred_remote_update =
            !pending_refreshes(&self.ctx.store).is_empty() || !pending_moves(&self.ctx).is_empty();
        let blocked_pinned_update = self
            .ctx
            .store
            .kv_prefix(PIN_UPDATE_WARNING_KEY)
            .is_ok_and(|entries| !entries.is_empty());
        let held_deletes: Vec<HeldDeletes> = self
            .ctx
            .store
            .held_deletes()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(pid, count)| {
                let p = self.ctx.store.project(pid).ok()??;
                Some(HeldDeletes {
                    project_id: p.project_id,
                    project: p.name,
                    count,
                })
            })
            .collect();
        let accounts = self.account_statuses();
        let user = status_user(&accounts);
        let signed_in = accounts.iter().any(|a| a.signed_in);
        let last_synced = if signed_in {
            self.last_synced_ms()
        } else {
            None
        };
        let (state, text, error, transfers) = {
            let mut st = self.st();
            st.last_status = Some(Instant::now());
            let mut transfers: Vec<Transfer> = st.transfers.values().cloned().collect();
            transfers.extend(upload::active_transfers());
            let active = !st.polling.is_empty()
                || !st.downloads.is_empty()
                || st.outbox_running
                || !transfers.is_empty();
            let (state, text) = if st.moving {
                ("syncing", "Flyttar synkmappen…".to_string())
            } else if st.recovering {
                ("syncing", "Förbereder återställning…".to_string())
            } else if !st.incomplete_recoveries.is_empty() {
                (
                    "error",
                    "Starta om Synka för att slutföra återställningen.".to_string(),
                )
            } else if !signed_in {
                let text = if st.signing_in.is_empty() {
                    "Logga in"
                } else {
                    "Väntar på inloggning i webbläsaren…"
                };
                ("signed_out", text.to_string())
            } else if let Some(e) = &st.last_error {
                ("error", format!("Fel: {e}"))
            } else if st.offline {
                let now = Instant::now();
                let retry = st
                    .next_due
                    .iter()
                    .filter(|(pid, _)| !st.polling.contains(pid))
                    .map(|(_, due)| due.saturating_duration_since(now).as_secs())
                    .min();
                ("offline", offline_text(retry))
            } else {
                match sync_phase(
                    active,
                    pending,
                    failed,
                    deferred_remote_update,
                    blocked_pinned_update,
                ) {
                    SyncPhase::Active => {
                        let down = transfers.iter().filter(|t| t.direction == "down").count();
                        let up = transfers.iter().filter(|t| t.direction == "up").count();
                        ("syncing", syncing_text(down, up))
                    }
                    SyncPhase::Waiting => (
                        "waiting",
                        if pending > 0 {
                            waiting_text(pending)
                        } else {
                            "Väntar på synk".to_string()
                        },
                    ),
                    SyncPhase::NeedsAttention => ("attention", "Behöver ses över".to_string()),
                    SyncPhase::UpToDate => {
                        ("synced", synced_text(last_synced, chrono::Local::now()))
                    }
                }
            };
            (state, text, st.last_error.clone(), transfers)
        };
        self.ctx.ui.send(UiMsg::Status(Status {
            user,
            text,
            state,
            error,
            pending_jobs: pending,
            failed_jobs: failed,
            transfers,
            sync_root: self.ctx.paths.sync_root.display().to_string(),
            held_deletes,
            local_bytes: self.st().local_bytes,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            accounts,
        }));
    }

    /// One entry per provider, in account order (see IPC.md `status.accounts`).
    fn account_statuses(&self) -> Vec<AccountStatus> {
        let signing: HashSet<Provider> = self.st().signing_in.clone();
        Provider::ALL
            .into_iter()
            .map(|provider| {
                let signed_in = self.ctx.signed_in(provider);
                AccountStatus {
                    provider,
                    label: provider.label(),
                    configured: match provider {
                        Provider::Acc => self.ctx.auth.is_configured(),
                        Provider::StreamBim => true,
                        Provider::Interaxo => self.ctx.interaxo.is_configured(),
                    },
                    signed_in,
                    signing_in: signing.contains(&provider),
                    user: if signed_in {
                        self.ctx.user_for(provider)
                    } else {
                        None
                    },
                    host: match provider {
                        Provider::StreamBim => self.ctx.streambim.host(),
                        _ => None,
                    },
                }
            })
            .collect()
    }

    /// Latest successful poll: this process, or the newest `last_sync_ms` stored for an enabled
    /// project (written when a delta is applied, so it can lag behind quiet polls).
    fn last_synced_ms(&self) -> Option<i64> {
        let mine = self.st().last_ok_ms;
        let stored = self.ctx.store.projects().ok().and_then(|ps| {
            ps.iter()
                .filter(|p| p.enabled)
                .filter_map(|p| p.last_sync_ms)
                .max()
        });
        mine.into_iter().chain(stored).max()
    }

    /// Publishes the `settings` event from the current settings.
    fn publish_settings(&self) {
        let s = self.ctx.cfg_now();
        let view = SettingsView {
            propagate_deletes: s.propagate_deletes,
            max_downloads: s.max_downloads,
            max_uploads: s.max_uploads,
            poll_active_secs: s.poll_active_secs,
            log_level: s.log_level.clone(),
            start_at_login: start_at_login_enabled(self.ctx.paths.packaged),
            auto_update: s.auto_update,
            sync_root: self.ctx.paths.sync_root.display().to_string(),
            client_id_configured: s.client_id_configured(),
            aps_client_id: s.explicit_client_id_override().map(str::to_string),
            aps_client_id_override_supported: true,
            aps_client_id_validation_supported: true,
            update_channel_configured: !s.update_channel.trim().is_empty(),
            providers: Provider::ALL.into_iter().map(Provider::as_str).collect(),
            interaxo_client_id: s
                .interaxo_client_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            interaxo_redirect_uri: s.interaxo_client().redirect_uri,
            interaxo_redirect_uri_default: INTERAXO_DEFAULT_REDIRECT,
            interaxo_client_secret_set: s
                .interaxo_client_secret_protected
                .as_deref()
                .map(str::trim)
                .is_some_and(|blob| !blob.is_empty()),
        };
        self.ctx.ui.send(UiMsg::Settings(view));
    }

    fn client_id_change_busy(&self) -> bool {
        let busy = {
            let st = self.st();
            !st.polling.is_empty()
                || !st.downloads.is_empty()
                || !st.transfers.is_empty()
                || st.outbox_running
                || st.moving
                || st.installing
                || st.maintenance_running != 0
        };
        busy || self.free_up_sem.available_permits() == 0
            || !upload::active_transfers().is_empty()
            || self.ctx.store.projects().map_or(true, |projects| {
                projects
                    .iter()
                    .any(|project| project_resetting(project.pid))
            })
    }

    fn client_validation_event(
        &self,
        request_id: &str,
        state: &'static str,
        error: Option<String>,
    ) {
        self.ctx.ui.send(UiMsg::ApsClientIdValidation {
            request_id: request_id.into(),
            state,
            error,
        });
    }

    fn client_validation_guard(&self, live: &Live) -> std::result::Result<(), String> {
        if self.ctx.auth.is_signed_in() || live.conn.is_some() {
            Err("Logga ut innan du testar ett APS Client ID. Den sparade inställningen ändrades inte.".into())
        } else if self.client_id_change_busy() {
            Err("Vänta tills pågående synk, återställning och uppdatering är klara. Den sparade inställningen ändrades inte.".into())
        } else {
            Ok(())
        }
    }

    async fn test_client_id(
        self: &Arc<Self>,
        client_id: String,
        request_id: String,
        live: &mut Live,
        itx: &UnboundedSender<Internal>,
    ) {
        let client_id = client_id.trim().to_string();
        if let Some(pending) = &live.client_validation {
            if pending.request_id == request_id {
                if pending.client_id == client_id {
                    // A reconnect may retry a request whose acknowledgement was lost.
                    self.client_validation_event(&request_id, "testing", None);
                } else {
                    self.cancel_sign_in(live, Provider::Acc).await;
                    self.client_validation_event(
                        &request_id,
                        "failed",
                        Some(
                            "Återanvänd inte ett test-ID för ett annat APS Client ID. Testa igen."
                                .into(),
                        ),
                    );
                    self.publish();
                }
                return;
            }
        }
        if live.sign_ins.get(&Provider::Acc).is_some_and(|attempt| {
            attempt.started_at.elapsed() < SIGN_IN_BURST && !attempt.task.is_finished()
        }) {
            self.client_validation_event(&request_id, "failed", Some(
                "En inloggning eller ett APS-test startades just. Vänta ett ögonblick eller avbryt först.".into()));
            return;
        }
        let result =
            crate::config::check_setting("aps_client_id", &json!(client_id)).and_then(|()| {
                if client_id.is_empty() {
                    Err("Ange ett APS Client ID att testa.".into())
                } else {
                    self.client_validation_guard(live)
                }
            });
        if let Err(error) = result {
            self.client_validation_event(&request_id, "failed", Some(error));
            return;
        }
        // Retrying must release the previous callback listener before the next flow binds.
        self.cancel_sign_in(live, Provider::Acc).await;
        if let Err(error) = self.client_validation_guard(live) {
            self.client_validation_event(&request_id, "failed", Some(error));
            self.publish();
            return;
        }
        let original = self.ctx.cfg_now();
        live.client_validation = Some(ClientIdValidation {
            request_id: request_id.clone(),
            client_id: client_id.clone(),
            original_override: original.explicit_client_id_override().map(str::to_string),
            original_effective_id: original.client_id(),
        });
        let generation = next_sign_in_generation(live);
        self.st().signing_in.insert(Provider::Acc);
        self.client_validation_event(&request_id, "testing", None);
        self.publish();
        let http = self.ctx.api.client().clone();
        let tx = itx.clone();
        let task = tokio::spawn(async move {
            let result = crate::auth::Auth::validate_client_id(client_id, http).await;
            let _ = tx.send(Internal::ClientIdValidated {
                generation,
                request_id,
                result,
            });
        });
        live.sign_ins.insert(
            Provider::Acc,
            SignInAttempt {
                task,
                generation,
                started_at: Instant::now(),
                mfa_pending: false,
            },
        );
    }

    async fn on_client_id_validated(
        self: &Arc<Self>,
        generation: u64,
        request_id: &str,
        result: Result<()>,
        live: &mut Live,
    ) {
        if live.restart_pending
            || live
                .sign_ins
                .get(&Provider::Acc)
                .is_none_or(|attempt| attempt.generation != generation)
            || live
                .client_validation
                .as_ref()
                .is_none_or(|p| p.request_id != request_id)
        {
            return;
        }
        let pending = live
            .client_validation
            .take()
            .expect("matched pending validation");
        live.sign_ins.remove(&Provider::Acc);
        self.st().signing_in.remove(&Provider::Acc);
        let result = match result {
            Ok(()) => self.client_validation_guard(live),
            Err(error) => Err(format!("APS-testet misslyckades: {error:#}")),
        };
        let current = self.ctx.cfg_now();
        let result = result.and_then(|()| {
            if current.explicit_client_id_override() != pending.original_override.as_deref()
                || current.client_id() != pending.original_effective_id
            {
                Err("APS-inställningen ändrades medan testet pågick. Testa igen.".into())
            } else {
                Ok(())
            }
        });
        let result = match result {
            Ok(()) => self
                .commit_client_id(&json!(pending.client_id), live)
                .await
                .map(|_| ()),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => self.client_validation_event(request_id, "saved", None),
            Err(error) => self.client_validation_event(request_id, "failed", Some(error)),
        }
        self.publish();
    }

    /// Returns whether production must restart. Tests can exercise this transition
    /// without setting the process-wide restart flag or changing project data.
    async fn set_client_id(self: &Arc<Self>, value: &Value, live: &mut Live) -> bool {
        self.commit_client_id(value, live).await.unwrap_or(false)
    }

    /// Private commit gate: called only after a successful test or to reset to defaults.
    async fn commit_client_id(
        self: &Arc<Self>,
        value: &Value,
        live: &mut Live,
    ) -> std::result::Result<bool, String> {
        if live.restart_pending {
            return Err("Synka startar redan om. Försök igen efter omstarten.".into());
        }
        let old = self.ctx.cfg_now();
        let mut next = old.clone();
        if let Err(error) = next.apply_setting("aps_client_id", value) {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                format!("Kunde inte ändra APS Client ID: {error}"),
            ));
            self.publish_settings();
            return Err("Ogiltigt APS Client ID. Inställningen ändrades inte.".into());
        }
        let changed = next.client_id() != old.client_id();
        if changed {
            if self.ctx.auth.is_signed_in() || live.conn.is_some() {
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Warning,
                    "Logga ut innan du ändrar APS Client ID.",
                ));
                self.publish_settings();
                return Err("Logga ut innan du ändrar APS Client ID.".into());
            }
            if self.client_id_change_busy() {
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Warning,
                    "Vänta tills pågående synk, återställning och uppdatering är klara innan du ändrar APS Client ID.",
                ));
                self.publish_settings();
                return Err(
                    "Vänta tills pågående arbete är klart innan du ändrar APS Client ID.".into(),
                );
            }

            self.cancel_sign_in(live, Provider::Acc).await;
            self.publish();
            // The previous worker may have finished committing a complete session
            // just before cancellation. It still requires an explicit sign-out.
            if self.ctx.auth.is_signed_in() || live.conn.is_some() {
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Warning,
                    "Inloggningen hann slutföras. Logga ut innan du ändrar APS Client ID.",
                ));
                self.publish_settings();
                self.publish();
                return Err(
                    "Inloggningen hann slutföras. Logga ut innan du ändrar APS Client ID.".into(),
                );
            }
            if self.client_id_change_busy() {
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Warning,
                    "Pågående arbete måste slutföras innan du ändrar APS Client ID. Försök igen när appen är klar.",
                ));
                self.publish_settings();
                return Err("Pågående arbete måste slutföras. APS Client ID ändrades inte.".into());
            }
            if let Err(error) = self.ctx.auth.sign_out().await {
                warn!("kunde inte rensa tidigare APS-inloggning: {error:#}");
                self.ctx.ui.send(UiMsg::notify(
                    Severity::Error,
                    "Kunde inte rensa tidigare APS-inloggning. APS Client ID ändrades inte.",
                ));
                self.publish_settings();
                return Err(
                    "Kunde inte rensa tidigare APS-inloggning. APS Client ID ändrades inte.".into(),
                );
            }
        }

        if let Err(error) = next.save(&self.ctx.paths.state_dir.join("settings.json")) {
            warn!("kunde inte spara APS-inställning: {error:#}");
            self.ctx.ui.send(UiMsg::notify(
                Severity::Error,
                "Kunde inte spara APS Client ID. Inställningen ändrades inte. Försök igen.",
            ));
            self.publish_settings();
            return Err(
                "Kunde inte spara APS Client ID. Inställningen ändrades inte. Försök igen.".into(),
            );
        }
        *self.ctx.settings.write().unwrap_or_else(|e| e.into_inner()) = next;
        info!("APS Client ID-inställning sparad");
        self.publish_settings();
        if changed {
            live.restart_pending = true;
            self.ctx.ui.send(UiMsg::notify(
                Severity::Info,
                "APS Client ID sparat. Synka startar om.",
            ));
        }
        Ok(changed)
    }

    /// Applies a validated `set_setting`, saves settings.json and republishes `settings`.
    /// max_downloads, max_uploads and log_level take effect after the next engine start.
    async fn set_setting(self: &Arc<Self>, key: &str, value: &Value) {
        if matches!(
            key,
            "interaxo_client_id" | "interaxo_redirect_uri" | "interaxo_client_secret"
        ) {
            self.set_interaxo_setting(key, value);
            return;
        }
        if key == "start_at_login" {
            let on = value.as_bool().unwrap_or(false);
            let packaged = self.ctx.paths.packaged;
            match tokio::task::spawn_blocking(move || set_start_at_login(packaged, on)).await {
                Ok(Ok(())) => info!("start vid inloggning: {on}"),
                Ok(Err(e)) => warn!("start vid inloggning misslyckades: {e:#}"),
                Err(e) => warn!("start vid inloggning avbröts: {e}"),
            }
        } else {
            let saved = {
                let mut s = self.ctx.settings.write().unwrap_or_else(|e| e.into_inner());
                match s.apply_setting(key, value) {
                    Ok(()) => Ok(s.clone()),
                    Err(e) => Err(e),
                }
            };
            match saved {
                Ok(cfg) => {
                    if let Err(e) = cfg.save(&self.ctx.paths.state_dir.join("settings.json")) {
                        warn!("kunde inte spara inställningar: {e:#}");
                    }
                    info!("inställning {key} ändrad");
                    if key == "auto_update" && cfg.auto_update && self.st().pending_update.is_some()
                    {
                        self.st().auto_install_at = Some(Instant::now());
                    }
                    if matches!(
                        key,
                        "propagate_deletes" | "poll_active_secs" | "auto_update"
                    ) {
                        self.wake.notify_one();
                    }
                }
                Err(e) => warn!("inställning {key}: {e}"),
            }
        }
        self.publish_settings();
    }

    /// The Interaxo client (id, redirect URI, secret) is saved only while Interaxo is signed out:
    /// the account keeps tokens for the client it was signed in with.
    fn set_interaxo_setting(self: &Arc<Self>, key: &str, value: &Value) {
        let busy = self.ctx.signed_in(Provider::Interaxo)
            || self.st().signing_in.contains(&Provider::Interaxo);
        if busy {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "Logga ut från Interaxo innan du ändrar klientinställningarna.",
            ));
            self.publish_settings();
            return;
        }
        let saved = {
            let mut s = self.ctx.settings.write().unwrap_or_else(|e| e.into_inner());
            match s.apply_setting(key, value) {
                Ok(()) => Ok(s.clone()),
                Err(e) => Err(e),
            }
        };
        match saved {
            Ok(cfg) => {
                if let Err(e) = cfg.save(&self.ctx.paths.state_dir.join("settings.json")) {
                    warn!("kunde inte spara inställningar: {e:#}");
                    self.ctx.ui.send(UiMsg::notify(
                        Severity::Error,
                        "Kunde inte spara Interaxo-inställningen. Försök igen.",
                    ));
                }
                self.ctx.interaxo.set_interaxo_client(cfg.interaxo_client());
                info!("inställning {key} ändrad");
            }
            Err(e) => warn!("inställning {key}: {e}"),
        }
        self.publish_settings();
        self.publish();
    }

    /// Frees clean local copies and clears offline pinning in the selected scope.
    fn free_up_space(self: &Arc<Self>, project_id: Option<String>) {
        let Some(guard) = self.try_maintenance_guard() else {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "Kunde inte frigöra plats: vänta tills den pågående återställningen eller flytten är klar.",
            ));
            return;
        };
        let Ok(permit) = self.free_up_sem.clone().try_acquire_owned() else {
            self.ctx.ui.send(UiMsg::notify(
                Severity::Warning,
                "Frigör utrymme pågår redan. Vänta tills det är klart.",
            ));
            return;
        };
        let me = self.clone();
        tokio::spawn(async move {
            let ctx = me.ctx.clone();
            let res = tokio::task::spawn_blocking(move || {
                // Keep the permit with the blocking scan: aborting the outer async task does
                // not stop a blocking task that has already started.
                let _permit = permit;
                let _guard = guard;
                free_up(&ctx, project_id.as_deref())
            })
            .await;
            match res {
                Ok(Ok((count, project))) => {
                    me.refresh_local_bytes(true);
                    info!("frigjorde plats: {count} filer");
                    me.ctx
                        .ui
                        .activity("free_up", &format!("{count} filer"), &project, "");
                    me.ctx.ui.send(UiMsg::notify(
                        Severity::Info,
                        format!("Frigjorde plats: {count} filer lokalt"),
                    ));
                }
                Ok(Err(e)) => {
                    warn!("frigör plats misslyckades: {e:#}");
                    me.ctx.ui.send(UiMsg::notify(
                        Severity::Error,
                        format!("Kunde inte frigöra plats: {e:#}"),
                    ));
                }
                Err(e) => {
                    warn!("frigör plats avbröts: {e}");
                    me.ctx.ui.send(UiMsg::notify(
                        Severity::Error,
                        "Kunde inte frigöra plats: arbetet avbröts",
                    ));
                }
            }
            me.publish();
        });
    }
}

/// Result of a FETCH_PLACEHOLDERS: the entries to create and the folder to mark populated.
struct Listing {
    specs: Vec<PlaceholderSpec>,
    mark: Option<(Pid, String)>,
    /// Parallel to specs. We stamp only successfully created file placeholders.
    file_versions: Vec<Option<(Pid, String, Option<i64>)>>,
}

// ---------------------------------------------------------------------- helpers (async/DB)

/// Run a store call on the blocking pool.
async fn db<T: Send + 'static>(
    ctx: &CtxRef,
    f: impl FnOnce(&Store) -> Result<T> + Send + 'static,
) -> Result<T> {
    let store = ctx.store.clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|e| anyhow!("databasuppgift avbröts: {e}"))?
}

async fn fail_placeholders(keys: OpKeys, status: i32) {
    let _ = tokio::task::spawn_blocking(move || cb::fail_placeholders(&keys, status)).await;
}

async fn fail_data(keys: OpKeys, offset: u64, length: u64, status: i32) {
    let _ = tokio::task::spawn_blocking(move || cb::fail_data(&keys, offset, length, status)).await;
}

fn is_transport(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::Transport(_)))
}

fn short(s: String) -> String {
    s.chars().take(120).collect()
}

fn jitter(d: Duration) -> Duration {
    let b: [u8; 4] = random_bytes();
    let f = u32::from_le_bytes(b) as f64 / u32::MAX as f64;
    d.mul_f64(0.9 + 0.2 * f)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = getrandom::fill(&mut b);
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Temporary id for a local-only item until the upload job gives it a remote id.
fn local_id() -> String {
    let b: [u8; 8] = random_bytes();
    format!("local-{}", hex(&b))
}

/// Idle time from GetLastInputInfo, in milliseconds.
fn idle_ms() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut lii).as_bool() {
            return 0;
        }
        let now = GetTickCount64() as u32;
        now.wrapping_sub(lii.dwTime) as u64
    }
}

/// Poll interval tiers: idle (> 30 min) 720 s; active (activity < 10 min) poll_active_secs;
/// recent (< 1 h) 120 s; else 300 s. The bool is true for the idle tier.
fn interval_secs(poll_active_secs: u64, st: &St, pid: Pid) -> (Duration, bool) {
    if idle_ms() > IDLE_MS {
        return (Duration::from_secs(720), true);
    }
    let d = match st.activity.get(&pid).map(|t| t.elapsed()) {
        Some(e) if e < ACTIVE_WINDOW => Duration::from_secs(poll_active_secs.max(5)),
        Some(e) if e < RECENT_WINDOW => Duration::from_secs(120),
        _ => Duration::from_secs(300),
    };
    (d, false)
}

/// Swedish status line for the synced state: "Senast synkad 14:32".
fn synced_text(last_ms: Option<i64>, now: chrono::DateTime<chrono::Local>) -> String {
    use chrono::TimeZone;
    let Some(at) = last_ms.and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single()) else {
        return "Senast synkad: aldrig".into();
    };
    let hm = at.format("%H:%M");
    let day = at.date_naive();
    let today = now.date_naive();
    if day == today {
        format!("Senast synkad {hm}")
    } else if day.succ_opt() == Some(today) {
        format!("Senast synkad i går {hm}")
    } else {
        format!("Senast synkad {} {hm}", at.format("%Y-%m-%d"))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SyncPhase {
    Active,
    Waiting,
    NeedsAttention,
    UpToDate,
}

fn sync_phase(
    active: bool,
    pending: i64,
    failed: i64,
    deferred_remote_update: bool,
    blocked_pinned_update: bool,
) -> SyncPhase {
    if active {
        SyncPhase::Active
    } else if blocked_pinned_update {
        SyncPhase::NeedsAttention
    } else if pending > 0 || deferred_remote_update {
        SyncPhase::Waiting
    } else if failed > 0 {
        SyncPhase::NeedsAttention
    } else {
        SyncPhase::UpToDate
    }
}

/// Swedish status line while transfers run: "Hämtar 2 filer, laddar upp 1".
fn syncing_text(down: usize, up: usize) -> String {
    let mut parts = Vec::new();
    if down > 0 {
        parts.push(format!(
            "Hämtar {down} {}",
            if down == 1 { "fil" } else { "filer" }
        ));
    }
    if up > 0 {
        parts.push(format!("laddar upp {up}"));
    }
    if !parts.is_empty() {
        return parts.join(", ");
    }
    "Synkar…".into()
}

fn waiting_text(pending: i64) -> String {
    format!(
        "{pending} {} väntar",
        if pending == 1 {
            "ändring"
        } else {
            "ändringar"
        }
    )
}

/// Swedish status line while offline: "Försöker igen om 40 s" when a retry is scheduled.
fn offline_text(retry_in_secs: Option<u64>) -> String {
    match retry_in_secs {
        Some(n) if n > 0 => format!("Försöker igen om {n} s"),
        _ => "Ingen anslutning till ACC".into(),
    }
}

/// Bytes of file data on disk in the populated folders of every enabled project. Uses the
/// directory listing only (attributes and sizes from FindNextFile): nothing is opened, so
/// nothing is hydrated.
fn local_bytes(ctx: &Ctx) -> u64 {
    use std::os::windows::fs::MetadataExt;
    const NOT_LOCAL: u32 = 0x0040_0000 | 0x0000_1000; // RECALL_ON_DATA_ACCESS | OFFLINE
    let mut total = 0u64;
    for p in ctx
        .store
        .projects()
        .unwrap_or_default()
        .iter()
        .filter(|p| p.enabled)
    {
        // Listing an unpopulated directory would ask us to populate it: walk populated ones only.
        if !ctx
            .store
            .item(p.pid, p.root_id())
            .ok()
            .flatten()
            .is_some_and(|r| r.populated)
        {
            continue;
        }
        let mut stack = vec![(
            project_dir(&ctx.paths.sync_root, p),
            p.root_id().to_string(),
        )];
        while let Some((dir, id)) = stack.pop() {
            let Ok(kids) = ctx.store.children(p.pid, &id) else {
                continue;
            };
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let Ok(md) = e.metadata() else { continue };
                if md.is_dir() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if let Some(k) = kids
                        .iter()
                        .find(|k| k.kind.is_dir() && naming::matches_local(&k.name, &name))
                    {
                        if k.populated {
                            stack.push((e.path(), k.id.clone()));
                        }
                    }
                } else if md.file_attributes() & NOT_LOCAL == 0 {
                    total += md.len();
                }
            }
        }
    }
    total
}

#[derive(Debug, PartialEq, Eq)]
enum FreeUpAction {
    Skip,
    ProtectDirty,
    Unpin,
    Dehydrate,
    UnpinAndDehydrate,
}

fn free_up_action(clean: bool, hydrated: bool, pin_state: PinState) -> FreeUpAction {
    if !clean {
        // A dirty file that inherits a folder pin needs its own pin before that folder is
        // unpinned. Keep all of its local bytes for the pending upload.
        return if pin_state == PinState::Inherit {
            FreeUpAction::ProtectDirty
        } else {
            FreeUpAction::Skip
        };
    }
    let needs_unpin = !matches!(pin_state, PinState::Unpinned | PinState::Excluded);
    match (needs_unpin, hydrated) {
        (true, true) => FreeUpAction::UnpinAndDehydrate,
        (true, false) => FreeUpAction::Unpin,
        (false, true) if !matches!(pin_state, PinState::Pinned | PinState::Inherit) => {
            FreeUpAction::Dehydrate
        }
        _ => FreeUpAction::Skip,
    }
}

fn free_up_directory_identity_matches(
    root: &Path,
    path: &Path,
    identity: &[u8],
    kind: Kind,
    pid: Pid,
    id: &str,
) -> bool {
    let Some(parts) = rel_components(root, path) else {
        return false;
    };
    let depth_matches = match kind {
        Kind::Hub => parts.len() == 1,
        Kind::ProjectRoot => parts.len() == 2,
        Kind::Folder => parts.len() >= 3,
        Kind::File => false,
    };
    depth_matches
        && ph::decode_identity(identity).is_some_and(|(actual_kind, actual_pid, actual_id)| {
            (actual_kind, actual_pid) == (kind, pid) && actual_id == id
        })
}

/// Reject path redirection at every component, including a plain leaf below a
/// replaced directory. Cloud Files placeholders are reparse points too: only
/// actual symlinks/junctions are rejected, not the reparse attribute itself.
fn local_path_is_safe(root: &Path, path: &Path) -> bool {
    let Some(parts) = rel_components(root, path) else {
        return false;
    };
    if parts.is_empty() {
        return false;
    }
    let mut current = root.to_path_buf();
    let last = parts.len() - 1;
    for (index, part) in parts.into_iter().enumerate() {
        if part == "." || part == ".." {
            return false;
        }
        current.push(part);
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        if (index != last && !metadata.is_dir())
            || metadata.file_type().is_symlink()
            || std::fs::read_link(&current).is_ok()
        {
            return false;
        }
    }
    true
}

/// Check every component before read_dir: a replaced junction or symlink must not redirect the
/// scan outside the sync root, even if the final path is a valid placeholder elsewhere.
fn free_up_directory_path_is_safe(root: &Path, path: &Path) -> bool {
    local_path_is_safe(root, path)
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn free_up_directory_is_owned(root: &Path, path: &Path, kind: Kind, pid: Pid, id: &str) -> bool {
    free_up_directory_path_is_safe(root, path)
        && ph::placeholder_info(path)
            .ok()
            .flatten()
            .is_some_and(|info| {
                free_up_directory_identity_matches(root, path, &info.identity, kind, pid, id)
            })
}

fn remember_free_up_directory(
    directories: &mut HashMap<PathBuf, (Kind, Pid, String)>,
    path: PathBuf,
    kind: Kind,
    pid: Pid,
    id: &str,
) -> bool {
    let expected = (kind, pid, id.to_string());
    match directories.get(&path) {
        Some(existing) => existing == &expected,
        None => {
            directories.insert(path, expected);
            true
        }
    }
}

fn remember_free_up_scope_roots(
    directories: &mut HashMap<PathBuf, (Kind, Pid, String)>,
    project: &Project,
    project_dir: &Path,
    include_shared_hub: bool,
) -> bool {
    let mut safe = remember_free_up_directory(
        directories,
        project_dir.to_path_buf(),
        Kind::ProjectRoot,
        project.pid,
        project.root_id(),
    );
    if include_shared_hub {
        if let Some(hub) = project_dir.parent() {
            safe &= remember_free_up_directory(
                directories,
                hub.to_path_buf(),
                Kind::Hub,
                0,
                &project.hub_id,
            );
        }
    }
    for top in &project.tops {
        safe &= remember_free_up_directory(
            directories,
            project_dir.join(sanitize_name(&top.name)),
            Kind::Folder,
            project.pid,
            &top.id,
        );
    }
    safe
}

/// A file may be changed only when its CFAPI identity names the live database row at this path.
/// An unknown or copied placeholder can still report local modifications and an inherited pin.
fn free_up_file_item(
    ctx: &Ctx,
    project: &Project,
    path: &Path,
    info: &PlaceholderInfo,
) -> Option<Item> {
    let (kind, pid, id) = ph::decode_identity(&info.identity)?;
    if kind != Kind::File || pid != project.pid || !at_item_path(ctx, project, &id, path) {
        return None;
    }
    ctx.store
        .item(pid, &id)
        .ok()
        .flatten()
        .filter(|item| item.kind == Kind::File && item.pid == pid && item.id == id)
}

fn free_up_retry_allowed(item: &Item, info: &PlaceholderInfo) -> bool {
    can_replace_remote_content(item, info) && info.pin_state != PinState::Pinned
}

/// Frees clean, hydrated files and removes offline pinning from known files and folders in the
/// selected project (or all projects). Unopened cloud folders are not enumerated. A project-
/// scoped action leaves its shared hub untouched, because that can contain other projects.
/// Returns (files freed, project name when only one was selected).
fn free_up(ctx: &Ctx, project_id: Option<&str>) -> Result<(usize, String)> {
    let mut freed = 0usize;
    let mut names = Vec::new();
    let mut directories = HashMap::new();
    let mut projects = HashMap::new();
    let mut retry_dehydrate = Vec::new();
    let mut can_unpin_directories = true;
    let selected_projects: Vec<_> = ctx
        .store
        .projects()?
        .into_iter()
        .filter(|p| p.enabled && project_id.is_none_or(|id| p.project_id == id))
        .collect();
    if project_id.is_some() && selected_projects.is_empty() {
        return Err(anyhow!("projektet finns inte eller synkas inte längre"));
    }
    for p in selected_projects {
        names.push(p.name.clone());
        projects.insert(p.pid, p.clone());
        let dir = project_dir(&ctx.paths.sync_root, &p);
        can_unpin_directories &=
            remember_free_up_scope_roots(&mut directories, &p, &dir, project_id.is_none());
        for folder in ctx.store.populated_folders(p.pid)? {
            let Some(parts) = ctx.store.path_of(p.pid, &folder.id)? else {
                continue;
            };
            let mut d = dir.clone();
            for part in &parts {
                d.push(sanitize_name(part));
            }
            if !free_up_directory_is_owned(
                &ctx.paths.sync_root,
                &d,
                Kind::Folder,
                p.pid,
                &folder.id,
            ) {
                can_unpin_directories = false;
                continue;
            }
            can_unpin_directories &= remember_free_up_directory(
                &mut directories,
                d.clone(),
                Kind::Folder,
                p.pid,
                &folder.id,
            );
            let entries = match std::fs::read_dir(&d) {
                Ok(entries) => entries,
                Err(_) => {
                    can_unpin_directories = false;
                    continue;
                }
            };
            for entry in entries {
                let Ok(e) = entry else {
                    can_unpin_directories = false;
                    continue;
                };
                let path = e.path();
                let Ok(kind) = e.file_type() else {
                    can_unpin_directories = false;
                    continue;
                };
                if kind.is_dir() {
                    if !free_up_directory_path_is_safe(&ctx.paths.sync_root, &path) {
                        can_unpin_directories = false;
                        continue;
                    }
                    match ph::placeholder_info(&path) {
                        Ok(Some(info)) => match ph::decode_identity(&info.identity) {
                            Some((Kind::Folder, pid, id))
                                if pid == p.pid
                                    && at_item_path(ctx, &p, &id, &path)
                                    && ctx
                                        .store
                                        .item(pid, &id)
                                        .ok()
                                        .flatten()
                                        .is_some_and(|item| item.kind == Kind::Folder) =>
                            {
                                can_unpin_directories &= remember_free_up_directory(
                                    &mut directories,
                                    path,
                                    Kind::Folder,
                                    pid,
                                    &id,
                                );
                            }
                            _ => can_unpin_directories = false,
                        },
                        // Its descendants were not scanned; keep ancestor pins unchanged.
                        Ok(None) => can_unpin_directories = false,
                        Err(_) => can_unpin_directories = false,
                    }
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                match ph::placeholder_info(&path) {
                    Ok(Some(info)) => {
                        let Some(item) = free_up_file_item(ctx, &p, &path, &info) else {
                            warn!("hoppar över främmande platshållare {}", path.display());
                            can_unpin_directories = false;
                            continue;
                        };
                        let clean = can_replace_remote_content(&item, &info);
                        let action = free_up_action(clean, info.hydrated, info.pin_state);
                        if action == FreeUpAction::ProtectDirty {
                            if let Err(error) = ph::pin(&path) {
                                warn!(
                                    "kunde inte skydda osynkad fil {}: {error:#}",
                                    path.display()
                                );
                                can_unpin_directories = false;
                            }
                            continue;
                        }
                        if matches!(
                            action,
                            FreeUpAction::Unpin | FreeUpAction::UnpinAndDehydrate
                        ) {
                            if let Err(error) = ph::unpin(&path) {
                                warn!(
                                    "kunde inte avmarkera behåll lokalt {}: {error:#}",
                                    path.display()
                                );
                                continue;
                            }
                        }
                        if matches!(
                            action,
                            FreeUpAction::Dehydrate | FreeUpAction::UnpinAndDehydrate
                        ) {
                            match ph::dehydrate(&path) {
                                Ok(()) => freed += 1,
                                Err(error) => {
                                    // A pinned ancestor can still block this first attempt.
                                    retry_dehydrate.push((p.pid, path.clone(), error.to_string()));
                                }
                            }
                        }
                    }
                    Ok(None) => {} // Local-only file; its bytes are not a cloud placeholder.
                    Err(error) => {
                        warn!(
                            "kunde inte läsa platshållaren {}: {error:#}",
                            path.display()
                        );
                        can_unpin_directories = false;
                    }
                }
            }
        }
    }
    let mut directories_ready = can_unpin_directories;
    if directories_ready {
        // Children first: dirty files have been protected before removing inherited folder pins.
        let mut directories: Vec<_> = directories.into_iter().collect();
        directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (path, (kind, pid, id)) in directories {
            if !free_up_directory_is_owned(&ctx.paths.sync_root, &path, kind, pid, &id) {
                warn!("hoppar över osäker synkmapp {}", path.display());
                directories_ready = false;
                break;
            }
            match ph::placeholder_info(&path) {
                Ok(Some(info))
                    if !matches!(info.pin_state, PinState::Unpinned | PinState::Excluded) =>
                {
                    if let Err(error) = ph::unpin(&path) {
                        warn!(
                            "kunde inte avmarkera behåll lokalt {}: {error:#}",
                            path.display()
                        );
                        directories_ready = false;
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    warn!("kunde inte läsa mappen {}: {error:#}", path.display());
                    directories_ready = false;
                    break;
                }
            }
        }
    }
    for (pid, path, first_error) in retry_dehydrate {
        if !directories_ready {
            warn!("kunde inte frigöra {}: {first_error}", path.display());
            continue;
        }
        let Some(project) = projects.get(&pid) else {
            continue;
        };
        let Ok(Some(info)) = ph::placeholder_info(&path) else {
            continue;
        };
        let Some(item) = free_up_file_item(ctx, project, &path, &info) else {
            continue;
        };
        if !free_up_retry_allowed(&item, &info) {
            continue;
        }
        if !info.hydrated {
            // CfSetPinState may have released the ranges before our explicit call.
            freed += 1;
            continue;
        }
        match ph::dehydrate(&path) {
            Ok(()) => freed += 1,
            Err(error) => warn!("kunde inte frigöra {}: {error:#}", path.display()),
        }
    }
    let project = if names.len() == 1 {
        names.remove(0)
    } else {
        String::new()
    };
    Ok((freed, project))
}

const STARTUP_TASK_ENABLED: [i32; 2] = [2, 4]; // StartupTaskState::Enabled, EnabledByPolicy

/// StartupTask "SynkaStartup" state. Unpackaged builds have no StartupTask: always false.
pub fn start_at_login_enabled(packaged: bool) -> bool {
    use windows::ApplicationModel::StartupTask;
    if !packaged {
        return false;
    }
    let Ok(op) = StartupTask::GetAsync(&windows::core::HSTRING::from(STARTUP_TASK_ID)) else {
        return false;
    };
    match op.join().and_then(|t| t.State()) {
        Ok(state) => STARTUP_TASK_ENABLED.contains(&state.0),
        Err(_) => false,
    }
}

/// Enables or disables the StartupTask. Unpackaged: ignored.
pub fn set_start_at_login(packaged: bool, on: bool) -> Result<()> {
    use windows::ApplicationModel::{StartupTask, StartupTaskState};
    if !packaged {
        return Ok(());
    }
    let task = StartupTask::GetAsync(&windows::core::HSTRING::from(STARTUP_TASK_ID))?.join()?;
    if on {
        let state = task.RequestEnableAsync()?.join()?;
        if state != StartupTaskState::Enabled && state != StartupTaskState::EnabledByPolicy {
            return Err(anyhow!(
                "Windows tillät inte start vid inloggning ({})",
                state.0
            ));
        }
    } else {
        task.Disable()?;
    }
    Ok(())
}

fn open_path(path: &Path) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let op: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

// ---------------------------------------------------------------------- naming

pub(crate) fn project_matches(p: &Project, hub_comp: &str, project_comp: &str) -> bool {
    naming::matches_local(&p.hub_name, hub_comp) && naming::matches_local(&p.name, project_comp)
}

pub(crate) fn project_dir(root: &Path, p: &Project) -> PathBuf {
    root.join(sanitize_name(&p.hub_name))
        .join(sanitize_name(&p.name))
}

/// Components of `path` below the sync root (case-insensitive prefix), or None if outside.
/// A `\\?\` (verbatim) prefix on either side is ignored.
pub(crate) fn rel_components(root: &Path, path: &Path) -> Option<Vec<String>> {
    fn plain(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        match s.strip_prefix(r"\\?\") {
            Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
            _ => p.to_path_buf(),
        }
    }
    let (root, path) = (plain(root), plain(path));
    let mut comps = path.components();
    for rc in root.components() {
        let c = comps.next()?;
        if !naming::same_name(
            &c.as_os_str().to_string_lossy(),
            &rc.as_os_str().to_string_lossy(),
        ) {
            return None;
        }
    }
    Some(
        comps
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect(),
    )
}

/// Display name of the signed-in user, to tell "locked by me" from "locked by someone else".
pub(crate) fn me(ctx: &Ctx) -> Option<String> {
    ctx.auth
        .user()
        .map(|u| u.name)
        .filter(|n| !n.trim().is_empty())
}

/// Locked in ACC by someone other than the signed-in user: the local copy is read-only. ACC
/// reports the holder by display name, so that is what is compared.
pub(crate) fn locked_by_other(it: &Item, me: Option<&str>) -> bool {
    match (&it.locked_by, me) {
        (None, _) => false,
        (Some(by), Some(me)) => !naming::same_name(by.trim(), me.trim()),
        (Some(_), None) => true,
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn attributes_of(path: &Path) -> u32 {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .map(|m| m.file_attributes())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------- placeholders

fn spec_for(it: &Item, me: Option<&str>) -> PlaceholderSpec {
    let is_dir = it.kind.is_dir();
    PlaceholderSpec {
        name: sanitize_name(&it.name),
        is_dir,
        size: if is_dir { 0 } else { it.size.max(0) as u64 },
        ctime_ms: it.ctime_ms,
        mtime_ms: it.mtime_ms,
        identity: ph::encode_identity(it.kind, it.pid, &it.id),
        // Cloud Files in-sync is independent of whether a directory is only partly
        // populated. A clean ACC folder can still fetch children on demand; leaving
        // every folder out of sync makes Explorer show a permanent pending state.
        in_sync: it.local_state == LocalState::Clean,
        on_demand: is_dir,
        read_only: !is_dir && locked_by_other(it, me),
    }
}

/// Local-only grouping directory for a hub. In sync (nothing to upload for it) and fully
/// populated by us (its children are the subscribed projects).
fn hub_dir_spec(p: &Project) -> PlaceholderSpec {
    PlaceholderSpec {
        name: sanitize_name(&p.hub_name),
        is_dir: true,
        size: 0,
        ctime_ms: now_ms(),
        mtime_ms: now_ms(),
        identity: ph::encode_identity(Kind::Hub, 0, &p.hub_id),
        in_sync: true,
        on_demand: false,
        read_only: false,
    }
}

fn project_dir_spec(p: &Project) -> PlaceholderSpec {
    PlaceholderSpec {
        name: sanitize_name(&p.name),
        is_dir: true,
        size: 0,
        ctime_ms: now_ms(),
        mtime_ms: now_ms(),
        identity: ph::encode_identity(Kind::ProjectRoot, p.pid, p.root_id()),
        in_sync: true,
        on_demand: true,
        read_only: false,
    }
}

/// Create hub and project placeholders for one project if they are missing.
fn ensure_project_dirs(ctx: &Ctx, p: &Project) -> Result<()> {
    let root = ctx.paths.sync_root.clone();
    std::fs::create_dir_all(&root)?;
    let hub_name = sanitize_name(&p.hub_name);
    let hub_dir = root.join(&hub_name);
    if !hub_dir.is_dir() {
        ph::create_placeholders(&root, &[hub_dir_spec(p)])?;
    }
    if !project_dir(&root, p).is_dir() {
        ph::create_placeholders(&hub_dir, &[project_dir_spec(p)])?;
    }
    if ctx.store.item(p.pid, p.root_id())?.is_none() {
        let now = now_ms();
        ctx.store.upsert_item(&Item {
            pid: p.pid,
            id: p.root_id().to_string(),
            parent: None,
            name: p.name.clone(),
            kind: Kind::ProjectRoot,
            remote_version: None,
            base_version: None,
            size: 0,
            ctime_ms: now,
            mtime_ms: now,
            storage_urn: None,
            version_urn: None,
            modified_by: None,
            locked_by: None,
            local_state: LocalState::Clean,
            populated: false,
        })?;
    }
    Ok(())
}

/// The user `status.user` shows: the first signed-in account in account order.
pub(crate) fn status_user(accounts: &[AccountStatus]) -> Option<UserInfo> {
    accounts
        .iter()
        .find(|a| a.signed_in)
        .and_then(|a| a.user.clone())
}

/// The id the sync root is registered under: the stored one, else the ACC user id (an existing
/// ACC install keeps its registration), else `<provider>:<user id>` of the first other provider.
pub(crate) fn root_uid(
    stored: Option<String>,
    acc_user: Option<&UserInfo>,
    first_other: Option<(Provider, &UserInfo)>,
) -> Option<String> {
    if let Some(uid) = stored.filter(|uid| !uid.trim().is_empty()) {
        return Some(uid);
    }
    if let Some(user) = acc_user {
        return Some(user.user_id.clone());
    }
    first_other.map(|(provider, user)| format!("{}:{}", provider.as_str(), user.user_id))
}

/// The hubs of a provider that is signed out: the hubs listed before, with their current
/// subscription flags, plus every subscribed project of that provider missing from them.
fn offline_hubs(
    provider: Provider,
    previous: &HubList,
    stored: &[Project],
    subscribed: &HashSet<String>,
) -> HubList {
    let mut hubs: HubList = previous
        .iter()
        .filter(|(hub, _)| hub.provider == provider)
        .map(|(hub, projects)| {
            let projects = projects
                .iter()
                .map(|(p, _)| (p.clone(), subscribed.contains(&p.id)))
                .collect();
            (hub.clone(), projects)
        })
        .collect();
    for project in stored
        .iter()
        .filter(|p| p.enabled && p.provider == provider)
    {
        let listed = hubs
            .iter()
            .any(|(_, ps)| ps.iter().any(|(p, _)| p.id == project.project_id));
        if listed {
            continue;
        }
        let info = ProjectInfo {
            id: project.project_id.clone(),
            hub_id: project.hub_id.clone(),
            name: project.name.clone(),
            root_folder_urn: None,
            scopes: Vec::new(),
            provider,
            meta: project.meta.clone(),
        };
        match hubs.iter_mut().find(|(hub, _)| hub.id == project.hub_id) {
            Some((_, ps)) => ps.push((info, true)),
            None => hubs.push((
                HubInfo {
                    id: project.hub_id.clone(),
                    name: project.hub_name.clone(),
                    region: None,
                    provider,
                },
                vec![(info, true)],
            )),
        }
    }
    hubs
}

/// A new attempt generation (never reused, so a stale completion cannot match a newer attempt).
fn next_sign_in_generation(live: &mut Live) -> u64 {
    live.sign_in_generation = live.sign_in_generation.wrapping_add(1);
    live.sign_in_generation
}

/// Ends a StreamBIM MFA wait after MFA_WAIT.
fn mfa_timer(
    provider: Provider,
    generation: u64,
    itx: &UnboundedSender<Internal>,
) -> tokio::task::JoinHandle<()> {
    let tx = itx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(MFA_WAIT).await;
        let _ = tx.send(Internal::SignInExpired {
            provider,
            generation,
        });
    })
}

/// Hub and project dirs of one provider's enabled projects (those with a current layout).
fn ensure_provider_dirs(ctx: &Ctx, provider: Provider) -> Result<Vec<Pid>> {
    let mut pids = Vec::new();
    for p in ctx.store.projects()? {
        if p.enabled && p.provider == provider && !ctx.store.old_layout(p.pid)? {
            if let Err(e) = ensure_project_dirs(ctx, &p) {
                warn!("kunde inte skapa mappar för {}: {e:#}", p.name);
            }
            pids.push(p.pid);
        }
    }
    Ok(pids)
}

/// Hub and project dirs for every enabled project. Returns the projects to poll, and the
/// project ids whose rows predate top folders (rebuilt by the caller, not polled before).
fn ensure_all_dirs(ctx: &Ctx) -> Result<(Vec<Pid>, Vec<String>)> {
    let mut pids = Vec::new();
    let mut relayout = Vec::new();
    for p in ctx.store.projects()? {
        if p.enabled {
            if ctx.store.old_layout(p.pid)? {
                relayout.push(p.project_id.clone());
                continue;
            }
            if let Err(e) = ensure_project_dirs(ctx, &p) {
                warn!("kunde inte skapa mappar för {}: {e:#}", p.name);
            }
            pids.push(p.pid);
        }
    }
    Ok((pids, relayout))
}

/// Rows the project directory should hold, as (upserts, deletes): one folder per top folder
/// (an existing row keeps its remote fields and gets the top-folder name and parent), and
/// every other child goes. Nothing is deleted while the top folders are unknown.
fn top_rows(store: &Store, p: &Project) -> Result<(Vec<Item>, Vec<String>)> {
    let mut ups = Vec::new();
    for t in &p.tops {
        let row = match store.item(p.pid, &t.id)? {
            Some(r)
                if r.kind == Kind::Folder
                    && r.name == t.name
                    && r.parent.as_deref() == Some(PROJECT_ROOT_ID) =>
            {
                continue
            }
            Some(mut r) => {
                r.kind = Kind::Folder;
                r.name = t.name.clone();
                r.parent = Some(PROJECT_ROOT_ID.to_string());
                r
            }
            None => {
                let now = now_ms();
                Item {
                    pid: p.pid,
                    id: t.id.clone(),
                    parent: Some(PROJECT_ROOT_ID.to_string()),
                    name: t.name.clone(),
                    kind: Kind::Folder,
                    remote_version: None,
                    base_version: None,
                    size: 0,
                    ctime_ms: now,
                    mtime_ms: now,
                    storage_urn: None,
                    version_urn: None,
                    modified_by: None,
                    locked_by: None,
                    local_state: LocalState::Clean,
                    populated: false,
                }
            }
        };
        ups.push(row);
    }
    let dels = if p.tops.is_empty() {
        Vec::new()
    } else {
        store
            .children(p.pid, PROJECT_ROOT_ID)?
            .into_iter()
            .filter(|c| p.top(&c.id).is_none())
            .map(|c| c.id)
            .collect()
    };
    Ok((ups, dels))
}

/// Make a remote delta agree with the project's top folders: they sit in the project directory
/// under their top-folder names (a listing never reports them; the feed does, but a full
/// enumeration must not drop them), and other children of the project directory go. A top
/// folder the delta deletes stays deleted (the next top-folder refresh settles it).
fn merge_tops(store: &Store, p: &Project, delta: &mut Delta) -> Result<()> {
    let (ups, dels) = top_rows(store, p)?;
    for t in &p.tops {
        if delta.deletes.contains(&t.id) {
            continue;
        }
        match delta.upserts.iter_mut().find(|u| u.id == t.id) {
            Some(u) => {
                u.kind = Kind::Folder;
                u.name = t.name.clone();
                u.parent = Some(PROJECT_ROOT_ID.to_string());
            }
            None => match ups.iter().find(|u| u.id == t.id) {
                Some(u) => delta.upserts.push(u.clone()),
                // Already right in the DB; a full enumeration still has to name it.
                None if delta.reset => {
                    if let Some(r) = store.item(p.pid, &t.id)? {
                        delta.upserts.push(r);
                    }
                }
                None => {}
            },
        }
    }
    for d in dels {
        if !delta.deletes.contains(&d) {
            delta.deletes.push(d);
        }
    }
    Ok(())
}

/// Remove a clean subtree. Files with local changes (not in sync, or not placeholders at all)
/// are kept and pushed to `kept`, so the caller can upload them again instead of losing them.
/// `populated` says whether the directory's children are on disk.
fn remove_clean(path: &Path, populated: bool, kept: &mut Vec<PathBuf>) {
    remove_clean_protected(path, populated, kept, &HashSet::new());
}

fn remove_clean_protected(
    path: &Path,
    populated: bool,
    kept: &mut Vec<PathBuf>,
    protected: &HashSet<PathBuf>,
) {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return;
    };
    if md.is_dir() {
        if populated {
            if let Ok(rd) = std::fs::read_dir(path) {
                for e in rd.flatten() {
                    let child = e.path();
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        remove_clean_protected(&child, true, kept, protected);
                    } else {
                        remove_file_if_clean(&child, kept, protected);
                    }
                }
            }
        }
        let _ = std::fs::remove_dir(path);
    } else {
        remove_file_if_clean(path, kept, protected);
    }
}

fn remove_file_if_clean(path: &Path, kept: &mut Vec<PathBuf>, protected: &HashSet<PathBuf>) {
    if protected.iter().any(|p| naming::same_path(p, path)) {
        kept.push(path.to_path_buf());
        return;
    }
    match ph::placeholder_info(path) {
        Ok(Some(info)) if placeholder_is_clean(&info) => {
            if let Err(e) = std::fs::remove_file(path) {
                warn!("kunde inte ta bort {}: {e}", path.display());
            }
        }
        Ok(_) => {
            info!("behåller lokal ändring: {}", path.display());
            kept.push(path.to_path_buf());
        }
        Err(e) => warn!("kunde inte läsa {}: {e:#}", path.display()),
    }
}

/// True when no local change exists below `dir` (only populated dirs are inspected).
fn tree_is_clean(ctx: &Ctx, pid: Pid, id: &str, dir: &Path) -> bool {
    tree_is_clean_with_entries(ctx, pid, id, dir, &mut |path| std::fs::read_dir(path))
}

fn tree_is_clean_with_entries<I>(
    ctx: &Ctx,
    pid: Pid,
    id: &str,
    dir: &Path,
    read_dir: &mut impl FnMut(&Path) -> std::io::Result<I>,
) -> bool
where
    I: IntoIterator<Item = std::io::Result<std::fs::DirEntry>>,
{
    let populated = match ctx.store.item(pid, id) {
        Ok(Some(item)) => item.populated,
        _ => return false,
    };
    if !populated {
        return true;
    }
    let Ok(rd) = read_dir(dir) else {
        return false;
    };
    for entry in rd {
        let Ok(e) = entry else {
            return false;
        };
        let path = e.path();
        let Ok(kind) = e.file_type() else {
            return false;
        };
        if kind.is_dir() {
            let name = e.file_name().to_string_lossy().into_owned();
            match find_child_local(ctx, pid, id, &name) {
                Some(it) => {
                    if !tree_is_clean_with_entries(ctx, pid, &it.id, &path, read_dir) {
                        return false;
                    }
                }
                None => return false,
            }
        } else {
            match ph::placeholder_info(&path) {
                Ok(Some(i)) if placeholder_is_clean(&i) => {}
                _ => return false,
            }
        }
    }
    true
}

// ---------------------------------------------------------------------- DB <-> disk paths

/// Sanitized names from the project root (exclusive) down to `id` (inclusive).
fn rel_path(store: &Store, p: &Project, id: &str) -> Option<Vec<String>> {
    let root_id = p.root_id().to_string();
    let mut names = Vec::new();
    let mut cur = id.to_string();
    for _ in 0..64 {
        if cur == root_id {
            names.reverse();
            return Some(names);
        }
        let it = store.item(p.pid, &cur).ok()??;
        names.push(sanitize_name(&it.name));
        cur = it.parent?;
    }
    None
}

/// Where an item lives on disk (whether or not it is there right now).
pub(crate) fn item_path(ctx: &Ctx, p: &Project, id: &str) -> Option<PathBuf> {
    let mut path = project_dir(&ctx.paths.sync_root, p);
    for n in rel_path(&ctx.store, p, id)? {
        path.push(n);
    }
    Some(path)
}

/// Absolute path of a folder that is populated (its children may be on disk).
fn dir_if_visible(ctx: &Ctx, p: &Project, id: &str) -> Option<PathBuf> {
    let it = ctx.store.item(p.pid, id).ok()??;
    if !it.populated {
        return None;
    }
    item_path(ctx, p, id)
}

/// Child row whose sanitized name matches a local name (deleted rows are ignored).
pub(crate) fn find_child_local(ctx: &Ctx, pid: Pid, parent: &str, local: &str) -> Option<Item> {
    ctx.store
        .children_named(pid, parent, local)
        .ok()?
        .into_iter()
        .find(|it| it.local_state != LocalState::Deleted)
}

/// Child row deleted locally (remote delete still queued) whose name matches `local`.
fn find_deleted_child(ctx: &Ctx, pid: Pid, parent: &str, local: &str) -> Option<Item> {
    ctx.store
        .children_named(pid, parent, local)
        .ok()?
        .into_iter()
        .find(|it| it.local_state == LocalState::Deleted)
}

/// `path` is where the item `id` lives. Only the placeholder at the item's own location speaks
/// for it: a copy that still carries its identity (an editor's renamed-away original, a
/// conflict copy before its upload) must never rename or delete the real file in ACC.
fn at_item_path(ctx: &Ctx, p: &Project, id: &str, path: &Path) -> bool {
    let Some(expected) = item_path(ctx, p, id) else {
        return false;
    };
    match rel_components(&ctx.paths.sync_root, path) {
        Some(_) => naming::same_path(&expected, path),
        // A path in another form than the sync root's (e.g. a mapped drive): the name decides.
        None => naming::same_name(&file_name_of(&expected), &file_name_of(path)),
    }
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

/// A same-name file in a listing of a non-ACC provider is our own create while a local-only row of
/// that name still waits for its upload job (`uploading`: items with a live upload job). The job
/// binds the row to the committed ID when its create returns, so the poll leaves the file out and
/// the next poll sees one consistent item.
fn own_named_create(
    store: &Store,
    pid: Pid,
    remote: &Item,
    uploading: &HashSet<String>,
) -> Result<bool> {
    let Some(parent) = remote.parent.as_deref() else {
        return Ok(false);
    };
    let name = sanitize_name(&remote.name);
    Ok(store
        .children_named(pid, parent, &name)?
        .into_iter()
        .any(|local| {
            (is_local_id(&local.id) || local.local_state == LocalState::New)
                && local.local_state != LocalState::Deleted
                && uploading.contains(&local.id)
                && naming::same_name(&name, &local.name)
        }))
}

/// Local intent wins over a poll that has not seen it yet: items with a queued rename or move
/// keep their local name and parent, and items with a queued delete are not re-created by a
/// remote update (they are still deleted locally).
fn keep_pending_local(store: &Store, pid: Pid, delta: &mut Delta) -> Result<()> {
    if delta.upserts.is_empty() && delta.deletes.is_empty() {
        return Ok(());
    }
    // create_item can succeed while its upload task is still binding the local-* row.
    // The OSS object ID is unique to that upload. Do not insert the same item under
    // its remote ID and mistake the still-present local bytes for a conflict.
    let mut own_creates = HashSet::new();
    let own_versions = store.pending_upload_storages(pid)?;
    for remote in delta.upserts.iter().filter(|it| it.kind == Kind::File) {
        if remote.storage_urn.as_deref().is_some_and(|storage| {
            own_versions
                .get(&remote.id)
                .is_some_and(|own| own.contains(storage))
        }) {
            own_creates.insert(remote.id.clone());
            continue;
        }
        let (Some(parent), Some(storage)) =
            (remote.parent.as_deref(), remote.storage_urn.as_deref())
        else {
            continue;
        };
        if store
            .children_named(pid, parent, &remote.name)?
            .into_iter()
            .any(|local| {
                is_local_id(&local.id)
                    && local.local_state != LocalState::Deleted
                    && local.storage_urn.as_deref() == Some(storage)
            })
        {
            own_creates.insert(remote.id.clone());
        }
    }
    // The other providers never list the marker of our own create: their listing shows the
    // committed file under its new ID, so it is recognised by name instead.
    if store
        .project(pid)?
        .is_some_and(|p| p.provider != Provider::Acc)
    {
        let uploading = store.pending_items(pid, &["upload"])?;
        for remote in delta.upserts.iter().filter(|it| it.kind == Kind::File) {
            if own_named_create(store, pid, remote, &uploading)? {
                own_creates.insert(remote.id.clone());
            }
        }
    }
    delta.upserts.retain(|up| !own_creates.contains(&up.id));
    // A reset enumerates every surviving remote ID. Keep our existing row in
    // that enumeration while its exact cloud version is being bound; otherwise
    // apply_delta would delete it (and finalize would discard its outbox job).
    if delta.reset {
        for id in &own_creates {
            if let Some(local) = store.item(pid, id)? {
                delta.upserts.push(local);
            }
        }
    }
    // A feed can report a delete and an upsert for the same ID in one batch.
    // The exact storage marker identifies our accepted version, so keep its
    // local row until the upload/finalize job has reconciled it.
    delta.deletes.retain(|id| !own_creates.contains(id));
    // A move or rename still queued, or refused by the provider, keeps the item where the user put
    // it. The listing of its remote place (the old or the new parent) does not show it there, and
    // must not delete it: that would re-create it at the other place on every poll.
    let mut moved_locally = store.pending_items(pid, &["rename", "move"])?;
    moved_locally.extend(store.refused_renames(pid)?);
    delta.deletes.retain(|id| !moved_locally.contains(id));
    if delta.upserts.is_empty() {
        return Ok(());
    }
    // A rename or move the provider refused (its job is parked) keeps the local name too: a
    // poll that still reports the old remote name must not move the file back.
    let mut pending = store.pending_items(pid, &["rename", "move", "delete"])?;
    pending.extend(moved_locally);
    if pending.is_empty() {
        return Ok(());
    }
    for u in delta.upserts.iter_mut().filter(|u| pending.contains(&u.id)) {
        if let Some(local) = store.item(pid, &u.id)? {
            u.name = local.name;
            u.parent = local.parent;
        }
    }
    Ok(())
}

/// The byte ranges `[a, b)` that fill `[start, total)` of a placeholder. A server that
/// answers Range requests gets parallel segments for large files; one without them gets a
/// single segment (the stream starts at 0 and skips to `a`, see `stream_segment`).
fn segment_plan(start: u64, total: u64, ranged: bool) -> Vec<(u64, u64)> {
    let remaining = total.saturating_sub(start);
    let segments = if ranged && remaining >= SEGMENT_MIN {
        SEGMENTS
    } else {
        1
    };
    let seg_len = remaining.div_ceil(segments).div_ceil(CHUNK as u64) * CHUNK as u64;
    let mut plan = Vec::new();
    let mut a = start;
    while a < total {
        let b = (a + seg_len).min(total);
        plan.push((a, b));
        a = b;
    }
    plan
}

/// Diff a fresh listing of one folder against the DB rows. Only changed or new items and
/// clean children missing remotely are reported.
fn diff_listing(local: &[Item], remote: Vec<Item>, out: &mut Delta) {
    let by_id: HashMap<&str, &Item> = local.iter().map(|i| (i.id.as_str(), i)).collect();
    let remote_ids: HashSet<String> = remote.iter().map(|r| r.id.clone()).collect();
    for r in remote {
        let changed = match by_id.get(r.id.as_str()) {
            None => true,
            Some(l) => {
                let mut r2 = r.clone();
                r2.local_state = l.local_state;
                r2.base_version = l.base_version;
                r2.populated = l.populated;
                r2.pid = l.pid;
                r2 != **l
            }
        };
        if changed {
            out.upserts.push(r);
        }
    }
    for l in local {
        // Local-only rows were never listed; dirty rows missing remotely are removed too (the
        // disk side keeps the changed file and uploads it again as a new file).
        if !is_local_id(&l.id) && l.local_state != LocalState::New && !remote_ids.contains(&l.id) {
            out.deletes.push(l.id.clone());
        }
    }
}

// ---------------------------------------------------------------------- remote -> disk

/// Resolve a deleted row against the pre-delta rows: its parent may already be
/// gone from the store when disk_apply runs (especially for a deleted folder).
fn deleted_path(
    ctx: &Ctx,
    p: &Project,
    id: &str,
    before: &HashMap<String, Item>,
) -> Option<PathBuf> {
    let mut names = Vec::new();
    let mut cur = id.to_string();
    for _ in 0..64 {
        if cur == p.root_id() {
            let mut path = project_dir(&ctx.paths.sync_root, p);
            for name in names.into_iter().rev() {
                path.push(name);
            }
            return Some(path);
        }
        let row = before
            .get(&cur)
            .cloned()
            .or_else(|| ctx.store.item(p.pid, &cur).ok().flatten())?;
        names.push(sanitize_name(&row.name));
        cur = row.parent?;
    }
    None
}

/// Apply store changes to the sync root. The caller holds `local_lock()`. Returns the side
/// effects to run afterwards.
fn disk_apply(ctx: &Ctx, p: &Project, changes: &[(Option<Item>, Option<Item>)]) -> Work {
    let mut w = Work::default();
    let me = me(ctx);
    let me = me.as_deref();
    let deleted: HashSet<String> = changes
        .iter()
        .filter(|(_, a)| a.is_none())
        .filter_map(|(b, _)| b.as_ref().map(|b| b.id.clone()))
        .collect();
    let before: HashMap<String, Item> = changes
        .iter()
        .filter_map(|(b, _)| b.as_ref().map(|b| (b.id.clone(), b.clone())))
        .collect();
    let mut protected = HashSet::new();
    let mut protected_ids = Vec::new();
    for (old, new) in changes {
        let Some(old) = old.as_ref().filter(|_| new.is_none()) else {
            continue;
        };
        if old.kind != Kind::File {
            continue;
        }
        let pending = ctx
            .store
            .has_job(p.pid, &old.id, &["upload", "finalize"])
            .unwrap_or(true);
        if matches!(
            old.local_state,
            LocalState::Dirty | LocalState::Error | LocalState::New
        ) || pending
        {
            if let Some(path) = deleted_path(ctx, p, &old.id, &before).filter(|path| exists(path)) {
                protected.insert(path);
                protected_ids.push(old.id.clone());
            }
        }
    }
    let mut order: Vec<(usize, usize)> = changes
        .iter()
        .enumerate()
        .map(|(i, (b, a))| {
            let it = a.as_ref().or(b.as_ref());
            let depth = it
                .and_then(|i| rel_path(&ctx.store, p, &i.id))
                .map_or(usize::MAX, |v| v.len());
            (depth, i)
        })
        .collect();
    order.sort_by_key(|(d, _)| *d);
    if deleted.len() >= MASS_DELETE as usize {
        protected.extend(rescue_mass_removal(ctx, p, changes, &deleted));
    }
    let mut kept: Vec<PathBuf> = protected.iter().cloned().collect();
    for (_, i) in order {
        let (b, a) = (&changes[i].0, &changes[i].1);
        let r = match (b, a) {
            (None, Some(a)) => create_item(ctx, p, a, me, &mut w),
            (Some(b), Some(a)) => update_item(ctx, p, b, a, me, &mut w, &mut kept),
            (Some(b), None) => {
                if b.parent.as_ref().is_some_and(|pa| deleted.contains(pa)) {
                    Ok(())
                } else {
                    remove_item(ctx, p, b, &mut kept, &protected)
                }
            }
            (None, None) => Ok(()),
        };
        if let Err(e) = r {
            warn!("{}: kunde inte tillämpa ändring: {e:#}", p.name);
        }
    }
    for id in protected_ids {
        // The remote ID no longer exists. Re-adoption gives the saved local bytes
        // a new create job; stale version/finalize jobs must not run against it.
        let _ = ctx.store.cancel_jobs(p.pid, &id, "upload");
        let _ = ctx.store.cancel_jobs(p.pid, &id, "finalize");
    }
    kept.sort();
    kept.dedup();
    for path in kept {
        adopt(ctx, p, &path, &mut w);
    }
    w
}

/// ACC removed MASS_DELETE or more items in one go (a folder deleted by mistake, or a feed
/// glitch): every local copy among them (opened or edited files) is moved to the rescue
/// folder first, keeping its path, instead of being deleted. Placeholders without data go as
/// usual. One notice tells the user where.
fn rescue_mass_removal(
    ctx: &Ctx,
    p: &Project,
    changes: &[(Option<Item>, Option<Item>)],
    deleted: &HashSet<String>,
) -> Vec<PathBuf> {
    let pdir = project_dir(&ctx.paths.sync_root, p);
    // Reserve a new directory atomically. A second ACC mass deletion in the same
    // minute must never overwrite a file saved by the first one.
    let dest = match reset::rescue_destination(ctx, &format!("{} borttaget i ACC", p.name)) {
        Ok(path) => Some(path),
        Err(e) => {
            warn!("kunde inte skapa räddningsmapp för {}: {e}", p.name);
            None
        }
    };
    let mut saved: Vec<String> = Vec::new();
    let mut keep = Vec::new();
    for b in changes
        .iter()
        .filter(|(_, a)| a.is_none())
        .filter_map(|(b, _)| b.as_ref())
    {
        if b.parent.as_ref().is_some_and(|pa| deleted.contains(pa)) {
            continue;
        }
        let Some(top) = b
            .parent
            .as_deref()
            .and_then(|pa| dir_if_visible(ctx, p, pa))
            .map(|d| d.join(sanitize_name(&b.name)))
        else {
            continue;
        };
        let mut stack = vec![top];
        while let Some(path) = stack.pop() {
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&path) {
                    stack.extend(rd.flatten().map(|e| e.path()));
                }
                continue;
            }
            let Ok(rel) = path.strip_prefix(&pdir) else {
                continue;
            };
            let has_data = ph::placeholder_info(&path)
                .ok()
                .flatten()
                .is_none_or(|i| i.hydrated || !placeholder_is_clean(&i));
            if !has_data || ignore::ignored(rel, attributes_of(&path)) {
                continue;
            }
            let Some(dest) = dest.as_ref() else {
                keep.push(path);
                continue;
            };
            watcher::suppress(&path, SUPPRESS);
            match save_mass_rescue_file(&path, &dest.join(rel)) {
                Ok(to) => saved.push(to.strip_prefix(dest).unwrap_or(&to).display().to_string()),
                Err(e) => {
                    warn!("kunde inte spara {} före borttagning: {e}", path.display());
                    keep.push(path);
                }
            }
        }
    }
    if saved.is_empty() {
        if let Some(dest) = dest.as_ref() {
            let _ = std::fs::remove_dir(dest);
        }
        return keep;
    }
    let dest = dest.expect("saved files require a reserved destination");
    let intro = format!(
        "{} objekt togs bort i ACC i {} {}.\r\n\r\nDe här filerna fanns på datorn, så de flyttades hit i stället för att tas bort. \
         Det som togs bort i ACC går att återställa där (Borttagna objekt) i en tid.",
        deleted.len(),
        p.name,
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    );
    reset::write_readme(&dest, &intro, &saved);
    warn!(
        "{}: {} objekt borttagna i ACC, {} lokala kopior sparade i {}",
        p.name,
        deleted.len(),
        saved.len(),
        dest.display()
    );
    ctx.ui.send(UiMsg::notify(
        Severity::Warning,
        format!(
            "{} objekt togs bort i ACC i {}. {} filer som fanns på datorn sparades i {}.",
            deleted.len(),
            p.name,
            saved.len(),
            dest.display()
        ),
    ));
    keep
}

/// Copy into a newly created file, then remove the original. `rename` and
/// `std::fs::copy` may replace an existing destination on Windows; neither is
/// safe for a recovery copy when two cloud items map to the same local name.
fn save_mass_rescue_file(from: &Path, to: &Path) -> std::io::Result<PathBuf> {
    use std::fs::OpenOptions;
    use std::io::{self, BufReader};

    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stem = to.file_stem().unwrap_or_default().to_string_lossy();
    let extension = to
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    let source = std::fs::File::open(from)?;
    for n in 0..1000 {
        let candidate = if n == 0 {
            to.to_path_buf()
        } else {
            to.with_file_name(format!("{stem} ({n}){extension}"))
        };
        let mut target = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let copied =
            io::copy(&mut BufReader::new(source), &mut target).and_then(|_| target.sync_all());
        drop(target);
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&candidate);
            return Err(error);
        }
        std::fs::remove_file(from)?;
        return Ok(candidate);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free rescue file name",
    ))
}

fn create_item(ctx: &Ctx, p: &Project, a: &Item, me: Option<&str>, w: &mut Work) -> Result<()> {
    let Some(parent) = a.parent.as_deref() else {
        return Ok(());
    };
    let Some(dir) = dir_if_visible(ctx, p, parent) else {
        return Ok(());
    };
    if !dir.is_dir() {
        return Ok(());
    }
    let path = dir.join(sanitize_name(&a.name));
    if exists(&path) {
        let same = ph::placeholder_info(&path)
            .ok()
            .flatten()
            .and_then(|i| ph::decode_identity(&i.identity))
            .is_some_and(|(_, _, id)| id == a.id);
        if same {
            return Ok(());
        }
        if matches!(ph::placeholder_info(&path), Ok(Some(ref i)) if placeholder_is_clean(i)) {
            // A clean leftover of an earlier item with this name (its removal failed): replace it.
            watcher::suppress(&path, SUPPRESS);
            if let Err(e) = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir(&path)) {
                warn!("kunde inte ersätta {}: {e}", path.display());
                return Ok(());
            }
        } else if a.kind != Kind::File {
            warn!("lokal mapp finns redan, hoppar över: {}", path.display());
            return Ok(());
        } else {
            // Someone created the same name in ACC while ours waits for its upload: keep both.
            let local = ctx.store.children(p.pid, parent)?.into_iter().find(|it| {
                it.id != a.id
                    && is_local_id(&it.id)
                    && naming::matches_local(&it.name, &file_name_of(&path))
            });
            conflict_rename(ctx, p, &path, parent, local, w)?;
        }
    }
    watcher::suppress(&path, SUPPRESS);
    ph::create_placeholders(&dir, &[spec_for(a, me)])?;
    if a.kind == Kind::File {
        // A newly created placeholder represents the current ACC tip, even if
        // the row retained an older base while its previous path was missing.
        ctx.store.set_base_version(p.pid, &a.id, a.remote_version)?;
    }
    // New in a folder kept on this device: it is kept too.
    if ph::dir_pinned(&dir) {
        if let Err(e) = ph::inherit_pin(&path) {
            warn!(
                "kunde inte ärva 'Behåll alltid' för {}: {e:#}",
                path.display()
            );
        }
        if a.kind == Kind::File {
            w.hydrate.push(path);
        }
    }
    Ok(())
}

/// Files to download because they are kept on this device, a few at a time on threads of
/// their own (each `hydrate` blocks until the file is complete): pinning a big folder queues
/// its files instead of filling the shared blocking pool that every DB task needs.
fn hydrate_later(paths: Vec<PathBuf>) {
    use std::collections::VecDeque;
    use std::sync::{Condvar, OnceLock};
    type Queue = (Mutex<(VecDeque<PathBuf>, HashSet<PathBuf>)>, Condvar);
    const WORKERS: usize = 2;
    static QUEUE: OnceLock<std::sync::Arc<Queue>> = OnceLock::new();
    if paths.is_empty() {
        return;
    }
    let q = QUEUE.get_or_init(|| {
        let q: std::sync::Arc<Queue> = std::sync::Arc::new((
            Mutex::new((VecDeque::new(), HashSet::new())),
            Condvar::new(),
        ));
        for n in 0..WORKERS {
            let q = q.clone();
            let _ = std::thread::Builder::new()
                .name(format!("synka-hydrate-{n}"))
                .spawn(move || loop {
                    let path = {
                        let mut g = q.0.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(p) = g.0.pop_front() {
                                break p;
                            }
                            g = q.1.wait(g).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    // Already here, or freed on purpose by the user since it was queued: leave it.
                    let wanted = ph::placeholder_info(&path).ok().flatten().is_some_and(|i| {
                        !i.hydrated
                            && !matches!(i.pin_state, PinState::Unpinned | PinState::Excluded)
                    });
                    if wanted {
                        if let Err(e) = ph::hydrate(&path) {
                            warn!("hydrering misslyckades för {}: {e:#}", path.display());
                        }
                    }
                    q.0.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .1
                        .remove(&path);
                });
        }
        q
    });
    let mut g = q.0.lock().unwrap_or_else(|e| e.into_inner());
    for p in paths {
        if g.1.insert(p.clone()) {
            g.0.push_back(p);
        }
    }
    drop(g);
    q.1.notify_all();
}

fn update_item(
    ctx: &Ctx,
    p: &Project,
    b: &Item,
    a: &Item,
    me: Option<&str>,
    w: &mut Work,
    kept: &mut Vec<PathBuf>,
) -> Result<()> {
    if a.local_state == LocalState::Deleted {
        return Ok(()); // deleted here; the queued delete wins
    }
    // Exact comparison: a case-only rename in ACC renames the local entry too.
    let moved = b.parent != a.parent || sanitize_name(&b.name) != sanitize_name(&a.name);
    let new_path = a
        .parent
        .as_deref()
        .and_then(|pa| dir_if_visible(ctx, p, pa))
        .map(|d| d.join(sanitize_name(&a.name)));
    if moved {
        let old_path = b
            .parent
            .as_deref()
            .and_then(|pa| dir_if_visible(ctx, p, pa))
            .map(|d| d.join(sanitize_name(&b.name)));
        if let Some(op) = old_path.filter(|o| exists(o)) {
            match &new_path {
                Some(np) if !exists(np) || naming::same_path(&op, np) => {
                    watcher::suppress(&op, SUPPRESS);
                    watcher::suppress(np, SUPPRESS);
                    if let Err(e) = std::fs::rename(&op, np) {
                        // Open in a program: the entry keeps its name until it is free, and
                        // nothing is created at the new name meanwhile (that would be a
                        // second copy, and the next save would upload a new file).
                        info!("{} är öppen, byter namn senare: {e}", op.display());
                        return pend_move(ctx, p, b, a, w);
                    }
                }
                Some(np) => {
                    warn!(
                        "namnkrock vid flytt, väntar: {} -> {}",
                        op.display(),
                        np.display()
                    );
                    return pend_move(ctx, p, b, a, w);
                }
                None => remove_clean(&op, b.populated, kept),
            }
        }
    }
    let Some(path) = new_path else { return Ok(()) };
    if !exists(&path) {
        return create_item(ctx, p, a, me, w);
    }
    if a.kind == Kind::File && b.remote_version != a.remote_version {
        apply_content(ctx, p, a, &path, me, w)?;
    }
    if a.kind == Kind::File && locked_by_other(b, me) != locked_by_other(a, me) {
        let _ = ph::set_read_only(&path, locked_by_other(a, me));
    }
    if (b.locked_by != a.locked_by
        || b.remote_version != a.remote_version
        || b.modified_by != a.modified_by)
        && a.kind == Kind::File
    {
        // The Låst av / Version / Ändrad av columns are read from the DB: let Explorer re-ask.
        crate::shell::refresh_item(&path);
    }
    Ok(())
}

const MOVE_KEY: &str = "rmove:";
const REFRESH_KEY: &str = "rrefresh:";

fn refresh_key(pid: Pid, id: &str) -> String {
    format!("{REFRESH_KEY}{pid}:{id}")
}

/// A poll from an older run may have inserted our newly created remote item next
/// to its temporary local row. Only the on-disk Cloud Files identity can prove
/// which remote item owns the path. Never discard a locally modified placeholder.
/// Returns true when a dirty alias owns this path and ordinary remote handling
/// should leave it alone until its upload job preserves the bytes.
fn repair_duplicate_alias(
    ctx: &Ctx,
    pid: Pid,
    parent: &str,
    name: &str,
    path: &Path,
    info: &PlaceholderInfo,
    w: &mut Work,
) -> Result<bool> {
    let Some((kind, identity_pid, identity_id)) = ph::decode_identity(&info.identity) else {
        return Ok(false);
    };
    if identity_pid != pid || kind != Kind::File || is_local_id(&identity_id) {
        return Ok(false);
    }
    let rows = ctx.store.children_named(pid, parent, name)?;
    let Some(remote) = rows.iter().find(|r| {
        r.id == identity_id && r.kind == Kind::File && r.local_state != LocalState::Deleted
    }) else {
        return Ok(false);
    };
    let same_size = std::fs::metadata(path).is_ok_and(|m| m.len() == remote.size.max(0) as u64);
    let mut preserve_dirty = false;
    for alias in rows.iter().filter(|r| {
        is_local_id(&r.id) && r.kind == Kind::File && r.local_state != LocalState::Deleted
    }) {
        if matches!(alias.local_state, LocalState::Dirty | LocalState::Error) {
            preserve_dirty = true;
            if !ctx.store.has_job(pid, &alias.id, &["upload"])? {
                resume_create_or_requeue(ctx, pid, alias, path, w);
            }
        } else if placeholder_is_clean(info) && same_size {
            // An executing upload may still be binding this row; let it finish.
            if !ctx.store.has_job(pid, &alias.id, &["upload"])? {
                ctx.store.delete_item(pid, &alias.id)?;
                info!(
                    "removed clean duplicate local alias {} for {}",
                    alias.id, identity_id
                );
            }
        } else if !ctx.store.has_job(pid, &alias.id, &["upload"])? {
            preserve_dirty = true;
            resume_create_or_requeue(ctx, pid, alias, path, w);
        }
    }
    Ok(preserve_dirty)
}

/// Remote changes and pin restorations that need another attempt after a restart.
fn pending_refreshes(store: &Store) -> Vec<(Pid, String)> {
    let mut pending = HashSet::new();
    for prefix in [REFRESH_KEY, PIN_RESTORE_KEY, PIN_UPDATE_WARNING_KEY] {
        for (key, _) in store.kv_prefix(prefix).unwrap_or_default() {
            if let Some((pid, id)) = key.strip_prefix(prefix).and_then(|s| s.split_once(':')) {
                if let Ok(pid) = pid.parse() {
                    pending.insert((pid, id.to_string()));
                }
            }
        }
    }
    let mut pending: Vec<_> = pending.into_iter().collect();
    pending.sort();
    pending
}

fn startup_refreshes(store: &Store) -> Vec<(Pid, String)> {
    let mut pending: HashSet<_> = pending_refreshes(store).into_iter().collect();
    pending.extend(store.stale_remote_files().unwrap_or_default());
    let mut pending: Vec<_> = pending.into_iter().collect();
    pending.sort();
    pending
}

fn move_key(pid: Pid, id: &str) -> String {
    format!("{MOVE_KEY}{pid}:{id}")
}

/// A remote rename/move (`b` -> `a`) that cannot reach the disk yet: the row keeps the local
/// name and parent (so local paths keep resolving), and the target is stored until
/// `retry_move` can apply it. Uploads meanwhile use the ACC name (`pending_remote_name`).
fn pend_move(ctx: &Ctx, p: &Project, b: &Item, a: &Item, w: &mut Work) -> Result<()> {
    let target = json!({ "name": a.name, "parent": a.parent }).to_string();
    ctx.store.kv_set(&move_key(p.pid, &a.id), Some(&target))?;
    let mut keep = a.clone();
    keep.name = b.name.clone();
    keep.parent = b.parent.clone();
    ctx.store.upsert_item(&keep)?;
    w.moves.push((p.pid, a.id.clone()));
    Ok(())
}

/// The ACC name of an item whose remote rename is still pending locally.
pub(crate) fn pending_remote_name(ctx: &Ctx, pid: Pid, id: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&ctx.store.kv_get(&move_key(pid, id)).ok()??).ok()?;
    v.get("name")?.as_str().map(str::to_string)
}

/// Stored pending moves, as (pid, id).
fn pending_moves(ctx: &Ctx) -> Vec<(Pid, String)> {
    ctx.store
        .kv_prefix(MOVE_KEY)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, _)| {
            let (pid, id) = k.strip_prefix(MOVE_KEY)?.split_once(':')?;
            Some((pid.parse().ok()?, id.to_string()))
        })
        .collect()
}

/// Apply a pending remote rename/move once the entry is free. True = settled (applied, or no
/// longer applicable); false = try again later. Caller holds `local_lock()`.
fn retry_move(ctx: &Ctx, pid: Pid, id: &str) -> bool {
    retry_move_with_refresh(ctx, pid, id, refresh_placeholder)
}

fn retry_move_with_refresh(
    ctx: &Ctx,
    pid: Pid,
    id: &str,
    refresh: impl Fn(&Ctx, Pid, &str) -> bool,
) -> bool {
    let key = move_key(pid, id);
    let target: Option<(String, Option<String>)> =
        ctx.store.kv_get(&key).ok().flatten().and_then(|s| {
            let v: Value = serde_json::from_str(&s).ok()?;
            Some((
                v.get("name")?.as_str()?.to_string(),
                v.get("parent").and_then(Value::as_str).map(str::to_string),
            ))
        });
    let forget = || {
        let _ = ctx.store.kv_set(&key, None);
        true
    };
    let Some((name, parent)) = target else {
        return forget();
    };
    let (Ok(Some(p)), Ok(Some(row))) = (ctx.store.project(pid), ctx.store.item(pid, id)) else {
        return forget();
    };
    if !p.enabled || row.local_state == LocalState::Deleted {
        return forget();
    }
    let mut moved = row.clone();
    moved.name = name;
    moved.parent = parent.or(row.parent.clone());
    let old = item_path(ctx, &p, id);
    let new_dir = moved
        .parent
        .as_deref()
        .and_then(|pa| dir_if_visible(ctx, &p, pa));
    match (old.filter(|o| exists(o)), new_dir) {
        (Some(o), Some(d)) => {
            let n = d.join(sanitize_name(&moved.name));
            if exists(&n) && !naming::same_path(&o, &n) {
                return false;
            }
            if !naming::same_path(&o, &n) {
                watcher::suppress(&o, SUPPRESS);
                watcher::suppress(&n, SUPPRESS);
                if std::fs::rename(&o, &n).is_err() {
                    return false;
                }
            }
        }
        // Moved out of what is on disk here: the local entry goes once it is free.
        (Some(o), None) => {
            let mut kept = Vec::new();
            remove_clean(&o, row.populated, &mut kept);
            if exists(&o) {
                return false;
            }
        }
        (None, _) => {}
    }
    if ctx.store.upsert_item(&moved).is_err() {
        return false;
    }
    info!("{} bytte namn/plats som i ACC", moved.name);
    // Keep the durable move marker until any accompanying content update is complete.
    // If this attempt fails after the rename, the next attempt sees the same path and
    // only retries the refresh.
    if refresh(ctx, pid, id) {
        forget()
    } else {
        false
    }
}

/// New remote content for an existing file: replace the placeholder when it has no local
/// changes, otherwise keep both versions.
const PIN_RESTORE_KEY: &str = "rpin:";
const PIN_UPDATE_WARNING_KEY: &str = "rpinwarn:";

fn pin_restore_key(pid: Pid, id: &str) -> String {
    format!("{PIN_RESTORE_KEY}{pid}:{id}")
}

fn pin_update_warning_key(pid: Pid, id: &str) -> String {
    format!("{PIN_UPDATE_WARNING_KEY}{pid}:{id}")
}

fn warn_pinned_update_waiting(ctx: &Ctx, pid: Pid, id: &str, path: &Path) {
    let key = pin_update_warning_key(pid, id);
    if ctx.store.kv_get(&key).ok().flatten().is_some() {
        return;
    }
    let _ = ctx.store.kv_set(&key, Some("1"));
    ctx.ui.send(UiMsg::notify(
        Severity::Warning,
        format!(
            "Nyare ACC-version av {} väntar. Stäng filen. Om en överordnad mapp har Behåll alltid på denna enhet, välj Frigör utrymme för den mappen. Synkningen försöker igen automatiskt.",
            file_name_of(path)
        ),
    ));
}

fn clear_pinned_update_warning(ctx: &Ctx, pid: Pid, id: &str) {
    let _ = ctx.store.kv_set(&pin_update_warning_key(pid, id), None);
}

fn restore_pin(path: &Path, desired: &str) -> Result<()> {
    match desired {
        "pinned" => ph::pin(path),
        "inherit" => ph::inherit_pin(path),
        _ => Err(anyhow!("okänt behåll-lokalt-tillstånd: {desired}")),
    }
}

fn retry_pin_restore(ctx: &Ctx, pid: Pid, id: &str, path: &Path) -> Result<()> {
    let key = pin_restore_key(pid, id);
    if let Some(desired) = ctx.store.kv_get(&key)? {
        match ph::placeholder_info(path) {
            Ok(Some(info)) if placeholder_belongs_to(&info, pid, id) => {}
            Ok(_) => {
                // The original file was removed or replaced. Never pin the replacement.
                ctx.store.kv_set(&key, None)?;
                return Ok(());
            }
            Err(error) => {
                if std::fs::symlink_metadata(path)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    ctx.store.kv_set(&key, None)?;
                    return Ok(());
                }
                return Err(error);
            }
        }
        restore_pin(path, &desired)?;
        ctx.store.kv_set(&key, None)?;
    }
    Ok(())
}

fn replace_clean_remote_file(
    ctx: &Ctx,
    pid: Pid,
    id: &str,
    path: &Path,
    spec: &PlaceholderSpec,
    pin_state: PinState,
) -> Result<()> {
    // CfUpdatePlaceholder rejects DEHYDRATE while pinned. Temporarily clear the
    // pin, invalidate all old ranges atomically with the version update, then
    // restore the user's exact pin state even when the update fails.
    let desired = match pin_state {
        PinState::Pinned => Some("pinned"),
        PinState::Inherit => Some("inherit"),
        _ => None,
    };
    let Some(desired) = desired else {
        return ph::update_placeholder(path, spec, true);
    };
    let key = pin_restore_key(pid, id);
    ctx.store.kv_set(&key, Some(desired))?;
    let update = ph::unpin(path).and_then(|_| ph::update_placeholder(path, spec, true));
    let restored = restore_pin(path, desired);
    if let Err(restore_error) = restored {
        return Err(match update {
            Ok(()) => restore_error,
            Err(update_error) => anyhow!(
                "uppdatering misslyckades: {update_error:#}; återställning av behåll lokalt misslyckades: {restore_error:#}"
            ),
        });
    }
    ctx.store.kv_set(&key, None)?;
    update
}

fn apply_content(
    ctx: &Ctx,
    p: &Project,
    a: &Item,
    path: &Path,
    me: Option<&str>,
    w: &mut Work,
) -> Result<()> {
    match ph::placeholder_info(path)? {
        Some(info) if can_replace_remote_content(a, &info) => {
            watcher::suppress(path, SUPPRESS);
            // Even a partially hydrated clean placeholder may hold old ranges.
            // Drop them atomically with the metadata update before any new-tip
            // fetch can fill the remaining gaps.
            if let Err(e) =
                replace_clean_remote_file(ctx, p.pid, &a.id, path, &spec_for(a, me), info.pin_state)
            {
                // Usually open in an application (the exclusive oplock is refused): try again
                // once it is closed. The DB already has the new version.
                debug!("{} är öppen, uppdateras senare: {e:#}", path.display());
                if matches!(info.pin_state, PinState::Pinned | PinState::Inherit) {
                    warn_pinned_update_waiting(ctx, p.pid, &a.id, path);
                }
                w.refresh.push((p.pid, a.id.clone()));
                return Ok(());
            }
            ctx.store.set_base_version(p.pid, &a.id, a.remote_version)?;
            clear_pinned_update_warning(ctx, p.pid, &a.id);
            if a.local_state != LocalState::Clean {
                ctx.store.set_local_state(p.pid, &a.id, LocalState::Clean)?;
                ctx.store.cancel_jobs(p.pid, &a.id, "upload")?;
            }
            if matches!(info.pin_state, PinState::Pinned | PinState::Inherit) {
                w.hydrate.push(path.to_path_buf());
            }
            Ok(())
        }
        Some(info) if !placeholder_belongs_to(&info, p.pid, &a.id) => Err(anyhow!(
            "filen vid {} tillhör inte ACC-objektet {}; uppdatering stoppad",
            path.display(),
            a.id
        )),
        _ => keep_both(ctx, p, a, path, w),
    }
}

/// The file at `path` has local changes and `item` (the DB row, already holding the newer
/// remote state) changed in ACC as well: keep both. The local file moves to
/// "name (konflikt <date>).ext" and is uploaded as a new file next to the original; the
/// original name gets the cloud version back as a placeholder. Caller holds `local_lock()`.
pub(crate) fn keep_both(
    ctx: &Ctx,
    p: &Project,
    item: &Item,
    path: &Path,
    w: &mut Work,
) -> Result<()> {
    let parent = item
        .parent
        .clone()
        .unwrap_or_else(|| p.root_id().to_string());
    conflict_rename(ctx, p, path, &parent, None, w)?;
    ctx.store.cancel_jobs(p.pid, &item.id, "upload")?;
    let mut clean = item.clone();
    clean.local_state = LocalState::Clean;
    clean.base_version = None;
    ctx.store.upsert_item(&clean)?;
    watcher::suppress(path, SUPPRESS);
    if let Some(dir) = path.parent() {
        ph::create_placeholders(dir, &[spec_for(&clean, me(ctx).as_deref())])?;
        ctx.store
            .set_base_version(p.pid, &clean.id, clean.remote_version)?;
    }
    Ok(())
}

/// Move a local file out of the way as "name (konflikt <date>).ext" and queue it for upload.
/// `local_row` is the file's own local-only row when it has one (it is renamed with the file).
pub(crate) fn conflict_rename(
    ctx: &Ctx,
    p: &Project,
    path: &Path,
    parent_id: &str,
    local_row: Option<Item>,
    w: &mut Work,
) -> Result<()> {
    // Never rename and reidentify a partial placeholder. Its missing ranges
    // still depend on the original remote object, which may no longer be the
    // current DB tip after a poll. Hydration here could mix two versions too.
    let placeholder = ph::placeholder_info(path)?;
    if let Some(info) = &placeholder {
        let size = std::fs::metadata(path)?.len();
        if !conflict_copy_has_all_bytes(info, size) {
            return Err(IncompleteConflictCopy {
                path: path.display().to_string(),
                on_disk_size: info.on_disk_size,
                size,
            }
            .into());
        }
    }
    let name = file_name_of(path);
    let dest = naming::conflict_path(path, &naming::conflict_stamp());
    let cname = file_name_of(&dest);
    let new_id = local_row
        .as_ref()
        .map(|row| row.id.clone())
        .unwrap_or_else(local_id);
    watcher::suppress(path, SUPPRESS);
    watcher::suppress(&dest, SUPPRESS);
    let identity = placeholder
        .as_ref()
        .map(|_| ph::encode_identity(Kind::File, p.pid, &new_id));
    rename_and_detach_conflict(path, &dest, identity.as_deref(), |destination, identity| {
        ph::set_identity(destination, identity, false)
    })?;
    let now = now_ms();
    let size = std::fs::metadata(&dest)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    let id = match local_row {
        Some(mut row) => {
            ctx.store.cancel_jobs(p.pid, &row.id, "upload")?;
            row.name = cname.clone();
            row.size = size;
            row.mtime_ms = now;
            ctx.store.upsert_item(&row)?;
            row.id
        }
        None => {
            let id = new_id;
            ctx.store.upsert_item(&Item {
                pid: p.pid,
                id: id.clone(),
                parent: Some(parent_id.to_string()),
                name: cname.clone(),
                kind: Kind::File,
                remote_version: None,
                base_version: None,
                size,
                ctime_ms: now,
                mtime_ms: now,
                storage_urn: None,
                version_urn: None,
                modified_by: None,
                locked_by: None,
                local_state: LocalState::New,
                populated: false,
            })?;
            id
        }
    };
    enqueue(
        ctx,
        p.pid,
        "upload",
        Some(&id),
        json!({ "path": dest.to_string_lossy() }),
    )?;
    w.enqueued = true;
    w.touched.insert(p.pid);
    ctx.ui.activity("conflict", &name, &p.name, &cname);
    ctx.ui.send(UiMsg::notify(
        Severity::Warning,
        format!("{name} ändrades både här och i ACC. Din version sparades som \"{cname}\"."),
    ));
    info!("konflikt: lokal version sparad som {cname}");
    Ok(())
}

/// Rename first, then sever the conflict copy's remote identity before any local-only row
/// or upload intent is recorded. A failed detach is rolled back when the file system allows it.
fn rename_and_detach_conflict(
    path: &Path,
    dest: &Path,
    identity: Option<&[u8]>,
    detach: impl FnOnce(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    std::fs::rename(path, dest)?;
    if let Some(identity) = identity {
        if let Err(error) = detach(dest, identity) {
            let (source, rollback_failed) = match std::fs::rename(dest, path) {
                Ok(()) => (error, false),
                Err(rollback) => (
                    error.context(format!(
                        "återställning av filnamn misslyckades: {rollback}; konfliktkopian finns kvar vid {}",
                        dest.display()
                    )),
                    true,
                ),
            };
            return Err(ConflictDetachFailed {
                path: path.display().to_string(),
                rollback_failed,
                source,
            }
            .into());
        }
    }
    Ok(())
}

fn remove_item(
    ctx: &Ctx,
    p: &Project,
    b: &Item,
    kept: &mut Vec<PathBuf>,
    protected: &HashSet<PathBuf>,
) -> Result<()> {
    let Some(parent) = b.parent.as_deref() else {
        return Ok(());
    };
    let Some(dir) = dir_if_visible(ctx, p, parent) else {
        return Ok(());
    };
    let path = dir.join(sanitize_name(&b.name));
    if !exists(&path) {
        return Ok(());
    }
    watcher::suppress(&path, SUPPRESS);
    remove_clean_protected(&path, b.populated, kept, protected);
    Ok(())
}

/// Bring a placeholder up to the version in the DB after an earlier attempt found the file
/// open. Returns true when nothing is left to do (updated, edited locally meanwhile — the
/// upload then keeps both — or gone), false to try again later.
fn refresh_placeholder(ctx: &Ctx, pid: Pid, id: &str) -> bool {
    let p = match ctx.store.project(pid) {
        Ok(Some(p)) => p,
        Ok(None) => {
            clear_pinned_update_warning(ctx, pid, id);
            let _ = ctx.store.kv_set(&pin_restore_key(pid, id), None);
            return true;
        }
        Err(_) => return false,
    };
    let it = match ctx.store.item(pid, id) {
        Ok(Some(it)) => it,
        Ok(None) => {
            clear_pinned_update_warning(ctx, pid, id);
            let _ = ctx.store.kv_set(&pin_restore_key(pid, id), None);
            return true;
        }
        Err(_) => return false,
    };
    if it.kind != Kind::File {
        clear_pinned_update_warning(ctx, pid, id);
        let _ = ctx.store.kv_set(&pin_restore_key(pid, id), None);
        return true;
    }
    let Some(path) = it
        .parent
        .as_deref()
        .and_then(|pa| dir_if_visible(ctx, &p, pa))
        .map(|d| d.join(sanitize_name(&it.name)))
    else {
        clear_pinned_update_warning(ctx, pid, id);
        let _ = ctx.store.kv_set(&pin_restore_key(pid, id), None);
        return true;
    };
    if let Err(error) = retry_pin_restore(ctx, pid, id, &path) {
        warn_pinned_update_waiting(ctx, pid, id, &path);
        warn!(
            "kunde inte återställa behåll lokalt för {}: {error:#}",
            path.display()
        );
        return false;
    }
    if !p.enabled || it.local_state != LocalState::Clean {
        clear_pinned_update_warning(ctx, pid, id);
        return true;
    }
    let info = match ph::placeholder_info(&path) {
        Ok(Some(info)) if placeholder_is_clean(&info) && placeholder_belongs_to(&info, pid, id) => {
            info
        }
        Ok(_) => {
            clear_pinned_update_warning(ctx, pid, id);
            return true;
        }
        // A sharing violation while querying the placeholder is still pending work.
        Err(_) => {
            let missing = std::fs::symlink_metadata(&path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            if missing {
                // No local bytes remain to invalidate. Retire the stale backing version;
                // the local deletion/recreation is handled by normal reconciliation.
                if ctx
                    .store
                    .set_base_version(pid, id, it.remote_version)
                    .is_err()
                {
                    return false;
                }
                clear_pinned_update_warning(ctx, pid, id);
            }
            return missing;
        }
    };
    let stale = match std::fs::metadata(&path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            m.len() as i64 != it.size.max(0) || mtime != it.mtime_ms
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if ctx
                .store
                .set_base_version(pid, id, it.remote_version)
                .is_err()
            {
                return false;
            }
            clear_pinned_update_warning(ctx, pid, id);
            return true;
        }
        Err(_) => return false,
    } || it.base_version != it.remote_version;
    if !stale {
        clear_pinned_update_warning(ctx, pid, id);
        return true;
    }
    watcher::suppress(&path, SUPPRESS);
    match replace_clean_remote_file(
        ctx,
        pid,
        id,
        &path,
        &spec_for(&it, me(ctx).as_deref()),
        info.pin_state,
    ) {
        Ok(()) => {
            if let Err(error) = ctx.store.set_base_version(pid, id, it.remote_version) {
                warn!("kunde inte spara basversion för {}: {error:#}", it.name);
                return false;
            }
            clear_pinned_update_warning(ctx, pid, id);
            info!("{}: uppdaterad till senaste versionen", it.name);
            if info.pin_state == PinState::Pinned || info.pin_state == PinState::Inherit {
                hydrate_later(vec![path]);
            }
            true
        }
        Err(error) => {
            if matches!(info.pin_state, PinState::Pinned | PinState::Inherit) {
                warn_pinned_update_waiting(ctx, pid, id, &path);
            }
            warn!("kunde inte uppdatera {}: {error:#}", path.display());
            false
        }
    }
}

// ---------------------------------------------------------------------- local -> remote

fn enqueue(ctx: &Ctx, pid: Pid, kind: &str, item: Option<&str>, payload: Value) -> Result<i64> {
    ctx.store.enqueue(pid, kind, item, &payload.to_string())
}

fn mark_dirty(ctx: &Ctx, pid: Pid, it: &Item, path: &Path, w: &mut Work) {
    if ctx.store.has_unsupported_job(pid, &it.id).unwrap_or(false) {
        // ACC has definitively rejected this file type. Keep it locally and do
        // not queue the same unsupported extension after each edit.
        return;
    }
    if let Err(e) = ctx.store.mark_dirty_with_base(pid, &it.id) {
        warn!("kunde inte markera ändring: {e:#}");
        return;
    }
    // Safe-save bursts can return here while an earlier upload is still queued.
    // Preserve that row and its exact-object commit intent. A finalizer also owns
    // this file: it will retarget its own row to upload when it sees newer bytes.
    if ctx
        .store
        .has_job(pid, &it.id, &["upload", "finalize"])
        .unwrap_or(true)
    {
        w.touched.insert(pid);
        return;
    }
    if enqueue(
        ctx,
        pid,
        "upload",
        Some(&it.id),
        json!({ "path": path.to_string_lossy() }),
    )
    .is_ok()
    {
        w.enqueued = true;
    }
    w.touched.insert(pid);
}

/// Queue the job a changed row lost (a Dirty file stays Dirty, a New one stays New).
fn requeue(ctx: &Ctx, pid: Pid, it: &Item, kind: &str, path: &Path, w: &mut Work) {
    if it.local_state == LocalState::Dirty {
        return mark_dirty(ctx, pid, it, path, w);
    }
    if enqueue(
        ctx,
        pid,
        kind,
        Some(&it.id),
        json!({ "path": path.to_string_lossy() }),
    )
    .is_ok()
    {
        w.enqueued = true;
    }
    w.touched.insert(pid);
}

/// Register a new local-only folder or file (LocalState::New) and queue its creation.
/// Returns the temporary id.
pub(crate) fn register_local(
    ctx: &Ctx,
    pid: Pid,
    parent: &str,
    name: &str,
    is_dir: bool,
    path: &Path,
) -> Result<String> {
    if !local_path_is_safe(&ctx.paths.sync_root, path) {
        return Err(anyhow!(
            "lokal sökväg innehåller en länk eller kan inte verifieras: {}",
            path.display()
        ));
    }
    if is_dir && !creates_folders(ctx, pid) {
        return Err(anyhow!("mappen finns bara lokalt: {}", path.display()));
    }
    let id = local_id();
    let now = now_ms();
    let size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
    ctx.store.upsert_item(&Item {
        pid,
        id: id.clone(),
        parent: Some(parent.to_string()),
        name: name.to_string(),
        kind: if is_dir { Kind::Folder } else { Kind::File },
        remote_version: None,
        base_version: None,
        size: if is_dir { 0 } else { size },
        ctime_ms: now,
        mtime_ms: now,
        storage_urn: None,
        version_urn: None,
        modified_by: None,
        locked_by: None,
        local_state: LocalState::New,
        populated: is_dir,
    })?;
    let kind = if is_dir { "mkdir" } else { "upload" };
    enqueue(
        ctx,
        pid,
        kind,
        Some(&id),
        json!({ "path": path.to_string_lossy() }),
    )?;
    Ok(id)
}

/// The delete job for `id` is about to run: is the item back at its place (a save that took
/// longer than the grace period, an undo the watcher has not reported yet)? Then the delete
/// is dropped and the file is handled as a local change. Caller must NOT hold `local_lock()`.
pub(crate) fn back_before_delete(ctx: &Ctx, p: &Project, id: &str) -> bool {
    let Some(path) = item_path(ctx, p, id) else {
        return false;
    };
    if !exists(&path) {
        return false;
    }
    {
        let _guard = local_lock();
        let _ = ctx.store.set_local_state(p.pid, id, LocalState::Clean);
    }
    // Clean again: a changed file there becomes a new version, an untouched one stays as is.
    let w = process_local(ctx, vec![path.clone()]);
    info!(
        "{} kom tillbaka innan borttagningen skickades, tas inte bort i ACC",
        path.display()
    );
    drop(w);
    true
}

/// A file saved "safely" (delete or rename-away of the original, then a new file under the
/// same name) continues the same ACC item: the queued delete is dropped and the new content
/// becomes a new version.
fn resurrect(ctx: &Ctx, pid: Pid, d: &Item, path: &Path, in_sync: bool, w: &mut Work) {
    if let Err(e) = ctx.store.cancel_jobs(pid, &d.id, "delete") {
        warn!("kunde inte ångra borttagning: {e:#}");
        return;
    }
    if in_sync {
        let _ = ctx.store.set_local_state(pid, &d.id, LocalState::Clean);
        w.touched.insert(pid);
    } else {
        mark_dirty(ctx, pid, d, path, w);
    }
    debug!(
        "{} återskapad lokalt: ny version i stället för borttagning",
        d.name
    );
}

/// The placeholder's identity does not name a live item at this location (left over after a
/// remote delete, or a copy of another placeholder).
fn stale_identity(ctx: &Ctx, p: &Project, identity: &[u8], path: &Path) -> bool {
    match ph::decode_identity(identity) {
        Some((_, pid, id)) if pid == p.pid => {
            !matches!(ctx.store.item(pid, &id), Ok(Some(_))) || !at_item_path(ctx, p, &id, path)
        }
        _ => true,
    }
}

/// Register a local file that has no live row (kept after its ACC item disappeared, or with a
/// stale identity) as new, creating rows for missing parent folders on the way, so its content
/// is uploaded instead of lost.
fn adopt(ctx: &Ctx, p: &Project, path: &Path, w: &mut Work) {
    let Some(comps) = rel_components(&ctx.paths.sync_root, path) else {
        return;
    };
    if comps.len() < 3 || !exists(path) {
        return;
    }
    if ignore::ignored(&comps.iter().collect::<PathBuf>(), attributes_of(path)) {
        return;
    }
    let mut parent = p.root_id().to_string();
    let mut cur = project_dir(&ctx.paths.sync_root, p);
    let rest = &comps[2..];
    for (i, c) in rest.iter().enumerate() {
        cur.push(c);
        let last = i + 1 == rest.len();
        match find_child_local(ctx, p.pid, &parent, c) {
            Some(it) if !last && it.kind.is_dir() => parent = it.id,
            Some(_) => return,
            None if parent == PROJECT_ROOT_ID => {
                not_synced_here(ctx, p, &cur);
                return;
            }
            None if !creates_folders(ctx, p.pid) && (!last || cur.is_dir()) => {
                not_created_here(ctx, p, &cur);
                return;
            }
            None => match register_local(ctx, p.pid, &parent, c, !last || cur.is_dir(), &cur) {
                Ok(id) => {
                    w.enqueued = true;
                    w.touched.insert(p.pid);
                    if !last {
                        parent = id;
                    }
                }
                Err(e) => {
                    warn!("kunde inte registrera {}: {e:#}", cur.display());
                    return;
                }
            },
        }
    }
}

/// Process local paths under the sync root (watcher batches or a rescan).
fn process_local(ctx: &Ctx, paths: Vec<PathBuf>) -> Work {
    process_local_with_info(ctx, paths, ph::placeholder_info)
}

fn process_local_with_info(
    ctx: &Ctx,
    paths: Vec<PathBuf>,
    mut placeholder_info: impl FnMut(&Path) -> Result<Option<PlaceholderInfo>>,
) -> Work {
    let _guard = local_lock();
    let mut w = Work::default();
    let projects = match ctx.store.projects() {
        Ok(p) => p,
        Err(_) => return w,
    };
    let mut seen = HashSet::new();
    for path in paths {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(comps) = rel_components(&ctx.paths.sync_root, &path) else {
            continue;
        };
        if comps.len() < 3 {
            continue;
        }
        let Some(p) = projects.iter().find(|p| {
            p.enabled && !project_resetting(p.pid) && project_matches(p, &comps[0], &comps[1])
        }) else {
            continue;
        };
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !local_path_is_safe(&ctx.paths.sync_root, &path) {
            warn!(
                "hoppar över länk eller overifierad lokal sökväg: {}",
                path.display()
            );
            continue;
        }
        let attrs = {
            use std::os::windows::fs::MetadataExt;
            md.file_attributes()
        };
        if ignore::ignored(&comps.iter().collect::<PathBuf>(), attrs) {
            continue;
        }
        let rest = &comps[2..];
        let leaf = rest[rest.len() - 1].clone();
        let mut parent = p.root_id().to_string();
        let mut ok = true;
        for c in &rest[..rest.len() - 1] {
            match find_child_local(ctx, p.pid, &parent, c) {
                Some(it) if it.kind.is_dir() => parent = it.id,
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        let info = match placeholder_info(&path) {
            Ok(info) => info,
            Err(error) => {
                warn!(
                    "kunde inte kontrollera platshållaren {}; försöker igen senare: {error:#}",
                    path.display()
                );
                w.local_retry.push(path);
                continue;
            }
        };
        if let Some(ref info) = info {
            match repair_duplicate_alias(ctx, p.pid, &parent, &leaf, &path, info, &mut w) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => warn!(
                    "could not reconcile duplicate local file {}: {e:#}",
                    path.display()
                ),
            }
        }
        let existing = find_child_local(ctx, p.pid, &parent, &leaf);
        if parent == PROJECT_ROOT_ID && existing.is_none() {
            // ACC has no place for it: the project directory holds only the top folders.
            not_synced_here(ctx, p, &path);
            continue;
        }
        if md.is_dir() {
            match &existing {
                // Stays local with its contents: no folder job the provider would refuse forever.
                None if info.is_none() && !creates_folders(ctx, p.pid) => {
                    not_created_here(ctx, p, &path)
                }
                None if info.is_none() => {
                    match register_local(ctx, p.pid, &parent, &leaf, true, &path) {
                        Ok(_) => {
                            w.enqueued = true;
                            w.touched.insert(p.pid);
                        }
                        Err(e) => warn!("kunde inte registrera mapp: {e:#}"),
                    }
                }
                Some(it)
                    if it.local_state == LocalState::New
                        && !ctx.store.has_job(p.pid, &it.id, &["mkdir"]).unwrap_or(true) =>
                {
                    requeue(ctx, p.pid, it, "mkdir", &path, &mut w)
                }
                _ => {}
            }
            continue;
        }
        match info {
            None => match existing {
                None => {
                    if let Some(d) = find_deleted_child(ctx, p.pid, &parent, &leaf)
                        .filter(|d| d.kind == Kind::File)
                    {
                        resurrect(ctx, p.pid, &d, &path, false, &mut w);
                    } else {
                        match register_local(ctx, p.pid, &parent, &leaf, false, &path) {
                            Ok(_) => {
                                w.enqueued = true;
                                w.touched.insert(p.pid);
                            }
                            Err(e) => warn!("kunde inte registrera fil: {e:#}"),
                        }
                    }
                }
                // A file that failed before gets a new chance with every new local change.
                Some(it) if matches!(it.local_state, LocalState::Clean | LocalState::Error) => {
                    mark_dirty(ctx, p.pid, &it, &path, &mut w)
                }
                // Changed and waiting for its upload or local finalization. If the job is gone
                // (lost in a crash, or dropped by an older version), queue it again.
                Some(it) if matches!(it.local_state, LocalState::Dirty | LocalState::New) => {
                    if !ctx
                        .store
                        .has_job(p.pid, &it.id, &["upload", "finalize"])
                        .unwrap_or(true)
                    {
                        info!("{}: ändrad men inte köad, köar uppladdningen igen", it.name);
                        resume_create_or_requeue(ctx, p.pid, &it, &path, &mut w);
                    }
                }
                Some(_) => {}
            },
            Some(info) => {
                if let Some(it) = existing
                    .as_ref()
                    .filter(|it| is_local_id(&it.id) && it.storage_urn.is_some())
                {
                    if !ctx
                        .store
                        .has_job(p.pid, &it.id, &["upload", "finalize"])
                        .unwrap_or(true)
                    {
                        resume_create_or_requeue(ctx, p.pid, it, &path, &mut w);
                    }
                }
                if info.pin_state == PinState::Pinned && !info.hydrated {
                    w.hydrate.push(path.clone());
                } else if info.pin_state == PinState::Unpinned
                    && info.hydrated
                    && placeholder_is_clean(&info)
                {
                    let _ = ph::dehydrate(&path);
                }
                match existing {
                    Some(it) => {
                        if !placeholder_is_clean(&info)
                            && matches!(it.local_state, LocalState::Clean | LocalState::Error)
                        {
                            mark_dirty(ctx, p.pid, &it, &path, &mut w);
                        }
                    }
                    None => {
                        let own = ph::decode_identity(&info.identity).map(|(_, _, id)| id);
                        match find_deleted_child(ctx, p.pid, &parent, &leaf) {
                            // The original came back under its own name (an undo, or an editor
                            // that renames the original back): nothing is deleted after all.
                            Some(d) if own.as_deref() == Some(d.id.as_str()) => resurrect(
                                ctx,
                                p.pid,
                                &d,
                                &path,
                                placeholder_is_clean(&info),
                                &mut w,
                            ),
                            _ if !placeholder_is_clean(&info)
                                && stale_identity(ctx, p, &info.identity, &path) =>
                            {
                                adopt(ctx, p, &path, &mut w)
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    w
}

/// Folders can be created in the project's provider (`remote::caps`). Unknown projects can.
fn creates_folders(ctx: &Ctx, pid: Pid) -> bool {
    ctx.store
        .project(pid)
        .ok()
        .flatten()
        .is_none_or(|p| remote::caps(p.provider).mkdir)
}

/// Tell the user once per path that a folder made here stays on this PC: the provider's API has
/// no folder creation, so the folder and what is inside it are not synced.
fn not_created_here(ctx: &Ctx, p: &Project, path: &Path) {
    static TOLD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
    if told.iter().any(|t| naming::same_path(t, path)) {
        return;
    }
    told.push(path.to_path_buf());
    drop(told);
    info!("skapas inte i {}: {}", p.provider.label(), path.display());
    ctx.ui.send(UiMsg::notify(
        Severity::Warning,
        format!(
            "{} API:t kan inte skapa mappar. {} finns bara på den här datorn.",
            p.provider.label(),
            file_name_of(path)
        ),
    ));
}

/// Tell the user once per path that something directly in a project directory stays local.
fn not_synced_here(ctx: &Ctx, p: &Project, path: &Path) {
    static TOLD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
    if told.iter().any(|t| naming::same_path(t, path)) {
        return;
    }
    told.push(path.to_path_buf());
    drop(told);
    info!("synkas inte (direkt i projektmappen): {}", path.display());
    let into = p
        .tops
        .first()
        .map_or("en av projektets mappar".to_string(), |t| {
            format!("«{}»", t.name)
        });
    ctx.ui.send(UiMsg::notify(
        Severity::Warning,
        format!(
            "{} synkas inte: det ligger direkt i projektmappen {}. Flytta det till {into}.",
            file_name_of(path),
            p.name
        ),
    ));
}

/// Full local scan of populated directories (after a watcher overflow, and at startup).
fn scan_populated(ctx: &Ctx) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let projects = ctx.store.projects().unwrap_or_default();
    for p in projects.iter().filter(|p| p.enabled) {
        let mut stack = vec![(
            project_dir(&ctx.paths.sync_root, p),
            p.root_id().to_string(),
        )];
        while let Some((dir, id)) = stack.pop() {
            let populated = ctx
                .store
                .item(p.pid, &id)
                .ok()
                .flatten()
                .is_some_and(|i| i.populated);
            if !populated {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                out.push(path.clone());
                if is_dir {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if let Some(it) = find_child_local(ctx, p.pid, &id, &name) {
                        if it.kind.is_dir() {
                            stack.push((path, it.id));
                        }
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------- delete / rename from CF

fn handle_delete(ctx: &Ctx, req: &DeleteReq) -> Work {
    let mut w = Work::default();
    if reset::under_reset(&req.path) {
        let _ = cb::ack_delete(&req.keys, 0);
        return w;
    }
    let Some((kind, pid, id)) = ph::decode_identity(&req.identity) else {
        let _ = cb::ack_delete(&req.keys, 0);
        return w;
    };
    if matches!(kind, Kind::Hub | Kind::ProjectRoot) {
        let _ = cb::ack_delete(&req.keys, STATUS_CLOUD_FILE_ACCESS_DENIED);
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            "Ta bort projektet via Synka i stället",
        ));
        return w;
    }
    if req.undelete {
        // The delete was taken back before it happened: the item stays, nothing goes to ACC.
        let _ = cb::ack_delete(&req.keys, 0);
        let _guard = local_lock();
        if let Ok(Some(it)) = ctx.store.item(pid, &id) {
            if it.local_state == LocalState::Deleted
                && ctx.store.cancel_jobs(pid, &id, "delete").is_ok()
            {
                let _ = ctx.store.set_local_state(pid, &id, LocalState::Clean);
                w.touched.insert(pid);
                debug!("borttagning ångrad: {}", it.name);
            }
        }
        return w;
    }
    if let Some(top) = top_at_path(ctx, pid, &id, &req.path) {
        let _ = cb::ack_delete(&req.keys, STATUS_CLOUD_FILE_ACCESS_DENIED);
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            format!("«{top}» är en av projektets toppmappar i ACC och kan inte tas bort här."),
        ));
        return w;
    }
    let _ = cb::ack_delete(&req.keys, 0);
    let _guard = local_lock();
    let Ok(Some(p)) = ctx.store.project(pid) else {
        return w;
    };
    if !p.enabled {
        return w;
    }
    if !at_item_path(ctx, &p, &id, &req.path) {
        debug!(
            "borttagen kopia med gammal identitet, påverkar inte ACC: {}",
            req.path.display()
        );
        return w;
    }
    queue_remote_delete(ctx, &p, &id, &mut w);
    w
}

/// Mark an item deleted locally and queue the remote delete (or forget it if never synced).
/// The delete waits DELETE_GRACE_MS so a safe save can still turn it into a new version.
fn queue_remote_delete(ctx: &Ctx, p: &Project, id: &str, w: &mut Work) {
    let Ok(Some(item)) = ctx.store.item(p.pid, id) else {
        return;
    };
    if !ctx.cfg_now().propagate_deletes || item.local_state == LocalState::Deleted {
        return;
    }
    if item.local_state == LocalState::New || is_local_id(id) {
        let _ = ctx.store.delete_item(p.pid, id);
        return;
    }
    if ctx
        .store
        .set_local_state(p.pid, id, LocalState::Deleted)
        .is_err()
    {
        return;
    }
    // A folder weighs as much as what it holds; one whose contents we never listed may hold
    // anything, so it always asks.
    let weight = if item.kind.is_dir() {
        let known = ctx.store.subtree_len(p.pid, id).unwrap_or(1) as i64;
        if item.populated || p.change_source == ChangeSourceKind::DesktopSync {
            known
        } else {
            known.max(MASS_DELETE)
        }
    } else {
        1
    };
    // The version the user saw when deleting: a newer one in ACC wins over the delete.
    let payload = json!({ "id": id, "is_dir": item.kind.is_dir(), "weight": weight, "ver": item.remote_version }).to_string();
    if ctx
        .store
        .enqueue_at(
            p.pid,
            "delete",
            Some(id),
            &payload,
            now_ms() + DELETE_GRACE_MS,
        )
        .is_ok()
    {
        w.enqueued = true;
        ctx.ui.activity("delete", &item.name, &p.name, "");
    }
    w.touched.insert(p.pid);
    hold_mass_deletes(ctx, p);
}

/// Hold the project's pending deletes once they add up to MASS_DELETE (and keep holding new
/// ones while some are held), and tell the user once.
fn hold_mass_deletes(ctx: &Ctx, p: &Project) {
    let already = ctx
        .store
        .held_deletes()
        .unwrap_or_default()
        .iter()
        .any(|(pid, _)| *pid == p.pid);
    if !already && ctx.store.pending_delete_weight(p.pid).unwrap_or(0) < MASS_DELETE {
        return;
    }
    if let Err(e) = ctx.store.hold_deletes(p.pid) {
        warn!("kunde inte hålla borttagningar: {e:#}");
        return;
    }
    if !already {
        let n = ctx.store.pending_delete_weight(p.pid).unwrap_or(0);
        warn!("{}: {n} borttagningar väntar på bekräftelse", p.name);
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            format!("Du tog bort {n} objekt i {}. De tas inte bort i ACC förrän du bekräftar i {APP_NAME}.", p.name),
        ));
    }
}

/// The user declined held deletes: the items come back (as placeholders; content is still in
/// ACC). Returns how many.
fn restore_held(ctx: &Ctx, p: &Project) -> Result<usize> {
    let _guard = local_lock();
    let ids = ctx.store.take_held_deletes(p.pid)?;
    restore_rows(ctx, p, &ids)
}

/// Bring locally deleted items back as placeholders (their content is still in ACC), parents
/// first. Caller holds `local_lock()`. Returns how many rows came back.
pub(crate) fn restore_rows(ctx: &Ctx, p: &Project, ids: &[String]) -> Result<usize> {
    let me = me(ctx);
    let mut rows: Vec<Item> = ids
        .iter()
        .filter_map(|id| ctx.store.item(p.pid, id).ok().flatten())
        .collect();
    rows.sort_by_key(|r| rel_path(&ctx.store, p, &r.id).map_or(usize::MAX, |v| v.len()));
    for r in &rows {
        ctx.store.set_local_state(p.pid, &r.id, LocalState::Clean)?;
        if r.kind.is_dir() {
            ctx.store.unpopulate_subtree(p.pid, &r.id)?;
        }
        let Some(dir) = r
            .parent
            .as_deref()
            .and_then(|pa| dir_if_visible(ctx, p, pa))
        else {
            continue;
        };
        let path = dir.join(sanitize_name(&r.name));
        if dir.is_dir() && !exists(&path) {
            let mut fresh = r.clone();
            fresh.local_state = LocalState::Clean;
            watcher::suppress(&path, SUPPRESS);
            if let Err(e) = ph::create_placeholders(&dir, &[spec_for(&fresh, me.as_deref())]) {
                warn!("kunde inte återställa {}: {e:#}", path.display());
            }
        }
    }
    Ok(rows.len())
}

/// The top-folder name when `id` at `path` is one of the project's top folders (the folders in
/// the project directory, e.g. "Project Files"): those are ACC's, not the user's to remove.
fn top_at_path(ctx: &Ctx, pid: Pid, id: &str, path: &Path) -> Option<String> {
    let it = ctx.store.item(pid, id).ok()??;
    if it.parent.as_deref() != Some(PROJECT_ROOT_ID) {
        return None;
    }
    let p = ctx.store.project(pid).ok()??;
    at_item_path(ctx, &p, id, path).then_some(it.name)
}

/// Where a rename sends an item.
enum Target {
    /// Out of the sync root: a delete for ACC.
    Outside,
    /// Another project, or the hub/root level: not supported.
    Elsewhere,
    /// Inside the same project: the components below the project folder.
    Within(Vec<String>),
}

fn rename_target(ctx: &Ctx, p: &Project, req: &RenameReq) -> Target {
    if !req.target_in_scope {
        return Target::Outside;
    }
    // In scope but not under the root as we spell it: refuse rather than guess.
    let Some(comps) = rel_components(&ctx.paths.sync_root, &req.target_path) else {
        return Target::Elsewhere;
    };
    if comps.len() < 3 || !project_matches(p, &comps[0], &comps[1]) {
        return Target::Elsewhere;
    }
    Target::Within(comps[2..].to_vec())
}

fn handle_rename(ctx: &Ctx, req: &RenameReq) -> Work {
    let mut w = Work::default();
    if reset::under_reset(&req.path) {
        let _ = cb::ack_rename(&req.keys, 0);
        return w;
    }
    let Some((kind, pid, id)) = ph::decode_identity(&req.identity) else {
        let _ = cb::ack_rename(&req.keys, 0);
        return w;
    };
    if matches!(kind, Kind::Hub | Kind::ProjectRoot) {
        let _ = cb::ack_rename(&req.keys, STATUS_CLOUD_FILE_ACCESS_DENIED);
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            "Byt namn på projektet i ACC, inte i Synka",
        ));
        return w;
    }
    let p = match ctx.store.project(pid) {
        Ok(Some(p)) if p.enabled => p,
        _ => {
            let _ = cb::ack_rename(&req.keys, 0);
            return w;
        }
    };
    let target = rename_target(ctx, &p, req);
    if matches!(target, Target::Elsewhere) && at_item_path(ctx, &p, &id, &req.path) {
        // ACC cannot move files between projects; copying works (it uploads a new file).
        let _ = cb::ack_rename(&req.keys, STATUS_CLOUD_FILE_ACCESS_DENIED);
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            "Det går inte att flytta filer mellan projekt. Kopiera dem i stället.",
        ));
        return w;
    }
    if let Some(top) = top_at_path(ctx, pid, &id, &req.path) {
        let _ = cb::ack_rename(&req.keys, STATUS_CLOUD_FILE_ACCESS_DENIED);
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            format!("«{top}» är en av projektets toppmappar i ACC och kan inte byta namn eller flyttas här."),
        ));
        return w;
    }
    if matches!(&target, Target::Within(rest) if rest.len() == 1)
        && at_item_path(ctx, &p, &id, &req.path)
    {
        let _ = cb::ack_rename(&req.keys, STATUS_CLOUD_FILE_ACCESS_DENIED);
        let into = p
            .tops
            .first()
            .map_or("en av projektets mappar".to_string(), |t| {
                format!("«{}»", t.name)
            });
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            format!(
                "Inget kan ligga direkt i projektmappen {}. Lägg det i {into}.",
                p.name
            ),
        ));
        return w;
    }
    let _ = cb::ack_rename(&req.keys, 0);
    let _guard = local_lock();
    if !at_item_path(ctx, &p, &id, &req.path) {
        debug!(
            "namnbyte på kopia med gammal identitet, påverkar inte ACC: {}",
            req.path.display()
        );
        return w;
    }
    // Our own renames (conflict copies, remote renames applied to disk) are not user intent:
    // the moved file was re-pointed at another item, or we touched the path a moment ago.
    let moved_identity = ph::placeholder_info(&req.target_path)
        .ok()
        .flatten()
        .map(|i| i.identity);
    if watcher::is_suppressed(&req.path)
        || moved_identity.is_some_and(|ident| ident != req.identity)
    {
        debug!("eget namnbyte, påverkar inte ACC: {}", req.path.display());
        return w;
    }
    let rest = match target {
        Target::Within(rest) => rest,
        Target::Outside | Target::Elsewhere => {
            queue_remote_delete(ctx, &p, &id, &mut w);
            return w;
        }
    };
    record_local_rename(ctx, &p, kind, &id, &rest, &mut w);
    w
}

/// A rename or move made here (`rest`: the new path below the project folder): the row
/// follows, and a rename or move job takes it to ACC. A rename to a temporary or backup name
/// is a pending delete instead. Caller holds `local_lock()`.
fn record_local_rename(
    ctx: &Ctx,
    p: &Project,
    kind: Kind,
    id: &str,
    rest: &[String],
    w: &mut Work,
) {
    let pid = p.pid;
    let id = id.to_string();
    let Some(new_name) = rest.last().cloned() else {
        return;
    };
    if kind == Kind::File && ignore::ignored_name(&new_name) {
        // Renamed to a temporary or backup name: how editors replace a file on save. The new
        // file under the old name (if one comes) continues the item; otherwise it is deleted.
        queue_remote_delete(ctx, p, &id, w);
        return;
    }
    let mut parent = p.root_id().to_string();
    for c in &rest[..rest.len() - 1] {
        match find_child_local(ctx, pid, &parent, c) {
            Some(it) if it.kind.is_dir() => parent = it.id,
            _ => {
                warn!("namnbyte: okänd målmapp för {}", rest.join("\\"));
                return;
            }
        }
    }
    let Ok(Some(item)) = ctx.store.item(pid, &id) else {
        return;
    };
    let moved = item.parent.as_deref() != Some(parent.as_str());
    let renamed = item.name != new_name;
    if !moved && !renamed {
        return;
    }
    let mut upd = item.clone();
    upd.name = new_name.clone();
    upd.parent = Some(parent.clone());
    if let Err(e) = ctx.store.upsert_item(&upd) {
        warn!("kunde inte spara namnbyte: {e:#}");
        return;
    }
    // The user's own rename wins over a remote one still waiting for this entry.
    let _ = ctx.store.kv_set(&move_key(pid, &id), None);
    // An ACC file-type rejection is permanent for that extension. A user who
    // changes the extension has changed the cause of the rejection, so give the
    // new type one fresh upload attempt. A same-type rename stays local only.
    let old_ext = Path::new(&item.name)
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase());
    let new_ext = Path::new(&new_name)
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase());
    let retry_new_type = kind == Kind::File
        && old_ext != new_ext
        && ctx
            .store
            .clear_unsupported_job(pid, &id)
            .is_ok_and(|removed| removed > 0);
    if item.local_state != LocalState::New {
        // The job carries intent only; it sends the row's name and parent at the time it runs.
        let is_dir = item.kind.is_dir();
        let r = if moved {
            enqueue(
                ctx,
                pid,
                "move",
                Some(&id),
                json!({ "id": id, "new_parent": parent, "new_name": new_name, "is_dir": is_dir }),
            )
        } else {
            enqueue(
                ctx,
                pid,
                "rename",
                Some(&id),
                json!({ "id": id, "new_name": new_name, "is_dir": is_dir }),
            )
        };
        if r.is_ok() {
            w.enqueued = true;
            let kind = if moved { "move" } else { "rename" };
            let detail = format!("från {}", item.name);
            ctx.ui.activity(kind, &new_name, &p.name, &detail);
        }
    }
    if retry_new_type {
        if item.local_state != LocalState::New {
            let _ = ctx.store.set_local_state(pid, &id, LocalState::Dirty);
        }
        let path = project_dir(&ctx.paths.sync_root, p).join(rest.iter().collect::<PathBuf>());
        if enqueue(
            ctx,
            pid,
            "upload",
            Some(&id),
            json!({ "path": path.to_string_lossy() }),
        )
        .is_ok()
        {
            w.enqueued = true;
        }
    }
    w.touched.insert(pid);
}

fn resume_create_or_requeue(ctx: &Ctx, pid: Pid, it: &Item, path: &Path, w: &mut Work) {
    match ctx.store.revive_create_intent(pid, &it.id) {
        Ok(true) => {
            w.enqueued = true;
            w.touched.insert(pid);
        }
        Ok(false) => requeue(ctx, pid, it, "upload", path, w),
        Err(e) => warn!("could not resume upload intent for {}: {e:#}", it.name),
    }
}

// ---------------------------------------------------------------------- reconcile

/// Once per connection, before the first rescan: what happened in the populated folders while
/// no engine listened (Windows lets deletes and renames of placeholders through then, without
/// a callback). A row whose entry is gone was deleted, unless a placeholder with its identity
/// sits elsewhere in the project: then it was renamed or moved. Both take the usual ways to
/// ACC (grace period, mass-delete hold, rename/move jobs). A missing top folder is put back,
/// never deleted, and a project whose folder is missing altogether is left alone.
fn reconcile(ctx: &Ctx) -> Work {
    let _guard = local_lock();
    let mut w = Work::default();
    for p in ctx
        .store
        .projects()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.enabled)
    {
        let pdir = project_dir(&ctx.paths.sync_root, &p);
        if !pdir.is_dir() || project_resetting(p.pid) || ctx.store.old_layout(p.pid).unwrap_or(true)
        {
            continue;
        }
        // Placeholders found on disk by the item they carry, and the rows that were looked for.
        let mut found: HashMap<String, PathBuf> = HashMap::new();
        let mut missing: Vec<Item> = Vec::new();
        let mut stack = vec![(pdir.clone(), p.root_id().to_string())];
        while let Some((dir, id)) = stack.pop() {
            if !ctx
                .store
                .item(p.pid, &id)
                .ok()
                .flatten()
                .is_some_and(|i| i.populated)
            {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut present: HashSet<String> = HashSet::new();
            for e in rd.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                present.insert(naming::fold_key(&name));
                if let Some(info) = ph::placeholder_info(&path).ok().flatten() {
                    if let Some((_, pid, iid)) = ph::decode_identity(&info.identity) {
                        if pid == p.pid {
                            found.insert(iid, path.clone());
                        }
                    }
                    if let Err(e) =
                        repair_duplicate_alias(ctx, p.pid, &id, &name, &path, &info, &mut w)
                    {
                        warn!(
                            "could not reconcile duplicate local file {}: {e:#}",
                            path.display()
                        );
                    }
                }
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    if let Some(k) =
                        find_child_local(ctx, p.pid, &id, &name).filter(|k| k.kind.is_dir())
                    {
                        stack.push((path, k.id));
                    }
                }
            }
            for k in ctx.store.children(p.pid, &id).unwrap_or_default() {
                if k.local_state != LocalState::Deleted
                    && !present.contains(&naming::name_key(&k.name))
                {
                    missing.push(k);
                }
            }
        }
        for it in missing {
            if it.parent.as_deref() == Some(PROJECT_ROOT_ID) {
                info!(
                    "{}: toppmappen {} saknades, läggs tillbaka",
                    p.name, it.name
                );
                let mut fresh = it.clone();
                fresh.populated = false;
                let _ = ctx.store.set_populated(p.pid, &it.id, false);
                let _ = ph::create_placeholders(&pdir, &[spec_for(&fresh, me(ctx).as_deref())]);
                continue;
            }
            let Some(path) = found.get(&it.id) else {
                info!("{}: {} togs bort medan motorn var stoppad", p.name, it.name);
                queue_remote_delete(ctx, &p, &it.id, &mut w);
                continue;
            };
            match rel_components(&ctx.paths.sync_root, path) {
                Some(comps) if comps.len() > 3 && project_matches(&p, &comps[0], &comps[1]) => {
                    info!("{}: {} flyttades medan motorn var stoppad", p.name, it.name);
                    record_local_rename(ctx, &p, it.kind, &it.id, &comps[2..], &mut w);
                }
                // Somewhere ACC cannot follow (another project, directly in the project folder),
                // which is refused when the engine runs: the item comes back at its place.
                _ => {
                    info!(
                        "{}: {} flyttades dit ACC inte kan följa, läggs tillbaka",
                        p.name, it.name
                    );
                    if it.kind.is_dir() {
                        let _ = ctx.store.unpopulate_subtree(p.pid, &it.id);
                    }
                    let mut fresh = it.clone();
                    fresh.populated = false;
                    if let Err(e) = create_item(ctx, &p, &fresh, me(ctx).as_deref(), &mut w) {
                        warn!("kunde inte lägga tillbaka {}: {e:#}", it.name);
                    }
                }
            }
        }
    }
    w
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Test shim: the Acc attempt with the current generation and "now" as its start.
    fn set_test_attempt(live: &mut Live, task: tokio::task::JoinHandle<()>) {
        let generation = live.sign_in_generation;
        live.sign_ins.insert(
            Provider::Acc,
            SignInAttempt {
                task,
                generation,
                started_at: Instant::now(),
                mfa_pending: false,
            },
        );
    }

    /// Test shim: moves the start of the Acc attempt (if one is pending).
    fn set_test_started(live: &mut Live, started: Instant) {
        if let Some(attempt) = live.sign_ins.get_mut(&Provider::Acc) {
            attempt.started_at = started;
        }
    }

    fn test_started(live: &Live) -> Option<Instant> {
        live.sign_ins.get(&Provider::Acc).map(|a| a.started_at)
    }

    fn login_test_live() -> Live {
        let (cf_tx, _) = unbounded_channel();
        let (w_tx, _) = unbounded_channel();
        Live {
            conn: None,
            watch: None,
            cf_tx,
            w_tx,
            sign_ins: HashMap::new(),
            sign_in_generation: 0,
            client_validation: None,
            restart_pending: false,
        }
    }

    const VALIDATION_REQUEST: &str = "82e8eba6-50d1-4be0-a81f-c84f32777725";

    fn stage_client_validation(f: &Fixture, live: &mut Live, client_id: &str) {
        let original = f.ctx.cfg_now();
        live.sign_in_generation = 19;
        // The validation runs as the Acc sign-in attempt (generation 19), started long enough ago
        // that a new click is not a burst.
        live.sign_ins.insert(
            Provider::Acc,
            SignInAttempt {
                task: tokio::spawn(std::future::pending::<()>()),
                generation: 19,
                started_at: Instant::now() - SIGN_IN_BURST - Duration::from_millis(1),
                mfa_pending: false,
            },
        );
        live.client_validation = Some(ClientIdValidation {
            request_id: VALIDATION_REQUEST.into(),
            client_id: client_id.into(),
            original_override: original.explicit_client_id_override().map(str::to_string),
            original_effective_id: original.client_id(),
        });
    }

    #[tokio::test]
    async fn resilience_client_validation_preconditions_fail_without_starting_browser() {
        for scenario in ["invalid", "busy", "signed-in"] {
            let mut f = fixture(&format!("aps-validation-precondition-{scenario}"));
            let settings = saved_client_override(&f, "old-test-client");
            if scenario == "signed-in" {
                client_override_signed_in(&mut f);
            }
            let engine = Engine::new(f.ctx.clone());
            let mut live = login_test_live();
            if scenario == "busy" {
                engine.st().maintenance_running = 1;
            }
            let (tx, mut rx) = unbounded_channel();
            let client = if scenario == "invalid" {
                "bad client"
            } else {
                "new-test-client"
            };
            engine
                .on_cmd(
                    TrayCmd::TestApsClientId {
                        client_id: client.into(),
                        request_id: VALIDATION_REQUEST.into(),
                    },
                    &mut live,
                    &tx,
                )
                .await;
            assert!(live.sign_ins.is_empty(), "started auth during {scenario}");
            assert!(live.client_validation.is_none());
            assert!(!live.restart_pending);
            assert!(rx.try_recv().is_err());
            assert_eq!(
                std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
                settings
            );
            assert!(f._rx.try_iter().any(|m| matches!(
                m,
                UiMsg::ApsClientIdValidation {
                    state: "failed",
                    error: Some(_),
                    ..
                }
            )));
        }
    }

    #[tokio::test]
    async fn resilience_client_validation_only_commits_current_success() {
        let f = fixture("aps-validation-success");
        saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        stage_client_validation(&f, &mut live, "new-test-client");
        engine
            .on_client_id_validated(19, VALIDATION_REQUEST, Ok(()), &mut live)
            .await;
        assert_eq!(
            f.ctx.cfg_now().aps_client_id.as_deref(),
            Some("new-test-client")
        );
        let saved = crate::config::Config::load(&f.ctx.paths.state_dir.join("settings.json"));
        assert_eq!(saved.aps_client_id.as_deref(), Some("new-test-client"));
        assert!(live.restart_pending);
        assert!(live.client_validation.is_none());
        assert!(f._rx.try_iter().any(|m| matches!(m,
            UiMsg::ApsClientIdValidation { request_id, state: "saved", error: None }
            if request_id == VALIDATION_REQUEST)));
    }

    #[tokio::test]
    async fn resilience_client_validation_failure_and_stale_success_preserve_account() {
        let f = fixture("aps-validation-failure");
        let settings = saved_client_override(&f, "old-test-client");
        let token = f.ctx.paths.state_dir.join("token.bin");
        std::fs::write(&token, b"test-only-unchanged-token").unwrap();
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        stage_client_validation(&f, &mut live, "new-test-client");
        engine
            .on_client_id_validated(18, VALIDATION_REQUEST, Ok(()), &mut live)
            .await;
        engine
            .on_client_id_validated(19, "a-different-operation", Ok(()), &mut live)
            .await;
        assert!(live.client_validation.is_some());
        assert!(f
            ._rx
            .try_iter()
            .all(|m| !matches!(m, UiMsg::ApsClientIdValidation { .. })));
        engine
            .on_client_id_validated(
                19,
                VALIDATION_REQUEST,
                Err(anyhow!("HTTP 401: invalid_client")),
                &mut live,
            )
            .await;
        assert!(!live.restart_pending);
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
            settings
        );
        assert_eq!(std::fs::read(token).unwrap(), b"test-only-unchanged-token");
        assert!(f._rx.try_iter().any(|m| matches!(m,
            UiMsg::ApsClientIdValidation { state: "failed", error: Some(error), .. }
            if error.contains("invalid_client"))));
    }

    #[tokio::test]
    async fn resilience_client_validation_cancel_releases_listener_and_discards_completion() {
        let f = fixture("aps-validation-cancel");
        let settings = saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        stage_client_validation(&f, &mut live, "new-test-client");
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        set_test_attempt(
            &mut live,
            tokio::spawn(async move {
                let _listener = listener;
                std::future::pending::<()>().await;
            }),
        );
        let (tx, _) = unbounded_channel();
        engine
            .on_cmd(
                TrayCmd::CancelApsClientIdTest {
                    request_id: "unrelated".into(),
                },
                &mut live,
                &tx,
            )
            .await;
        assert!(live.client_validation.is_some());
        engine
            .on_cmd(
                TrayCmd::CancelApsClientIdTest {
                    request_id: VALIDATION_REQUEST.into(),
                },
                &mut live,
                &tx,
            )
            .await;
        let _rebound = tokio::net::TcpListener::bind(address)
            .await
            .expect("cancel retained callback listener");
        engine
            .on_client_id_validated(19, VALIDATION_REQUEST, Ok(()), &mut live)
            .await;
        assert!(!live.restart_pending);
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
            settings
        );
        assert!(f._rx.try_iter().any(|m| matches!(
            m,
            UiMsg::ApsClientIdValidation {
                state: "cancelled",
                ..
            }
        )));
    }

    #[tokio::test]
    async fn resilience_client_validation_rechecks_busy_and_original_identity() {
        for scenario in ["busy", "settings-changed", "save-failed"] {
            let f = fixture(&format!("aps-validation-{scenario}"));
            let original_bytes = saved_client_override(&f, "old-test-client");
            let engine = Engine::new(f.ctx.clone());
            let mut live = login_test_live();
            stage_client_validation(&f, &mut live, "new-test-client");
            match scenario {
                "busy" => engine.st().maintenance_running = 1,
                "settings-changed" => {
                    saved_client_override(&f, "other-test-client");
                }
                "save-failed" => {
                    std::fs::remove_file(f.ctx.paths.state_dir.join("settings.json")).unwrap();
                    std::fs::create_dir(f.ctx.paths.state_dir.join("settings.json")).unwrap();
                }
                _ => unreachable!(),
            }
            engine
                .on_client_id_validated(19, VALIDATION_REQUEST, Ok(()), &mut live)
                .await;
            assert!(!live.restart_pending, "restarted for {scenario}");
            let expected = if scenario == "settings-changed" {
                "other-test-client"
            } else {
                "old-test-client"
            };
            assert_eq!(f.ctx.cfg_now().aps_client_id.as_deref(), Some(expected));
            if scenario == "busy" {
                assert_eq!(
                    std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
                    original_bytes
                );
            }
            let events = f._rx.try_iter().collect::<Vec<_>>();
            assert!(
                events.iter().any(|m| matches!(
                    m,
                    UiMsg::ApsClientIdValidation {
                        state: "failed",
                        ..
                    }
                )),
                "{scenario}"
            );
            assert!(
                !events
                    .iter()
                    .any(|m| matches!(m, UiMsg::ApsClientIdValidation { state: "saved", .. })),
                "{scenario}"
            );
        }
    }

    #[tokio::test]
    async fn resilience_client_validation_same_identity_requires_real_success_without_restart() {
        let f = fixture("aps-validation-same");
        saved_client_override(&f, "same-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        stage_client_validation(&f, &mut live, "same-test-client");
        let (tx, mut rx) = unbounded_channel();
        engine
            .on_cmd(
                TrayCmd::TestApsClientId {
                    client_id: " same-test-client ".into(),
                    request_id: VALIDATION_REQUEST.into(),
                },
                &mut live,
                &tx,
            )
            .await;
        assert!(
            rx.try_recv().is_err(),
            "a repeated request started another browser test"
        );
        let events = f._rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|m| matches!(
            m,
            UiMsg::ApsClientIdValidation {
                state: "testing",
                ..
            }
        )));
        assert!(!events
            .iter()
            .any(|m| matches!(m, UiMsg::ApsClientIdValidation { state: "saved", .. })));
        engine
            .on_client_id_validated(19, VALIDATION_REQUEST, Ok(()), &mut live)
            .await;
        assert!(!live.restart_pending);
        assert!(f
            ._rx
            .try_iter()
            .any(|m| matches!(m, UiMsg::ApsClientIdValidation { state: "saved", .. })));
    }

    #[tokio::test]
    async fn resilience_client_validation_burst_does_not_replace_current_browser() {
        let f = fixture("aps-validation-burst");
        saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        stage_client_validation(&f, &mut live, "new-test-client");
        set_test_attempt(&mut live, tokio::spawn(std::future::pending::<()>()));
        set_test_started(&mut live, Instant::now());
        let (tx, mut rx) = unbounded_channel();
        for operation in 0..50 {
            engine
                .on_cmd(
                    TrayCmd::TestApsClientId {
                        client_id: "other-test-client".into(),
                        request_id: format!("82e8eba6-50d1-4be0-a81f-{operation:012x}"),
                    },
                    &mut live,
                    &tx,
                )
                .await;
        }
        assert_eq!(live.sign_in_generation, 19);
        assert_eq!(
            live.client_validation.as_ref().unwrap().request_id,
            VALIDATION_REQUEST
        );
        assert!(!live.sign_ins[&Provider::Acc].task.is_finished());
        assert!(rx.try_recv().is_err());
        assert_eq!(
            f._rx
                .try_iter()
                .filter(|m| matches!(
                    m,
                    UiMsg::ApsClientIdValidation {
                        state: "failed",
                        ..
                    }
                ))
                .count(),
            50
        );
        engine.cancel_all_sign_ins(&mut live).await;
    }

    #[tokio::test]
    async fn resilience_client_validation_unknown_cancel_completes_without_touching_other_test() {
        let f = fixture("aps-validation-unknown-cancel");
        let settings = saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        let (tx, _) = unbounded_channel();
        engine
            .on_cmd(
                TrayCmd::CancelApsClientIdTest {
                    request_id: VALIDATION_REQUEST.into(),
                },
                &mut live,
                &tx,
            )
            .await;
        assert!(f._rx.try_iter().any(|m| matches!(m,
            UiMsg::ApsClientIdValidation { request_id, state: "cancelled", error: None }
            if request_id == VALIDATION_REQUEST)));
        stage_client_validation(&f, &mut live, "new-test-client");
        set_test_attempt(&mut live, tokio::spawn(std::future::pending::<()>()));
        engine
            .on_cmd(
                TrayCmd::CancelApsClientIdTest {
                    request_id: "82e8eba6-50d1-4be0-a81f-000000000001".into(),
                },
                &mut live,
                &tx,
            )
            .await;
        assert_eq!(live.sign_in_generation, 19);
        assert_eq!(
            live.client_validation.as_ref().unwrap().request_id,
            VALIDATION_REQUEST
        );
        assert!(!live.sign_ins[&Provider::Acc].task.is_finished());
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
            settings
        );
        engine.cancel_all_sign_ins(&mut live).await;
    }

    #[tokio::test]
    async fn resilience_client_validation_late_cancel_preserves_saved_outcome() {
        let f = fixture("aps-validation-late-cancel");
        saved_client_override(&f, "same-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        stage_client_validation(&f, &mut live, "same-test-client");
        engine
            .on_client_id_validated(19, VALIDATION_REQUEST, Ok(()), &mut live)
            .await;
        let (tx, _) = unbounded_channel();
        engine
            .on_cmd(
                TrayCmd::CancelApsClientIdTest {
                    request_id: VALIDATION_REQUEST.into(),
                },
                &mut live,
                &tx,
            )
            .await;
        let outcome = f
            .ctx
            .ui
            .hub
            .snapshot()
            .into_iter()
            .map(|line| serde_json::from_str::<Value>(&line).unwrap())
            .find(|event| {
                event["type"] == "aps_client_id_validation"
                    && event["request_id"] == VALIDATION_REQUEST
            })
            .unwrap();
        assert_eq!(outcome["state"], "saved");
        assert!(!live.restart_pending);
        assert_eq!(
            f.ctx.cfg_now().aps_client_id.as_deref(),
            Some("same-test-client")
        );
        assert!(!f._rx.try_iter().any(|m| matches!(
            m,
            UiMsg::ApsClientIdValidation {
                state: "cancelled",
                ..
            }
        )));
    }

    #[tokio::test]
    async fn resilience_client_validation_rejects_unvalidated_generic_command() {
        let f = fixture("aps-validation-bypass");
        let settings = saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        let (tx, mut rx) = unbounded_channel();
        engine
            .on_cmd(
                TrayCmd::SetSetting {
                    key: "aps_client_id".into(),
                    value: json!("new-test-client"),
                },
                &mut live,
                &tx,
            )
            .await;
        assert!(!live.restart_pending);
        assert!(live.client_validation.is_none());
        assert!(rx.try_recv().is_err());
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
            settings
        );
    }

    fn saved_client_override(f: &Fixture, id: &str) -> Vec<u8> {
        let mut config = f.ctx.cfg_now();
        config.aps_client_id = Some(id.into());
        let path = f.ctx.paths.state_dir.join("settings.json");
        config.save(&path).unwrap();
        *f.ctx.settings.write().unwrap() = config;
        std::fs::read(path).unwrap()
    }

    pub(crate) fn client_override_signed_in(f: &mut Fixture) {
        let state = f.ctx.paths.state_dir.clone();
        std::fs::write(state.join("token.bin"), b"test-only-existing-credentials").unwrap();
        std::fs::write(
            state.join("user.json"),
            serde_json::to_vec(&UserInfo {
                user_id: "test-only-user".into(),
                name: "Test User".into(),
                email: "test@example.invalid".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let http = crate::http::build_client();
        let auth = Arc::new(crate::auth::Auth::new(
            f.ctx.cfg_now().client_id(),
            http.clone(),
            &state,
        ));
        let ctx = Arc::get_mut(&mut f.ctx).unwrap();
        ctx.api = crate::http::Api::new(http, auth.clone());
        ctx.auth = auth;
        assert!(ctx.auth.is_signed_in());
    }

    #[tokio::test]
    async fn resilience_client_override_requires_explicit_sign_out() {
        let mut f = fixture("aps-signed-in");
        let settings = saved_client_override(&f, "old-test-client");
        client_override_signed_in(&mut f);
        let token = std::fs::read(f.ctx.paths.state_dir.join("token.bin")).unwrap();
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();

        assert!(
            !engine
                .set_client_id(&json!("new-test-client"), &mut live)
                .await
        );
        assert!(f.ctx.auth.is_signed_in());
        assert!(!live.restart_pending);
        assert_eq!(
            f.ctx.cfg_now().aps_client_id.as_deref(),
            Some("old-test-client")
        );
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
            settings
        );
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("token.bin")).unwrap(),
            token
        );
        assert!(f
            ._rx
            .try_iter()
            .any(|message| matches!(message, UiMsg::Notify { body, .. }
            if body.contains("APS") && body.contains("Logga ut"))));
    }

    #[tokio::test]
    async fn resilience_client_override_refuses_active_work_and_preserves_settings() {
        for kind in [
            "poll",
            "download",
            "outbox",
            "move",
            "install",
            "maintenance",
            "free-up",
            "reset",
        ] {
            let f = fixture(&format!("aps-busy-{kind}"));
            let settings = saved_client_override(&f, "old-test-client");
            let engine = Engine::new(f.ctx.clone());
            let mut live = login_test_live();
            let free_up = (kind == "free-up")
                .then(|| engine.free_up_sem.clone().try_acquire_owned().unwrap());
            let reset = (kind == "reset")
                .then(|| reset::Guard::new(vec![(f.dir.clone(), false)], vec![f.p.pid]));
            {
                let mut state = engine.st();
                match kind {
                    "poll" => {
                        state.polling.insert(f.p.pid);
                    }
                    "download" => {
                        state.downloads.insert(1, Vec::new());
                    }
                    "outbox" => state.outbox_running = true,
                    "move" => state.moving = true,
                    "install" => state.installing = true,
                    "maintenance" => state.maintenance_running = 1,
                    _ => {}
                }
            }
            assert!(
                !engine
                    .set_client_id(&json!("new-test-client"), &mut live)
                    .await,
                "accepted during {kind}"
            );
            assert!(!live.restart_pending);
            assert_eq!(
                f.ctx.cfg_now().aps_client_id.as_deref(),
                Some("old-test-client")
            );
            assert_eq!(
                std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
                settings
            );
            assert!(f._rx.try_iter().any(
                |message| matches!(message, UiMsg::Notify { body, .. } if body.contains("APS"))
            ));
            drop(reset);
            drop(free_up);
        }
    }

    #[tokio::test]
    async fn resilience_client_override_tracks_queued_maintenance_before_first_poll() {
        let f = fixture("aps-queued-work");
        saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        let (release, wait) = tokio::sync::oneshot::channel();
        engine.spawn_maintenance(async move {
            let _ = wait.await;
        });
        assert_eq!(engine.st().maintenance_running, 1);
        assert!(
            !engine
                .set_client_id(&json!("new-test-client"), &mut live)
                .await
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while engine.st().maintenance_running != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            engine
                .set_client_id(&json!("new-test-client"), &mut live)
                .await
        );
        assert!(live.restart_pending);
    }

    #[tokio::test]
    async fn resilience_client_override_cancels_login_and_keeps_project_files_and_jobs() {
        let f = fixture("aps-pending-login");
        saved_client_override(&f, "old-test-client");
        let local = f.dir.join("unsynced-private-test.bin");
        std::fs::write(&local, b"local work must remain untouched").unwrap();
        let job = f.ctx.store.enqueue(f.p.pid, "upload", None, "{}").unwrap();
        std::fs::write(
            f.ctx.paths.state_dir.join("token.bin"),
            b"legacy-token-without-profile",
        )
        .unwrap();
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        live.sign_in_generation = 20;
        engine.st().signing_in.insert(Provider::Acc);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        set_test_attempt(
            &mut live,
            tokio::spawn(async move {
                let _listener = listener;
                let _ = ready_tx.send(());
                std::future::pending::<()>().await;
            }),
        );
        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();

        assert!(
            engine
                .set_client_id(&json!(" new-test-client "), &mut live)
                .await
        );
        assert!(live.restart_pending);
        assert!(live.sign_ins.is_empty());
        assert!(engine.st().signing_in.is_empty());
        // Cancelling removes the attempt; the counter only moves when a new attempt starts.
        assert_eq!(live.sign_in_generation, 20);
        let _rebound = tokio::net::TcpListener::bind(address)
            .await
            .expect("APS change left the old callback bound");
        assert!(!f.ctx.paths.state_dir.join("token.bin").exists());
        assert!(!f.ctx.paths.state_dir.join("user.json").exists());
        engine
            .on_signed_in(
                Provider::Acc,
                20,
                Err(anyhow!("stale old-client completion")),
                &mut live,
                &unbounded_channel().0,
            )
            .await;
        assert!(engine.st().last_error.is_none());
        let (tx, mut rx) = unbounded_channel();
        engine
            .on_cmd(
                TrayCmd::SignIn {
                    provider: Provider::Acc,
                    streambim: None,
                },
                &mut live,
                &tx,
            )
            .await;
        engine.on_cmd(TrayCmd::InstallUpdate, &mut live, &tx).await;
        assert!(
            live.sign_ins.is_empty(),
            "restart accepted an old-client sign-in"
        );
        assert_eq!(
            engine.st().maintenance_running,
            0,
            "restart accepted another operation"
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            std::fs::read(local).unwrap(),
            b"local work must remain untouched"
        );
        assert!(f.ctx.store.project(f.p.pid).unwrap().unwrap().enabled);
        assert_eq!(f.ctx.store.due_jobs(now_ms(), 10).unwrap()[0].id, job);
        let saved = crate::config::Config::load(&f.ctx.paths.state_dir.join("settings.json"));
        assert_eq!(saved.aps_client_id.as_deref(), Some("new-test-client"));
    }

    #[tokio::test]
    async fn resilience_client_override_save_failure_preserves_reported_and_saved_setting() {
        let f = fixture("aps-save-failure");
        let settings = saved_client_override(&f, "old-test-client");
        std::fs::create_dir(f.ctx.paths.state_dir.join("settings.tmp")).unwrap();
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        assert!(
            !engine
                .set_client_id(&json!("new-test-client"), &mut live)
                .await
        );
        assert!(!live.restart_pending);
        assert_eq!(
            f.ctx.cfg_now().aps_client_id.as_deref(),
            Some("old-test-client")
        );
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
            settings
        );
        let messages: Vec<_> = f._rx.try_iter().collect();
        assert!(messages.iter().any(|message| matches!(message, UiMsg::Settings(s)
            if s.aps_client_id.as_deref() == Some("old-test-client") && s.aps_client_id_override_supported)));
        assert!(messages.iter().any(
            |message| matches!(message, UiMsg::Notify { severity: Severity::Error, body, .. }
            if body.contains("APS") && !body.contains("new-test-client"))
        ));
    }

    #[tokio::test]
    async fn resilience_client_override_cleanup_failure_cannot_save_or_restart() {
        let f = fixture("aps-cleanup-failure");
        let settings = saved_client_override(&f, "old-test-client");
        std::fs::create_dir(f.ctx.paths.state_dir.join("token.bin")).unwrap();
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        assert!(
            !engine
                .set_client_id(&json!("new-test-client"), &mut live)
                .await
        );
        assert!(!live.restart_pending);
        assert_eq!(
            f.ctx.cfg_now().aps_client_id.as_deref(),
            Some("old-test-client")
        );
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("settings.json")).unwrap(),
            settings
        );
        assert!(f.ctx.paths.state_dir.join("token.bin").is_dir());
        assert!(f._rx.try_iter().any(
            |message| matches!(message, UiMsg::Notify { severity: Severity::Error, body, .. }
            if body.contains("APS") && body.contains("rensa"))
        ));
    }

    #[tokio::test]
    async fn resilience_client_override_normalized_noop_preserves_session_and_login_task() {
        let mut f = fixture("aps-noop");
        saved_client_override(&f, " old-test-client ");
        client_override_signed_in(&mut f);
        let token = std::fs::read(f.ctx.paths.state_dir.join("token.bin")).unwrap();
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        live.sign_in_generation = 8;
        set_test_attempt(&mut live, tokio::spawn(std::future::pending()));
        engine.st().outbox_running = true;
        assert!(
            !engine
                .set_client_id(&json!(" \told-test-client\n"), &mut live)
                .await
        );
        assert!(!live.restart_pending);
        assert_eq!(live.sign_in_generation, 8);
        assert!(!live.sign_ins.is_empty());
        assert!(f.ctx.auth.is_signed_in());
        assert_eq!(
            std::fs::read(f.ctx.paths.state_dir.join("token.bin")).unwrap(),
            token
        );
        let saved = crate::config::Config::load(&f.ctx.paths.state_dir.join("settings.json"));
        assert_eq!(saved.aps_client_id.as_deref(), Some("old-test-client"));
        engine.cancel_all_sign_ins(&mut live).await;
    }

    #[tokio::test]
    async fn resilience_client_override_reset_restores_fallback_and_persists_null() {
        let f = fixture("aps-reset");
        saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        assert!(engine.set_client_id(&json!(" \t\r\n"), &mut live).await);
        assert!(live.restart_pending);
        let path = f.ctx.paths.state_dir.join("settings.json");
        let saved = crate::config::Config::load(&path);
        assert!(saved.aps_client_id.is_none());
        assert!(saved.client_id() == crate::config::Config::default().client_id());
        let json: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(json["aps_client_id"].is_null());
    }

    #[tokio::test]
    async fn resilience_client_override_signed_out_deferred_work_keeps_restart_markers() {
        let f = fixture("aps-deferred");
        saved_client_override(&f, "old-test-client");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        engine.schedule_refresh_after(f.p.pid, "deferred-item".into(), Duration::ZERO);
        f.ctx
            .store
            .kv_set(
                &move_key(f.p.pid, "deferred-item"),
                Some(r#"{"name":"later.bin","parent":"co.ROOT"}"#),
            )
            .unwrap();
        engine.schedule_move(f.p.pid, "deferred-item".into());
        assert!(!engine.st().refreshing.is_empty());
        assert!(
            engine
                .set_client_id(&json!("new-test-client"), &mut live)
                .await
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !engine.st().refreshing.is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let key = (f.p.pid, "deferred-item".to_string());
        assert!(startup_refreshes(&f.ctx.store).contains(&key));
        assert!(pending_moves(&f.ctx).contains(&key));
        assert_eq!(engine.st().maintenance_running, 0);
    }

    #[tokio::test]
    async fn resilience_login_click_burst_keeps_one_attempt_then_allows_retry() {
        let mut f = fixture("login-burst");
        let auth = Arc::new(crate::auth::Auth::new(
            String::new(),
            crate::http::build_client(),
            &f.ctx.paths.state_dir,
        ));
        Arc::get_mut(&mut f.ctx).unwrap().auth = auth;
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        live.sign_in_generation = 5;
        let started = Instant::now();
        engine.st().signing_in.insert(Provider::Acc);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        set_test_attempt(
            &mut live,
            tokio::spawn(async move {
                let _listener = listener;
                let _ = ready_tx.send(());
                std::future::pending::<()>().await;
            }),
        );
        ready_rx.await.unwrap();
        set_test_started(&mut live, started);
        let (tx, mut rx) = unbounded_channel();

        for _ in 0..20 {
            engine
                .on_cmd(
                    TrayCmd::SignIn {
                        provider: Provider::Acc,
                        streambim: None,
                    },
                    &mut live,
                    &tx,
                )
                .await;
        }
        assert_eq!(
            live.sign_in_generation, 5,
            "a click burst replaced the attempt"
        );
        assert_eq!(test_started(&live), Some(started));
        assert!(!live.sign_ins[&Provider::Acc].task.is_finished());
        assert!(
            tokio::net::TcpListener::bind(address).await.is_err(),
            "the first browser's callback listener was dropped during the burst"
        );
        assert!(
            rx.try_recv().is_err(),
            "a burst started another authentication flow"
        );

        // Model a deliberate click after the debounce interval without a wall-clock sleep.
        set_test_started(
            &mut live,
            Instant::now() - SIGN_IN_BURST - Duration::from_millis(1),
        );
        engine
            .on_cmd(
                TrayCmd::SignIn {
                    provider: Provider::Acc,
                    streambim: None,
                },
                &mut live,
                &tx,
            )
            .await;
        assert_eq!(live.sign_in_generation, 6);
        let _rebound = tokio::net::TcpListener::bind(address)
            .await
            .expect("a deliberate retry did not release the original listener");
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the deliberate retry was ignored")
            .unwrap()
        {
            Internal::SignedIn {
                provider,
                generation,
                result,
            } => {
                assert_eq!(generation, 6);
                assert!(result.is_err(), "the empty client ID must fail locally");
                engine
                    .on_signed_in(
                        provider,
                        generation,
                        result,
                        &mut live,
                        &unbounded_channel().0,
                    )
                    .await;
                assert!(test_started(&live).is_none());
                assert!(live.sign_ins.is_empty());
            }
            Internal::ClientIdValidated { .. } => panic!("unexpected APS validation completion"),
            Internal::SignInExpired { .. } => panic!("no MFA wait in this test"),
            Internal::Quit => panic!("a retry unexpectedly stopped the engine"),
        }
    }

    #[tokio::test]
    async fn resilience_login_retry_releases_old_listener_and_reenters_auth() {
        let mut f = fixture("login-retry");
        // A missing client ID exercises the real Auth error path without launching
        // a browser or contacting Autodesk. A token without a user is not a session.
        let auth = Arc::new(crate::auth::Auth::new(
            String::new(),
            crate::http::build_client(),
            &f.ctx.paths.state_dir,
        ));
        Arc::get_mut(&mut f.ctx).unwrap().auth = auth;
        std::fs::write(
            f.ctx.paths.state_dir.join("token.bin"),
            b"private test token",
        )
        .unwrap();
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        live.sign_in_generation = 7;
        engine.st().signing_in.insert(Provider::Acc);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        set_test_attempt(
            &mut live,
            tokio::spawn(async move {
                let _listener = listener;
                let _ = ready_tx.send(());
                std::future::pending::<()>().await;
            }),
        );
        ready_rx.await.unwrap();
        // The pending attempt started before the burst window: this click replaces it.
        set_test_started(
            &mut live,
            Instant::now() - SIGN_IN_BURST - Duration::from_millis(1),
        );

        let (tx, mut rx) = unbounded_channel();
        engine
            .on_cmd(
                TrayCmd::SignIn {
                    provider: Provider::Acc,
                    streambim: None,
                },
                &mut live,
                &tx,
            )
            .await;
        let _rebound = tokio::net::TcpListener::bind(address)
            .await
            .expect("retry left the old OAuth callback listener bound");
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("retry silently ignored the new click")
            .expect("login completion channel closed");
        match message {
            Internal::SignedIn {
                provider,
                generation,
                result,
            } => {
                assert_eq!(generation, 8);
                assert!(result.is_err(), "the empty client ID must fail locally");
                assert!(!engine.st().signing_in.is_empty());
                engine
                    .on_signed_in(
                        provider,
                        generation,
                        result,
                        &mut live,
                        &unbounded_channel().0,
                    )
                    .await;
                assert!(engine.st().signing_in.is_empty());
                assert!(live.sign_ins.is_empty());
            }
            Internal::ClientIdValidated { .. } => panic!("unexpected APS validation completion"),
            Internal::SignInExpired { .. } => panic!("no MFA wait in this test"),
            Internal::Quit => panic!("retry unexpectedly stopped the engine"),
        }
    }

    #[tokio::test]
    async fn resilience_login_logout_cancels_pending_task_before_removing_tokens() {
        let f = fixture("login-logout");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        live.sign_in_generation = 3;
        engine.st().signing_in.insert(Provider::Acc);
        let token = f.ctx.paths.state_dir.join("token.bin");
        std::fs::write(&token, b"private existing test token").unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let late_token = token.clone();
        set_test_attempt(
            &mut live,
            tokio::spawn(async move {
                let _listener = listener;
                let _ = ready_tx.send(());
                let _ = wait.await;
                std::fs::write(late_token, b"late credentials must never be committed").unwrap();
            }),
        );
        ready_rx.await.unwrap();

        let (tx, _) = unbounded_channel();
        engine
            .on_cmd(TrayCmd::SignOut { provider: None }, &mut live, &tx)
            .await;
        assert!(
            release.send(()).is_err(),
            "logout left the sign-in worker alive"
        );
        assert!(
            !token.exists(),
            "a late sign-in restored credentials after logout"
        );
        assert!(!f.ctx.auth.is_signed_in());
        assert!(engine.st().signing_in.is_empty());
        assert_eq!(live.sign_in_generation, 3);
        assert!(live.sign_ins.is_empty());
        assert!(test_started(&live).is_none());
        let _rebound = tokio::net::TcpListener::bind(address)
            .await
            .expect("logout did not release the pending callback listener");
    }

    #[tokio::test]
    async fn resilience_login_stale_completion_cannot_clear_replacement_attempt() {
        let f = fixture("login-stale");
        let engine = Engine::new(f.ctx.clone());
        let mut live = login_test_live();
        live.sign_in_generation = 9;
        let started = Instant::now();
        set_test_attempt(&mut live, tokio::spawn(std::future::pending()));
        set_test_started(&mut live, started);
        engine.st().signing_in.insert(Provider::Acc);

        engine
            .on_signed_in(
                Provider::Acc,
                8,
                Err(anyhow!("error from the cancelled attempt")),
                &mut live,
                &unbounded_channel().0,
            )
            .await;
        assert!(
            !engine.st().signing_in.is_empty(),
            "old completion cleared the new attempt"
        );
        assert!(
            engine.st().last_error.is_none(),
            "old failure replaced the current status"
        );
        assert!(
            !live.sign_ins.is_empty(),
            "old completion detached the active worker"
        );
        assert_eq!(test_started(&live), Some(started));
        assert!(
            f._rx.try_recv().is_err(),
            "old failure was published to the UI"
        );
        engine.cancel_all_sign_ins(&mut live).await;
    }

    #[test]
    fn resilience_login_pending_status_is_visible_without_a_new_protocol_state() {
        let f = fixture("login-status");
        let engine = Engine::new(f.ctx.clone());
        engine.st().signing_in.insert(Provider::Acc);
        engine.publish_now();
        match f._rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            UiMsg::Status(status) => {
                assert_eq!(status.state, "signed_out");
                assert!(status.user.is_none());
                assert_eq!(status.text, "Väntar på inloggning i webbläsaren…");
            }
            _ => panic!("expected a status message"),
        }
    }

    #[test]
    fn blocked_remote_refresh_survives_a_restart() {
        let f = fixture("refresh-restart");
        let id = "urn:adsk.wipemea:dm.lineage:example";
        let key = refresh_key(f.p.pid, id);
        let engine = Engine::new(f.ctx.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            engine.schedule_refresh(f.p.pid, id.to_string());
            engine.schedule_refresh(f.p.pid, id.to_string());
            assert_eq!(
                engine.st().refreshing.get(&(f.p.pid, id.to_string())),
                Some(&true)
            );
        });
        drop(rt);
        let reopened = Store::open(&f.ctx.paths.db_file).unwrap();
        assert_eq!(
            pending_refreshes(&reopened),
            vec![(f.p.pid, id.to_string())],
        );
        let pin_key = pin_restore_key(f.p.pid, id);
        let warning_key = pin_update_warning_key(f.p.pid, id);
        reopened.kv_set(&pin_key, Some("pinned")).unwrap();
        reopened.kv_set(&warning_key, Some("1")).unwrap();
        // Every marker survives on its own, and together they schedule just one refresh.
        assert_eq!(
            pending_refreshes(&reopened),
            vec![(f.p.pid, id.to_string())],
        );
        reopened.kv_set(&key, None).unwrap();
        assert_eq!(
            pending_refreshes(&reopened),
            vec![(f.p.pid, id.to_string())],
        );
        reopened.kv_set(&pin_key, None).unwrap();
        assert_eq!(
            pending_refreshes(&reopened),
            vec![(f.p.pid, id.to_string())],
        );
        // The item was removed while the app was down. Its warning must not remain in Overview.
        assert!(refresh_placeholder(&f.ctx, f.p.pid, id));
        assert!(reopened.kv_get(&warning_key).unwrap().is_none());
        assert!(pending_refreshes(&reopened).is_empty());
    }

    #[test]
    fn startup_recovers_stale_visible_remote_file_without_a_marker() {
        let f = fixture("stale-file-recovery");
        let mut item = row(&f.p, "co.STALE", Some("co.ROOT"), "stale.bin", Kind::File);
        item.base_version = Some(1);
        item.remote_version = Some(2);
        f.ctx.store.upsert_item(&item).unwrap();
        assert_eq!(
            startup_refreshes(&f.ctx.store),
            vec![(f.p.pid, item.id.clone())]
        );
        // If it vanished locally before restart, no old bytes remain to protect.
        assert!(refresh_placeholder(&f.ctx, f.p.pid, &item.id));
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, &item.id)
                .unwrap()
                .unwrap()
                .base_version,
            item.remote_version
        );
        assert!(startup_refreshes(&f.ctx.store).is_empty());
        item.base_version = None;
        f.ctx.store.upsert_item(&item).unwrap();
        assert!(startup_refreshes(&f.ctx.store).is_empty());
        assert!(fetch_would_mix_conflict_versions(&item));
    }

    #[test]
    fn freshly_listed_file_gets_its_callback_placeholder_base() {
        let f = fixture("callback-placeholder-base");
        let mut item = row(&f.p, "co.FRESH", Some("co.ROOT"), "fresh.bin", Kind::File);
        item.base_version = None;
        f.ctx.store.upsert_item(&item).unwrap();
        let engine = Engine::new(f.ctx.clone());
        let req = FetchPlaceholdersReq {
            keys: keys(),
            path: f.dir.clone(),
            identity: ph::encode_identity(Kind::Folder, f.p.pid, "co.ROOT"),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listing = rt.block_on(engine.build_listing(&req)).unwrap();
        let (_, id, version) = listing.file_versions.into_iter().flatten().next().unwrap();
        assert_eq!((&id, version), (&item.id, item.remote_version));
        // The callback reports success for this newly created entry; the same CAS runs in
        // fetch_placeholders before it lets a pinned-folder hydration start.
        f.ctx
            .store
            .set_base_if_unset_at_tip(f.p.pid, &id, version)
            .unwrap();
        let hydrated_row = f.ctx.store.item(f.p.pid, &id).unwrap().unwrap();
        assert!(!fetch_would_mix_conflict_versions(&hydrated_row));
    }

    #[test]
    fn pin_restore_marker_never_pins_a_replacement_file() {
        let f = fixture("pin-replacement-guard");
        let id = "co.ORIGINAL";
        let path = f.dir.join("replacement.bin");
        std::fs::write(&path, b"other file").unwrap();
        let key = pin_restore_key(f.p.pid, id);
        f.ctx.store.kv_set(&key, Some("pinned")).unwrap();
        retry_pin_restore(&f.ctx, f.p.pid, id, &path).unwrap();
        assert!(f.ctx.store.kv_get(&key).unwrap().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"other file");
    }

    #[test]
    fn status_lines() {
        use chrono::{DateTime, Local, TimeZone};
        let now = Local.with_ymd_and_hms(2026, 10, 16, 15, 0, 0).unwrap();
        let ms = |t: DateTime<Local>| t.timestamp_millis();
        let today = Local.with_ymd_and_hms(2026, 10, 16, 14, 32, 0).unwrap();
        let yesterday = Local.with_ymd_and_hms(2026, 10, 15, 14, 32, 0).unwrap();
        let older = Local.with_ymd_and_hms(2026, 10, 12, 9, 5, 0).unwrap();
        assert_eq!(synced_text(None, now), "Senast synkad: aldrig");
        assert_eq!(synced_text(Some(ms(today)), now), "Senast synkad 14:32");
        assert_eq!(
            synced_text(Some(ms(yesterday)), now),
            "Senast synkad i går 14:32"
        );
        assert_eq!(
            synced_text(Some(ms(older)), now),
            "Senast synkad 2026-10-12 09:05"
        );
        assert_eq!(syncing_text(2, 1), "Hämtar 2 filer, laddar upp 1");
        assert_eq!(syncing_text(1, 0), "Hämtar 1 fil");
        assert_eq!(syncing_text(0, 0), "Synkar…");
        assert_eq!(waiting_text(1), "1 ändring väntar");
        assert_eq!(waiting_text(4), "4 ändringar väntar");
        assert_eq!(offline_text(Some(40)), "Försöker igen om 40 s");
        assert_eq!(offline_text(Some(0)), "Ingen anslutning till ACC");
        assert_eq!(offline_text(None), "Ingen anslutning till ACC");
    }

    #[test]
    fn sync_phase_distinguishes_active_work_from_queued_jobs() {
        assert_eq!(sync_phase(true, 0, 0, false, false), SyncPhase::Active);
        assert_eq!(sync_phase(true, 1, 1, false, false), SyncPhase::Active);
        assert_eq!(sync_phase(false, 1, 1, false, false), SyncPhase::Waiting);
        assert_eq!(sync_phase(false, 0, 0, true, false), SyncPhase::Waiting);
        assert_eq!(
            sync_phase(false, 0, 1, false, false),
            SyncPhase::NeedsAttention
        );
        assert_eq!(
            sync_phase(false, 0, 0, false, true),
            SyncPhase::NeedsAttention
        );
        assert_eq!(
            sync_phase(false, 1, 0, true, true),
            SyncPhase::NeedsAttention
        );
        assert_eq!(sync_phase(false, 0, 0, false, false), SyncPhase::UpToDate);
    }

    #[test]
    fn conflict_copy_requires_every_local_byte_before_losing_remote_identity() {
        let mut info = PlaceholderInfo {
            identity: vec![],
            in_sync: false,
            pin_state: PinState::Inherit,
            hydrated: false,
            on_disk_size: 0,
            modified_size: 0,
        };
        assert!(!conflict_copy_has_all_bytes(&info, 1024));
        info.on_disk_size = 512;
        assert!(!conflict_copy_has_all_bytes(&info, 1024));
        // A stale or inconsistent recall flag must not override missing ranges.
        info.hydrated = true;
        assert!(!conflict_copy_has_all_bytes(&info, 1024));
        info.on_disk_size = 1024;
        assert!(conflict_copy_has_all_bytes(&info, 1024));
        info.hydrated = false;
        assert!(!conflict_copy_has_all_bytes(&info, 1024));
        assert!(conflict_copy_has_all_bytes(&info, 0));
    }

    #[test]
    fn failed_conflict_detach_restores_original_name_before_creating_a_job() {
        let f = fixture("failed-conflict-detach");
        let original = f.dir.join("model.rvt");
        let conflict = f.dir.join("model (konflikt).rvt");
        std::fs::write(&original, b"local bytes").unwrap();
        let error = rename_and_detach_conflict(
            &original,
            &conflict,
            Some(b"new identity"),
            |_destination, _identity| Err(anyhow!("simulated CFAPI refusal")),
        )
        .unwrap_err();
        let failure = error.downcast_ref::<ConflictDetachFailed>().unwrap();
        assert!(!failure.rollback_failed);
        assert_eq!(std::fs::read(&original).unwrap(), b"local bytes");
        assert!(!conflict.exists());
        assert_eq!(f.ctx.store.job_counts().unwrap(), (0, 0));
    }

    #[test]
    fn fetch_does_not_mix_new_tip_into_dirty_old_placeholder() {
        let f = fixture("conflicted-fetch");
        let mut item = row(&f.p, "co.FILE", Some("co.ROOT"), "plan.rvt", Kind::File);
        item.local_state = LocalState::Dirty;
        item.base_version = Some(1);
        item.remote_version = Some(2);
        assert!(fetch_would_mix_conflict_versions(&item));
        item.base_version = None;
        assert!(fetch_would_mix_conflict_versions(&item));
        item.local_state = LocalState::Error;
        assert!(fetch_would_mix_conflict_versions(&item));
        item.remote_version = None;
        assert!(fetch_would_mix_conflict_versions(&item));
        item.remote_version = Some(2);
        item.base_version = Some(2);
        assert!(!fetch_would_mix_conflict_versions(&item));
        item.local_state = LocalState::Clean;
        item.base_version = None;
        assert!(fetch_would_mix_conflict_versions(&item));
        item.remote_version = None;
        assert!(!fetch_would_mix_conflict_versions(&item));
        item.remote_version = Some(2);
        item.base_version = Some(1);
        assert!(
            fetch_would_mix_conflict_versions(&item),
            "a clean old placeholder must not fetch new-tip bytes before replacement"
        );
    }

    #[test]
    fn locks_by_others_make_files_read_only() {
        let mut it = Item {
            pid: 1,
            id: "L".into(),
            parent: Some("co.R".into()),
            name: "plan.rvt".into(),
            kind: Kind::File,
            remote_version: Some(2),
            base_version: Some(2),
            size: 1,
            ctime_ms: 0,
            mtime_ms: 0,
            storage_urn: None,
            version_urn: None,
            modified_by: None,
            locked_by: None,
            local_state: LocalState::Clean,
            populated: false,
        };
        assert!(!locked_by_other(&it, Some("Anna Berg")));
        it.locked_by = Some("anna berg".into());
        assert!(
            !locked_by_other(&it, Some("Anna Berg")),
            "my own lock keeps the file writable"
        );
        assert!(locked_by_other(&it, Some("Bo Ek")));
        assert!(locked_by_other(&it, None));
        assert!(spec_for(&it, Some("Bo Ek")).read_only);
        assert!(!spec_for(&it, Some("Anna Berg")).read_only);
    }

    #[test]
    fn clean_remote_folder_is_in_sync_even_before_population() {
        let mut folder = Item {
            pid: 1,
            id: "co.F".into(),
            parent: Some("co.R".into()),
            name: "Ritningar".into(),
            kind: Kind::Folder,
            remote_version: None,
            base_version: None,
            size: 0,
            ctime_ms: 0,
            mtime_ms: 0,
            storage_urn: None,
            version_urn: None,
            modified_by: None,
            locked_by: None,
            local_state: LocalState::Clean,
            populated: false,
        };
        let spec = spec_for(&folder, None);
        assert!(
            spec.in_sync,
            "partial population must not imply pending sync"
        );
        assert!(spec.on_demand);

        folder.local_state = LocalState::Dirty;
        assert!(
            !spec_for(&folder, None).in_sync,
            "local changes are still pending"
        );
    }

    #[test]
    fn dirty_database_state_blocks_remote_replacement_even_if_cfapi_says_clean() {
        let f = fixture("dirty-content-guard");
        let mut file = row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        let mut info = PlaceholderInfo {
            identity: ph::encode_identity(Kind::File, f.p.pid, &file.id),
            in_sync: true,
            pin_state: PinState::Inherit,
            hydrated: true,
            on_disk_size: 5,
            modified_size: 0,
        };
        assert!(can_replace_remote_content(&file, &info));
        file.local_state = LocalState::Dirty;
        assert!(
            !can_replace_remote_content(&file, &info),
            "pending upload owns the bytes"
        );
        file.local_state = LocalState::Error;
        assert!(
            !can_replace_remote_content(&file, &info),
            "a failed upload still owns the bytes"
        );
        file.local_state = LocalState::Clean;
        info.modified_size = 1;
        assert!(
            !can_replace_remote_content(&file, &info),
            "modified ranges also protect local bytes"
        );
        info.modified_size = 0;
        info.identity = ph::encode_identity(Kind::File, f.p.pid, "co.OTHER");
        assert!(
            !can_replace_remote_content(&file, &info),
            "a foreign placeholder must not be replaced"
        );
    }

    #[test]
    fn free_up_policy_unpins_only_clean_files_and_protects_dirty_inherited_pins() {
        assert_eq!(
            free_up_action(true, true, PinState::Pinned),
            FreeUpAction::UnpinAndDehydrate
        );
        assert_eq!(
            free_up_action(true, false, PinState::Pinned),
            FreeUpAction::Unpin
        );
        assert_eq!(
            free_up_action(true, true, PinState::Inherit),
            FreeUpAction::UnpinAndDehydrate
        );
        assert_eq!(
            free_up_action(false, true, PinState::Inherit),
            FreeUpAction::ProtectDirty
        );
        assert_eq!(
            free_up_action(false, true, PinState::Pinned),
            FreeUpAction::Skip
        );
        assert_eq!(
            free_up_action(true, true, PinState::Unpinned),
            FreeUpAction::Dehydrate
        );
    }

    #[test]
    fn project_free_up_unpins_only_its_project_subtree() {
        let f = fixture("free-up-project-scope");
        let mut scoped = HashMap::new();
        assert!(remember_free_up_scope_roots(
            &mut scoped,
            &f.p,
            &f.pdir,
            false,
        ));
        assert_eq!(
            scoped.get(&f.pdir),
            Some(&(Kind::ProjectRoot, f.p.pid, f.p.root_id().to_string()))
        );
        assert_eq!(
            scoped.get(&f.dir),
            Some(&(Kind::Folder, f.p.pid, "co.ROOT".to_string()))
        );
        assert!(
            !scoped.contains_key(f.pdir.parent().unwrap()),
            "shared hub pin"
        );
        assert!(scoped.values().all(|(_, pid, _)| *pid == f.p.pid));

        let mut app_wide = HashMap::new();
        assert!(remember_free_up_scope_roots(
            &mut app_wide,
            &f.p,
            &f.pdir,
            true,
        ));
        assert_eq!(
            app_wide.get(f.pdir.parent().unwrap()),
            Some(&(Kind::Hub, 0, f.p.hub_id.clone()))
        );
    }

    #[test]
    fn project_free_up_rejects_unknown_or_unsubscribed_project() {
        let f = fixture("free-up-unknown-project");
        assert!(free_up(&f.ctx, Some("b.unknown"))
            .unwrap_err()
            .to_string()
            .contains("finns inte"));

        f.ctx.store.set_project_enabled(f.p.pid, false).unwrap();
        assert!(free_up(&f.ctx, Some(&f.p.project_id))
            .unwrap_err()
            .to_string()
            .contains("synkas inte"));
    }

    #[test]
    fn overlapping_free_up_is_rejected_without_taking_the_running_permit() {
        let f = fixture("free-up-overlap");
        let engine = Engine::new(f.ctx.clone());
        let running = engine.free_up_sem.clone().try_acquire_owned().unwrap();

        engine.free_up_space(Some(f.p.project_id.clone()));
        let notice = f._rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            notice,
            UiMsg::Notify {
                body,
                severity: Severity::Warning,
                ..
            } if body.contains("pågår redan")
        ));
        assert!(engine.free_up_sem.clone().try_acquire_owned().is_err());
        drop(running);
        assert!(engine.free_up_sem.clone().try_acquire_owned().is_ok());
    }

    #[test]
    fn free_up_file_requires_matching_live_synka_identity() {
        let f = fixture("free-up-file-identity");
        let file = row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        f.ctx.store.upsert_item(&file).unwrap();
        let path = f.dir.join("rapid.bin");
        let mut info = PlaceholderInfo {
            identity: ph::encode_identity(Kind::File, f.p.pid, &file.id),
            in_sync: false,
            pin_state: PinState::Inherit,
            hydrated: true,
            on_disk_size: 5,
            modified_size: 5,
        };
        assert!(free_up_file_item(&f.ctx, &f.p, &path, &info).is_some());
        info.identity = ph::encode_identity(Kind::File, f.p.pid + 1, &file.id);
        assert!(free_up_file_item(&f.ctx, &f.p, &path, &info).is_none());
        info.identity = ph::encode_identity(Kind::File, f.p.pid, "unknown");
        assert!(free_up_file_item(&f.ctx, &f.p, &path, &info).is_none());
        info.identity = ph::encode_identity(Kind::Folder, f.p.pid, &file.id);
        assert!(free_up_file_item(&f.ctx, &f.p, &path, &info).is_none());
        info.identity = ph::encode_identity(Kind::File, f.p.pid, &file.id);
        assert!(free_up_file_item(&f.ctx, &f.p, &f.dir.join("copy.bin"), &info).is_none());
    }

    #[test]
    fn free_up_retry_rechecks_clean_bytes_and_user_pin() {
        let f = fixture("free-up-retry");
        let mut file = row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        let mut info = PlaceholderInfo {
            identity: ph::encode_identity(Kind::File, f.p.pid, &file.id),
            in_sync: true,
            pin_state: PinState::Unpinned,
            hydrated: true,
            on_disk_size: 5,
            modified_size: 0,
        };
        assert!(free_up_retry_allowed(&file, &info));
        file.local_state = LocalState::Dirty;
        assert!(!free_up_retry_allowed(&file, &info));
        file.local_state = LocalState::Clean;
        info.modified_size = 1;
        assert!(!free_up_retry_allowed(&file, &info));
        info.modified_size = 0;
        info.pin_state = PinState::Pinned;
        assert!(!free_up_retry_allowed(&file, &info));
    }

    #[test]
    fn free_up_directory_requires_its_own_synka_identity_and_path() {
        let root = Path::new(r"C:\Synka");
        let path = Path::new(r"C:\Synka\Hub\Project\Project Files");
        let own = ph::encode_identity(Kind::Folder, 7, "co.ROOT");
        let foreign = ph::encode_identity(Kind::Folder, 8, "co.ROOT");
        assert!(free_up_directory_identity_matches(
            root,
            path,
            &own,
            Kind::Folder,
            7,
            "co.ROOT"
        ));
        assert!(!free_up_directory_identity_matches(
            root,
            path,
            &foreign,
            Kind::Folder,
            7,
            "co.ROOT"
        ));
        assert!(!free_up_directory_identity_matches(
            root,
            path,
            &own,
            Kind::Folder,
            7,
            "co.OTHER"
        ));
        assert!(!free_up_directory_identity_matches(
            root,
            Path::new(r"D:\Other\Hub\Project\Project Files"),
            &own,
            Kind::Folder,
            7,
            "co.ROOT"
        ));
    }

    #[test]
    fn components_below_the_root() {
        let root = Path::new(r"C:\Users\anna\Synka");
        assert_eq!(
            rel_components(root, Path::new(r"c:\users\Anna\synka\Hub\Proj\a.pdf")),
            Some(vec![
                "Hub".to_string(),
                "Proj".to_string(),
                "a.pdf".to_string()
            ])
        );
        assert_eq!(
            rel_components(root, Path::new(r"C:\Users\anna\Desktop\a.pdf")),
            None
        );
        assert_eq!(rel_components(root, root), Some(vec![]));
    }

    #[test]
    fn listing_diff_keeps_local_only_rows() {
        let row = |id: &str, state: LocalState| Item {
            pid: 1,
            id: id.into(),
            parent: Some("co.R".into()),
            name: format!("{id}.pdf"),
            kind: Kind::File,
            remote_version: Some(1),
            base_version: Some(1),
            size: 1,
            ctime_ms: 0,
            mtime_ms: 0,
            storage_urn: None,
            version_urn: None,
            modified_by: None,
            locked_by: None,
            local_state: state,
            populated: false,
        };
        let local = vec![
            row("A", LocalState::Clean),
            row("B", LocalState::Dirty),
            row("local-1", LocalState::New),
            row("C", LocalState::Clean),
        ];
        let mut changed = row("C", LocalState::Clean);
        changed.remote_version = Some(2);
        let mut d = Delta::default();
        diff_listing(&local, vec![row("A", LocalState::Clean), changed], &mut d);
        assert_eq!(
            d.upserts.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["C"]
        );
        assert_eq!(
            d.deletes,
            vec!["B".to_string()],
            "dirty rows gone remotely go; local-only rows stay"
        );
    }

    // ---- engine behaviour against a real temp folder and DB. Under test the Cloud Files
    // calls are unavailable, so every file looks like a plain (non-placeholder) file.

    pub(crate) struct Fixture {
        pub(crate) ctx: CtxRef,
        pub(crate) p: Project,
        /// The project directory (`...\Byggstyrning\Köpmannen`).
        pub(crate) pdir: PathBuf,
        /// Its top folder "Project Files" (row "co.ROOT"), where the tests work.
        pub(crate) dir: PathBuf,
        pub(crate) _rx: std::sync::mpsc::Receiver<UiMsg>,
        // Struct fields drop in declaration order: release the SQLite connection
        // before removing its files, which Windows refuses while they are open.
        _cleanup: FixtureCleanup,
    }

    struct FixtureCleanup(PathBuf);

    impl Drop for FixtureCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn fixture(tag: &str) -> Fixture {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "synka-engine-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        let state = base.join("state");
        let root = base.join("Synka");
        std::fs::create_dir_all(&state).unwrap();
        let paths = crate::paths::Paths {
            state_dir: state.clone(),
            log_dir: state.join("logs"),
            db_file: state.join("synka.db"),
            sync_root: root.clone(),
            packaged: false,
            package_family_name: None,
        };
        let store = Arc::new(Store::open(&paths.db_file).unwrap());
        let http = crate::http::build_client();
        let auth = Arc::new(crate::auth::Auth::new("test".into(), http.clone(), &state));
        let api = crate::http::Api::new(http.clone(), auth.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let ui = crate::app::UiSender {
            tx,
            main_thread_id: 0,
            hub: Arc::new(crate::ipc::IpcHub::new(root.clone())),
        };
        let cfg = crate::config::Config::default();
        let streambim = Arc::new(crate::streambim_auth::StreamBimAuth::new(
            http.clone(),
            &state,
        ));
        let streambim_api = crate::http::Api::with_source(
            http.clone(),
            crate::http::TokenSource::StreamBim(streambim.clone()),
        );
        let interaxo = Arc::new(crate::auth::Auth::with_files(
            String::new(),
            http.clone(),
            &state,
            "token-interaxo.bin",
            "user-interaxo.json",
        ));
        let interaxo_api = crate::http::Api::new(http, interaxo.clone());
        let ctx = Arc::new(Ctx {
            settings: std::sync::RwLock::new(cfg.clone()),
            cfg,
            paths,
            store,
            auth,
            api,
            streambim,
            streambim_api,
            interaxo,
            interaxo_api,
            ui,
        });
        let mut p = Project {
            pid: 0,
            hub_id: "b.hub".into(),
            hub_name: "Byggstyrning".into(),
            project_id: "b.proj".into(),
            name: "Köpmannen".into(),
            root_folder_urn: "urn:adsk.wipemea:fs.folder:co.ROOT".into(),
            urn_ns: "wipemea".into(),
            tops: vec![TopFolder {
                id: "co.ROOT".into(),
                name: "Project Files".into(),
            }],
            oss_scopes: None,
            change_source: ChangeSourceKind::PublicWalk,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: Provider::Acc,
            meta: None,
        };
        p.pid = ctx.store.upsert_project(&p).unwrap();
        let pdir = project_dir(&root, &p);
        let dir = pdir.join("Project Files");
        std::fs::create_dir_all(&dir).unwrap();
        ctx.store
            .upsert_item(&row(
                &p,
                PROJECT_ROOT_ID,
                None,
                "Köpmannen",
                Kind::ProjectRoot,
            ))
            .unwrap();
        ctx.store
            .upsert_item(&row(
                &p,
                "co.ROOT",
                Some(PROJECT_ROOT_ID),
                "Project Files",
                Kind::Folder,
            ))
            .unwrap();
        Fixture {
            ctx,
            p,
            pdir,
            dir,
            _rx: rx,
            _cleanup: FixtureCleanup(base),
        }
    }

    pub(crate) fn row(p: &Project, id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Item {
        Item {
            pid: p.pid,
            id: id.into(),
            parent: parent.map(Into::into),
            name: name.into(),
            kind,
            remote_version: (kind == Kind::File).then_some(3),
            base_version: (kind == Kind::File).then_some(3),
            size: 0,
            ctime_ms: 0,
            mtime_ms: 0,
            storage_urn: None,
            version_urn: None,
            modified_by: None,
            locked_by: None,
            local_state: LocalState::Clean,
            populated: kind != Kind::File,
        }
    }

    fn jobs(f: &Fixture) -> Vec<(String, Option<String>)> {
        f.ctx
            .store
            .due_jobs(i64::MAX, 100)
            .unwrap()
            .into_iter()
            .map(|j| (j.kind, j.item))
            .collect()
    }

    fn keys() -> OpKeys {
        OpKeys {
            connection_key: 0,
            transfer_key: 0,
            request_key: 0,
            file_id: 0,
        }
    }

    fn delete_req(f: &Fixture, id: &str, path: &Path) -> DeleteReq {
        DeleteReq {
            keys: keys(),
            path: path.to_path_buf(),
            identity: ph::encode_identity(Kind::File, f.p.pid, id),
            undelete: false,
        }
    }

    fn rename_req(f: &Fixture, id: &str, from: &Path, to: &Path) -> RenameReq {
        RenameReq {
            keys: keys(),
            path: from.to_path_buf(),
            target_path: to.to_path_buf(),
            identity: ph::encode_identity(Kind::File, f.p.pid, id),
            target_in_scope: true,
        }
    }

    #[test]
    fn new_local_file_is_registered_and_queued() {
        let f = fixture("new");
        let path = f.dir.join("Ritning.pdf");
        std::fs::write(&path, b"x").unwrap();
        process_local(&f.ctx, vec![path.clone()]);
        let kids = f.ctx.store.children(f.p.pid, "co.ROOT").unwrap();
        assert_eq!(kids.len(), 1);
        assert!(is_local_id(&kids[0].id) && kids[0].local_state == LocalState::New);
        assert_eq!(
            jobs(&f),
            vec![("upload".to_string(), Some(kids[0].id.clone()))]
        );
        // The same event again changes nothing.
        process_local(&f.ctx, vec![path]);
        assert_eq!(f.ctx.store.children(f.p.pid, "co.ROOT").unwrap().len(), 1);
    }

    #[test]
    fn delete_then_recreate_is_a_new_version() {
        let f = fixture("safesave");
        let doc = row(
            &f.p,
            "LIN1",
            Some("co.ROOT"),
            "Beskrivning.docx",
            Kind::File,
        );
        f.ctx.store.upsert_item(&doc).unwrap();
        let path = f.dir.join("Beskrivning.docx");

        let w = handle_delete(&f.ctx, &delete_req(&f, "LIN1", &path));
        assert!(w.enqueued);
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "LIN1")
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Deleted
        );
        assert!(
            f.ctx.store.due_jobs(now_ms(), 10).unwrap().is_empty(),
            "the delete waits for its grace period"
        );
        assert_eq!(
            f.ctx
                .store
                .next_job_due()
                .unwrap()
                .map(|t| t > now_ms() + 10_000),
            Some(true)
        );

        // The editor writes the new content under the same name (case may differ).
        std::fs::write(f.dir.join("beskrivning.DOCX"), b"v4").unwrap();
        process_local(&f.ctx, vec![f.dir.join("beskrivning.DOCX")]);
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "LIN1")
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Dirty
        );
        assert_eq!(
            jobs(&f),
            vec![("upload".to_string(), Some("LIN1".to_string()))],
            "no delete, one new version"
        );
        assert_eq!(
            f.ctx.store.children(f.p.pid, "co.ROOT").unwrap().len(),
            1,
            "no second item"
        );
    }

    #[test]
    fn rename_to_a_temp_name_is_a_pending_delete() {
        let f = fixture("tmpname");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.dwg", Kind::File))
            .unwrap();
        let from = f.dir.join("plan.dwg");
        handle_rename(
            &f.ctx,
            &rename_req(&f, "LIN1", &from, &f.dir.join("plan.bak")),
        );
        let it = f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap();
        assert_eq!(it.local_state, LocalState::Deleted);
        assert_eq!(it.name, "plan.dwg", "no rename is sent for a temp name");
        assert_eq!(
            f.ctx
                .store
                .pending_items(f.p.pid, &["delete"])
                .unwrap()
                .len(),
            1
        );
        assert!(f
            .ctx
            .store
            .pending_items(f.p.pid, &["rename", "move"])
            .unwrap()
            .is_empty());

        // Deleting the renamed-away copy later must not touch ACC: it is not at the item's path.
        let w = handle_delete(&f.ctx, &delete_req(&f, "LIN1", &f.dir.join("plan.bak")));
        assert!(!w.enqueued);
    }

    #[test]
    fn folders_renamed_to_backup_like_names_are_renamed_not_deleted() {
        let f = fixture("dirbackup");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "co.G", Some("co.ROOT"), "Gammalt", Kind::Folder))
            .unwrap();
        let mut req = rename_req(
            &f,
            "co.G",
            &f.dir.join("Gammalt"),
            &f.dir.join("Gammalt_backup"),
        );
        req.identity = ph::encode_identity(Kind::Folder, f.p.pid, "co.G");
        handle_rename(&f.ctx, &req);
        let it = f.ctx.store.item(f.p.pid, "co.G").unwrap().unwrap();
        assert_eq!(
            (it.name.as_str(), it.local_state),
            ("Gammalt_backup", LocalState::Clean)
        );
        assert_eq!(
            jobs(&f).into_iter().map(|j| j.0).collect::<Vec<_>>(),
            vec!["rename".to_string()]
        );
    }

    #[test]
    fn rename_and_move_inside_the_project() {
        let f = fixture("rename");
        f.ctx
            .store
            .upsert_item(&row(
                &f.p,
                "co.A",
                Some("co.ROOT"),
                "Ritningar",
                Kind::Folder,
            ))
            .unwrap();
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        std::fs::create_dir_all(f.dir.join("Ritningar")).unwrap();
        handle_rename(
            &f.ctx,
            &rename_req(
                &f,
                "LIN1",
                &f.dir.join("plan.pdf"),
                &f.dir.join("Plan A.pdf"),
            ),
        );
        let it = f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap();
        assert_eq!(
            (it.name.as_str(), it.parent.as_deref()),
            ("Plan A.pdf", Some("co.ROOT"))
        );
        handle_rename(
            &f.ctx,
            &rename_req(
                &f,
                "LIN1",
                &f.dir.join("Plan A.pdf"),
                &f.dir.join("ritningar").join("Plan A.pdf"),
            ),
        );
        let it = f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap();
        assert_eq!(it.parent.as_deref(), Some("co.A"));
        let kinds: Vec<String> = jobs(&f).into_iter().map(|j| j.0).collect();
        assert_eq!(kinds, vec!["rename".to_string(), "move".to_string()]);
    }

    #[test]
    fn segment_plan_uses_parallel_ranges_only_when_the_server_ranges() {
        let big = SEGMENT_MIN * 2 + 17;
        // Ranged: several segments that tile [start, total) in order, each chunk aligned.
        let plan = segment_plan(0, big, true);
        assert_eq!(plan.len() as u64, SEGMENTS);
        assert_eq!(plan.first().unwrap().0, 0);
        assert_eq!(plan.last().unwrap().1, big);
        for pair in plan.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
            assert_eq!(pair[0].1 % CHUNK as u64, 0);
        }
        // Without Range: one segment from the requested start, however large the file.
        let start = 3 * ALIGN;
        assert_eq!(segment_plan(start, big, false), vec![(start, big)]);
        // Small files and empty remainders.
        assert_eq!(segment_plan(0, 5, true), vec![(0, 5)]);
        assert!(segment_plan(big, big, true).is_empty());
        assert!(segment_plan(big, big, false).is_empty());
    }

    #[test]
    fn unsupported_local_file_retries_only_after_extension_changes() {
        let f = fixture("unsupported-rename");
        let mut local = row(
            &f.p,
            "local-blocked",
            Some("co.ROOT"),
            "blocked.svg",
            Kind::File,
        );
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        f.ctx.store.upsert_item(&local).unwrap();

        let old = f.dir.join("blocked.svg");
        std::fs::write(&old, b"local data").unwrap();
        let job = f
            .ctx
            .store
            .enqueue(f.p.pid, "upload", Some(&local.id), "{}")
            .unwrap();
        f.ctx
            .store
            .job_unsupported(job, "File with extension svg is not allowed to upload.")
            .unwrap();

        let same_type = f.dir.join("still-blocked.svg");
        std::fs::rename(&old, &same_type).unwrap();
        handle_rename(&f.ctx, &rename_req(&f, &local.id, &old, &same_type));
        assert!(f.ctx.store.has_unsupported_job(f.p.pid, &local.id).unwrap());
        assert!(
            jobs(&f).is_empty(),
            "a same-type rename must not retry the rejected upload"
        );

        let allowed = f.dir.join("allowed.txt");
        std::fs::rename(&same_type, &allowed).unwrap();
        handle_rename(&f.ctx, &rename_req(&f, &local.id, &same_type, &allowed));
        assert!(!f.ctx.store.has_unsupported_job(f.p.pid, &local.id).unwrap());
        let due = f.ctx.store.due_jobs(i64::MAX, 100).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].kind, "upload");
        assert_eq!(due[0].item.as_deref(), Some(local.id.as_str()));
        assert_eq!(
            serde_json::from_str::<Value>(&due[0].payload).unwrap()["path"],
            allowed.to_string_lossy().as_ref()
        );
        assert_eq!(
            f.ctx.store.item(f.p.pid, &local.id).unwrap().unwrap().name,
            "allowed.txt"
        );
    }

    #[test]
    fn moves_to_another_project_are_refused() {
        let f = fixture("xproj");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        let other = f
            .ctx
            .paths
            .sync_root
            .join("Byggstyrning")
            .join("Annat projekt")
            .join("plan.pdf");
        let w = handle_rename(
            &f.ctx,
            &rename_req(&f, "LIN1", &f.dir.join("plan.pdf"), &other),
        );
        assert!(!w.enqueued);
        let it = f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap();
        assert_eq!(
            it.local_state,
            LocalState::Clean,
            "nothing is deleted in ACC"
        );
        assert!(jobs(&f).is_empty() && f.ctx.store.next_job_due().unwrap().is_none());
    }

    #[test]
    fn remote_poll_does_not_undo_a_pending_rename() {
        let f = fixture("pending");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        handle_rename(
            &f.ctx,
            &rename_req(&f, "LIN1", &f.dir.join("plan.pdf"), &f.dir.join("ny.pdf")),
        );
        let mut remote = row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File);
        remote.remote_version = Some(4);
        let mut d = Delta {
            upserts: vec![remote],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut d).unwrap();
        assert_eq!(d.upserts[0].name, "ny.pdf");
        assert_eq!(
            d.upserts[0].remote_version,
            Some(4),
            "remote content changes still apply"
        );
    }

    #[test]
    fn edits_whose_item_vanished_are_uploaded_again() {
        let f = fixture("adopt");
        // A folder deleted in ACC while it held a local edit: the rows are gone, the files stay.
        let sub = f.dir.join("Underlag");
        std::fs::create_dir_all(&sub).unwrap();
        let kept = sub.join("mätning.xlsx");
        std::fs::write(&kept, b"local edit").unwrap();
        let mut w = Work::default();
        adopt(&f.ctx, &f.p, &kept, &mut w);
        let folder =
            find_child_local(&f.ctx, f.p.pid, "co.ROOT", "Underlag").expect("folder re-registered");
        assert!(is_local_id(&folder.id) && folder.kind == Kind::Folder);
        let file = find_child_local(&f.ctx, f.p.pid, &folder.id, "mätning.xlsx")
            .expect("file re-registered");
        assert_eq!(file.local_state, LocalState::New);
        let kinds: Vec<String> = jobs(&f).into_iter().map(|j| j.0).collect();
        assert_eq!(kinds, vec!["mkdir".to_string(), "upload".to_string()]);
        // Ignored files are never adopted.
        std::fs::write(sub.join("~$mätning.xlsx"), b"lock").unwrap();
        adopt(&f.ctx, &f.p, &sub.join("~$mätning.xlsx"), &mut w);
        assert_eq!(jobs(&f).len(), 2);
    }

    #[test]
    fn deleting_a_local_only_file_just_forgets_it() {
        let f = fixture("forget");
        let path = f.dir.join("utkast.txt");
        std::fs::write(&path, b"x").unwrap();
        process_local(&f.ctx, vec![path.clone()]);
        let id = f.ctx.store.children(f.p.pid, "co.ROOT").unwrap()[0]
            .id
            .clone();
        let mut w = Work::default();
        queue_remote_delete(&f.ctx, &f.p, &id, &mut w);
        assert!(f.ctx.store.item(f.p.pid, &id).unwrap().is_none());
        assert!(!w.enqueued);
    }

    #[test]
    fn mass_deletes_are_held_and_can_be_restored() {
        let f = fixture("massdel");
        for i in 0..30 {
            f.ctx
                .store
                .upsert_item(&row(
                    &f.p,
                    &format!("L{i}"),
                    Some("co.ROOT"),
                    &format!("f{i}.pdf"),
                    Kind::File,
                ))
                .unwrap();
        }
        for i in 0..10 {
            handle_delete(
                &f.ctx,
                &delete_req(&f, &format!("L{i}"), &f.dir.join(format!("f{i}.pdf"))),
            );
        }
        assert!(
            f.ctx.store.held_deletes().unwrap().is_empty(),
            "a few deletes just wait for the grace period"
        );
        for i in 10..30 {
            handle_delete(
                &f.ctx,
                &delete_req(&f, &format!("L{i}"), &f.dir.join(format!("f{i}.pdf"))),
            );
        }
        assert_eq!(
            f.ctx.store.held_deletes().unwrap(),
            vec![(f.p.pid, 30)],
            "every delete of the burst is held"
        );
        assert_eq!(
            f.ctx.store.next_job_due().unwrap(),
            None,
            "nothing will run on its own"
        );

        let n = restore_held(&f.ctx, &f.p).unwrap();
        assert_eq!(n, 30);
        assert!(f.ctx.store.held_deletes().unwrap().is_empty());
        assert!(f
            .ctx
            .store
            .children(f.p.pid, "co.ROOT")
            .unwrap()
            .iter()
            .all(|r| r.local_state == LocalState::Clean));
    }

    #[test]
    fn deleting_a_big_folder_asks_first() {
        let f = fixture("bigdir");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "co.D", Some("co.ROOT"), "Arkiv", Kind::Folder))
            .unwrap();
        for i in 0..40 {
            f.ctx
                .store
                .upsert_item(&row(
                    &f.p,
                    &format!("L{i}"),
                    Some("co.D"),
                    &format!("f{i}.pdf"),
                    Kind::File,
                ))
                .unwrap();
        }
        let mut req = delete_req(&f, "co.D", &f.dir.join("Arkiv"));
        req.identity = ph::encode_identity(Kind::Folder, f.p.pid, "co.D");
        handle_delete(&f.ctx, &req);
        assert_eq!(f.ctx.store.held_deletes().unwrap(), vec![(f.p.pid, 41)]);
        assert!(f.ctx.store.release_deletes(f.p.pid).unwrap() == 1);
    }

    #[test]
    fn reset_saves_unsynced_files_before_removing_the_copy() {
        let f = fixture("rescue");
        let aside = f
            .pdir
            .with_file_name(".synka-reset-20261016-101010-Köpmannen");
        std::fs::create_dir_all(aside.join("Ritningar")).unwrap();
        std::fs::write(aside.join("Ritningar").join("plan.dwg"), b"local edit").unwrap();
        std::fs::write(aside.join("Ritningar").join("~$plan.dwg"), b"lock").unwrap();
        let (saved, dest) = reset::finish(&f.ctx, &aside, "Köpmannen");
        assert_eq!(
            saved, 2,
            "ignored lock files are local-only and must also be rescued"
        );
        let dest = dest.expect("rescue folder");
        assert!(dest.starts_with(f.ctx.paths.sync_root.parent().unwrap()));
        assert_eq!(
            std::fs::read(dest.join("Ritningar").join("plan.dwg")).unwrap(),
            b"local edit"
        );
        assert_eq!(
            std::fs::read(dest.join("Ritningar").join("~$plan.dwg")).unwrap(),
            b"lock"
        );
        let readme = std::fs::read_to_string(dest.join("LÄS MIG.txt")).unwrap();
        assert!(
            readme.contains("Köpmannen byggdes om")
                && readme.contains(&format!("Ritningar{}plan.dwg", std::path::MAIN_SEPARATOR)),
            "{readme}"
        );
        assert!(!aside.exists(), "the set-aside copy is gone");
    }

    #[test]
    fn interrupted_resets_are_found_at_startup() {
        let f = fixture("leftover");
        let aside = f
            .pdir
            .with_file_name(".synka-reset-20261016-101010-Köpmannen");
        std::fs::create_dir_all(&aside).unwrap();
        let found = reset::leftovers(&f.ctx);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "Köpmannen");
        assert!(ignore::ignored_name(
            ".synka-reset-20261016-101010-Köpmannen"
        ));
    }

    #[test]
    fn callbacks_inside_a_reset_change_nothing() {
        let f = fixture("guard");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        let _g = reset::guard_aside(&f.dir);
        let w = handle_delete(&f.ctx, &delete_req(&f, "LIN1", &f.dir.join("plan.pdf")));
        assert!(!w.enqueued);
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "LIN1")
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Clean
        );
    }

    #[test]
    fn nothing_new_is_synced_directly_in_the_project_directory() {
        let f = fixture("toplevel");
        let file = f.pdir.join("lös fil.pdf");
        std::fs::write(&file, b"x").unwrap();
        std::fs::create_dir_all(f.pdir.join("Egen mapp")).unwrap();
        process_local(&f.ctx, vec![file.clone(), f.pdir.join("Egen mapp")]);
        let kids: Vec<String> = f
            .ctx
            .store
            .children(f.p.pid, PROJECT_ROOT_ID)
            .unwrap()
            .into_iter()
            .map(|k| k.id)
            .collect();
        assert_eq!(kids, vec!["co.ROOT".to_string()], "only the top folder");
        assert!(jobs(&f).is_empty());
        let mut w = Work::default();
        adopt(&f.ctx, &f.p, &file, &mut w);
        assert!(jobs(&f).is_empty() && !w.enqueued);
        // The same file inside Project Files is synced.
        let inside = f.dir.join("lös fil.pdf");
        std::fs::write(&inside, b"x").unwrap();
        process_local(&f.ctx, vec![inside]);
        assert_eq!(jobs(&f).len(), 1);
    }

    #[test]
    fn top_folders_stay_where_acc_has_them() {
        let f = fixture("tops");
        let mut del = delete_req(&f, "co.ROOT", &f.dir);
        del.identity = ph::encode_identity(Kind::Folder, f.p.pid, "co.ROOT");
        let w = handle_delete(&f.ctx, &del);
        assert!(!w.enqueued);
        let mut ren = rename_req(&f, "co.ROOT", &f.dir, &f.pdir.join("Mina filer"));
        ren.identity = ph::encode_identity(Kind::Folder, f.p.pid, "co.ROOT");
        handle_rename(&f.ctx, &ren);
        let top = f.ctx.store.item(f.p.pid, "co.ROOT").unwrap().unwrap();
        assert_eq!(
            (top.name.as_str(), top.local_state),
            ("Project Files", LocalState::Clean)
        );
        assert!(jobs(&f).is_empty() && f.ctx.store.next_job_due().unwrap().is_none());

        // Nothing moves into the project directory either.
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        handle_rename(
            &f.ctx,
            &rename_req(
                &f,
                "LIN1",
                &f.dir.join("plan.pdf"),
                &f.pdir.join("plan.pdf"),
            ),
        );
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "LIN1")
                .unwrap()
                .unwrap()
                .parent
                .as_deref(),
            Some("co.ROOT")
        );
        assert!(jobs(&f).is_empty());
    }

    #[test]
    fn deltas_keep_the_top_folders_in_the_project_directory() {
        let mut f = fixture("merge");
        // A walk never lists the top folders; a lost one goes, a new one comes.
        f.ctx
            .store
            .upsert_item(&row(
                &f.p,
                "co.OLD",
                Some(PROJECT_ROOT_ID),
                "Gammal",
                Kind::Folder,
            ))
            .unwrap();
        f.p.tops.push(TopFolder {
            id: "co.B".into(),
            name: "04 BIM".into(),
        });
        let mut d = Delta::default();
        merge_tops(&f.ctx.store, &f.p, &mut d).unwrap();
        assert_eq!(
            d.upserts
                .iter()
                .map(|u| (u.id.as_str(), u.name.as_str()))
                .collect::<Vec<_>>(),
            vec![("co.B", "04 BIM")]
        );
        assert_eq!(d.upserts[0].parent.as_deref(), Some(PROJECT_ROOT_ID));
        assert_eq!(d.deletes, vec!["co.OLD".to_string()]);

        // A full enumeration names every top folder, so none is dropped; the project directory
        // row survives it too.
        f.ctx.store.apply_delta(f.p.pid, &d).unwrap();
        let mut full = Delta {
            reset: true,
            upserts: vec![row(&f.p, "LIN9", Some("co.B"), "modell.ifc", Kind::File)],
            ..Default::default()
        };
        merge_tops(&f.ctx.store, &f.p, &mut full).unwrap();
        let applied = f.ctx.store.apply_delta(f.p.pid, &full).unwrap();
        assert!(applied.changes.iter().all(|(b, a)| !(a.is_none()
            && b.as_ref()
                .is_some_and(|b| b.id == "co.ROOT" || b.id == "co.B"))));
        assert!(f
            .ctx
            .store
            .item(f.p.pid, PROJECT_ROOT_ID)
            .unwrap()
            .is_some());
        assert!(f.ctx.store.item(f.p.pid, "co.ROOT").unwrap().is_some());
        assert_eq!(
            item_path(&f.ctx, &f.p, "LIN9").unwrap(),
            f.pdir.join("04 BIM").join("modell.ifc")
        );

        // A top folder the feed deletes stays deleted.
        let mut gone = Delta {
            deletes: vec!["co.B".into()],
            ..Default::default()
        };
        merge_tops(&f.ctx.store, &f.p, &mut gone).unwrap();
        assert!(gone.upserts.is_empty());
    }

    #[test]
    fn projects_from_before_top_folders_are_rebuilt_not_polled() {
        let f = fixture("relayout");
        let mut old = f.p.clone();
        old.project_id = "b.old".into();
        old.tops = Vec::new();
        old.pid = f.ctx.store.upsert_project(&old).unwrap();
        f.ctx
            .store
            .upsert_item(&row(&old, "co.PF", None, "Gammalt", Kind::ProjectRoot))
            .unwrap();
        let (pids, relayout) = ensure_all_dirs(&f.ctx).unwrap();
        assert_eq!(pids, vec![f.p.pid]);
        assert_eq!(relayout, vec!["b.old".to_string()]);
        assert!(
            f.ctx
                .store
                .item(old.pid, PROJECT_ROOT_ID)
                .unwrap()
                .is_none(),
            "left alone until the rebuild"
        );
    }

    #[test]
    fn moving_the_sync_root_saves_unsynced_work_and_forgets_everything() {
        let f = fixture("move");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        std::fs::write(f.dir.join("ny fil.docx"), b"unsynced").unwrap();
        let to = f.ctx.paths.sync_root.with_file_name("D").join("Synka");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();

        let moved = relocate::set_aside(&f.ctx, &to).unwrap();
        assert_eq!(moved.asides.len(), 1);
        assert!(!f.pdir.exists(), "the hub folder is set aside");
        assert!(
            f.ctx.store.item(f.p.pid, "LIN1").unwrap().is_none(),
            "rows are forgotten"
        );
        assert!(project_resetting(f.p.pid), "the project stays stopped");
        let saved = crate::config::Config::load(&f.ctx.paths.state_dir.join("settings.json"));
        assert_eq!(
            saved.sync_root.as_deref(),
            Some(to.to_string_lossy().as_ref())
        );

        let (aside, hub) = &moved.asides[0];
        let (n, dest) = reset::finish(&f.ctx, aside, hub);
        assert_eq!(n, 1);
        let dest = dest.unwrap();
        assert_eq!(
            std::fs::read(
                dest.join("Köpmannen")
                    .join("Project Files")
                    .join("ny fil.docx")
            )
            .unwrap(),
            b"unsynced"
        );
        drop(moved.guard);

        // The new process: the old root goes once it is empty.
        let mut paths = f.ctx.paths.clone();
        let old = paths.sync_root.clone();
        paths.sync_root = to.clone();
        let ctx2 = Ctx {
            cfg: f.ctx.cfg.clone(),
            settings: std::sync::RwLock::new(f.ctx.cfg.clone()),
            paths,
            store: f.ctx.store.clone(),
            auth: f.ctx.auth.clone(),
            api: f.ctx.api.clone(),
            streambim: f.ctx.streambim.clone(),
            streambim_api: f.ctx.streambim_api.clone(),
            interaxo: f.ctx.interaxo.clone(),
            interaxo_api: f.ctx.interaxo_api.clone(),
            ui: f.ctx.ui.clone(),
        };
        assert_eq!(relocate::forget_old_root(&ctx2), None);
        assert!(!old.exists(), "the empty old root is removed");
        assert_eq!(relocate::forget_old_root(&ctx2), None, "only once");
    }

    #[test]
    fn a_move_with_an_open_file_changes_nothing() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = fixture("moveopen");
        let path = f.dir.join("öppen.rvt");
        std::fs::write(&path, b"x").unwrap();
        f.ctx
            .store
            .set_populated(f.p.pid, PROJECT_ROOT_ID, true)
            .unwrap();
        f.ctx.store.set_populated(f.p.pid, "co.ROOT", true).unwrap();
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let to = f.ctx.paths.sync_root.with_file_name("E").join("Synka");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        let err = relocate::set_aside(&f.ctx, &to).err().expect("refused");
        assert!(format!("{err:#}").contains("öppen.rvt"), "{err:#}");
        assert!(f.pdir.exists() && path.exists());
        assert!(f.ctx.store.item(f.p.pid, "co.ROOT").unwrap().is_some());
        assert!(!project_resetting(f.p.pid));
        assert_eq!(
            crate::config::Config::load(&f.ctx.paths.state_dir.join("settings.json")).sync_root,
            None
        );
    }

    #[test]
    fn changed_rows_without_a_job_are_queued_again() {
        let f = fixture("requeue");
        let mut doc = row(&f.p, "LIN1", Some("co.ROOT"), "mätning.xlsx", Kind::File);
        doc.local_state = LocalState::Dirty;
        f.ctx.store.upsert_item(&doc).unwrap();
        let mut dir = row(&f.p, "local-0001", Some("co.ROOT"), "Ny mapp", Kind::Folder);
        dir.local_state = LocalState::New;
        f.ctx.store.upsert_item(&dir).unwrap();
        std::fs::write(f.dir.join("mätning.xlsx"), b"edit").unwrap();
        std::fs::create_dir_all(f.dir.join("Ny mapp")).unwrap();
        process_local(
            &f.ctx,
            vec![f.dir.join("mätning.xlsx"), f.dir.join("Ny mapp")],
        );
        let mut got = jobs(&f);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("mkdir".to_string(), Some("local-0001".to_string())),
                ("upload".to_string(), Some("LIN1".to_string()))
            ]
        );
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "local-0001")
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::New
        );
        // Once queued, a new event adds nothing.
        process_local(
            &f.ctx,
            vec![f.dir.join("mätning.xlsx"), f.dir.join("Ny mapp")],
        );
        assert_eq!(jobs(&f).len(), 2);
    }

    #[test]
    fn shortcut_files_stay_local() {
        let f = fixture("url");
        std::fs::write(f.dir.join("Länk till ACC.url"), b"[InternetShortcut]").unwrap();
        process_local(&f.ctx, vec![f.dir.join("Länk till ACC.url")]);
        assert!(f.ctx.store.children(f.p.pid, "co.ROOT").unwrap().is_empty());
        assert!(jobs(&f).is_empty());
    }

    #[test]
    fn a_file_back_before_its_delete_runs_is_kept() {
        let f = fixture("back");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.dwg", Kind::File))
            .unwrap();
        handle_delete(&f.ctx, &delete_req(&f, "LIN1", &f.dir.join("plan.dwg")));
        assert!(
            !back_before_delete(&f.ctx, &f.p, "LIN1"),
            "still gone: the delete goes ahead"
        );
        // A slow save writes the file back after the grace period.
        std::fs::write(f.dir.join("plan.dwg"), b"v2").unwrap();
        assert!(back_before_delete(&f.ctx, &f.p, "LIN1"));
        let it = f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap();
        assert_eq!(
            it.local_state,
            LocalState::Dirty,
            "its content becomes a new version"
        );
        assert!(f.ctx.store.has_job(f.p.pid, "LIN1", &["upload"]).unwrap());
    }

    #[test]
    fn an_undeleted_file_cancels_its_delete() {
        let f = fixture("undelete");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        handle_delete(&f.ctx, &delete_req(&f, "LIN1", &f.dir.join("plan.pdf")));
        let mut undo = delete_req(&f, "LIN1", &f.dir.join("plan.pdf"));
        undo.undelete = true;
        handle_delete(&f.ctx, &undo);
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "LIN1")
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Clean
        );
        assert!(
            f.ctx.store.next_job_due().unwrap().is_none(),
            "no delete is left"
        );
    }

    #[test]
    fn a_remote_rename_of_an_open_file_waits_instead_of_duplicating() {
        let f = fixture("pendmove");
        let before = row(&f.p, "LIN1", Some("co.ROOT"), "plan.dwg", Kind::File);
        f.ctx.store.upsert_item(&before).unwrap();
        std::fs::write(f.dir.join("plan.dwg"), b"x").unwrap();
        let mut after = before.clone();
        after.name = "plan A.dwg".into();
        let mut w = Work::default();
        pend_move(&f.ctx, &f.p, &before, &after, &mut w).unwrap();
        assert_eq!(w.moves, vec![(f.p.pid, "LIN1".to_string())]);
        // Locally it keeps its name (paths still resolve); uploads use the ACC name.
        assert_eq!(
            f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap().name,
            "plan.dwg"
        );
        assert_eq!(
            pending_remote_name(&f.ctx, f.p.pid, "LIN1").as_deref(),
            Some("plan A.dwg")
        );
        assert_eq!(pending_moves(&f.ctx), vec![(f.p.pid, "LIN1".to_string())]);
        // Something else holds the new name: wait.
        std::fs::write(f.dir.join("plan A.dwg"), b"other").unwrap();
        assert!(!retry_move(&f.ctx, f.p.pid, "LIN1"));
        std::fs::remove_file(f.dir.join("plan A.dwg")).unwrap();
        // Free: the rename lands on disk and in the DB.
        assert!(retry_move(&f.ctx, f.p.pid, "LIN1"));
        assert!(f.dir.join("plan A.dwg").exists() && !f.dir.join("plan.dwg").exists());
        assert_eq!(
            f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap().name,
            "plan A.dwg"
        );
        assert_eq!(pending_remote_name(&f.ctx, f.p.pid, "LIN1"), None);

        // A rename by the user meanwhile wins over the pending one.
        let b2 = row(&f.p, "LIN2", Some("co.ROOT"), "fasad.dwg", Kind::File);
        f.ctx.store.upsert_item(&b2).unwrap();
        std::fs::write(f.dir.join("fasad.dwg"), b"x").unwrap();
        let mut a2 = b2.clone();
        a2.name = "fasad B.dwg".into();
        pend_move(&f.ctx, &f.p, &b2, &a2, &mut Work::default()).unwrap();
        std::fs::rename(f.dir.join("fasad.dwg"), f.dir.join("min.dwg")).unwrap();
        handle_rename(
            &f.ctx,
            &rename_req(&f, "LIN2", &f.dir.join("fasad.dwg"), &f.dir.join("min.dwg")),
        );
        assert_eq!(pending_remote_name(&f.ctx, f.p.pid, "LIN2"), None);
        assert!(retry_move(&f.ctx, f.p.pid, "LIN2"));
        assert_eq!(
            f.ctx.store.item(f.p.pid, "LIN2").unwrap().unwrap().name,
            "min.dwg"
        );
    }

    #[test]
    fn moved_file_keeps_retry_marker_until_content_refresh_succeeds() {
        let f = fixture("move-content-retry");
        let before = row(&f.p, "LIN-MOVE", Some("co.ROOT"), "old.dwg", Kind::File);
        f.ctx.store.upsert_item(&before).unwrap();
        std::fs::write(f.dir.join("old.dwg"), b"local bytes").unwrap();
        let mut after = before.clone();
        after.name = "new.dwg".into();
        pend_move(&f.ctx, &f.p, &before, &after, &mut Work::default()).unwrap();
        assert!(!retry_move_with_refresh(
            &f.ctx,
            f.p.pid,
            &before.id,
            |_, _, _| false
        ));
        assert!(f.dir.join("new.dwg").exists());
        assert_eq!(pending_moves(&f.ctx), vec![(f.p.pid, before.id.clone())]);
        assert!(retry_move_with_refresh(
            &f.ctx,
            f.p.pid,
            &before.id,
            |_, _, _| true
        ));
        assert!(f.dir.join("new.dwg").exists());
        assert!(pending_moves(&f.ctx).is_empty());
    }

    #[test]
    fn a_mass_delete_in_acc_saves_the_local_copies() {
        let f = fixture("massremote");
        f.ctx
            .store
            .upsert_item(&row(&f.p, "co.D", Some("co.ROOT"), "Arkiv", Kind::Folder))
            .unwrap();
        std::fs::create_dir_all(f.dir.join("Arkiv")).unwrap();
        for i in 0..30 {
            f.ctx
                .store
                .upsert_item(&row(
                    &f.p,
                    &format!("L{i}"),
                    Some("co.D"),
                    &format!("f{i}.pdf"),
                    Kind::File,
                ))
                .unwrap();
        }
        // Two of them were opened here (under Wine every file counts as local data).
        std::fs::write(f.dir.join("Arkiv").join("f1.pdf"), b"one").unwrap();
        std::fs::write(f.dir.join("Arkiv").join("f2.pdf"), b"two").unwrap();
        let applied = f
            .ctx
            .store
            .apply_delta(
                f.p.pid,
                &Delta {
                    deletes: vec!["co.D".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(applied.changes.len(), 31);
        disk_apply(&f.ctx, &f.p, &applied.changes);
        assert!(!f.dir.join("Arkiv").exists(), "gone from the sync folder");
        let rescue = std::fs::read_dir(reset::rescue_root(&f.ctx))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .next()
            .expect("rescue folder");
        assert!(rescue
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Köpmannen borttaget i ACC"));
        let saved = rescue.join("Project Files").join("Arkiv");
        assert_eq!(std::fs::read(saved.join("f1.pdf")).unwrap(), b"one");
        assert_eq!(std::fs::read(saved.join("f2.pdf")).unwrap(), b"two");
        assert!(std::fs::read_to_string(rescue.join("LÄS MIG.txt"))
            .unwrap()
            .contains("31 objekt togs bort i ACC"));
        assert!(jobs(&f).is_empty(), "nothing is uploaded back");

        // A second deletion before the clock advances must reserve another
        // recovery folder and leave the first recovery's bytes untouched.
        std::fs::create_dir_all(f.dir.join("Arkiv")).unwrap();
        std::fs::write(f.dir.join("Arkiv").join("f1.pdf"), b"later one").unwrap();
        std::fs::write(f.dir.join("Arkiv").join("f2.pdf"), b"later two").unwrap();
        let deleted = applied
            .changes
            .iter()
            .filter(|(_, a)| a.is_none())
            .filter_map(|(b, _)| b.as_ref().map(|b| b.id.clone()))
            .collect();
        assert!(rescue_mass_removal(&f.ctx, &f.p, &applied.changes, &deleted).is_empty());
        let folders: Vec<_> = std::fs::read_dir(reset::rescue_root(&f.ctx))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(folders.len(), 2);
        assert_eq!(std::fs::read(saved.join("f1.pdf")).unwrap(), b"one");
        let second = folders.into_iter().find(|path| path != &rescue).unwrap();
        assert_eq!(
            std::fs::read(second.join("Project Files").join("Arkiv").join("f1.pdf")).unwrap(),
            b"later one"
        );
    }

    #[test]
    fn mass_rescue_file_names_never_replace_a_previous_copy() {
        let f = fixture("rescueclash");
        let destination = f.ctx.paths.state_dir.join("rescued").join("plan.dwg");
        let first_source = f.ctx.paths.state_dir.join("first.dwg");
        let second_source = f.ctx.paths.state_dir.join("second.dwg");
        std::fs::write(&first_source, b"first bytes").unwrap();
        std::fs::write(&second_source, b"second bytes").unwrap();
        let first = save_mass_rescue_file(&first_source, &destination).unwrap();
        let second = save_mass_rescue_file(&second_source, &destination).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first).unwrap(), b"first bytes");
        assert_eq!(std::fs::read(second).unwrap(), b"second bytes");
        assert!(!first_source.exists() && !second_source.exists());
    }

    #[test]
    fn deletes_made_while_the_engine_was_stopped_are_sent() {
        let f = fixture("offline");
        f.ctx
            .store
            .set_populated(f.p.pid, PROJECT_ROOT_ID, true)
            .unwrap();
        f.ctx.store.set_populated(f.p.pid, "co.ROOT", true).unwrap();
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "kvar.pdf", Kind::File))
            .unwrap();
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN2", Some("co.ROOT"), "borta.pdf", Kind::File))
            .unwrap();
        f.ctx
            .store
            .upsert_item(&row(&f.p, "co.U", Some("co.ROOT"), "Utgått", Kind::Folder))
            .unwrap();
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN3", Some("co.U"), "gammal.pdf", Kind::File))
            .unwrap();
        std::fs::write(f.dir.join("kvar.pdf"), b"x").unwrap();
        let w = reconcile(&f.ctx);
        assert!(w.enqueued);
        let state = |id: &str| f.ctx.store.item(f.p.pid, id).unwrap().unwrap().local_state;
        assert_eq!(state("LIN1"), LocalState::Clean);
        assert_eq!(state("LIN2"), LocalState::Deleted);
        assert_eq!(
            state("co.U"),
            LocalState::Deleted,
            "the folder goes as one delete"
        );
        assert_eq!(state("LIN3"), LocalState::Clean, "its contents go with it");
        assert_eq!(
            f.ctx
                .store
                .pending_items(f.p.pid, &["delete"])
                .unwrap()
                .len(),
            2
        );
        // A second pass finds nothing new.
        assert!(!reconcile(&f.ctx).enqueued);
    }

    #[test]
    fn a_missing_project_or_top_folder_is_never_deleted() {
        let f = fixture("offline-top");
        f.ctx
            .store
            .set_populated(f.p.pid, PROJECT_ROOT_ID, true)
            .unwrap();
        f.ctx.store.set_populated(f.p.pid, "co.ROOT", true).unwrap();
        f.ctx
            .store
            .upsert_item(&row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File))
            .unwrap();
        std::fs::remove_dir_all(&f.dir).unwrap();
        let w = reconcile(&f.ctx);
        assert!(!w.enqueued, "Project Files is put back, not deleted");
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "co.ROOT")
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Clean
        );
        assert!(
            !f.ctx
                .store
                .item(f.p.pid, "co.ROOT")
                .unwrap()
                .unwrap()
                .populated
        );
        std::fs::remove_dir_all(&f.pdir).unwrap();
        assert!(
            !reconcile(&f.ctx).enqueued,
            "no project folder: nothing is judged"
        );
        assert!(jobs(&f).is_empty());
    }

    #[test]
    fn poll_of_our_create_waits_for_the_persisted_upload_intent() {
        let f = fixture("own-create-poll");
        let mut local = row(&f.p, "local-one", Some("co.ROOT"), "rapid.bin", Kind::File);
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        local.storage_urn = Some("urn:adsk.objects:os.object:bucket/unique-upload".into());
        f.ctx.store.upsert_item(&local).unwrap();
        f.ctx
            .store
            .enqueue(f.p.pid, "upload", Some(&local.id), "{}")
            .unwrap();
        let mut remote = row(&f.p, "remote-one", Some("co.ROOT"), "rapid.bin", Kind::File);
        remote.storage_urn = local.storage_urn.clone();
        let mut own_delta = Delta {
            upserts: vec![remote.clone()],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut own_delta).unwrap();
        assert!(
            own_delta.upserts.is_empty(),
            "poll must not create a competing row before the upload binds its ID"
        );
        let mut colleague = remote.clone();
        colleague.storage_urn = Some("urn:adsk.objects:os.object:bucket/someone-else".into());
        let mut other_delta = Delta {
            upserts: vec![colleague],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut other_delta).unwrap();
        assert_eq!(
            other_delta.upserts.len(),
            1,
            "an unrelated upload remains visible"
        );

        // A prior build could already have inserted the remote row. Binding still
        // merges the local alias and moves its outbox reference without a PK error.
        f.ctx.store.upsert_item(&remote).unwrap();
        f.ctx
            .store
            .rekey_item(f.p.pid, &local.id, &remote.id)
            .unwrap();
        assert_eq!(
            f.ctx
                .store
                .children_named(f.p.pid, "co.ROOT", "rapid.bin")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(jobs(&f), vec![("upload".into(), Some(remote.id))]);
    }

    #[test]
    fn poll_does_not_conflict_with_our_pending_version_or_finalization() {
        let f = fixture("own-version-poll");
        let mut local = row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        local.storage_urn = Some("old-storage".into());
        local.local_state = LocalState::Dirty;
        f.ctx.store.upsert_item(&local).unwrap();
        let job = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&local.id),
                &serde_json::json!({
                    "path": "rapid.bin",
                    "intent_kind": "version",
                    "intent_storage": "exact-new-storage",
                    "intent_size": 5,
                    "intent_mtime_ms": 1,
                    "intent_mtime_ticks": 2,
                })
                .to_string(),
            )
            .unwrap();
        let mut own_tip = local.clone();
        own_tip.remote_version = Some(4);
        own_tip.storage_urn = Some("exact-new-storage".into());
        let mut delta = Delta {
            upserts: vec![own_tip.clone()],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
        assert!(
            delta.upserts.is_empty(),
            "our own version must not trigger keep_both while binding"
        );
        f.ctx
            .store
            .retarget_job(
                job,
                "finalize",
                &serde_json::json!({
                    "path": "rapid.bin", "storage": "exact-new-storage", "version": 4,
                })
                .to_string(),
            )
            .unwrap();
        let mut delta = Delta {
            upserts: vec![own_tip.clone()],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
        assert!(
            delta.upserts.is_empty(),
            "local finalization still owns this ACC version"
        );
        own_tip.storage_urn = Some("colleague-storage".into());
        own_tip.remote_version = Some(5);
        let mut delta = Delta {
            upserts: vec![own_tip],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
        assert_eq!(
            delta.upserts.len(),
            1,
            "a colleague's different storage remains visible"
        );
    }

    #[test]
    fn legacy_finalize_suppresses_only_the_bound_committed_version() {
        let f = fixture("legacy-finalize-storage");
        let mut bound = row(&f.p, "LIN-legacy", Some("co.ROOT"), "rapid.bin", Kind::File);
        bound.remote_version = Some(4);
        bound.base_version = Some(4);
        bound.local_state = LocalState::Dirty;
        bound.storage_urn = Some("our-committed-storage".into());
        f.ctx.store.upsert_item(&bound).unwrap();
        let job = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "finalize",
                Some(&bound.id),
                &serde_json::json!({
                    "path": "rapid.bin", "version": 4, "usn": 123,
                })
                .to_string(),
            )
            .unwrap();
        let mut delta = Delta {
            upserts: vec![bound.clone()],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
        assert!(
            delta.upserts.is_empty(),
            "legacy finalize still identifies its own tip"
        );
        let mut colleague = bound.clone();
        colleague.remote_version = Some(5);
        colleague.storage_urn = Some("colleague-storage".into());
        let mut delta = Delta {
            upserts: vec![colleague],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
        assert_eq!(delta.upserts.len(), 1, "colleague bytes remain visible");
        f.ctx
            .store
            .retarget_job(
                job,
                "finalize",
                &serde_json::json!({
                    "path": "rapid.bin", "version": 3, "usn": 123,
                })
                .to_string(),
            )
            .unwrap();
        let mut delta = Delta {
            upserts: vec![bound],
            ..Default::default()
        };
        keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
        assert_eq!(
            delta.upserts.len(),
            1,
            "a stale finalize cannot claim a newer bound row"
        );
    }

    #[test]
    fn reset_keeps_our_pending_version_through_finalization_and_restart() {
        let f = fixture("own-version-reset");
        let mut local = row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        local.local_state = LocalState::Dirty;
        local.storage_urn = Some("older-storage".into());
        f.ctx.store.upsert_item(&local).unwrap();
        let job = f.ctx.store.enqueue(f.p.pid, "upload", Some(&local.id), &serde_json::json!({
            "path": "rapid.bin", "intent_kind": "version", "intent_storage": "exact-storage",
        }).to_string()).unwrap();
        let mut own_tip = local.clone();
        own_tip.remote_version = Some(4);
        own_tip.storage_urn = Some("exact-storage".into());
        let mut delta = Delta {
            reset: true,
            upserts: vec![own_tip.clone()],
            ..Default::default()
        };
        merge_tops(&f.ctx.store, &f.p, &mut delta).unwrap();
        keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
        f.ctx.store.apply_delta(f.p.pid, &delta).unwrap();
        assert!(f.ctx.store.item(f.p.pid, &local.id).unwrap().is_some());
        f.ctx
            .store
            .retarget_job(
                job,
                "finalize",
                &serde_json::json!({
                    "path": "rapid.bin", "storage": "exact-storage", "version": 4,
                })
                .to_string(),
            )
            .unwrap();
        let reopened = Store::open(&f.ctx.paths.db_file).unwrap();
        let mut delta = Delta {
            reset: true,
            upserts: vec![own_tip],
            ..Default::default()
        };
        merge_tops(&reopened, &f.p, &mut delta).unwrap();
        keep_pending_local(&reopened, f.p.pid, &mut delta).unwrap();
        reopened.apply_delta(f.p.pid, &delta).unwrap();
        assert!(
            reopened.item(f.p.pid, &local.id).unwrap().is_some(),
            "finalize still owns this row after restart"
        );
        assert_eq!(reopened.due_jobs(i64::MAX, 10).unwrap()[0].kind, "finalize");
    }

    #[test]
    fn remote_delete_preserves_dirty_file_even_with_pending_upload() {
        let f = fixture("remote-delete-dirty");
        let path = f.dir.join("rapid.bin");
        std::fs::write(&path, b"precious local bytes").unwrap();
        let mut old = row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        old.local_state = LocalState::Dirty;
        f.ctx.store.upsert_item(&old).unwrap();
        f.ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&old.id),
                &serde_json::json!({
                    "path": path, "intent_kind": "version", "intent_storage": "old-intent",
                })
                .to_string(),
            )
            .unwrap();
        let delta = Delta {
            deletes: vec![old.id.clone()],
            ..Default::default()
        };
        let applied = f.ctx.store.apply_delta(f.p.pid, &delta).unwrap();
        let w = disk_apply(&f.ctx, &f.p, &applied.changes);
        assert!(w.enqueued);
        assert_eq!(std::fs::read(&path).unwrap(), b"precious local bytes");
        assert!(f.ctx.store.item(f.p.pid, &old.id).unwrap().is_none());
        let rebound = find_child_local(&f.ctx, f.p.pid, "co.ROOT", "rapid.bin").unwrap();
        assert!(is_local_id(&rebound.id) && rebound.local_state == LocalState::New);
        assert_eq!(jobs(&f), vec![("upload".into(), Some(rebound.id))]);
    }

    #[test]
    fn duplicate_alias_repair_requires_clean_matching_remote_identity() {
        let f = fixture("own-create-repair");
        let path = f.dir.join("rapid.bin");
        std::fs::write(&path, b"local bytes stay intact").unwrap();
        let mut remote = row(&f.p, "remote-one", Some("co.ROOT"), "rapid.bin", Kind::File);
        remote.size = std::fs::metadata(&path).unwrap().len() as i64;
        let mut alias = row(&f.p, "local-one", Some("co.ROOT"), "rapid.bin", Kind::File);
        alias.remote_version = None;
        alias.base_version = None;
        alias.local_state = LocalState::Dirty;
        f.ctx.store.upsert_item(&remote).unwrap();
        f.ctx.store.upsert_item(&alias).unwrap();
        let mut info = PlaceholderInfo {
            identity: ph::encode_identity(Kind::File, f.p.pid, &remote.id),
            in_sync: true,
            pin_state: PinState::Inherit,
            hydrated: true,
            on_disk_size: 23,
            modified_size: 1,
        };
        let mut w = Work::default();
        assert!(repair_duplicate_alias(
            &f.ctx,
            f.p.pid,
            "co.ROOT",
            "rapid.bin",
            &path,
            &info,
            &mut w
        )
        .unwrap());
        assert!(
            f.ctx.store.item(f.p.pid, &alias.id).unwrap().is_some(),
            "dirty alias is preserved"
        );
        assert!(w.enqueued, "dirty alias has a bound upload job");
        f.ctx
            .store
            .cancel_jobs(f.p.pid, &alias.id, "upload")
            .unwrap();
        info.identity = ph::encode_identity(Kind::File, f.p.pid, "unrelated");
        info.modified_size = 0;
        assert!(!repair_duplicate_alias(
            &f.ctx,
            f.p.pid,
            "co.ROOT",
            "rapid.bin",
            &path,
            &info,
            &mut Work::default()
        )
        .unwrap());
        assert!(
            f.ctx.store.item(f.p.pid, &alias.id).unwrap().is_some(),
            "wrong identity cannot delete the alias"
        );
        info.identity = ph::encode_identity(Kind::File, f.p.pid, &remote.id);
        assert!(repair_duplicate_alias(
            &f.ctx,
            f.p.pid,
            "co.ROOT",
            "rapid.bin",
            &path,
            &info,
            &mut Work::default()
        )
        .unwrap());
        assert!(
            f.ctx.store.item(f.p.pid, &alias.id).unwrap().is_some(),
            "Dirty state still protects the bytes when CFAPI says clean"
        );
        alias.local_state = LocalState::New;
        f.ctx.store.upsert_item(&alias).unwrap();
        f.ctx
            .store
            .cancel_jobs(f.p.pid, &alias.id, "upload")
            .unwrap();
        remote.size += 1;
        f.ctx.store.upsert_item(&remote).unwrap();
        assert!(repair_duplicate_alias(
            &f.ctx,
            f.p.pid,
            "co.ROOT",
            "rapid.bin",
            &path,
            &info,
            &mut Work::default()
        )
        .unwrap());
        assert!(
            f.ctx.store.item(f.p.pid, &alias.id).unwrap().is_some(),
            "size mismatch cannot delete the alias"
        );
        remote.size -= 1;
        f.ctx.store.upsert_item(&remote).unwrap();
        f.ctx
            .store
            .cancel_jobs(f.p.pid, &alias.id, "upload")
            .unwrap();
        assert!(!repair_duplicate_alias(
            &f.ctx,
            f.p.pid,
            "co.ROOT",
            "rapid.bin",
            &path,
            &info,
            &mut Work::default()
        )
        .unwrap());
        assert!(f.ctx.store.item(f.p.pid, &alias.id).unwrap().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"local bytes stay intact");
    }
}

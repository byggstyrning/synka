//! Outbox processing: uploads and remote mutations, with bounded retries.
//!
//! Job kinds and payloads (JSON in outbox.payload):
//!   "upload"  {"path": abs path}            new file (item LocalState::New) or new version
//!                                            (Dirty). Waits until the file can be opened with
//!                                            FILE_SHARE_READ only (not being written); while it
//!                                            is open elsewhere the job is deferred 10 s without
//!                                            counting an attempt.
//!   "finalize" {"path", "size", "mtime_ticks", "version", "usn"} retries only local CFAPI
//!                                            placeholder finalization after ACC accepted bytes.
//!   "mkdir"   {"path": abs path}            create remote folder, convert local dir to placeholder
//!   "rename"  {"id": .., "new_name": ..}    (rename/move send the row's CURRENT name/parent)
//!   "move"    {"id": .., "new_parent": .., "new_name": ..}
//!   "delete"  {"id": .., "is_dir": bool}    queued with a grace period (safe saves); skipped
//!                                            when the item came back or its folder goes too
//! A queued path that no longer exists is followed to the row's current location.
//! Upload flow: if the local copy was made from an older version than ACC has now
//!   (base_version set and != remote_version) -> keep both (`sync::keep_both`): the local file
//!   becomes "name (konflikt YYYY-MM-DD HHMM).ext" and is uploaded as a new file, the original
//!   name gets the cloud version back. Else: remote::create_storage -> remote::upload_file ->
//!   remote::create_version (existing; the ACC name is kept) or remote::create_item (new; a 409 "name
//!   exists" preserves the local file as a separately uploaded conflict copy). Then store row: base_version =
//!   remote_version = new number, LocalState::Clean; on disk the file becomes (or is re-pointed
//!   as) an in-sync placeholder of the item, with watcher::suppress around it.
//! Retry policy: 429/5xx/transport -> backoff 30 s * 2^attempts (max 1 h), dead after 8
//!   attempts; 4xx other than 429 -> dead immediately (LocalState::Error, UiMsg::Notify once).
//!   Dead jobs come back with `retry_failed` or the next local change of the file.
//! Concurrency: Semaphore(cfg.max_uploads). The engine runs a pass when a job is enqueued and
//!   otherwise sleeps until the next job is due (`Store::next_job_due`).
//!
//! Projects whose remote state changed through a successful job are queued for the engine:
//! drain them with `take_dirty_projects()` and poll those projects.

#[cfg(test)]
#[path = "upload_resilience_tests.rs"]
mod resilience_tests;

#[cfg(test)]
#[path = "upload_metadata_tests.rs"]
mod metadata_tests;

use crate::api::dm;
use crate::app::{CtxRef, Severity, Transfer, UiMsg};
use crate::cf::placeholder;
use crate::http::ApiError;
use crate::model::*;
use crate::remote;
use crate::store::OutboxJob;
use crate::sync::{self, naming, sanitize_name, watcher};
use anyhow::anyhow;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::Metadata;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const MAX_ATTEMPTS: i64 = 8;
const BASE_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 3_600_000;
const BATCH: usize = 50;
/// A file another program holds open is tried again after this long (no attempt counted).
const LOCKED_RETRY_MS: i64 = 10_000;
/// Retry after a network or sign-in error (the engine does not run the queue while offline).
const OFFLINE_RETRY_MS: i64 = 60_000;
/// FILE_SHARE_READ: open the file so that only readers may share it.
const FILE_SHARE_READ_BITS: u32 = 0x1;
const SUPPRESS_FOR: Duration = Duration::from_secs(5);

static SEM: OnceLock<Semaphore> = OnceLock::new();
static DIRTY: Mutex<Option<HashSet<Pid>>> = Mutex::new(None);

/// Uploads in progress: (tracking id, transfer). Read by the engine's status.
static ACTIVE: Mutex<Vec<(u64, Transfer)>> = Mutex::new(Vec::new());
static NEXT_TRACK: AtomicU64 = AtomicU64::new(1);
static UPLOADING_PATHS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Only one outbox job may create a version for a local path at a time.
struct UploadLease(String);

impl UploadLease {
    fn take(path: &Path) -> Option<Self> {
        let key = path.to_string_lossy().to_lowercase();
        let inserted = UPLOADING_PATHS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        // Release the mutex before constructing a lease. `then_some` eagerly
        // drops a rejected lease, whose Drop both re-locks this mutex and removes
        // the path still owned by another worker.
        inserted.then(|| Self(key))
    }
}

impl Drop for UploadLease {
    fn drop(&mut self) {
        UPLOADING_PATHS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Registers one upload for progress reporting; removed on drop.
struct Tracked(u64);

impl Tracked {
    fn start(name: &str, project: &str) -> Tracked {
        let id = NEXT_TRACK.fetch_add(1, Ordering::Relaxed);
        let t = Transfer {
            name: name.to_string(),
            project: project.to_string(),
            direction: "up",
            done: 0,
            total: 0,
        };
        ACTIVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, t));
        Tracked(id)
    }

    fn progress(&self, done: u64, total: u64) {
        let mut g = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, t)) = g.iter_mut().find(|(i, _)| *i == self.0) {
            t.done = done;
            t.total = total;
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        ACTIVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(i, _)| *i != self.0);
    }
}

/// Uploads currently running (for the status event).
pub fn active_transfers() -> Vec<Transfer> {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(_, t)| t.clone())
        .collect()
}

/// Projects changed by successful jobs since the last call (engine re-polls them).
pub fn take_dirty_projects() -> Vec<Pid> {
    let mut g = DIRTY.lock().unwrap_or_else(|e| e.into_inner());
    g.take()
        .map(|s| s.into_iter().collect())
        .unwrap_or_default()
}

fn mark_dirty(pid: Pid) {
    let mut g = DIRTY.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(HashSet::new).insert(pid);
}

/// Permanent failure: the job goes dead at once.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Permanent(String);

fn perm(msg: impl Into<String>) -> anyhow::Error {
    Permanent(msg.into()).into()
}

enum Step {
    Done,
    /// File is open, or the current upload job was replaced by a conflict-copy upload.
    /// Do not delete its outbox ID: SQLite may reuse it for the replacement job.
    Locked,
}

/// Run all due jobs with every provider connected (the engine calls `process_due_except`).
#[cfg(test)]
pub async fn process_due(ctx: &CtxRef) -> anyhow::Result<usize> {
    process_due_except(ctx, &HashSet::new()).await
}

/// `process_due` for an engine in which the providers in `offline` have no connection: their
/// jobs wait without a request and without an attempt, and the other providers' jobs run.
pub async fn process_due_except(
    ctx: &CtxRef,
    offline: &HashSet<Provider>,
) -> anyhow::Result<usize> {
    let jobs: Vec<OutboxJob> = ctx
        .store
        .due_jobs(now_ms(), BATCH)?
        .into_iter()
        .filter(|job| !waits(ctx, job, offline))
        .collect();
    if jobs.is_empty() {
        return Ok(0);
    }
    let sem: &'static Semaphore = SEM.get_or_init(|| Semaphore::new(ctx.cfg.max_uploads.max(1)));
    let mut set: JoinSet<bool> = JoinSet::new();
    for job in jobs {
        let permit = sem
            .acquire()
            .await
            .map_err(|e| anyhow!("upload semaphore closed: {e}"))?;
        let ctx = ctx.clone();
        set.spawn(async move {
            let _permit = permit;
            run_job(ctx, job).await
        });
    }
    let mut processed = 0;
    while let Some(r) = set.join_next().await {
        match r {
            Ok(true) => processed += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!("outbox task failed: {e}"),
        }
    }
    Ok(processed)
}

/// A job waits like offline work (see `handle_failure`): it is postponed without a request and
/// without spending an attempt, when its project cannot be served (its provider is signed out, or
/// StreamBIM is signed in to another region) or its provider has no connection (`offline`). The
/// other providers' jobs are not affected. A job of a gone project counts as ACC's. True when it
/// waits.
fn waits(ctx: &CtxRef, job: &OutboxJob, offline: &HashSet<Provider>) -> bool {
    let project = ctx.store.project(job.pid).ok().flatten();
    let provider = project.as_ref().map(|p| p.provider).unwrap_or_default();
    let connected = match &project {
        Some(p) => ctx.project_connected(p),
        None => ctx.signed_in(provider),
    };
    let waiting = if !connected {
        let target = match &project {
            Some(p) => ctx.sign_in_target(p),
            None => provider.label().to_string(),
        };
        format!("Logga in på {target} för att synka.")
    } else if offline.contains(&provider) {
        format!("Ingen anslutning till {}.", provider.label())
    } else {
        return false;
    };
    let _ = ctx
        .store
        .job_postponed(job.id, now_ms() + OFFLINE_RETRY_MS, &waiting);
    true
}

/// Returns true when the job reached a final state (done or failed), false when it was
/// deferred because the file is locked.
async fn run_job(ctx: CtxRef, job: OutboxJob) -> bool {
    if sync::project_resetting(job.pid) {
        let _ = ctx.store.defer_job(job.id, now_ms() + LOCKED_RETRY_MS);
        return false;
    }
    match execute(&ctx, &job).await {
        Ok(Step::Done) => {
            if let Err(e) = finish_job(&ctx, &job) {
                tracing::warn!("outbox: finish {}: {e:#}", job.id);
            }
            mark_dirty(job.pid);
            true
        }
        Ok(Step::Locked) => {
            if let Err(e) = ctx.store.defer_job(job.id, now_ms() + LOCKED_RETRY_MS) {
                tracing::warn!("outbox: defer {}: {e:#}", job.id);
            }
            false
        }
        Err(e) => {
            // Remote object already gone: the desired end state is reached.
            let gone = matches!(job.kind.as_str(), "rename" | "move" | "delete")
                && e.downcast_ref::<ApiError>().and_then(ApiError::status) == Some(404);
            if gone {
                let _ = ctx.store.job_done(job.id);
                mark_dirty(job.pid);
            } else {
                handle_failure(&ctx, &job, &e);
            }
            true
        }
    }
}

/// A watcher can observe a save after the upload's final content check but before
/// its job is deleted. Coalescing keeps that old job, so completion must restore
/// a queue entry for any local edits still owned by the row at this path.
fn finish_job(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<()> {
    ctx.store.job_done(job.id)?;
    if !matches!(job.kind.as_str(), "upload" | "finalize") {
        return Ok(());
    }
    let fields = payload(job)?;
    let queued_path = PathBuf::from(str_field(&fields, "path")?);
    let _guard = sync::local_lock();
    let path = current_path(ctx, job)
        .filter(|path| path.is_file())
        .unwrap_or(queued_path);
    if !path.is_file() || sync::project_resetting(job.pid) {
        return Ok(());
    }
    // A completed create can have adopted a remote ID after this worker took
    // its job snapshot. Resolve the current row rather than trusting job.item.
    let Some(loc) = locate(ctx, &path)? else {
        return Ok(());
    };
    let Some(item) = loc.existing.filter(|item| {
        item.kind == Kind::File && matches!(item.local_state, LocalState::Dirty | LocalState::New)
    }) else {
        return Ok(());
    };
    if loc.project.pid == job.pid
        && !ctx
            .store
            .has_job(job.pid, &item.id, &["upload", "finalize"])?
    {
        // This is process_local's recovery path, under the same lock as its
        // state check. A clean plain file must not become dirty during cleanup.
        sync::resume_create_or_requeue(ctx, job.pid, &item, &path, &mut sync::Work::default());
    }
    Ok(())
}

fn is_transient(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<Permanent>().is_some()
        || err.downcast_ref::<sync::IncompleteConflictCopy>().is_some()
        || err
            .downcast_ref::<sync::ConflictDetachFailed>()
            .is_some_and(|failure| failure.rollback_failed)
    {
        return false;
    }
    match err.downcast_ref::<ApiError>() {
        Some(ApiError::Status { status, .. }) => *status == 408 || *status == 429 || *status >= 500,
        // The provider does not offer this operation, or the engine's own message: the same
        // request fails the same way.
        Some(ApiError::Unsupported(_) | ApiError::Message(_)) => false,
        // Transport, auth and decode problems are retried.
        Some(_) => true,
        // Local I/O and "parent folder not synced yet" are retried.
        None => true,
    }
}

/// No network or no valid sign-in: nothing is wrong with the job itself.
fn is_offline(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<ApiError>(),
        Some(ApiError::Transport(_) | ApiError::Auth(_))
    )
}

/// The provider's API does not offer the operation (`ApiError::Unsupported`). Retrying cannot
/// help, so the job is parked like a refused file type (`handle_failure`).
fn unsupported_operation(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<ApiError>(),
        Some(ApiError::Unsupported(_))
    )
}

/// ACC's explicit file-type rejection is permanent. Other 403 responses may be
/// permissions problems, so require the documented extension detail as well.
fn unsupported_file_type(err: &anyhow::Error) -> bool {
    let Some(ApiError::Status { status: 403, body }) = err.downcast_ref::<ApiError>() else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value["errors"].as_array().is_some_and(|errors| {
        errors.iter().any(|error| {
            error["code"] == "ERR_NOT_ALLOWED"
                && error["detail"].as_str().is_some_and(|detail| {
                    detail.starts_with("File with extension ")
                        && detail.ends_with(" is not allowed to upload.")
                })
        })
    })
}

/// This explicit rejection proves that the attempted version was not committed.
/// A lost reply or a server failure must keep its storage intent for reconciliation.
fn rejected_mime_type(err: &anyhow::Error) -> bool {
    let Some(ApiError::Status { status: 400, body }) = err.downcast_ref::<ApiError>() else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value["errors"].as_array().is_some_and(|errors| {
        errors.iter().any(|error| {
            error["code"] == "ERR_WIP_ERROR" && error["title"] == "VER_MISMATCH_MIMETYPE"
        })
    })
}

pub(crate) fn handle_failure(ctx: &CtxRef, job: &OutboxJob, err: &anyhow::Error) {
    let msg = format!("{err:#}");
    if is_offline(err) {
        // A pause, not a failure: offline for hours must not use up the job's attempts.
        if let Err(e) = ctx
            .store
            .job_postponed(job.id, now_ms() + OFFLINE_RETRY_MS, &msg)
        {
            tracing::warn!("outbox: postpone {}: {e:#}", job.id);
        }
        tracing::debug!(
            "outbox: job {} ({}) waits for the network: {msg}",
            job.id,
            job.kind
        );
        return;
    }
    // Parked for good (dead = 3, local change kept, one notice): ACC refused the file type, or
    // the provider does not offer the operation.
    let refused_file_type = job.kind == "upload" && unsupported_file_type(err);
    if refused_file_type || unsupported_operation(err) {
        if let Err(e) = ctx.store.job_unsupported(job.id, &msg) {
            tracing::warn!("outbox: job_unsupported {}: {e:#}", job.id);
        }
        if let Some(id) = row_id(ctx, job) {
            let _ = ctx.store.set_local_state(job.pid, &id, LocalState::Error);
        }
        tracing::info!("outbox: unsupported job {} ({}): {msg}", job.id, job.kind);
        let project = ctx
            .store
            .project(job.pid)
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default();
        ctx.ui
            .activity("unsupported", &describe(job), &project, &msg);
        ctx.ui.send(UiMsg::notify(
            Severity::Info,
            format!("Kunde inte synkronisera {}: {msg}", describe(job)),
        ));
        return;
    }
    let dead = !is_transient(err) || job.attempts + 1 >= MAX_ATTEMPTS;
    if dead {
        if let Err(e) = ctx.store.job_failed(job.id, now_ms(), &msg, true) {
            tracing::warn!("outbox: job_failed {}: {e:#}", job.id);
        }
        if let Some(id) = row_id(ctx, job) {
            let _ = ctx.store.set_local_state(job.pid, &id, LocalState::Error);
        }
        tracing::warn!("outbox: job {} ({}) dead: {msg}", job.id, job.kind);
        let project = ctx
            .store
            .project(job.pid)
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default();
        ctx.ui.activity("error", &describe(job), &project, &msg);
        notify(
            ctx,
            format!("Kunde inte synkronisera {}: {msg}", describe(job)),
        );
    } else {
        let shift = job.attempts.clamp(0, 20) as u32;
        let delay = (BASE_BACKOFF_MS << shift).min(MAX_BACKOFF_MS);
        if let Err(e) = ctx.store.job_failed(job.id, now_ms() + delay, &msg, false) {
            tracing::warn!("outbox: job_failed {}: {e:#}", job.id);
        }
        tracing::info!(
            "outbox: job {} ({}) retry in {} s: {msg}",
            job.id,
            job.kind,
            delay / 1000
        );
    }
}

fn notify(ctx: &CtxRef, body: String) {
    ctx.ui.send(UiMsg::notify(Severity::Error, body));
}

fn describe(job: &OutboxJob) -> String {
    serde_json::from_str::<Value>(&job.payload)
        .ok()
        .and_then(|v| {
            v.get("path")
                .and_then(Value::as_str)
                .map(|p| {
                    Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.to_string())
                })
                .or_else(|| v.get("id").and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| format!("job {}", job.id))
}

/// Local row affected by a job, for LocalState::Error.
fn row_id(ctx: &CtxRef, job: &OutboxJob) -> Option<String> {
    if let Some(id) = &job.item {
        return Some(id.clone());
    }
    if job.kind == "upload" || job.kind == "mkdir" {
        let v: Value = serde_json::from_str(&job.payload).ok()?;
        let path = PathBuf::from(v.get("path")?.as_str()?);
        return locate(ctx, &path)
            .ok()
            .flatten()
            .and_then(|l| l.existing)
            .map(|e| e.id);
    }
    None
}

async fn execute(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<Step> {
    match job.kind.as_str() {
        "upload" => upload(ctx, job).await,
        "finalize" => finalize_upload(ctx, job),
        "mkdir" => mkdir(ctx, job).await,
        "rename" => rename(ctx, job).await,
        "move" => move_item(ctx, job).await,
        "delete" => delete(ctx, job).await,
        other => Err(perm(format!("unknown job kind {other}"))),
    }
}

fn payload(job: &OutboxJob) -> anyhow::Result<Value> {
    serde_json::from_str(&job.payload).map_err(|e| perm(format!("bad payload: {e}")))
}

fn str_field<'a>(v: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| perm(format!("payload missing {key}")))
}

/// A local path resolved to its project, existing row (if any) and parent folder.
struct Located {
    project: Project,
    existing: Option<Item>,
    parent: String,
    name: String,
}

/// Resolve `path` (under the sync root) to project, parent folder id and existing row, with
/// the same name rules as the engine. Ok(None) = not inside a synced project. Err = a parent
/// folder is not known (yet).
fn locate(ctx: &CtxRef, path: &Path) -> anyhow::Result<Option<Located>> {
    let Some(comps) = sync::rel_components(&ctx.paths.sync_root, path) else {
        return Ok(None);
    };
    if !sync::local_path_is_safe(&ctx.paths.sync_root, path) {
        return Err(perm(format!(
            "lokal sökväg innehåller en länk eller kan inte verifieras: {}",
            path.display()
        )));
    }
    // hub, project, then at least the item name
    if comps.len() < 3 {
        return Ok(None);
    }
    let Some(project) = ctx
        .store
        .projects()?
        .into_iter()
        .find(|p| p.enabled && sync::project_matches(p, &comps[0], &comps[1]))
    else {
        return Ok(None);
    };
    let pid = project.pid;
    let mut parent = project.root_id().to_string();
    let last = comps.len() - 1;
    for dir in &comps[2..last] {
        match sync::find_child_local(ctx, pid, &parent, dir) {
            Some(it) if it.kind.is_dir() => parent = it.id,
            _ => return Err(anyhow!("parent folder \"{dir}\" is not synced yet")),
        }
    }
    let name = comps[last].clone();
    let existing = sync::find_child_local(ctx, pid, &parent, &name);
    Ok(Some(Located {
        project,
        existing,
        parent,
        name,
    }))
}

fn open_shared(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_BITS)
        .open(path)
}

fn is_sharing_violation(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33)
    matches!(e.raw_os_error(), Some(32) | Some(33))
}

fn mtime_ms(m: &Metadata) -> i64 {
    ms_of(m.modified().ok())
}

fn ctime_ms(m: &Metadata) -> i64 {
    ms_of(m.created().ok())
}

struct CreateIntent {
    storage: String,
    size: u64,
    mtime_ms: i64,
    mtime_ticks: u64,
    usn: Option<i64>,
    version: bool,
}

impl CreateIntent {
    fn from_payload(v: &Value) -> anyhow::Result<Option<Self>> {
        let Some(storage) = v.get("intent_storage").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(Self {
            storage: storage.to_string(),
            size: v["intent_size"]
                .as_u64()
                .ok_or_else(|| perm("intent missing size"))?,
            mtime_ms: v["intent_mtime_ms"]
                .as_i64()
                .ok_or_else(|| perm("intent missing mtime"))?,
            mtime_ticks: v["intent_mtime_ticks"]
                .as_u64()
                .ok_or_else(|| perm("intent missing write time"))?,
            usn: v["intent_usn"].as_i64(),
            version: v["intent_kind"] == "version",
        }))
    }

    fn matches(&self, meta: &Metadata) -> bool {
        meta.len() == self.size && meta.last_write_time() == self.mtime_ticks
    }
}

/// A tip with the exact persisted OSS object is our own version, even if the
/// create_version response was lost. Advance the base before retrying newer
/// local bytes so our commit is never mistaken for a colleague's edit.
fn adopt_version_tip(remote: &mut Item, tip: Item, intent: &CreateIntent) -> bool {
    if tip.storage_urn.as_deref() != Some(intent.storage.as_str()) || tip.remote_version.is_none() {
        return false;
    }
    remote.remote_version = tip.remote_version;
    remote.base_version = tip.remote_version;
    remote.storage_urn = tip.storage_urn;
    remote.version_urn = tip.version_urn;
    remote.size = intent.size as i64;
    remote.mtime_ms = intent.mtime_ms;
    remote.local_state = LocalState::Dirty;
    true
}

/// Check the file through a read-only handle that refuses concurrent writers. Compare the
/// full filesystem timestamp, not the millisecond value we store in ACC: rapid same-size
/// saves can share one millisecond but still have distinct NTFS write times.
fn unchanged_since(path: &Path, before: &Metadata) -> std::io::Result<bool> {
    let file = open_shared(path)?;
    let after = file.metadata()?;
    Ok(after.len() == before.len() && after.modified()? == before.modified()?)
}

/// Read a change-journal guard when available. A volume without a USN journal
/// keeps the existing size/time fingerprint path instead of stalling uploads.
fn available_usn(path: &Path) -> Option<i64> {
    match placeholder::file_usn(path) {
        Ok(usn) => usn,
        Err(error) => {
            tracing::warn!(
                "USN for {}: {error:#}; using file fingerprint",
                path.display()
            );
            None
        }
    }
}

fn original_usn_matches(path: &Path, expected: Option<i64>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    available_usn(path).is_none_or(|current| current == expected)
}

/// A rejected pre-upload USN means new bytes need a new cloud version only
/// when the source actually moved since our snapshot. A CFAPI failure after
/// its own metadata transition must instead retry local finalization of the
/// already committed version. If a previously available journal cannot be
/// read now, conservatively treat the source as changed.
fn needs_new_upload_after_mark_error(
    path: &Path,
    snapshot: &Metadata,
    expected_usn: Option<i64>,
    error: &anyhow::Error,
) -> bool {
    if !error.is::<placeholder::UsnMismatch>() {
        return false;
    }
    if !unchanged_since(path, snapshot).unwrap_or(false) {
        return true;
    }
    expected_usn.is_some_and(
        |expected| !matches!(placeholder::file_usn(path), Ok(Some(current)) if current == expected),
    )
}

/// Bring a local row up to the latest ACC tip while retaining the version from
/// which the local edit began. A changed tip requires a conflict copy.
fn apply_remote_tip(ex: &mut Item, tip: Item) -> bool {
    if tip.remote_version > ex.remote_version {
        ex.remote_version = tip.remote_version;
        ex.version_urn = tip.version_urn;
        ex.storage_urn = tip.storage_urn;
        ex.size = tip.size;
        ex.mtime_ms = tip.mtime_ms;
        ex.modified_by = tip.modified_by;
        ex.locked_by = tip.locked_by;
    }
    ex.base_version.is_some() && ex.remote_version.is_some() && ex.remote_version != ex.base_version
}

async fn preserve_conflict(
    ctx: &CtxRef,
    p: &Project,
    ex: &Item,
    path: &Path,
) -> anyhow::Result<()> {
    let (ctx2, p2, ex2, path2) = (ctx.clone(), p.clone(), ex.clone(), path.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let _guard = sync::local_lock();
        let mut w = sync::Work::default();
        sync::keep_both(&ctx2, &p2, &ex2, &path2, &mut w)
    })
    .await
    .map_err(|e| anyhow!("conflict task: {e}"))??;
    Ok(())
}

/// A 409 on a new item has no durable request marker proving the colliding ACC item is ours.
/// Preserve the bytes as a new local-only item and let the outbox create it under a free name.
async fn preserve_new_item_collision(
    ctx: &CtxRef,
    p: &Project,
    path: &Path,
    parent: &str,
    local_row: Option<Item>,
) -> anyhow::Result<()> {
    let (ctx2, p2, path2, parent2) = (
        ctx.clone(),
        p.clone(),
        path.to_path_buf(),
        parent.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        let _guard = sync::local_lock();
        let mut work = sync::Work::default();
        sync::conflict_rename(&ctx2, &p2, &path2, &parent2, local_row, &mut work)
    })
    .await
    .map_err(|e| anyhow!("collision task: {e}"))??;
    Ok(())
}

fn ms_of(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// After ACC accepts bytes, make the local item an in-sync placeholder of `id`.
/// A failure must remain visible to the outbox, because Windows will still consider
/// locally modified byte ranges unsynced even though the database has a cloud version.
fn mark_on_disk(
    path: &Path,
    kind: Kind,
    pid: Pid,
    id: &str,
    size: u64,
    uploaded_usn: Option<i64>,
) -> anyhow::Result<()> {
    // suppress() matches by prefix. Limit it to the file: the watcher delays its own
    // metadata events, while edits inside a parent folder must still be inspected.
    if kind == Kind::File {
        watcher::suppress(path, SUPPRESS_FOR);
    }
    let identity = placeholder::encode_identity(kind, pid, id);
    if placeholder::placeholder_info(path)?.is_some() {
        if kind == Kind::File {
            placeholder::set_uploaded_file_guarded(path, &identity, size, uploaded_usn)
        } else {
            placeholder::set_identity(path, &identity, true)
        }
    } else {
        match (kind, uploaded_usn) {
            (Kind::File, Some(usn)) => {
                placeholder::convert_to_placeholder_guarded(path, &identity, usn)
            }
            _ => placeholder::convert_to_placeholder(path, &identity, true),
        }
    }
}

fn retry_changed_upload(
    ctx: &CtxRef,
    job: &OutboxJob,
    item_id: &str,
    path: &Path,
) -> anyhow::Result<Step> {
    ctx.store
        .set_local_state(job.pid, item_id, LocalState::Dirty)?;
    // A saved create intent refers to the old OSS bytes. Drop it from the job
    // before retrying so a new save gets a fresh storage object.
    ctx.store.retarget_job(
        job.id,
        "upload",
        &serde_json::json!({ "path": path }).to_string(),
    )?;
    Ok(Step::Locked)
}

/// Bind a newly created ACC item to the local job before deciding whether its
/// saved storage still represents the current file. A lost create response may
/// be recovered by a matching storage URN even after another local save; in
/// that case the old bytes remain a valid first cloud version and the newer
/// bytes must upload as a version of this bound remote item.
struct UploadedSnapshot<'a> {
    path: &'a Path,
    meta: &'a Metadata,
    mtime: i64,
    size: i64,
    usn: Option<i64>,
    intent: Option<&'a CreateIntent>,
}

fn bind_created_file(
    ctx: &CtxRef,
    job: &OutboxJob,
    loc: &Located,
    stale: Option<Item>,
    created: Item,
    storage: Option<String>,
    snapshot: &UploadedSnapshot<'_>,
) -> anyhow::Result<(Item, bool)> {
    let _guard = sync::local_lock();
    // A poll may have seen a colleague's later version and moved the local
    // bytes to a conflict copy while create_item was in flight. In that case
    // the temporary row now names the copy: never rekey it (or its new job)
    // onto the original remote item.
    if let Some(stale) = stale.as_ref().filter(|s| s.id != created.id) {
        let still_here = ctx
            .store
            .item(loc.project.pid, &stale.id)?
            .is_some_and(|current| {
                current.parent.as_deref() == Some(loc.parent.as_str())
                    && naming::same_name(&sanitize_name(&current.name), &loc.name)
            });
        if !still_here {
            return Ok((
                ctx.store
                    .item(loc.project.pid, &created.id)?
                    .unwrap_or(created),
                true,
            ));
        }
    }
    if let Some(stale) = stale.filter(|s| s.id != created.id) {
        ctx.store
            .rekey_item(loc.project.pid, &stale.id, &created.id)?;
    }
    let later_tip = ctx
        .store
        .item(loc.project.pid, &created.id)?
        .filter(|target| {
            target.storage_urn != storage
                && target.remote_version.unwrap_or_default()
                    >= created.remote_version.unwrap_or_default()
        });
    if let Some(mut target) = later_tip {
        // The local bytes descend from our accepted first version, while the
        // DB must retain the colleague's newer tip. The next upload sees this
        // base/tip mismatch and saves both versions.
        target.base_version = created.remote_version;
        target.local_state = LocalState::Dirty;
        ctx.store.upsert_item(&target)?;
        retry_changed_upload(ctx, job, &target.id, snapshot.path)?;
        return Ok((target, true));
    }
    let mut row = Item {
        pid: loc.project.pid,
        parent: Some(loc.parent.clone()),
        kind: Kind::File,
        base_version: created.remote_version,
        size: snapshot.size,
        ctime_ms: if created.ctime_ms > 0 {
            created.ctime_ms
        } else {
            ctime_ms(snapshot.meta)
        },
        mtime_ms: snapshot.mtime,
        storage_urn: storage,
        locked_by: None,
        local_state: LocalState::Clean,
        populated: false,
        ..created
    };
    let changed = snapshot
        .intent
        .is_some_and(|intent| !intent.matches(snapshot.meta))
        || !unchanged_since(snapshot.path, snapshot.meta).unwrap_or(false)
        || !original_usn_matches(snapshot.path, snapshot.usn);
    if changed {
        row.local_state = LocalState::Dirty;
        ctx.store.upsert_item(&row)?;
        retry_changed_upload(ctx, job, &row.id, snapshot.path)?;
    }
    Ok((row, changed))
}

/// Finish a previously committed cloud version without uploading it again. The exact
/// Windows write time guards against finalizing bytes saved after the ACC commit.
fn finalize_upload(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<Step> {
    let fields = payload(job)?;
    let item_id = job
        .item
        .as_deref()
        .ok_or_else(|| perm("finalize missing item"))?;
    let Some(row) = ctx.store.item(job.pid, item_id)? else {
        return Ok(Step::Done);
    };
    let original_path = PathBuf::from(str_field(&fields, "path")?);
    // A rename may leave a different new file at the old path. Follow this row's
    // current location before using the saved path from the accepted upload.
    let Some(path) = current_path(ctx, job).filter(|p| p.is_file()) else {
        if original_path.is_file() {
            tracing::info!("finalize skipped obsolete path {}", original_path.display());
        }
        return Ok(Step::Done);
    };
    let size = fields["size"]
        .as_u64()
        .ok_or_else(|| perm("finalize missing size"))?;
    let mtime_ticks = fields["mtime_ticks"]
        .as_u64()
        .ok_or_else(|| perm("finalize missing mtime_ticks"))?;
    let version = fields["version"]
        .as_i64()
        .ok_or_else(|| perm("finalize missing version"))?;
    let current = std::fs::metadata(&path)?;
    let uploaded_usn = fields["usn"].as_i64();
    if current.len() != size
        || current.last_write_time() != mtime_ticks
        || row.remote_version != Some(version)
        || !original_usn_matches(&path, uploaded_usn)
    {
        // Another save or a newer cloud version superseded the committed bytes. Keep
        // the local file dirty and let the ordinary upload path reconcile it.
        return retry_changed_upload(ctx, job, item_id, &path);
    }
    if let Err(error) = mark_on_disk(&path, Kind::File, job.pid, item_id, size, uploaded_usn) {
        if needs_new_upload_after_mark_error(&path, &current, uploaded_usn, &error) {
            return retry_changed_upload(ctx, job, item_id, &path);
        }
        if let Some(usn) = error
            .downcast_ref::<placeholder::FinalizationRetryUsn>()
            .map(|retry| retry.usn)
        {
            ctx.store.update_finalize_usn(job.id, usn)?;
        }
        return Err(error);
    }
    ctx.store
        .set_local_state(job.pid, item_id, LocalState::Clean)?;
    Ok(Step::Done)
}

/// Where the job's item is now, when its queued path no longer exists (renamed or moved
/// locally after the job was queued).
fn current_path(ctx: &CtxRef, job: &OutboxJob) -> Option<PathBuf> {
    let id = job.item.as_deref()?;
    let p = ctx.store.project(job.pid).ok()??;
    sync::item_path(ctx, &p, id)
}

/// The queued path is gone and the item is nowhere: forget a local-only row (it was never
/// created in ACC), so a later file with the same name is not mistaken for it.
fn forget_local_row(ctx: &CtxRef, job: &OutboxJob) {
    if let Some(id) = job.item.as_deref().filter(|id| is_local_id(id)) {
        let _ = ctx.store.delete_item(job.pid, id);
    }
}

async fn upload(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<Step> {
    let fields = payload(job)?;
    let mut path = PathBuf::from(str_field(&fields, "path")?);
    if !path.is_file() {
        match current_path(ctx, job).filter(|p| p.is_file()) {
            Some(p) => path = p,
            None => {
                // Removed before upload; the delete (if any) is handled on its own.
                forget_local_row(ctx, job);
                return Ok(Step::Done);
            }
        }
    }
    let Some(_lease) = UploadLease::take(&path) else {
        return Ok(Step::Locked);
    };
    let Some(mut loc) = locate(ctx, &path)? else {
        return Ok(Step::Done);
    };
    let p = &loc.project;
    let pid = p.pid;
    // Poll can insert a remote row under the same name before this job binds its
    // local-* row. Keep the job attached to its own bytes rather than treating the
    // remote row as an ordinary version target.
    if let Some(id) = job.item.as_deref().filter(|id| is_local_id(id)) {
        if let Some(local) = ctx.store.item(pid, id)?.filter(|row| {
            row.parent.as_deref() == Some(loc.parent.as_str())
                && naming::same_name(&sanitize_name(&row.name), &loc.name)
        }) {
            loc.existing = Some(local);
        }
    }
    if job.item.is_none() {
        // Older jobs queued below a newly created folder had no item ID. Give
        // the path one bound job before any ACC request; an unbound and a bound
        // job must never create the same file independently.
        let _guard = sync::local_lock();
        match sync::find_child_local(ctx, pid, &loc.parent, &loc.name) {
            None => {
                sync::register_local(ctx, pid, &loc.parent, &loc.name, false, &path)?;
            }
            Some(row) if !ctx.store.has_job(pid, &row.id, &["upload", "finalize"])? => {
                ctx.store.enqueue(
                    pid,
                    "upload",
                    Some(&row.id),
                    &serde_json::json!({ "path": path }).to_string(),
                )?;
            }
            Some(_) => {}
        }
        return Ok(Step::Done);
    }
    // The queued item no longer lives at this path (moved away in ACC): it has no local
    // changes of its own any more; what is here is uploaded as whatever row the path has.
    if let Some(id) = job.item.as_deref() {
        if loc.existing.as_ref().map(|e| e.id.as_str()) != Some(id) {
            if let Ok(Some(row)) = ctx.store.item(pid, id) {
                if row.local_state == LocalState::Dirty {
                    let _ = ctx.store.set_local_state(pid, id, LocalState::Clean);
                }
            }
        }
    }
    // Parent folder still has its temporary id: its mkdir job has not finished. Transient.
    if is_local_id(&loc.parent) {
        return Err(anyhow!("parent folder is not created in ACC yet"));
    }
    match open_shared(&path) {
        Ok(f) => drop(f),
        Err(e) if is_sharing_violation(&e) => return Ok(Step::Locked),
        Err(e) => return Err(e.into()),
    }
    // The in-sync flag alone is insufficient: Cloud Files can still report locally
    // appended bytes in MODIFIED ranges, especially after a zero-byte file grows.
    if matches!(placeholder::placeholder_info(&path), Ok(Some(ref i)) if i.in_sync && i.modified_size == 0)
        && loc
            .existing
            .as_ref()
            .is_some_and(|e| e.local_state == LocalState::Clean)
    {
        return Ok(Step::Done);
    }

    let mut existing = loc.existing.clone().filter(|e| e.kind == Kind::File);
    let saved_intent = CreateIntent::from_payload(&fields)?;
    // Only ACC's listing can show that a saved intent's object is our committed version. Elsewhere
    // the saved marker proves nothing, so a resumed intent always uploads again.
    let intent_commits = remote::intent_proves_commit(p.provider);
    let (mut checked_tip, upload_info) =
        if let Some(remote) = existing.as_ref().filter(|item| !is_local_id(&item.id)) {
            let (tip, info) = remote::get_item_upload_info(ctx, p, &remote.id).await?;
            // Check even a resumed intent before it can create another version.
            info.ensure_writable()
                .map_err(|error| perm(error.to_string()))?;
            (Some(tip), Some(info))
        } else {
            (None, None)
        };
    if let (Some(intent), Some(remote), true) = (&saved_intent, existing.as_mut(), intent_commits) {
        if intent.version
            && !is_local_id(&remote.id)
            && remote.storage_urn.as_deref() != Some(intent.storage.as_str())
        {
            // create_version may have succeeded even when its reply was lost. The
            // exact OSS object proves whether the current ACC tip is our commit.
            let tip = checked_tip
                .as_ref()
                .expect("remote target was checked")
                .clone();
            if adopt_version_tip(remote, tip, intent) {
                ctx.store.upsert_item(remote)?;
            }
        }
    }
    if let (Some(intent), Some(remote), true) = (&saved_intent, existing.as_ref(), intent_commits) {
        if !is_local_id(&remote.id)
            && remote.remote_version.is_some()
            && remote.storage_urn.as_deref() == Some(intent.storage.as_str())
        {
            ctx.store
                .set_base_version(pid, &remote.id, remote.remote_version)?;
            ctx.store
                .set_local_state(pid, &remote.id, LocalState::Dirty)?;
            let meta = std::fs::metadata(&path)?;
            if !intent.matches(&meta) || !original_usn_matches(&path, intent.usn) {
                return retry_changed_upload(ctx, job, &remote.id, &path);
            }
            if let Err(error) =
                mark_on_disk(&path, Kind::File, pid, &remote.id, intent.size, intent.usn)
            {
                if error.is::<placeholder::UsnMismatch>() {
                    return retry_changed_upload(ctx, job, &remote.id, &path);
                }
                return Err(error);
            }
            if changed_after_mark(ctx, pid, &remote.id, &path, &meta)? {
                return retry_changed_upload(ctx, job, &remote.id, &path);
            }
            ctx.store
                .set_local_state(pid, &remote.id, LocalState::Clean)?;
            return Ok(Step::Done);
        }
    }
    if let Some(ex) = existing
        .as_mut()
        .filter(|e| e.base_version.is_some() && e.remote_version.is_some() && !is_local_id(&e.id))
    {
        // The last poll may be minutes old: ask ACC for the tip now, so a version a colleague
        // saved in the meantime is not buried under this one.
        if let Some(tip) = checked_tip.take() {
            apply_remote_tip(ex, tip);
        }
    }
    if let Some(ex) = &existing {
        // Changed in ACC since the local copy was downloaded: keep both. (No base version means
        // the local file was not made from any ACC version, e.g. saved over a placeholder that
        // was never opened: that is a plain new version.)
        if ex.base_version.is_some()
            && ex.remote_version.is_some()
            && ex.remote_version != ex.base_version
        {
            preserve_conflict(ctx, p, ex, &path).await?;
            return Ok(Step::Locked);
        }
    }

    let tracked = Tracked::start(&loc.name, &p.name);
    let meta = std::fs::metadata(&path)?;
    let (mtime, size, storage, uploaded_usn) = if let Some(intent) = &saved_intent {
        (
            intent.mtime_ms,
            intent.size as i64,
            intent.storage.clone(),
            intent.usn,
        )
    } else {
        let storage = remote::create_storage(ctx, p, &loc.parent, &loc.name).await?;
        let uploaded_usn = available_usn(&path);
        remote::upload_file(
            ctx,
            p,
            &storage,
            &path,
            upload_info.as_ref().and_then(dm::UploadInfo::mime_type),
            &|d, t| tracked.progress(d, t),
        )
        .await?;
        (mtime_ms(&meta), meta.len() as i64, storage, uploaded_usn)
    };

    // Edited again while uploading: what was read may mix old and new bytes. No version is
    // made from it; the job runs again in a moment with the new content (the unused storage
    // object is left for OSS to expire).
    if saved_intent.is_none()
        && (!unchanged_since(&path, &meta).unwrap_or(false)
            || !original_usn_matches(&path, uploaded_usn))
    {
        tracing::info!("{} ändrades under uppladdningen, laddar upp igen", loc.name);
        return Ok(Step::Locked);
    }
    // Large OSS transfers can take minutes. A colleague may have committed a new
    // tip after our pre-upload check; preserve both before creating our version.
    // This POST carries no atomic tip precondition, so a narrower race still
    // remains between this GET and POST /versions.
    if let Some(ex) = existing.as_mut().filter(|e| !is_local_id(&e.id)) {
        let (tip, info) = remote::get_item_upload_info(ctx, p, &ex.id).await?;
        info.ensure_writable()
            .map_err(|error| perm(error.to_string()))?;
        if ex.base_version.is_some() && ex.remote_version.is_some() && apply_remote_tip(ex, tip) {
            preserve_conflict(ctx, p, ex, &path).await?;
            return Ok(Step::Locked);
        }
        if info.mime_type() != upload_info.as_ref().and_then(dm::UploadInfo::mime_type) {
            return Err(anyhow!(
                "ACC file MIME type changed during upload; retry with current metadata"
            ));
        }
    }
    let state = LocalState::Clean;
    let mut intent_persisted = saved_intent.is_some();
    if saved_intent.is_none() {
        if let Some(item) = existing.as_ref() {
            let mut intent_payload = fields.clone();
            intent_payload["intent_storage"] = Value::String(storage.clone());
            intent_payload["intent_size"] = Value::from(size as u64);
            intent_payload["intent_mtime_ms"] = Value::from(mtime);
            intent_payload["intent_mtime_ticks"] = Value::from(meta.last_write_time());
            intent_payload["intent_usn"] = uploaded_usn.map(Value::from).unwrap_or(Value::Null);
            if is_local_id(&item.id) {
                intent_payload["intent_kind"] = Value::String("create".into());
                ctx.store.save_create_intent(
                    pid,
                    &item.id,
                    job.id,
                    &storage,
                    &intent_payload.to_string(),
                )?;
            } else {
                intent_payload["intent_kind"] = Value::String("version".into());
                ctx.store.save_version_intent(
                    pid,
                    &item.id,
                    job.id,
                    &intent_payload.to_string(),
                )?;
            }
            intent_persisted = true;
        }
    }

    let (row, kind) = match existing {
        Some(ex) if ex.remote_version.is_some() => {
            // The ACC name stays as it is (the local name may be sanitized, or a rename in ACC
            // may still wait for the file to close here).
            let acc_name =
                sync::pending_remote_name(ctx, pid, &ex.id).unwrap_or_else(|| ex.name.clone());
            let version =
                remote::create_version(ctx, p, &ex.id, &acc_name, &storage, &path, &|d, t| {
                    tracked.progress(d, t)
                })
                .await;
            let (ver, vurn) = match version {
                Ok(version) => version,
                Err(error) => {
                    let error: anyhow::Error = error.into();
                    if rejected_mime_type(&error) {
                        // This response proves only this exact object was rejected.
                        // A watcher may already have replaced the job's intent.
                        if let Err(clear_error) = ctx.store.clear_upload_intent(job.id, &storage) {
                            tracing::warn!(
                                "outbox: could not clear rejected upload intent {}: {clear_error:#}",
                                job.id
                            );
                        }
                    }
                    return Err(error);
                }
            };
            // ACC returns the version URN and we keep the OSS object we uploaded. The other
            // providers return the storage handle of the version they committed, and have no
            // version URN.
            let (storage_urn, version_urn) = if p.provider == Provider::Acc {
                (Some(storage), Some(vurn))
            } else {
                (Some(vurn), None)
            };
            let row = Item {
                remote_version: Some(ver),
                base_version: Some(ver),
                version_urn,
                storage_urn,
                size,
                mtime_ms: mtime,
                local_state: state,
                ..ex
            };
            (row, "new_version")
        }
        stale => {
            let created =
                remote::create_item(ctx, p, &loc.parent, &loc.name, &storage, &path, &|d, t| {
                    tracked.progress(d, t)
                })
                .await;
            let (created, kind) = match created {
                Ok(c) => (c, "upload"),
                // A persisted OSS object is an exact marker for our own create whose
                // response was lost. A different object remains a genuine collision.
                Err(e) if e.status() == Some(409) => {
                    let found = remote::folder_contents(ctx, p, &loc.parent)
                        .await?
                        .into_iter()
                        .find(|r| {
                            r.kind == Kind::File
                                && naming::same_name(&sanitize_name(&r.name), &loc.name)
                        })
                        .ok_or(e)?;
                    if found.storage_urn.as_deref() == Some(storage.as_str()) && intent_persisted {
                        (found, "upload")
                    } else {
                        preserve_new_item_collision(
                            ctx,
                            p,
                            &path,
                            &loc.parent,
                            stale.clone().filter(|r| is_local_id(&r.id)),
                        )
                        .await?;
                        return Ok(Step::Locked);
                    }
                }
                Err(e) => return Err(e.into()),
            };
            let snapshot = UploadedSnapshot {
                path: &path,
                meta: &meta,
                mtime,
                size,
                usn: uploaded_usn,
                intent: saved_intent.as_ref(),
            };
            // ACC binds the OSS object we uploaded. The other providers bind the handle their
            // commit returned: the marker only named the attempt.
            let committed = if p.provider == Provider::Acc {
                Some(storage)
            } else {
                created.storage_urn.clone()
            };
            let (row, changed) =
                bind_created_file(ctx, job, &loc, stale, created, committed, &snapshot)?;
            if changed {
                return Ok(Step::Locked);
            }
            (row, kind)
        }
    };
    // ACC accepted this version, but a save may have started during create_item/
    // create_version after the first check above. Keep this job alive and the row dirty;
    // otherwise mark_on_disk would stamp bytes we never uploaded as in sync, and its
    // watcher suppression could hide the final change.
    if saved_intent.as_ref().is_some_and(|i| !i.matches(&meta))
        || !unchanged_since(&path, &meta).unwrap_or(false)
        || !original_usn_matches(&path, uploaded_usn)
    {
        let mut row = row;
        row.local_state = LocalState::Dirty;
        ctx.store.upsert_item(&row)?;
        tracing::info!(
            "{} ändrades medan ACC bekräftade versionen, laddar upp igen",
            loc.name
        );
        return retry_changed_upload(ctx, job, &row.id, &path);
    }
    ctx.store.upsert_item(&row)?;
    if !unchanged_since(&path, &meta).unwrap_or(false) || !original_usn_matches(&path, uploaded_usn)
    {
        tracing::info!(
            "{} ändrades innan platshållaren uppdaterades, laddar upp igen",
            loc.name
        );
        return retry_changed_upload(ctx, job, &row.id, &path);
    }
    if let Err(error) = mark_on_disk(&path, Kind::File, pid, &row.id, size as u64, uploaded_usn) {
        if needs_new_upload_after_mark_error(&path, &meta, uploaded_usn, &error) {
            tracing::info!(
                "{} ändrades innan synkstatus kunde sättas, laddar upp igen",
                loc.name
            );
            return retry_changed_upload(ctx, job, &row.id, &path);
        }
        tracing::warn!(
            "placeholder for {}: {error:#}; retrying local finalization",
            path.display()
        );
        ctx.store.set_local_state(pid, &row.id, LocalState::Dirty)?;
        ctx.store.retarget_job(
            job.id,
            "finalize",
            &serde_json::json!({
                "path": path,
                "size": size as u64,
                "mtime_ticks": meta.last_write_time(),
                "version": row.remote_version,
                "usn": error
                    .downcast_ref::<placeholder::FinalizationRetryUsn>()
                    .map(|retry| retry.usn)
                    .or(uploaded_usn),
                "storage": row.storage_urn,
            })
            .to_string(),
        )?;
        return Ok(Step::Locked);
    }
    // A save can start after the final pre-mark check closes its read handle, but
    // before Cloud Files marks the bytes as synced. Keep the job and force a new
    // upload; otherwise a same-size edit could be promoted to validated cloud
    // data even though ACC only has the previous bytes.
    if changed_after_mark(ctx, pid, &row.id, &path, &meta)? {
        tracing::info!(
            "{} ändrades när platshållaren markerades synkad, laddar upp igen",
            loc.name
        );
        return retry_changed_upload(ctx, job, &row.id, &path);
    }
    ctx.ui.activity(kind, &loc.name, &p.name, "");
    drop(tracked);
    Ok(Step::Done)
}

fn changed_after_mark(
    ctx: &CtxRef,
    pid: Pid,
    id: &str,
    path: &Path,
    before: &Metadata,
) -> anyhow::Result<bool> {
    if unchanged_since(path, before).unwrap_or(false) {
        return Ok(false);
    }
    ctx.store.set_local_state(pid, id, LocalState::Dirty)?;
    if matches!(placeholder::placeholder_info(path), Ok(Some(_))) {
        let identity = placeholder::encode_identity(Kind::File, pid, id);
        if let Err(e) = placeholder::set_identity(path, &identity, false) {
            tracing::warn!(
                "could not clear sync state after a concurrent save of {}: {e:#}",
                path.display()
            );
        }
    }
    Ok(true)
}

async fn mkdir(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<Step> {
    let mut path = PathBuf::from(str_field(&payload(job)?, "path")?);
    if !path.is_dir() {
        match current_path(ctx, job).filter(|p| p.is_dir()) {
            Some(p) => path = p,
            None => {
                forget_local_row(ctx, job);
                return Ok(Step::Done);
            }
        }
    }
    let Some(loc) = locate(ctx, &path)? else {
        return Ok(Step::Done);
    };
    let p = &loc.project;
    // A row registered by the engine for this new dir carries a temporary "local-" id: it
    // does NOT exist in ACC yet. Only a real remote folder means "already there".
    let local_row = loc
        .existing
        .as_ref()
        .filter(|e| e.kind == Kind::Folder && is_local_id(&e.id))
        .map(|e| e.id.clone());
    if let Some(ex) = loc
        .existing
        .as_ref()
        .filter(|e| e.kind == Kind::Folder && local_row.is_none())
    {
        if let Err(error) = mark_on_disk(&path, Kind::Folder, p.pid, &ex.id, 0, None) {
            tracing::warn!("placeholder for {}: {error:#}", path.display());
        }
        return Ok(Step::Done);
    }
    // The parent may itself be a just-created local folder whose job has not run yet.
    if is_local_id(&loc.parent) {
        return Err(anyhow!("parent folder is not created in ACC yet"));
    }
    let created = match remote::create_folder(ctx, p, &loc.parent, &loc.name).await {
        Ok(c) => c,
        // Created meanwhile (by someone else, or by an attempt whose answer was lost): use it.
        Err(e) if e.status() == Some(409) => remote::folder_contents(ctx, p, &loc.parent)
            .await?
            .into_iter()
            .find(|r| {
                r.kind == Kind::Folder && naming::same_name(&sanitize_name(&r.name), &loc.name)
            })
            .ok_or(e)?,
        Err(e) => return Err(e.into()),
    };
    if let Some(old) = &local_row {
        ctx.store.rekey_item(p.pid, old, &created.id)?;
    }
    let now = now_ms();
    let row = Item {
        pid: p.pid,
        parent: Some(loc.parent.clone()),
        name: created.name.clone(),
        kind: Kind::Folder,
        remote_version: None,
        base_version: None,
        size: 0,
        ctime_ms: if created.ctime_ms > 0 {
            created.ctime_ms
        } else {
            now
        },
        mtime_ms: if created.mtime_ms > 0 {
            created.mtime_ms
        } else {
            now
        },
        storage_urn: None,
        version_urn: None,
        modified_by: created.modified_by,
        locked_by: None,
        local_state: LocalState::Clean,
        // The local directory already holds its children.
        populated: true,
        id: created.id,
    };
    ctx.store.upsert_item(&row)?;
    if let Err(error) = mark_on_disk(&path, Kind::Folder, p.pid, &row.id, 0, None) {
        tracing::warn!("placeholder for {}: {error:#}", path.display());
    }
    queue_unsynced_children(ctx, p.pid, &row.id, &path);
    Ok(Step::Done)
}

/// Files/dirs created inside a new folder before it existed remotely may have no row and no
/// job (their watcher events can arrive before the parent is known). Queue those that have
/// no row under the folder yet; rows registered by the engine were rekeyed and are skipped.
fn queue_unsynced_children(ctx: &CtxRef, pid: Pid, folder_id: &str, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let child = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = child.strip_prefix(&ctx.paths.sync_root).unwrap_or(&child);
        let attrs = std::os::windows::fs::MetadataExt::file_attributes(&match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        });
        if crate::sync::ignore::ignored(rel, attrs) {
            continue;
        }
        if matches!(placeholder::placeholder_info(&child), Ok(Some(_))) {
            continue;
        }
        let _guard = sync::local_lock();
        if sync::find_child_local(ctx, pid, folder_id, &name).is_some() {
            continue;
        }
        if let Err(e) = sync::register_local(ctx, pid, folder_id, &name, child.is_dir(), &child) {
            tracing::warn!("kunde inte köa {}: {e:#}", child.display());
        }
    }
}

/// Rename a remote file or folder. ACC renames a file through its tip version, so a file
/// without one cannot be renamed there yet. The other providers have no version URNs.
async fn rename_remote(ctx: &CtxRef, p: &Project, it: &Item, new_name: &str) -> anyhow::Result<()> {
    if it.kind.is_dir() {
        remote::patch_folder(ctx, p, &it.id, Some(new_name), None).await?;
    } else {
        if p.provider == Provider::Acc && it.version_urn.is_none() {
            return Err(anyhow!("file {} has no remote version yet", it.name));
        }
        remote::rename_file(ctx, p, &it.id, it.version_urn.as_deref(), new_name).await?;
    }
    Ok(())
}

/// The row a rename/move job applies, as it is now. None = nothing to do (gone, deleted
/// locally since, or never created in ACC).
fn job_row(ctx: &CtxRef, job: &OutboxJob, id: &str) -> anyhow::Result<Option<(Project, Item)>> {
    let (Some(p), Some(it)) = (ctx.store.project(job.pid)?, ctx.store.item(job.pid, id)?) else {
        return Ok(None);
    };
    if it.local_state == LocalState::Deleted || is_local_id(&it.id) {
        return Ok(None);
    }
    Ok(Some((p, it)))
}

/// Jobs send the row's current name and parent, not what was current when they were queued:
/// several renames collapse into the last one, and a parent created meanwhile is rekeyed.
async fn rename(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<Step> {
    let v = payload(job)?;
    let Some((p, it)) = job_row(ctx, job, str_field(&v, "id")?)? else {
        return Ok(Step::Done);
    };
    let name = it.name.clone();
    rename_remote(ctx, &p, &it, &name).await?;
    Ok(Step::Done)
}

async fn move_item(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<Step> {
    let v = payload(job)?;
    let Some((p, it)) = job_row(ctx, job, str_field(&v, "id")?)? else {
        return Ok(Step::Done);
    };
    let parent = it
        .parent
        .clone()
        .ok_or_else(|| perm("item has no parent"))?;
    if is_local_id(&parent) {
        return Err(anyhow!("target folder is not created in ACC yet"));
    }
    if it.kind.is_dir() {
        remote::patch_folder(ctx, &p, &it.id, Some(&it.name), Some(&parent)).await?;
    } else {
        remote::move_file(ctx, &p, &it.id, &parent).await?;
        rename_remote(ctx, &p, &it, &it.name).await?;
    }
    Ok(Step::Done)
}

async fn delete(ctx: &CtxRef, job: &OutboxJob) -> anyhow::Result<Step> {
    let v = payload(job)?;
    let id = str_field(&v, "id")?;
    let is_dir = v.get("is_dir").and_then(Value::as_bool).unwrap_or(false);
    let (Some(p), Some(row)) = (ctx.store.project(job.pid)?, ctx.store.item(job.pid, id)?) else {
        return Ok(Step::Done);
    };
    // Restored locally during the grace period (a safe save): nothing to delete.
    if row.local_state != LocalState::Deleted {
        return Ok(Step::Done);
    }
    // Back on disk although the watcher has not said so yet (a slow save, an undo).
    let (ctx2, p2, id2) = (ctx.clone(), p.clone(), id.to_string());
    if tokio::task::spawn_blocking(move || sync::back_before_delete(&ctx2, &p2, &id2))
        .await
        .unwrap_or(false)
    {
        return Ok(Step::Done);
    }
    // Someone saved a newer version in ACC after the user deleted the file here: the newer
    // work wins. The file comes back with that version instead of being deleted for everyone.
    let deleted_ver = v.get("ver").and_then(Value::as_i64);
    if !is_dir
        && deleted_ver.is_some()
        && row.remote_version.is_some()
        && row.remote_version != deleted_ver
    {
        let (ctx2, p2, id2) = (ctx.clone(), p.clone(), id.to_string());
        tokio::task::spawn_blocking(move || {
            let _guard = sync::local_lock();
            sync::restore_rows(&ctx2, &p2, &[id2])
        })
        .await
        .map_err(|e| anyhow!("restore task: {e}"))??;
        let by = row
            .modified_by
            .as_deref()
            .map(|b| format!(" av {b}"))
            .unwrap_or_default();
        tracing::info!("{}: ny version i ACC, tas inte bort", row.name);
        ctx.ui.send(UiMsg::notify(
            Severity::Warning,
            format!(
                "{} ändrades i ACC{by} efter att du tog bort den. Den nya versionen finns kvar.",
                row.name
            ),
        ));
        return Ok(Step::Done);
    }
    // The parent folder is being deleted too: that one call removes this item as well.
    let parent_deleted = row
        .parent
        .as_deref()
        .and_then(|pa| ctx.store.item(job.pid, pa).ok().flatten())
        .is_some_and(|pa| pa.local_state == LocalState::Deleted);
    if !parent_deleted {
        if is_dir {
            remote::delete_folder(ctx, &p, id).await?;
        } else {
            remote::delete_file(ctx, &p, id).await?;
        }
    }
    // Gone remotely and locally: forget the row (and, for folders, everything under it).
    // Otherwise a LocalState::Deleted row lingers, because the walk only prunes Clean rows.
    ctx.store.delete_item(job.pid, id)?;
    Ok(Step::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn same_size_save_after_snapshot_is_detected() {
        let path =
            std::env::temp_dir().join(format!("synka-upload-snapshot-{}.bin", std::process::id()));
        std::fs::write(&path, b"first").unwrap();
        let before = std::fs::metadata(&path).unwrap();
        assert!(unchanged_since(&path, &before).unwrap());
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(&path, b"other").unwrap();
        assert!(!unchanged_since(&path, &before).unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cfapi_metadata_failure_keeps_committed_version_for_finalization() {
        let path = std::env::temp_dir().join(format!(
            "synka-mark-classification-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let result = (|| -> anyhow::Result<()> {
            std::fs::write(&path, b"cloud bytes")?;
            let snapshot = std::fs::metadata(&path)?;
            let usn = placeholder::file_usn(&path)?.context("test file needs USN journal")?;
            assert!(!needs_new_upload_after_mark_error(
                &path,
                &snapshot,
                Some(usn),
                &anyhow!("CFAPI metadata update failed")
            ));
            assert!(
                !needs_new_upload_after_mark_error(
                    &path,
                    &snapshot,
                    Some(usn),
                    &placeholder::UsnMismatch.into()
                ),
                "a spurious mismatch on unchanged bytes stays in finalize"
            );
            std::fs::write(&path, b"other bytes")?;
            assert!(
                needs_new_upload_after_mark_error(
                    &path,
                    &snapshot,
                    Some(usn),
                    &placeholder::UsnMismatch.into()
                ),
                "an actual edit needs a fresh version"
            );
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result.unwrap();
    }

    #[test]
    fn save_between_snapshot_and_cloud_mark_keeps_upload_dirty() {
        let f = crate::sync::tests::fixture("save-during-mark");
        let path = f.dir.join("plan.bin");
        std::fs::write(&path, b"first").unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let row =
            crate::sync::tests::row(&f.p, "LIN-mark", Some("co.ROOT"), "plan.bin", Kind::File);
        f.ctx.store.upsert_item(&row).unwrap();
        let job = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&row.id),
                &serde_json::json!({ "path": path }).to_string(),
            )
            .unwrap();
        assert!(
            unchanged_since(&path, &before).unwrap(),
            "the pre-mark snapshot passes"
        );
        std::thread::sleep(Duration::from_millis(10));
        // Simulate an editor save in the gap before mark_on_disk returns.
        std::fs::write(&path, b"other").unwrap();
        assert!(changed_after_mark(&f.ctx, f.p.pid, &row.id, &path, &before).unwrap());
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, &row.id)
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Dirty
        );
        assert_eq!(
            f.ctx.store.due_jobs(i64::MAX, 10).unwrap()[0].id,
            job,
            "the upload stays queued"
        );
    }

    #[test]
    fn newer_acc_tip_after_a_long_transfer_requires_conflict_copy() {
        let f = crate::sync::tests::fixture("tip-after-transfer");
        let mut local =
            crate::sync::tests::row(&f.p, "LIN-tip", Some("co.ROOT"), "model.rvt", Kind::File);
        assert_eq!(local.base_version, Some(3));
        let mut tip = local.clone();
        tip.remote_version = Some(4);
        tip.version_urn = Some("version-four".into());
        tip.storage_urn = Some("storage-four".into());
        tip.size = 1_073_741_841;
        assert!(
            apply_remote_tip(&mut local, tip),
            "a version committed during upload must not be overwritten"
        );
        assert_eq!(
            local.base_version,
            Some(3),
            "the edited base version remains unchanged"
        );
        assert_eq!(local.remote_version, Some(4));
        assert_eq!(local.storage_urn.as_deref(), Some("storage-four"));
        let mut unchanged =
            crate::sync::tests::row(&f.p, "LIN-tip", Some("co.ROOT"), "model.rvt", Kind::File);
        let same_tip = unchanged.clone();
        assert!(
            !apply_remote_tip(&mut unchanged, same_tip),
            "an unchanged tip is safe to continue"
        );
    }

    #[test]
    fn acc_name_collision_keeps_local_bytes_as_a_separate_upload() {
        let f = crate::sync::tests::fixture("new-item-collision");
        let path = f.dir.join("plan.dwg");
        std::fs::write(&path, b"my drawing").unwrap();
        let mut local = crate::sync::tests::row(
            &f.p,
            "local-collision",
            Some("co.ROOT"),
            "plan.dwg",
            Kind::File,
        );
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        f.ctx.store.upsert_item(&local).unwrap();
        let old_job = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&local.id),
                &serde_json::json!({ "path": path }).to_string(),
            )
            .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(preserve_new_item_collision(
            &f.ctx,
            &f.p,
            &path,
            "co.ROOT",
            Some(local.clone()),
        ))
        .unwrap();

        assert!(!path.exists(), "the cloud name is left for the remote item");
        let renamed = f.ctx.store.item(f.p.pid, &local.id).unwrap().unwrap();
        assert_eq!(renamed.local_state, LocalState::New);
        assert!(renamed.name.starts_with("plan (konflikt "));
        assert_eq!(
            std::fs::read(f.dir.join(&renamed.name)).unwrap(),
            b"my drawing"
        );
        let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(
            jobs.len(),
            1,
            "the old upload is replaced by one for the conflict name"
        );
        assert_eq!(jobs[0].item.as_deref(), Some(local.id.as_str()));
        assert_eq!(jobs[0].kind, "upload");
        f.ctx
            .store
            .defer_job(old_job, now_ms() + LOCKED_RETRY_MS)
            .unwrap();
        assert_eq!(
            f.ctx.store.due_jobs(i64::MAX, 10).unwrap().len(),
            1,
            "defer must not lose a reused outbox ID"
        );
    }

    #[test]
    fn changed_file_supersedes_local_only_finalize_without_another_stale_commit() {
        let f = crate::sync::tests::fixture("finalize-changed");
        let path = f.dir.join("plan.bin");
        std::fs::write(&path, b"first").unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let mut row = crate::sync::tests::row(
            &f.p,
            "LIN-finalize",
            Some("co.ROOT"),
            "plan.bin",
            Kind::File,
        );
        row.local_state = LocalState::Dirty;
        row.size = before.len() as i64;
        f.ctx.store.upsert_item(&row).unwrap();
        let id = f
            .ctx
            .store
            .enqueue(f.p.pid, "upload", Some(&row.id), "{}")
            .unwrap();
        f.ctx
            .store
            .retarget_job(
                id,
                "finalize",
                &serde_json::json!({
                    "path": path,
                    "size": before.len(),
                    "mtime_ticks": before.last_write_time(),
                    "version": row.remote_version,
                })
                .to_string(),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(&path, b"other").unwrap();
        let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().pop().unwrap();
        assert_eq!(
            job.kind, "finalize",
            "the accepted version must not be uploaded twice"
        );
        assert!(matches!(finalize_upload(&f.ctx, &job), Ok(Step::Locked)));
        let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].kind, "upload",
            "the newly changed bytes still need a cloud version"
        );
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, &row.id)
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Dirty
        );
    }

    #[test]
    fn a_newer_version_in_acc_wins_over_a_local_delete() {
        let f = crate::sync::tests::fixture("newerwins");
        let row = crate::sync::tests::row(&f.p, "LIN1", Some("co.ROOT"), "plan.pdf", Kind::File);
        f.ctx.store.upsert_item(&row).unwrap();
        sync::queue_remote_delete(&f.ctx, &f.p, "LIN1", &mut sync::Work::default());
        // A colleague's new version arrives with the next poll (the row stays Deleted).
        let mut newer = row.clone();
        newer.remote_version = Some(4);
        newer.modified_by = Some("Anna".into());
        f.ctx
            .store
            .apply_delta(
                f.p.pid,
                &Delta {
                    upserts: vec![newer],
                    ..Default::default()
                },
            )
            .unwrap();
        let job = f
            .ctx
            .store
            .due_jobs(i64::MAX, 10)
            .unwrap()
            .into_iter()
            .find(|j| j.kind == "delete")
            .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            matches!(rt.block_on(delete(&f.ctx, &job)), Ok(Step::Done)),
            "no API call is made"
        );
        let it = f.ctx.store.item(f.p.pid, "LIN1").unwrap().unwrap();
        assert_eq!(
            (it.local_state, it.remote_version),
            (LocalState::Clean, Some(4))
        );
    }

    #[test]
    fn offline_errors_are_a_pause() {
        let auth = anyhow::Error::from(ApiError::Auth(anyhow!("refresh token expired")));
        assert!(is_offline(&auth));
        let st = anyhow::Error::from(ApiError::Status {
            status: 503,
            body: String::new(),
        });
        assert!(!is_offline(&st), "a server error counts");
        assert!(!is_offline(&anyhow!(
            "parent folder is not created in ACC yet"
        )));
    }

    #[test]
    fn retry_classification() {
        let st = |s: u16| {
            anyhow::Error::from(ApiError::Status {
                status: s,
                body: String::new(),
            })
        };
        assert!(is_transient(&st(429)));
        assert!(is_transient(&st(503)));
        assert!(!is_transient(&st(409)));
        assert!(!is_transient(&anyhow::Error::from(ApiError::Unsupported(
            "stöds inte".into()
        ))));
        assert!(!is_transient(&st(403)));
        assert!(!is_transient(&perm("bad")));
        assert!(!is_transient(
            &sync::IncompleteConflictCopy {
                path: "partial.rvt".into(),
                on_disk_size: 5,
                size: 10,
            }
            .into()
        ));
        assert!(is_transient(
            &sync::ConflictDetachFailed {
                path: "model.rvt".into(),
                rollback_failed: false,
                source: anyhow!("CFAPI sharing violation"),
            }
            .into()
        ));
        assert!(!is_transient(
            &sync::ConflictDetachFailed {
                path: "model.rvt".into(),
                rollback_failed: true,
                source: anyhow!("rollback failed"),
            }
            .into()
        ));
        assert!(is_transient(&anyhow!(
            "CfGetPlaceholderInfo sharing violation"
        )));
    }

    #[test]
    fn provider_refusals_park_the_job_without_retry() {
        // A provider without the operation: permanent, never a network pause, and the parked
        // text is the provider's own message, verbatim.
        let refused = anyhow::Error::from(ApiError::Unsupported("Interaxo stöds inte ännu".into()));
        assert!(unsupported_operation(&refused));
        assert!(!is_transient(&refused));
        assert!(!is_offline(&refused));
        assert_eq!(format!("{refused:#}"), "Interaxo stöds inte ännu");

        // The engine's own messages are permanent as well, but are not provider refusals.
        let message = anyhow::Error::from(ApiError::Message("ingen ACC-mapp".into()));
        assert!(!unsupported_operation(&message));
        assert!(!is_transient(&message));
        assert!(!is_offline(&message));
        assert_eq!(format!("{message:#}"), "ingen ACC-mapp");

        let forbidden = anyhow::Error::from(ApiError::Status {
            status: 403,
            body: String::new(),
        });
        assert!(!unsupported_operation(&forbidden));
        assert!(!unsupported_operation(&anyhow!(
            "parent folder is not created in ACC yet"
        )));
    }

    #[test]
    fn incomplete_conflict_fails_job_once_and_preserves_local_file() {
        let f = crate::sync::tests::fixture("incomplete-conflict-job");
        let path = f.dir.join("model.rvt");
        std::fs::write(&path, b"precious local bytes").unwrap();
        let mut item = crate::sync::tests::row(
            &f.p,
            "LIN-conflict",
            Some("co.ROOT"),
            "model.rvt",
            Kind::File,
        );
        item.local_state = LocalState::Dirty;
        item.remote_version = Some(4);
        item.base_version = Some(3);
        f.ctx.store.upsert_item(&item).unwrap();
        f.ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&item.id),
                &serde_json::json!({ "path": path }).to_string(),
            )
            .unwrap();
        let job = f.ctx.store.due_jobs(i64::MAX, 1).unwrap().remove(0);
        let err = sync::IncompleteConflictCopy {
            path: path.display().to_string(),
            on_disk_size: 5,
            size: 100,
        }
        .into();

        handle_failure(&f.ctx, &job, &err);

        assert_eq!(std::fs::read(&path).unwrap(), b"precious local bytes");
        assert_eq!(f.ctx.store.job_counts().unwrap(), (0, 1));
        assert!(f.ctx.store.due_jobs(i64::MAX, 1).unwrap().is_empty());
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, &item.id)
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Error
        );
    }

    #[test]
    fn only_accs_explicit_file_type_rejection_is_skipped() {
        let rejected = anyhow::Error::from(ApiError::Status {
            status: 403,
            body: r#"{"errors":[{"code":"ERR_NOT_ALLOWED","detail":"File with extension svg is not allowed to upload."}]}"#.into(),
        });
        assert!(unsupported_file_type(&rejected));
        let denied = anyhow::Error::from(ApiError::Status {
            status: 403,
            body: r#"{"errors":[{"code":"ERR_NOT_ALLOWED","detail":"Permission denied."}]}"#.into(),
        });
        assert!(!unsupported_file_type(&denied));
    }

    #[test]
    fn create_intent_survives_a_failed_job_and_keeps_its_exact_storage() {
        let f = crate::sync::tests::fixture("create-intent");
        let mut local =
            crate::sync::tests::row(&f.p, "local-one", Some("co.ROOT"), "rapid.bin", Kind::File);
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        f.ctx.store.upsert_item(&local).unwrap();
        let id = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&local.id),
                r#"{"path":"rapid.bin"}"#,
            )
            .unwrap();
        let persisted = serde_json::json!({
            "path": "rapid.bin",
            "intent_storage": "urn:adsk.objects:os.object:bucket/unique-upload",
            "intent_size": 42,
            "intent_mtime_ms": 123,
            "intent_mtime_ticks": 456,
            "intent_usn": 789,
        });
        f.ctx
            .store
            .save_create_intent(
                f.p.pid,
                &local.id,
                id,
                persisted["intent_storage"].as_str().unwrap(),
                &persisted.to_string(),
            )
            .unwrap();
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, &local.id)
                .unwrap()
                .unwrap()
                .storage_urn
                .as_deref(),
            persisted["intent_storage"].as_str()
        );
        f.ctx
            .store
            .job_failed(id, now_ms(), "temporary error", true)
            .unwrap();
        assert!(f
            .ctx
            .store
            .revive_create_intent(f.p.pid, &local.id)
            .unwrap());
        let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(jobs.len(), 1);
        let intent = CreateIntent::from_payload(&payload(&jobs[0]).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            intent.storage,
            persisted["intent_storage"].as_str().unwrap()
        );
        assert_eq!(intent.usn, Some(789));
    }

    #[test]
    fn lost_create_reply_then_edit_binds_remote_before_new_upload() {
        let f = crate::sync::tests::fixture("lost-create-edited");
        let path = f.dir.join("rapid.bin");
        std::fs::write(&path, b"old-version").unwrap();
        let old = std::fs::metadata(&path).unwrap();
        let storage = "urn:adsk.objects:os.object:bucket/exact-first-upload";
        let mut local = crate::sync::tests::row(
            &f.p,
            "local-rapid",
            Some("co.ROOT"),
            "rapid.bin",
            Kind::File,
        );
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        f.ctx.store.upsert_item(&local).unwrap();
        let job_id = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&local.id),
                &serde_json::json!({ "path": path }).to_string(),
            )
            .unwrap();
        let intent_payload = serde_json::json!({
            "path": path,
            "intent_storage": storage,
            "intent_size": old.len(),
            "intent_mtime_ms": mtime_ms(&old),
            "intent_mtime_ticks": old.last_write_time(),
            "intent_usn": available_usn(&path),
        });
        f.ctx
            .store
            .save_create_intent(
                f.p.pid,
                &local.id,
                job_id,
                storage,
                &intent_payload.to_string(),
            )
            .unwrap();
        std::fs::write(&path, b"edited-current-file").unwrap();
        let current = std::fs::metadata(&path).unwrap();
        let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().pop().unwrap();
        let intent = CreateIntent::from_payload(&payload(&job).unwrap())
            .unwrap()
            .unwrap();
        let mut created =
            crate::sync::tests::row(&f.p, "LIN-first", Some("co.ROOT"), "rapid.bin", Kind::File);
        created.remote_version = Some(1);
        created.storage_urn = Some(storage.into());
        let loc = Located {
            project: f.p.clone(),
            existing: Some(local.clone()),
            parent: "co.ROOT".into(),
            name: "rapid.bin".into(),
        };
        let snapshot = UploadedSnapshot {
            path: &path,
            meta: &current,
            mtime: intent.mtime_ms,
            size: intent.size as i64,
            usn: intent.usn,
            intent: Some(&intent),
        };
        let (bound, changed) = bind_created_file(
            &f.ctx,
            &job,
            &loc,
            Some(local.clone()),
            created,
            Some(storage.into()),
            &snapshot,
        )
        .unwrap();
        assert!(changed, "the newer local bytes require a second upload");
        assert_eq!(bound.id, "LIN-first");
        assert_eq!(bound.local_state, LocalState::Dirty);
        assert!(f.ctx.store.item(f.p.pid, &local.id).unwrap().is_none());
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, &bound.id)
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Dirty
        );
        let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "upload");
        assert_eq!(jobs[0].item.as_deref(), Some("LIN-first"));
        assert!(payload(&jobs[0]).unwrap().get("intent_storage").is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"edited-current-file");
        assert_eq!(
            std::fs::read_dir(&f.dir).unwrap().count(),
            1,
            "no conflict copy was created"
        );
    }

    #[test]
    fn delayed_create_reply_keeps_colleague_tip_and_local_bytes() {
        let f = crate::sync::tests::fixture("delayed-create-colleague");
        let path = f.dir.join("rapid.bin");
        std::fs::write(&path, b"our original bytes").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mut local = crate::sync::tests::row(
            &f.p,
            "local-rapid",
            Some("co.ROOT"),
            "rapid.bin",
            Kind::File,
        );
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        f.ctx.store.upsert_item(&local).unwrap();
        let job_id = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&local.id),
                &serde_json::json!({ "path": path }).to_string(),
            )
            .unwrap();
        let job = f
            .ctx
            .store
            .due_jobs(i64::MAX, 10)
            .unwrap()
            .into_iter()
            .find(|j| j.id == job_id)
            .unwrap();
        let mut created =
            crate::sync::tests::row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        created.remote_version = Some(1);
        created.storage_urn = Some("our-storage".into());
        let mut colleague = created.clone();
        colleague.remote_version = Some(2);
        colleague.base_version = Some(1);
        colleague.storage_urn = Some("colleague-storage".into());
        f.ctx.store.upsert_item(&colleague).unwrap();
        let loc = Located {
            project: f.p.clone(),
            existing: Some(local.clone()),
            parent: "co.ROOT".into(),
            name: "rapid.bin".into(),
        };
        let snapshot = UploadedSnapshot {
            path: &path,
            meta: &meta,
            mtime: mtime_ms(&meta),
            size: meta.len() as i64,
            usn: available_usn(&path),
            intent: None,
        };
        let (bound, changed) = bind_created_file(
            &f.ctx,
            &job,
            &loc,
            Some(local.clone()),
            created,
            Some("our-storage".into()),
            &snapshot,
        )
        .unwrap();
        assert!(changed, "a newer cloud tip needs conflict handling");
        assert_eq!(bound.remote_version, Some(2));
        assert_eq!(bound.base_version, Some(1));
        assert_eq!(bound.storage_urn.as_deref(), Some("colleague-storage"));
        assert_eq!(
            f.ctx.store.item(f.p.pid, &bound.id).unwrap().unwrap(),
            bound
        );
        assert!(f.ctx.store.item(f.p.pid, &local.id).unwrap().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"our original bytes");
        let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].item.as_deref(), Some("LIN-rapid"));
        assert!(payload(&jobs[0]).unwrap().get("intent_storage").is_none());
    }

    #[test]
    fn non_acc_create_binds_its_own_row_the_poll_already_inserted() {
        // Interaxo/StreamBIM: a poll listed our committed file under its ID before the create's
        // reply was bound. The committed handle identifies it; the attempt marker never does.
        let f = crate::sync::tests::fixture("non-acc-bind-own-poll");
        let path = f.dir.join("ritning.pdf");
        std::fs::write(&path, b"our bytes").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mut local = crate::sync::tests::row(
            &f.p,
            "local-ritning",
            Some("co.ROOT"),
            "ritning.pdf",
            Kind::File,
        );
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        f.ctx.store.upsert_item(&local).unwrap();
        let job_id = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&local.id),
                &serde_json::json!({ "path": path }).to_string(),
            )
            .unwrap();
        let job = f
            .ctx
            .store
            .due_jobs(i64::MAX, 10)
            .unwrap()
            .into_iter()
            .find(|j| j.id == job_id)
            .unwrap();
        let committed = "interaxo:I-900:1.0";
        let mut created =
            crate::sync::tests::row(&f.p, "I-900", Some("co.ROOT"), "ritning.pdf", Kind::File);
        created.remote_version = Some(1000);
        created.base_version = Some(1000);
        created.storage_urn = Some(committed.into());
        f.ctx.store.upsert_item(&created).unwrap();
        let loc = Located {
            project: f.p.clone(),
            existing: Some(local.clone()),
            parent: "co.ROOT".into(),
            name: "ritning.pdf".into(),
        };
        let snapshot = UploadedSnapshot {
            path: &path,
            meta: &meta,
            mtime: mtime_ms(&meta),
            size: meta.len() as i64,
            usn: available_usn(&path),
            intent: None,
        };
        let (bound, changed) = bind_created_file(
            &f.ctx,
            &job,
            &loc,
            Some(local.clone()),
            created,
            Some(committed.into()),
            &snapshot,
        )
        .unwrap();
        assert!(
            !changed,
            "our own commit is not a colleague's newer version"
        );
        assert_eq!(bound.id, "I-900");
        assert_eq!(bound.remote_version, Some(1000));
        assert_eq!(bound.base_version, Some(1000));
        assert_eq!(bound.storage_urn.as_deref(), Some(committed));
        assert_eq!(bound.version_urn, None);
        assert!(f.ctx.store.item(f.p.pid, &local.id).unwrap().is_none());
    }

    #[test]
    fn delayed_create_reply_does_not_steal_renamed_conflict_copy() {
        let f = crate::sync::tests::fixture("delayed-create-conflict-copy");
        let path = f.dir.join("rapid.bin");
        let copy = f.dir.join("rapid (konflikt).bin");
        std::fs::write(&copy, b"our bytes").unwrap();
        let meta = std::fs::metadata(&copy).unwrap();
        let mut local = crate::sync::tests::row(
            &f.p,
            "local-rapid",
            Some("co.ROOT"),
            "rapid (konflikt).bin",
            Kind::File,
        );
        local.remote_version = None;
        local.base_version = None;
        local.local_state = LocalState::New;
        f.ctx.store.upsert_item(&local).unwrap();
        let job_id = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&local.id),
                &serde_json::json!({ "path": copy }).to_string(),
            )
            .unwrap();
        let job = f
            .ctx
            .store
            .due_jobs(i64::MAX, 10)
            .unwrap()
            .into_iter()
            .find(|j| j.id == job_id)
            .unwrap();
        let mut created =
            crate::sync::tests::row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        created.remote_version = Some(1);
        created.storage_urn = Some("our-storage".into());
        let mut colleague = created.clone();
        colleague.remote_version = Some(2);
        colleague.storage_urn = Some("colleague-storage".into());
        f.ctx.store.upsert_item(&colleague).unwrap();
        let loc = Located {
            project: f.p.clone(),
            existing: Some(local.clone()),
            parent: "co.ROOT".into(),
            name: "rapid.bin".into(),
        };
        let snapshot = UploadedSnapshot {
            path: &path,
            meta: &meta,
            mtime: mtime_ms(&meta),
            size: meta.len() as i64,
            usn: available_usn(&copy),
            intent: None,
        };
        let (_, changed) = bind_created_file(
            &f.ctx,
            &job,
            &loc,
            Some(local.clone()),
            created,
            Some("our-storage".into()),
            &snapshot,
        )
        .unwrap();
        assert!(changed);
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, "LIN-rapid")
                .unwrap()
                .unwrap()
                .remote_version,
            Some(2)
        );
        assert_eq!(
            f.ctx.store.item(f.p.pid, &local.id).unwrap().unwrap().name,
            "rapid (konflikt).bin"
        );
        assert_eq!(std::fs::read(&copy).unwrap(), b"our bytes");
        assert_eq!(
            f.ctx.store.due_jobs(i64::MAX, 10).unwrap()[0]
                .item
                .as_deref(),
            Some(local.id.as_str())
        );
    }

    #[test]
    fn lost_version_reply_then_edit_adopts_exact_tip_before_next_upload() {
        let f = crate::sync::tests::fixture("version-intent-adopt");
        let path = f.dir.join("rapid.bin");
        std::fs::write(&path, b"newer-local-bytes").unwrap();
        let mut remote =
            crate::sync::tests::row(&f.p, "LIN-rapid", Some("co.ROOT"), "rapid.bin", Kind::File);
        remote.remote_version = Some(3);
        remote.base_version = Some(3);
        remote.storage_urn = Some("old-storage".into());
        f.ctx.store.upsert_item(&remote).unwrap();
        let intent_payload = serde_json::json!({
            "path": path,
            "intent_kind": "version",
            "intent_storage": "exact-committed-storage",
            "intent_size": 5,
            "intent_mtime_ms": 1,
            "intent_mtime_ticks": 2,
            "intent_usn": null,
        });
        let job_id = f
            .ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&remote.id),
                &intent_payload.to_string(),
            )
            .unwrap();
        let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().pop().unwrap();
        let intent = CreateIntent::from_payload(&payload(&job).unwrap())
            .unwrap()
            .unwrap();
        assert!(intent.version);
        let mut tip = remote.clone();
        tip.remote_version = Some(4);
        tip.storage_urn = Some("exact-committed-storage".into());
        tip.version_urn = Some("v4".into());
        assert!(adopt_version_tip(&mut remote, tip, &intent));
        f.ctx.store.upsert_item(&remote).unwrap();
        assert_eq!(
            (remote.base_version, remote.remote_version),
            (Some(4), Some(4))
        );
        assert!(matches!(
            retry_changed_upload(&f.ctx, &job, &remote.id, &path),
            Ok(Step::Locked)
        ));
        let stored = f.ctx.store.item(f.p.pid, &remote.id).unwrap().unwrap();
        assert_eq!(
            (
                stored.base_version,
                stored.remote_version,
                stored.local_state
            ),
            (Some(4), Some(4), LocalState::Dirty)
        );
        let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job_id);
        assert_eq!(jobs[0].item.as_deref(), Some(remote.id.as_str()));
        assert!(payload(&jobs[0]).unwrap().get("intent_storage").is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"newer-local-bytes");
        assert_eq!(
            std::fs::read_dir(&f.dir).unwrap().count(),
            1,
            "no conflict copy"
        );
    }
}

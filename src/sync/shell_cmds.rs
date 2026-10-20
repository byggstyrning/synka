//! Explorer context-menu verbs (`TrayCmd::Shell`, sent from `IExplorerCommand::Invoke` in
//! `crate::shell`). Each command runs in its own task so the run loop never waits on ACC.
//!
//! Lås refuses files the DB already shows as locked, so nobody's reservation is taken over by
//! accident; Lås upp always asks ACC (it decides whether an admin may release another user's
//! lock). After a change the row's `locked_by` is updated at once, Explorer is told to re-read
//! the item, and the project is polled.

use super::{hydrate_later, is_local_id, watcher, Engine};
use crate::app::{Severity, UiMsg};
use crate::cf::placeholder as ph;
use crate::http::ApiError;
use crate::model::{Item, Kind, Pid, Project};
use crate::remote;
use crate::shell::{self, ShellVerb};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use std::collections::HashSet;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

/// ACC requests in flight at once for one multi-selection.
const PARALLEL: usize = 4;
/// Keeps the watcher quiet about our own read-only change.
const SUPPRESS: Duration = Duration::from_secs(5);

type Target = (PathBuf, Option<(Project, Item)>);

impl Engine {
    pub(super) fn on_shell(self: &Arc<Self>, verb: ShellVerb, paths: Vec<PathBuf>) {
        let me = self.clone();
        self.spawn_maintenance(async move { me.shell_verb(verb, paths).await });
    }

    async fn shell_verb(self: Arc<Self>, verb: ShellVerb, paths: Vec<PathBuf>) {
        if matches!(verb, ShellVerb::Pin | ShellVerb::Unpin) {
            self.shell_pin(paths, verb == ShellVerb::Pin).await;
            return;
        }
        if !self.ctx.auth.is_signed_in() {
            self.shell_warn("Logga in först".into());
            return;
        }
        match verb {
            ShellVerb::SyncNow => self.shell_sync(paths).await,
            ShellVerb::Pin | ShellVerb::Unpin => unreachable!(),
            // Confirmed in Explorer, then sent as TrayCmd::Reset; kept for completeness.
            ShellVerb::Reset => {
                for path in paths {
                    self.clone().reset(None, Some(path)).await;
                }
            }
            ShellVerb::OpenInAcc => self.shell_open(paths).await,
            ShellVerb::CopyLink => self.shell_copy(paths).await,
            ShellVerb::Lock => self.shell_lock(paths, true).await,
            ShellVerb::Unlock => self.shell_lock(paths, false).await,
        }
    }

    /// Apply Explorer's offline availability command on a worker. Directory enumeration may
    /// populate remote placeholders, and hydration runs in the existing two-worker queue.
    async fn shell_pin(&self, paths: Vec<PathBuf>, keep: bool) {
        let root = self.ctx.paths.sync_root.clone();
        let result = blocking(move || pin_selection(&root, paths, keep)).await;
        if let Some(result) = result {
            hydrate_later(result.hydrate);
            if let Some(error) = result.first_error {
                let action = if keep {
                    "behålla lokalt"
                } else {
                    "sluta behålla lokalt"
                };
                self.shell_warn(format!(
                    "Kunde inte {action} {} objekt: {error}",
                    result.failed
                ));
            }
            if result.pending > 0 {
                self.shell_warn(format!(
                    "{} ändrade filer behålls lokalt tills de har synkats färdigt",
                    result.pending
                ));
            }
        }
    }

    async fn shell_sync(&self, paths: Vec<PathBuf>) {
        let ctx = self.ctx.clone();
        let pids = blocking(move || {
            paths
                .iter()
                .flat_map(|p| shell::projects_at(&ctx, p))
                .map(|p| p.pid)
                .collect::<HashSet<Pid>>()
        })
        .await
        .unwrap_or_default();
        self.poll_now(&pids);
    }

    async fn shell_open(&self, paths: Vec<PathBuf>) {
        let Some((path, found)) = self.shell_resolve(paths).await.into_iter().next() else {
            return;
        };
        let Some((p, it)) = found else {
            self.shell_warn(not_found(&path));
            return;
        };
        match self.web_link(&p, &it).await {
            Ok(url) => shell::open_url(&url),
            Err(e) => self.shell_warn(format!(
                "Kunde inte öppna {} i ACC: {}",
                it.name,
                reason(&e)
            )),
        }
    }

    async fn shell_copy(&self, paths: Vec<PathBuf>) {
        let targets = self.shell_resolve(paths).await;
        let results: Vec<Result<String, String>> = futures_util::stream::iter(targets)
            .map(|(path, found)| async move {
                let (p, it) = found.ok_or_else(|| not_found(&path))?;
                self.web_link(&p, &it).await.map_err(|e| {
                    format!(
                        "Kunde inte hämta ACC-länken för {}: {}",
                        it.name,
                        reason(&e)
                    )
                })
            })
            .buffered(PARALLEL)
            .collect()
            .await;
        let (links, errors): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
        let links: Vec<String> = links.into_iter().filter_map(Result::ok).collect();
        if !links.is_empty() {
            shell::copy_text(links.join("\r\n"));
        }
        if let Some(Err(first)) = errors.into_iter().next() {
            self.shell_warn(first);
        }
    }

    async fn shell_lock(&self, paths: Vec<PathBuf>, lock: bool) {
        let me = self
            .ctx
            .auth
            .user()
            .map(|u| u.name)
            .filter(|n| !n.trim().is_empty());
        let targets = self.shell_resolve(paths).await;
        let pids: HashSet<Pid> = futures_util::stream::iter(targets)
            .map(|t| self.lock_one(t, lock, me.as_deref()))
            .buffer_unordered(PARALLEL)
            .filter_map(|pid| async move { pid })
            .collect()
            .await;
        self.poll_now(&pids);
    }

    /// Returns the project to poll when ACC accepted the change.
    async fn lock_one(&self, (path, found): Target, lock: bool, me: Option<&str>) -> Option<Pid> {
        let Some((p, it)) = found else {
            self.shell_warn(not_found(&path));
            return None;
        };
        if it.kind != Kind::File {
            return None;
        }
        if is_local_id(&it.id) {
            self.shell_warn(format!("{} finns inte i ACC ännu", it.name));
            return None;
        }
        match (lock, it.locked_by.as_deref()) {
            (true, Some(who)) => {
                let who = if who.trim().is_empty() {
                    "någon annan"
                } else {
                    who
                };
                self.shell_warn(format!("{} är redan låst av {who}", it.name));
                return None;
            }
            (false, None) => {
                self.shell_warn(format!("{} är inte låst", it.name));
                return None;
            }
            _ => {}
        }
        match remote::set_reserved(&self.ctx, &p, &it.id, lock).await {
            Ok(by) => {
                let locked_by =
                    lock.then(|| by.or_else(|| me.map(str::to_string)).unwrap_or_default());
                self.after_lock(path, &p, it, locked_by).await;
                Some(p.pid)
            }
            Err(e) => {
                let what = if lock { "låsa" } else { "låsa upp" };
                self.shell_warn(format!(
                    "Kunde inte {what} {}: {}",
                    it.name,
                    reason(&e.into())
                ));
                None
            }
        }
    }

    async fn after_lock(&self, path: PathBuf, p: &Project, it: Item, locked_by: Option<String>) {
        let ctx = self.ctx.clone();
        let (pid, id) = (p.pid, it.id.clone());
        let locked = locked_by.is_some();
        let saved = blocking(move || -> Result<()> {
            ctx.store.set_locked_by(pid, &id, locked_by.as_deref())?;
            if !locked {
                // Files locked by others are read-only on disk; this one is free now.
                watcher::suppress(&path, SUPPRESS);
                let _ = ph::set_read_only(&path, false);
            }
            shell::refresh_item(&path);
            Ok(())
        })
        .await;
        if let Some(Err(e)) = saved {
            tracing::warn!("kunde inte spara låsstatus: {e:#}");
        }
        let kind = if locked { "lock" } else { "unlock" };
        self.ctx.ui.activity(kind, &it.name, &p.name, "");
    }

    async fn web_link(&self, p: &Project, it: &Item) -> Result<String> {
        if is_local_id(&it.id) {
            return Err(anyhow!("finns inte i ACC ännu"));
        }
        if it.kind == Kind::Hub {
            return Err(anyhow!("ingen ACC-mapp"));
        }
        Ok(remote::web_link(&self.ctx, p, it).await?)
    }

    async fn shell_resolve(&self, paths: Vec<PathBuf>) -> Vec<Target> {
        let ctx = self.ctx.clone();
        blocking(move || {
            paths
                .into_iter()
                .map(|p| {
                    let found = shell::resolve_path(&ctx, &p);
                    (p, found)
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// Poll these projects now (only projects the scheduler already runs).
    fn poll_now(&self, pids: &HashSet<Pid>) {
        if pids.is_empty() {
            return;
        }
        let now = Instant::now();
        {
            let mut st = self.st();
            for pid in pids {
                if let Some(due) = st.next_due.get_mut(pid) {
                    *due = now;
                }
            }
        }
        self.wake.notify_one();
    }

    fn shell_warn(&self, msg: String) {
        self.ctx.ui.send(UiMsg::notify(Severity::Warning, msg));
    }
}

#[derive(Default)]
struct PinResult {
    hydrate: Vec<PathBuf>,
    failed: usize,
    pending: usize,
    first_error: Option<String>,
}

impl PinResult {
    fn error(&mut self, path: &Path, error: impl std::fmt::Display) {
        self.failed += 1;
        self.first_error
            .get_or_insert_with(|| format!("{}: {error}", path.display()));
    }
}

/// Enumerate selected folders so the command covers existing children. Future children inherit
/// the selected folder's CFAPI pin state. Symlinks are skipped so a junction cannot carry the
/// command outside the sync root. Never open file data while walking.
fn pin_selection(sync_root: &Path, paths: Vec<PathBuf>, keep: bool) -> PinResult {
    let mut result = PinResult::default();
    let mut selected = HashSet::new();
    for path in paths {
        if !selected.insert(path.clone()) {
            continue;
        }
        if shell::components_below(sync_root, &path).is_none_or(|parts| parts.len() < 2) {
            result.error(&path, "utanför en synkad projektmapp");
            continue;
        }
        if !matches!(ph::placeholder_info(&path), Ok(Some(_))) {
            result.error(&path, "vänta tills objektet har synkats till ACC");
            continue;
        }
        if keep {
            // Establish the intent before a potentially slow on-demand directory listing, so
            // children arriving while we enumerate inherit the selected folder's pin.
            if let Err(error) = ph::pin(&path) {
                result.error(&path, error);
                continue;
            }
        }
        let mut tree = Vec::new();
        let mut stack = vec![path.clone()];
        while let Some(next) = stack.pop() {
            let metadata = match std::fs::symlink_metadata(&next) {
                Ok(metadata) => metadata,
                Err(error) => {
                    result.error(&next, error);
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                && !matches!(ph::placeholder_info(&next), Ok(Some(_)))
            {
                // A junction or other non-cloud reparse point may lead outside the sync root.
                continue;
            }
            if keep {
                match ph::placeholder_info(&next) {
                    Ok(Some(info)) => {
                        // The selected root was pinned before enumeration. Pin each existing
                        // child before listing it, so newly discovered descendants inherit it.
                        if next != path {
                            if let Err(error) = ph::pin(&next) {
                                result.error(&next, error);
                                continue;
                            }
                        }
                        if !metadata.is_dir() && !info.hydrated {
                            result.hydrate.push(next.clone());
                        }
                    }
                    Ok(None) => {} // local-only child: its complete data is already on disk
                    Err(error) => result.error(&next, error),
                }
            } else {
                tree.push((next.clone(), metadata.is_dir()));
            }
            if metadata.is_dir() {
                match std::fs::read_dir(&next) {
                    Ok(entries) => {
                        for entry in entries {
                            match entry {
                                Ok(entry) => stack.push(entry.path()),
                                Err(error) => result.error(&next, error),
                            }
                        }
                    }
                    Err(error) => result.error(&next, error),
                }
            }
        }
        // Unpin children first so any dirty child can be explicitly pinned before its parent's
        // inherited pin is removed.
        tree.reverse();
        for (next, is_dir) in tree {
            let info = match ph::placeholder_info(&next) {
                Ok(Some(info)) => info,
                Ok(None) => continue, // local-only child: its complete data is already on disk
                Err(error) => {
                    result.error(&next, error);
                    continue;
                }
            };
            if !is_dir && (!info.in_sync || info.modified_size > 0) {
                // Do not let auto-dehydration discard a file whose local edits are pending.
                match ph::pin(&next) {
                    Ok(()) => result.pending += 1,
                    Err(error) => result.error(&next, error),
                }
            } else if let Err(error) = ph::unpin(&next) {
                result.error(&next, error);
            }
        }
        shell::refresh_item(&path);
    }
    result
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

fn not_found(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("Hittar inte {name} i Synka")
}

/// Short Swedish reason for a failed ACC call.
fn reason(e: &anyhow::Error) -> String {
    match e.downcast_ref::<ApiError>() {
        Some(ApiError::Status {
            status: 401 | 403, ..
        }) => "behörighet saknas".into(),
        Some(ApiError::Status { status: 404, .. }) => "finns inte i ACC".into(),
        Some(ApiError::Status {
            status: 409 | 423, ..
        }) => "filen är låst av någon annan".into(),
        Some(ApiError::Status { status, .. }) => format!("ACC svarade HTTP {status}"),
        Some(ApiError::Transport(_)) => "ingen anslutning till ACC".into(),
        _ => format!("{e:#}").chars().take(120).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_short_and_swedish() {
        let status = |s: u16| {
            anyhow::Error::from(ApiError::Status {
                status: s,
                body: "{\"long\":\"json\"}".into(),
            })
        };
        assert_eq!(reason(&status(403)), "behörighet saknas");
        assert_eq!(reason(&status(409)), "filen är låst av någon annan");
        assert_eq!(reason(&status(500)), "ACC svarade HTTP 500");
        assert_eq!(
            reason(&anyhow!("finns inte i ACC ännu")),
            "finns inte i ACC ännu"
        );
        assert_eq!(reason(&anyhow!("x".repeat(500))).chars().count(), 120);
        assert_eq!(
            not_found(Path::new(r"C:\a\Plan.rvt")),
            "Hittar inte Plan.rvt i Synka"
        );
    }
}

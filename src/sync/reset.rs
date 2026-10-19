//! Starting over for a project or one folder, without losing work and without a separate
//! repair tool.
//!
//! 1. The target directory is renamed aside inside its parent (".synka-reset-<stamp>-<name>",
//!    hidden). That is one atomic step: if a file inside is open, the rename fails and nothing
//!    has changed yet (the user is told which files to close).
//! 2. The DB forgets the target's contents (and, for a project, its cursor), and a fresh,
//!    unpopulated placeholder takes the original path. The next listing/poll rebuilds it
//!    from ACC; files download again when opened. A pinned target stays pinned.
//! 3. In the background, everything in the set-aside copy that was not in sync with ACC (local
//!    edits, new files) is copied to "%USERPROFILE%\Synka - återställda filer\<name> <stamp>\",
//!    keeping relative paths; then the copy is deleted. A leftover from an interrupted run is
//!    finished the same way at the next start.
//!
//! While this runs, cfapi delete/rename notifications for these paths are ours and change
//! nothing in ACC, and listings requested under the set-aside copy are answered empty.

use super::*;
use std::os::windows::ffi::OsStrExt;

pub(super) const ASIDE_PREFIX: &str = ".synka-reset-";
const RESCUE_DIR: &str = "Synka - återställda filer";

/// Paths under reset: (path, answer listings under it with nothing).
static RESETTING: Mutex<Vec<(PathBuf, bool)>> = Mutex::new(Vec::new());
/// Projects being reset: their polls and jobs wait.
static RESETTING_PIDS: Mutex<Vec<Pid>> = Mutex::new(Vec::new());

fn under(path: &Path, base: &Path) -> bool {
    rel_components(base, path).is_some()
}

/// cfapi notifications for this path come from a reset in progress.
pub(super) fn under_reset(path: &Path) -> bool {
    RESETTING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(p, _)| under(path, p))
}

/// Listings under a set-aside copy are answered empty (its rows are gone).
pub(super) fn listing_suppressed(path: &Path) -> bool {
    RESETTING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(p, empty)| *empty && under(path, p))
}

pub(crate) fn project_resetting(pid: Pid) -> bool {
    RESETTING_PIDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&pid)
}

/// Registers paths (and projects) as under reset until dropped.
#[derive(Debug)]
pub(super) struct Guard {
    paths: Vec<PathBuf>,
    pids: Vec<Pid>,
}

impl Guard {
    pub(super) fn new(paths: Vec<(PathBuf, bool)>, pids: Vec<Pid>) -> Guard {
        RESETTING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(paths.iter().cloned());
        RESETTING_PIDS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(pids.iter().copied());
        Guard {
            paths: paths.into_iter().map(|(p, _)| p).collect(),
            pids,
        }
    }

    /// Also hold `path` (answering listings under it with nothing when `empty`).
    pub(super) fn add(&mut self, path: PathBuf, empty: bool) {
        RESETTING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((path.clone(), empty));
        self.paths.push(path);
    }

    /// Keep only the guard for `keep` (the set-aside copy) and release the project.
    fn narrow(&mut self, keep: &Path) {
        let mut v = RESETTING.lock().unwrap_or_else(|e| e.into_inner());
        for p in self.paths.iter().filter(|p| p.as_path() != keep) {
            if let Some(i) = v.iter().position(|(q, _)| q == p) {
                v.remove(i);
            }
        }
        self.paths.retain(|p| p.as_path() == keep);
        drop(v);
        self.release_pid();
    }

    fn release_pid(&mut self) {
        let mut v = RESETTING_PIDS.lock().unwrap_or_else(|e| e.into_inner());
        for pid in self.pids.drain(..) {
            if let Some(i) = v.iter().position(|q| *q == pid) {
                v.remove(i);
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut v = RESETTING.lock().unwrap_or_else(|e| e.into_inner());
        for p in &self.paths {
            if let Some(i) = v.iter().position(|(q, _)| q == p) {
                v.remove(i);
            }
        }
        drop(v);
        self.release_pid();
    }
}

/// What the first, quick phase leaves for the background phase.
pub(super) struct SetAside {
    pub name: String,
    pub aside: Option<PathBuf>,
    pub guard: Guard,
}

/// Keep a partially moved project's callbacks/jobs paused until the engine exits.
/// The engine retains this error; dropping it releases only the in-memory guard.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(super) struct IncompleteReset {
    message: String,
    _guard: Guard,
}

pub(super) fn incomplete(message: String, guard: Guard) -> anyhow::Error {
    IncompleteReset {
        message,
        _guard: guard,
    }
    .into()
}

/// Phase 1 (caller must NOT hold `local_lock()`; this takes it): swap the target for a fresh
/// placeholder and forget its contents. `folder` None = the whole project.
pub(super) fn begin(ctx: &Ctx, p: &Project, folder: Option<&Path>) -> Result<SetAside> {
    let _lock = local_lock();
    let root_dir = project_dir(&ctx.paths.sync_root, p);
    let (dir, row) = target(ctx, p, &root_dir, folder)?;
    let name = row
        .as_ref()
        .map_or_else(|| p.name.clone(), |r| r.name.clone());
    let pinned = ph::placeholder_info(&dir)
        .ok()
        .flatten()
        .is_some_and(|i| i.pin_state == PinState::Pinned);
    let original_attributes = attributes_of(&dir);
    let mut guard = Guard::new(vec![(dir.clone(), false)], vec![p.pid]);
    let aside = if exists(&dir) {
        match move_aside(&dir, &name, &mut guard) {
            Ok(aside) => {
                let _ = set_hidden(&aside);
                Some(aside)
            }
            Err(error) => {
                let open = open_files(ctx, p, &dir, row.as_ref().map(|r| r.id.as_str()), 5);
                return Err(rename_failure(&dir, &error, &open));
            }
        }
    } else {
        None
    };

    let reset_result = match &row {
        None => ctx.store.reset_project(p.pid),
        Some(r) => ctx.store.reset_folder(p.pid, &r.id),
    };
    if let Err(error) = reset_result {
        if let Some(aside) = &aside {
            // Store's reset uses one transaction; an error leaves the old rows/jobs.
            // Roll back only with a no-replace rename: a newly appeared source wins.
            match rename_with_retries(aside, &dir) {
                Ok(()) => {
                    let _ = set_hidden_state(&dir, original_attributes & 2 != 0);
                    return Err(anyhow!("Återställningen avbröts och projektmappen återställdes. Dina lokala filer finns kvar. Databasen kunde inte återställas: {error:#}"));
                }
                Err(rollback) => {
                    return Err(incomplete(format!("Återställningen avbröts. Dina lokala filer finns kvar i den undanflyttade kopian \"{}\". Starta om Synka för att fortsätta återställningen. Mappen kunde inte flyttas tillbaka: {rollback}. Databasfel: {error:#}", aside.display()), guard));
                }
            }
        }
        return Err(error);
    }
    let rebuilt = (|| -> Result<()> {
        match &row {
            None => ensure_project_dirs(ctx, p),
            Some(r) => {
                let fresh = ctx
                    .store
                    .item(p.pid, &r.id)?
                    .ok_or_else(|| anyhow!("mappen försvann"))?;
                if let Some(parent) = dir.parent() {
                    ph::create_placeholders(parent, &[spec_for(&fresh, me(ctx).as_deref())])?;
                }
                Ok(())
            }
        }
    })();
    if let Err(error) = rebuilt {
        if let Some(aside) = &aside {
            return Err(incomplete(format!("Projektet kunde inte byggas om. Dina lokala filer finns kvar i den undanflyttade kopian \"{}\". Starta om Synka för att fortsätta återställningen. Detaljer: {error:#}", aside.display()), guard));
        }
        return Err(error);
    }
    if pinned {
        if let Err(e) = ph::pin(&dir) {
            warn!(
                "kunde inte behålla 'Behåll alltid' för {}: {e:#}",
                dir.display()
            );
        }
    }
    if let Some(aside) = &aside {
        guard.narrow(aside);
    }
    Ok(SetAside { name, aside, guard })
}

/// Rename only. Never replace an existing entry and never copy/delete on failure.
pub(super) fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
    let wide = |path: &Path| -> std::io::Result<Vec<u16>> {
        // std::fs::rename expands absolute paths for Win32. Preserve that support
        // while using explicit no-replace flags, including paths beyond MAX_PATH.
        let path = std::path::absolute(path)?;
        let input: Vec<u16> = path.as_os_str().encode_wide().collect();
        let mut output = if input.starts_with(&[92, 92, 63, 92]) {
            input
        } else if input.starts_with(&[92, 92]) {
            let mut output: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
            output.extend_from_slice(&input[2..]);
            output
        } else {
            let mut output: Vec<u16> = r"\\?\".encode_utf16().collect();
            output.extend(input);
            output
        };
        output.push(0);
        Ok(output)
    };
    let from = wide(from)?;
    let to = wide(to)?;
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    }
    .map_err(|error| std::io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32))
}

/// Windows may briefly retain a metadata/indexing handle even with all apps closed.
/// Retry only sharing/access refusals, never an existing destination or another error.
/// Every attempt remains an atomic no-replace rename on the same volume.
pub(super) fn rename_with_retries(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut delays = [100, 300, 600].into_iter();
    loop {
        match rename_no_replace(from, to) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && std::fs::symlink_metadata(to).is_err() =>
            {
                let Some(delay) = delays.next() else {
                    return Err(error);
                };
                std::thread::sleep(Duration::from_millis(delay));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn move_aside(dir: &Path, name: &str, guard: &mut Guard) -> std::io::Result<PathBuf> {
    let base = format!(
        "{ASIDE_PREFIX}{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        sanitize_name(name)
    );
    move_aside_named(dir, &base, guard)
}

fn move_aside_named(dir: &Path, base: &str, guard: &mut Guard) -> std::io::Result<PathBuf> {
    for attempt in 0..1000 {
        let name = if attempt == 0 {
            base.to_string()
        } else {
            format!("{base} ({attempt})")
        };
        let aside = dir.with_file_name(name);
        if std::fs::symlink_metadata(&aside).is_ok() {
            continue;
        }
        guard.add(aside.clone(), true);
        match rename_with_retries(dir, &aside) {
            Ok(()) => return Ok(aside),
            // A concurrent creator may win after the existence check. Preserve both.
            Err(_) if std::fs::symlink_metadata(&aside).is_ok() => continue,
            Err(error) => {
                warn!(
                    error_code = error.raw_os_error(),
                    source_attributes = attributes_of(dir),
                    destination_present = std::fs::symlink_metadata(&aside).is_ok(),
                    delete_probe_error = rename_probe(dir).err().and_then(|e| e.raw_os_error()),
                    "atomic reset rename refused; source retained"
                );
                return Err(error);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free reset copy name",
    ))
}

pub(super) fn rename_failure(dir: &Path, error: &std::io::Error, open: &[String]) -> anyhow::Error {
    let cause = if !open.is_empty() {
        format!("Windows håller de här filerna eller mapparna öppna: {}. Stäng program och Utforskaren-fönster som använder dem och försök igen.", open.join(", "))
    } else if matches!(error.raw_os_error(), Some(5 | 32 | 33)) {
        "Windows nekade åtkomst till mappen. Starta om Synka och försök igen. Om felet kvarstår, starta om Windows och exportera diagnostik.".into()
    } else {
        "Mappen kunde inte flyttas undan. Försök igen eller exportera diagnostik.".into()
    };
    anyhow!(
        "{cause} Dina lokala filer finns kvar. Sökväg: \"{}\". Windows-fel: {error}",
        dir.display()
    )
}

/// The directory to reset and its folder row (None for the project root).
fn target(
    ctx: &Ctx,
    p: &Project,
    root_dir: &Path,
    folder: Option<&Path>,
) -> Result<(PathBuf, Option<Item>)> {
    let Some(path) = folder else {
        return Ok((root_dir.to_path_buf(), None));
    };
    let comps = rel_components(&ctx.paths.sync_root, path)
        .filter(|c| c.len() >= 2 && project_matches(p, &c[0], &c[1]))
        .ok_or_else(|| anyhow!("{} ligger inte i projektet {}", path.display(), p.name))?;
    if comps.len() == 2 {
        return Ok((root_dir.to_path_buf(), None));
    }
    let mut parent = p.root_id().to_string();
    let mut row = None;
    for c in &comps[2..] {
        let it = find_child_local(ctx, p.pid, &parent, c)
            .filter(|it| it.kind.is_dir())
            .ok_or_else(|| anyhow!("okänd mapp: {c}"))?;
        parent = it.id.clone();
        row = Some(it);
    }
    if row.as_ref().is_some_and(|r| is_local_id(&r.id)) {
        return Err(anyhow!("mappen finns inte i ACC än"));
    }
    Ok((path.to_path_buf(), row))
}

/// Up to `max` materialized files or directories that block rename sharing.
/// Probes metadata only; unpopulated cloud directories are never enumerated.
pub(super) fn open_files(
    ctx: &Ctx,
    p: &Project,
    dir: &Path,
    folder_id: Option<&str>,
    max: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    if max == 0 {
        return out;
    }
    let mut stack = vec![(
        dir.to_path_buf(),
        folder_id.unwrap_or(p.root_id()).to_string(),
    )];
    // Probe materialized known folders even if an ancestor's listing is not populated.
    // No enumeration of an unpopulated cloud directory is needed for this diagnostic.
    for item in ctx.store.populated_folders(p.pid).unwrap_or_default() {
        if let Ok(Some(parts)) = ctx.store.path_of(p.pid, &item.id) {
            let path = parts
                .iter()
                .fold(project_dir(&ctx.paths.sync_root, p), |path, part| {
                    path.join(sanitize_name(part))
                });
            if under(&path, dir) && path != dir {
                stack.push((path, item.id));
            }
        }
    }
    let mut visited = HashSet::new();
    while let Some((d, id)) = stack.pop() {
        if !visited.insert(d.clone()) {
            continue;
        }
        if is_open_elsewhere(&d) {
            out.push(format!(
                "mappen {}",
                d.file_name().unwrap_or_default().to_string_lossy()
            ));
            if out.len() >= max {
                return out;
            }
        }
        if !ctx
            .store
            .item(p.pid, &id)
            .ok()
            .flatten()
            .is_some_and(|i| i.populated)
        {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if let Some(k) = find_child_local(ctx, p.pid, &id, &name) {
                    stack.push((path, k.id));
                } else if is_open_elsewhere(&path) {
                    out.push(format!("mappen {name}"));
                    if out.len() >= max {
                        return out;
                    }
                }
                continue;
            }
            if is_open_elsewhere(&path) {
                out.push(name);
                if out.len() >= max {
                    return out;
                }
            }
        }
    }
    out
}

/// Open metadata only, including the reparse point itself: never hydrate file data.
fn rename_probe(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    std::fs::OpenOptions::new()
        .access_mode(DELETE.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
        .map(|_| ())
}

fn is_open_elsewhere(path: &Path) -> bool {
    rename_probe(path).is_err_and(|error| matches!(error.raw_os_error(), Some(32 | 33)))
}

pub(super) fn set_hidden(path: &Path) -> std::io::Result<()> {
    set_hidden_state(path, true)
}

pub(super) fn set_hidden_state(path: &Path, hidden: bool) -> std::io::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_FLAGS_AND_ATTRIBUTES,
    };
    let w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let a = GetFileAttributesW(PCWSTR(w.as_ptr()));
        SetFileAttributesW(
            PCWSTR(w.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(if hidden {
                a | FILE_ATTRIBUTE_HIDDEN.0
            } else {
                a & !FILE_ATTRIBUTE_HIDDEN.0
            }),
        )
        .map_err(std::io::Error::other)
    }
}

/// Phase 2 (background, no lock): save what was not in sync, then delete the set-aside copy.
/// Returns (files saved, where).
pub(super) fn finish(ctx: &Ctx, aside: &Path, name: &str) -> (usize, Option<PathBuf>) {
    let dest = match rescue_destination(ctx, name) {
        Ok(dest) => dest,
        Err(e) => {
            warn!(
                "kunde inte skapa räddningsmapp för {}: {e}; behåller {}",
                name,
                aside.display()
            );
            ctx.ui.send(UiMsg::notify(Severity::Warning, format!(
                "Kunde inte spara lokala filer från {name}. Den undanflyttade kopian finns kvar i {}.",
                aside.display()
            )));
            return (0, Some(aside.to_path_buf()));
        }
    };
    let mut saved = 0usize;
    let mut list: Vec<String> = Vec::new();
    let mut stack = vec![aside.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(e) => {
                warn!(
                    "kunde inte lista {}: {e}; behåller {}",
                    d.display(),
                    aside.display()
                );
                write_readme(&dest, &rebuilt_intro(name), &list);
                return (saved, Some(dest));
            }
        };
        for entry in rd {
            let e = match entry {
                Ok(e) => e,
                Err(err) => {
                    warn!(
                        "kunde inte läsa post i {}: {err}; behåller {}",
                        d.display(),
                        aside.display()
                    );
                    write_readme(&dest, &rebuilt_intro(name), &list);
                    return (saved, Some(dest));
                }
            };
            let path = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(aside) else {
                continue;
            };
            // Ignored files are never uploaded to ACC. They can still be important
            // local recovery data (notably Revit *_backup directories and .sv$ files).
            // A placeholder with modified ranges is also unsynced even if its in-sync
            // flag has not yet been cleared.
            let unsynced = match ph::placeholder_info(&path) {
                Ok(Some(i)) => {
                    ignore::ignored(rel, attributes_of(&path)) || !placeholder_is_clean(&i)
                }
                Ok(None) => true,
                Err(_) => true,
            };
            if !unsynced {
                continue;
            }
            let to = dest.join(rel);
            let r = to
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::copy(&path, &to).map(|_| ()));
            match r {
                Ok(()) => {
                    saved += 1;
                    list.push(rel.display().to_string());
                }
                Err(e) => {
                    // Keep the set-aside copy: it is the only place this file still exists.
                    warn!(
                        "kunde inte spara {}: {e}; behåller {}",
                        path.display(),
                        aside.display()
                    );
                    write_readme(&dest, &rebuilt_intro(name), &list);
                    return (saved, Some(dest));
                }
            }
        }
    }
    if saved > 0 {
        write_readme(&dest, &rebuilt_intro(name), &list);
    } else {
        let _ = std::fs::remove_dir(&dest);
    }
    for attempt in 0..3 {
        match std::fs::remove_dir_all(aside) {
            Ok(()) => break,
            Err(e) if attempt == 2 => warn!(
                "kunde inte ta bort {}: {e} (försöker vid nästa start)",
                aside.display()
            ),
            Err(_) => std::thread::sleep(Duration::from_secs(5)),
        }
    }
    (saved, (saved > 0).then_some(dest))
}

/// Atomically reserve a fresh destination so two resets in one minute cannot
/// overwrite each other's rescued files.
pub(super) fn rescue_destination(ctx: &Ctx, name: &str) -> std::io::Result<PathBuf> {
    let root = rescue_root(ctx);
    std::fs::create_dir_all(&root)?;
    let base = format!(
        "{} {}",
        sanitize_name(name),
        chrono::Local::now().format("%Y-%m-%d %H%M%S")
    );
    for n in 0..1000 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!(" ({n})")
        };
        let path = root.join(format!("{base}{suffix}"));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free rescue folder name",
    ))
}

/// Where saved copies go: "Synka - återställda filer" next to the sync root.
pub(super) fn rescue_root(ctx: &Ctx) -> PathBuf {
    ctx.paths
        .sync_root
        .parent()
        .unwrap_or(&ctx.paths.sync_root)
        .join(RESCUE_DIR)
}

fn rebuilt_intro(name: &str) -> String {
    format!(
        "{name} byggdes om från ACC {}.\r\n\r\nDe här filerna hade ändringar som inte hunnit synkas till ACC, så de sparades här \
         i stället för att försvinna. Kopiera tillbaka det du vill behålla till synkmappen, så laddas det upp. \
         Revit-molnmodeller ska synkas via Revit; kopiera inte tillbaka dem för uppladdning med Synka.",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    )
}

/// "LÄS MIG.txt" in a rescue folder: what these files are (`intro`) and the list of them.
pub(super) fn write_readme(dest: &Path, intro: &str, files: &[String]) {
    if files.is_empty() {
        return;
    }
    let mut text = format!("{intro}\r\n\r\n");
    for f in files {
        text.push_str(f);
        text.push_str("\r\n");
    }
    use std::io::Write;
    for n in 0..1000 {
        let filename = if n == 0 {
            "LÄS MIG.txt".to_string()
        } else {
            format!("LÄS MIG ({n}).txt")
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest.join(&filename))
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(text.as_bytes()) {
                    warn!("kunde inte skriva {filename} i {}: {e}", dest.display());
                }
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                warn!("kunde inte skapa {filename} i {}: {e}", dest.display());
                return;
            }
        }
    }
    warn!(
        "kunde inte hitta ett ledigt filnamn för LÄS MIG.txt i {}",
        dest.display()
    );
}

/// Set-aside copies left by an interrupted reset (populated folders and hub level only).
pub(super) fn leftovers(ctx: &Ctx) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&ctx.paths.sync_root)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    dirs.extend(scan_populated(ctx).into_iter().filter(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(ASIDE_PREFIX))
    }));
    for d in dirs {
        let candidates: Vec<PathBuf> = if d
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(ASIDE_PREFIX))
        {
            vec![d]
        } else {
            // Hub level: project set-asides live next to the project folders.
            std::fs::read_dir(&d)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .is_some_and(|n| n.to_string_lossy().starts_with(ASIDE_PREFIX))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        for c in candidates {
            let n = c
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // ".synka-reset-YYYYMMDD-HHMMSS-<name>"
            let name = n
                .strip_prefix(ASIDE_PREFIX)
                .and_then(|r| r.splitn(3, '-').nth(2))
                .unwrap_or("återställning")
                .to_string();
            out.push((c, name));
        }
    }
    out
}

/// Guard for finishing a leftover found at startup.
pub(super) fn guard_aside(aside: &Path) -> Guard {
    Guard::new(vec![(aside.to_path_buf(), true)], Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_rescues_ignored_revit_backups_without_overwriting_existing_files() {
        let f = crate::sync::tests::fixture("rescue-revit-backup");
        let aside = f
            .pdir
            .with_file_name(".synka-reset-20261017-101010-Köpmannen");
        std::fs::create_dir_all(aside.join("model_backup")).unwrap();
        std::fs::write(aside.join("model_backup").join("0001.rvt"), b"revit backup").unwrap();
        std::fs::write(aside.join("model.sv$"), b"recovery bytes").unwrap();
        std::fs::write(aside.join("LÄS MIG.txt"), b"user notes").unwrap();

        let (saved, dest) = finish(&f.ctx, &aside, "Köpmannen");
        let dest = dest.unwrap();
        assert_eq!(saved, 3);
        assert_eq!(
            std::fs::read(dest.join("model_backup").join("0001.rvt")).unwrap(),
            b"revit backup"
        );
        assert_eq!(
            std::fs::read(dest.join("model.sv$")).unwrap(),
            b"recovery bytes"
        );
        assert_eq!(
            std::fs::read(dest.join("LÄS MIG.txt")).unwrap(),
            b"user notes"
        );
        assert!(std::fs::read_to_string(dest.join("LÄS MIG (1).txt"))
            .unwrap()
            .contains("model_backup"));
        assert!(!aside.exists());

        let next_aside = f
            .pdir
            .with_file_name(".synka-reset-20261017-101011-Köpmannen");
        std::fs::create_dir_all(&next_aside).unwrap();
        std::fs::write(next_aside.join("model.sv$"), b"new recovery bytes").unwrap();
        let (next_saved, next_dest) = finish(&f.ctx, &next_aside, "Köpmannen");
        let next_dest = next_dest.unwrap();
        assert_eq!(next_saved, 1);
        assert_ne!(
            dest, next_dest,
            "repeated resets need separate rescue folders"
        );
        assert_eq!(
            std::fs::read(dest.join("model.sv$")).unwrap(),
            b"recovery bytes"
        );
        assert_eq!(
            std::fs::read(next_dest.join("model.sv$")).unwrap(),
            b"new recovery bytes"
        );
    }
}

#[cfg(test)]
#[path = "reset_recovery_tests.rs"]
mod recovery_tests;

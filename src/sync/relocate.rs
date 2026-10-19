//! Moving the sync root (Settings → Synkmapp → Flytta…). A move starts over completely: every project is rebuilt from ACC at the new place, and
//! nothing that was not in sync is lost.
//!
//! 1. `check`: the new place is a new or empty folder on a local NTFS drive, not inside or
//!    around the current sync root, not inside another provider's sync root (OneDrive's
//!    Documents, Dropbox...: Windows refuses nested sync roots), not a system folder.
//! 2. `set_aside` (engine stopped polling and jobs): every hub folder is renamed aside in one
//!    step each, as a project reset does; a file open in another program aborts the move before
//!    anything changed. The DB forgets every project's rows, jobs and cursors, settings.json
//!    names the new root and kv `moved_from` the old one.
//! 3. The engine rescues the set-aside copies (`reset::finish`: unsynced files to "Synka -
//!    återställda filer") while the old root is still connected, then restarts itself. The new
//!    process registers the sync root at the new place (which replaces the old registration),
//!    builds the projects there and removes the old, now empty folder (`forget_old_root`).
//!
//! Rebuilding everything in place (`reset_all`) is the same without the move: each project is
//! reset where it is.

use super::*;
use std::os::windows::ffi::OsStrExt;

const MOVED_FROM: &str = "moved_from";

/// Why `to` cannot be the sync root, in words for the user. `to` is the full new path.
pub(crate) fn check(current: &Path, to: &Path) -> std::result::Result<(), String> {
    check_paths(current, to)?;
    check_volume(to)?;
    let others = cf::other_sync_roots().unwrap_or_default();
    check_others(to, &others)
}

/// The checks that need no Windows API (also run by the tests).
fn check_paths(current: &Path, to: &Path) -> std::result::Result<(), String> {
    if !to.is_absolute() || to.parent().is_none() || to.file_name().is_none() {
        return Err(format!(
            "{} är ingen giltig plats för synkmappen",
            to.display()
        ));
    }
    if naming::same_path(current, to) {
        return Err("Synkmappen ligger redan där".into());
    }
    if rel_components(current, to).is_some() {
        return Err("Den nya platsen ligger i den nuvarande synkmappen".into());
    }
    if rel_components(to, current).is_some() {
        return Err("Den nuvarande synkmappen ligger i den nya platsen".into());
    }
    for var in [
        "WINDIR",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
        "LOCALAPPDATA",
        "APPDATA",
    ] {
        if let Some(sys) = std::env::var_os(var).map(PathBuf::from) {
            if rel_components(&sys, to).is_some() {
                return Err(format!(
                    "{} är en systemmapp. Välj en annan plats.",
                    sys.display()
                ));
            }
        }
    }
    let parent = to.parent().unwrap_or(to);
    if !parent.is_dir() {
        return Err(format!("Mappen {} finns inte", parent.display()));
    }
    match std::fs::symlink_metadata(to) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => match std::fs::read_dir(to).map(|mut rd| rd.next().is_none()) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "{} finns redan och är inte tom. Välj en ny eller tom mapp.",
                to.display()
            )),
            Err(e) => Err(format!("{} går inte att läsa: {e}", to.display())),
        },
        Ok(_) => Err(format!("{} finns redan och är en fil", to.display())),
    }
}

fn check_others(to: &Path, others: &[(String, String)]) -> std::result::Result<(), String> {
    for (path, name) in others {
        let root = Path::new(path);
        if rel_components(root, to).is_some() || rel_components(to, root).is_some() {
            let who = if name.is_empty() || name.starts_with('@') {
                "en annan molntjänst".to_string()
            } else {
                name.clone()
            };
            return Err(format!(
                "{} synkas redan av {who} (t.ex. OneDrive). Välj en annan plats.",
                root.display()
            ));
        }
    }
    Ok(())
}

/// Local fixed NTFS drive: the Cloud Files API works nowhere else.
fn check_volume(to: &Path) -> std::result::Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
    };
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let existing = to.ancestors().find(|a| a.is_dir()).unwrap_or(to);
    let mut vol = [0u16; 261];
    unsafe { GetVolumePathNameW(PCWSTR(wide(existing).as_ptr()), &mut vol) }
        .map_err(|e| format!("Kan inte läsa disken för {}: {e}", to.display()))?;
    const DRIVE_FIXED: u32 = 3;
    if unsafe { GetDriveTypeW(PCWSTR(vol.as_ptr())) } != DRIVE_FIXED {
        return Err(
            "Synkmappen måste ligga på en lokal disk, inte en nätverksenhet eller ett USB-minne"
                .into(),
        );
    }
    let mut fs = [0u16; 32];
    unsafe { GetVolumeInformationW(PCWSTR(vol.as_ptr()), None, None, None, None, Some(&mut fs)) }
        .map_err(|e| format!("Kan inte läsa disken för {}: {e}", to.display()))?;
    let fs = String::from_utf16_lossy(&fs[..fs.iter().position(|c| *c == 0).unwrap_or(fs.len())]);
    if !fs.eq_ignore_ascii_case("NTFS") {
        return Err(format!(
            "Synkmappen måste ligga på en NTFS-disk (den här är {fs})"
        ));
    }
    Ok(())
}

/// What `set_aside` leaves for the engine: the set-aside hub copies and the guard over them.
pub(super) struct Moved {
    pub asides: Vec<(PathBuf, String)>,
    pub guard: reset::Guard,
}

/// Step 2 (polls and jobs stopped, caller must NOT hold `local_lock()`): set every hub folder
/// aside, forget every project, and point settings.json at `to`. A move refusal restores
/// earlier moved hubs where possible; later failures explicitly retain the recovery copies.
pub(super) fn set_aside(ctx: &Ctx, to: &Path) -> Result<Moved> {
    let _lock = local_lock();
    let root = ctx.paths.sync_root.clone();
    let projects: Vec<Project> = ctx.store.projects()?;
    let pids: Vec<Pid> = projects.iter().map(|p| p.pid).collect();
    // Files open in other programs first: one look, before anything moves.
    let open: Vec<String> = projects
        .iter()
        .filter(|p| p.enabled)
        .flat_map(|p| reset::open_files(ctx, p, &project_dir(&root, p), None, 5))
        .take(5)
        .collect();
    if !open.is_empty() {
        return Err(anyhow!("Stäng de här filerna först: {}", open.join(", ")));
    }
    let mut guard = reset::Guard::new(Vec::new(), pids);
    let mut asides: Vec<(PathBuf, PathBuf, String, u32)> = Vec::new();
    let mut hubs: Vec<String> = projects
        .iter()
        .map(|p| sanitize_name(&p.hub_name))
        .collect();
    hubs.sort();
    hubs.dedup_by(|a, b| naming::same_name(a, b));
    for hub in hubs {
        let dir = root.join(&hub);
        if !exists(&dir) {
            continue;
        }
        let original_attributes = attributes_of(&dir);
        guard.add(dir.clone(), false);
        let aside = match reset::move_aside(&dir, &hub, &mut guard) {
            Ok(aside) => aside,
            Err(error) => {
                let mut retained = Vec::new();
                for (original, aside, _, attributes) in asides.iter().rev() {
                    match reset::rename_with_retries(aside, original) {
                        Ok(()) => {
                            let _ = reset::set_hidden_state(original, attributes & 2 != 0);
                        }
                        Err(rollback) => {
                            warn!("kunde inte flytta tillbaka {}: {rollback}", aside.display());
                            retained.push(aside.display().to_string());
                        }
                    }
                }
                let open: Vec<String> = projects
                    .iter()
                    .filter(|p| naming::same_name(&sanitize_name(&p.hub_name), &hub))
                    .flat_map(|p| reset::open_files(ctx, p, &project_dir(&root, p), None, 5))
                    .take(5)
                    .collect();
                let failure = reset::rename_failure(&dir, &error, &open);
                if !retained.is_empty() {
                    return Err(reset::incomplete(format!("Flytten avbröts. Dina lokala filer finns kvar i undanflyttade kopior. Starta om Synka för att fortsätta återställningen. Kopior: {:?}. {failure:#}", retained), guard));
                }
                return Err(failure);
            }
        };
        let _ = reset::set_hidden(&aside);
        asides.push((dir, aside, hub, original_attributes));
    }
    let committed = (|| -> Result<()> {
        for p in &projects {
            ctx.store.reset_project(p.pid)?;
        }
        ctx.store
            .kv_set(MOVED_FROM, Some(&root.to_string_lossy()))?;
        let mut settings = ctx
            .settings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut cfg = settings.clone();
        cfg.sync_root = (!is_default_root(to)).then(|| to.to_string_lossy().into_owned());
        cfg.save(&ctx.paths.state_dir.join("settings.json"))?;
        *settings = cfg;
        Ok(())
    })();
    if let Err(error) = committed {
        let retained = asides
            .iter()
            .map(|(_, aside, _, _)| aside.display().to_string())
            .collect::<Vec<_>>();
        return Err(reset::incomplete(format!("Flytten kunde inte slutföras. Dina lokala filer finns kvar i undanflyttade kopior. Starta om Synka för att fortsätta återställningen. Kopior: {:?}. Detaljer: {error:#}", retained), guard));
    }
    Ok(Moved {
        asides: asides.into_iter().map(|(_, a, hub, _)| (a, hub)).collect(),
        guard,
    })
}

fn is_default_root(to: &Path) -> bool {
    std::env::var_os("USERPROFILE")
        .is_some_and(|home| naming::same_path(&PathBuf::from(home).join("Synka"), to))
}

/// In the new process, once the sync root is registered at its new place: remove the old root
/// if nothing is left in it. Returns the old root when something was left behind.
pub(super) fn forget_old_root(ctx: &Ctx) -> Option<PathBuf> {
    let old = PathBuf::from(ctx.store.kv_get(MOVED_FROM).ok()??);
    let _ = ctx.store.kv_set(MOVED_FROM, None);
    if naming::same_path(&old, &ctx.paths.sync_root) || !exists(&old) {
        return None;
    }
    // Explorer may have left its folder settings behind; that alone is not the user's.
    let entries: Vec<String> = std::fs::read_dir(&old)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if entries.len() == 1 && entries[0].eq_ignore_ascii_case("desktop.ini") {
        let _ = std::fs::remove_file(old.join(&entries[0]));
    }
    match std::fs::remove_dir(&old) {
        Ok(()) => {
            info!("gamla synkmappen {} borttagen", old.display());
            None
        }
        Err(e) => {
            warn!("gamla synkmappen {} är kvar: {e}", old.display());
            Some(old)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("synka-relocate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_a_new_or_empty_folder_outside_the_root_will_do() {
        let base = temp("paths");
        let current = base.join("Synka");
        std::fs::create_dir_all(&current).unwrap();
        assert!(check_paths(&current, &base.join("D").join("Synka"))
            .unwrap_err()
            .contains("finns inte"));
        std::fs::create_dir_all(base.join("D")).unwrap();
        assert_eq!(
            check_paths(&current, &base.join("D").join("Synka")),
            Ok(()),
            "a new folder"
        );
        std::fs::create_dir_all(base.join("D").join("Tom")).unwrap();
        assert_eq!(
            check_paths(&current, &base.join("D").join("Tom")),
            Ok(()),
            "an empty folder"
        );
        std::fs::write(base.join("D").join("Tom").join("x.txt"), b"x").unwrap();
        assert!(check_paths(&current, &base.join("D").join("Tom"))
            .unwrap_err()
            .contains("inte tom"));
        assert!(
            check_paths(&current, &base.join("synka"))
                .unwrap_err()
                .contains("redan"),
            "same folder, other case"
        );
        assert!(check_paths(&current, &current.join("Ny"))
            .unwrap_err()
            .contains("i den nuvarande"));
        assert!(check_paths(&current.join("Inne"), &base)
            .unwrap_err()
            .contains("ligger i den nya"));
        assert!(check_paths(&current, Path::new("relativ")).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn other_providers_folders_are_refused() {
        let others = vec![(
            r"C:\Users\u\OneDrive - Byggstyrning".to_string(),
            "OneDrive - Byggstyrning".to_string(),
        )];
        let inside = Path::new(r"C:\Users\u\OneDrive - Byggstyrning\Dokument\Synka");
        assert!(check_others(inside, &others)
            .unwrap_err()
            .contains("OneDrive - Byggstyrning"));
        assert!(
            check_others(Path::new(r"C:\Users\u"), &others).is_err(),
            "around one"
        );
        assert_eq!(
            check_others(Path::new(r"D:\Projekt\Synka"), &others),
            Ok(())
        );
    }
}

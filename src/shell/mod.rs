//! Explorer COM command/state implementations served by the engine process. They are packaged
//! as an out-of-proc COM server (`com:ExeServer`, started with `-Embedding` when no engine runs
//! yet). The current Cloud Files manifest deliberately leaves these handlers unattached:
//! Windows then recognizes the sync root and provides native status and offline pin commands.
//!
//! The class objects live on the main STA thread, so Explorer's calls are dispatched through
//! the tray message loop: they answer from the path and SQLite only and never wait on ACC.
//! Verbs copy the selection and hand it to the engine as `TrayCmd::Shell`
//! (`sync::shell_cmds`).
//!
//! The CLSIDs are in ui/Package.appxmanifest. Never change them after a release; package
//! registration keys the COM classes on them.

pub mod com;
pub mod flyout;
mod states;
mod verbs;

use crate::app::{CtxRef, TrayCmd};
use crate::cf::placeholder as ph;
use crate::model::{Item, Kind, LocalState, Project};
use crate::store::Store;
use std::ffi::c_void;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;
use windows::core::{GUID, HSTRING};

pub const CLSID_CUSTOM_STATES: GUID = GUID::from_u128(0x2f892c05_5a3b_490a_8bd2_10c97fe767c2);
pub const CLSID_OPEN_IN_ACC: GUID = GUID::from_u128(0x24c0680f_0816_4889_900c_43a8430704fe);
pub const CLSID_COPY_LINK: GUID = GUID::from_u128(0x2e404b33_2db0_4f72_a038_c543dfa8705d);
pub const CLSID_LOCK: GUID = GUID::from_u128(0xb813aff2_7aad_4d6c_bc11_e7bad042a146);
pub const CLSID_UNLOCK: GUID = GUID::from_u128(0x6d483c5f_7570_4011_a737_ae4966d95986);
pub const CLSID_SYNC_NOW: GUID = GUID::from_u128(0x57a11603_cfd1_410a_822a_adc029f82687);
pub const CLSID_RESET: GUID = GUID::from_u128(0xdad03286_009f_4f91_8289_e6fe77d813c7);
pub const CLSID_PIN: GUID = GUID::from_u128(0xc087c0b2_bd8d_468e_bf4f_85320ad4e06d);
pub const CLSID_UNPIN: GUID = GUID::from_u128(0x932783f8_3108_499b_a4b2_0d410d82c349);
/// Windows 11 cloud status flyout (`cloudFiles2:StorageProviderStatusUISourceFactory`).
pub const CLSID_STATUS_UI: GUID = GUID::from_u128(0x35b42230_10d2_4774_bc7b_d75c38279590);

/// The app icon inside the package (the MSIX root is one level above `engine\`).
pub const APP_ICON_URI: &str = "ms-appx:///Assets/Square44x44Logo.png";

/// Custom state ids. Stable forever: Explorer stores state per id.
pub const PROP_LOCKED_BY: i32 = 2;
pub const PROP_VERSION: i32 = 4;
pub const PROP_MODIFIED_BY: i32 = 5;

/// Registered on the sync root (`cf::register`); the handler answers with these ids.
pub const PROPERTY_DEFINITIONS: [(i32, &str); 3] = [
    (PROP_LOCKED_BY, "Låst av"),
    (PROP_VERSION, "Version"),
    (PROP_MODIFIED_BY, "Ändrad av"),
];

/// A context-menu verb. One COM class each, so Explorer can place and cache them separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellVerb {
    OpenInAcc,
    CopyLink,
    Lock,
    Unlock,
    SyncNow,
    /// Keep files available offline; for folders this includes their descendants.
    Pin,
    /// Stop keeping files offline and allow Windows to free local space.
    Unpin,
    /// Rebuild a project or folder from ACC (unsynced changes are saved first).
    Reset,
}

impl ShellVerb {
    pub const ALL: [ShellVerb; 8] = [
        ShellVerb::OpenInAcc,
        ShellVerb::CopyLink,
        ShellVerb::Lock,
        ShellVerb::Unlock,
        ShellVerb::SyncNow,
        ShellVerb::Pin,
        ShellVerb::Unpin,
        ShellVerb::Reset,
    ];

    pub fn clsid(self) -> GUID {
        match self {
            ShellVerb::OpenInAcc => CLSID_OPEN_IN_ACC,
            ShellVerb::CopyLink => CLSID_COPY_LINK,
            ShellVerb::Lock => CLSID_LOCK,
            ShellVerb::Unlock => CLSID_UNLOCK,
            ShellVerb::SyncNow => CLSID_SYNC_NOW,
            ShellVerb::Pin => CLSID_PIN,
            ShellVerb::Unpin => CLSID_UNPIN,
            ShellVerb::Reset => CLSID_RESET,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            ShellVerb::OpenInAcc => "Visa i ACC",
            ShellVerb::CopyLink => "Kopiera ACC-länk",
            ShellVerb::Lock => "Lås",
            ShellVerb::Unlock => "Lås upp",
            ShellVerb::SyncNow => "Synka nu",
            ShellVerb::Pin => "Synka alltid – behåll lokalt",
            ShellVerb::Unpin => "Sluta behålla lokalt",
            ShellVerb::Reset => "Bygg om från ACC…",
        }
    }
}

static CONTEXT: OnceLock<CtxRef> = OnceLock::new();
static COMMANDS: OnceLock<UnboundedSender<TrayCmd>> = OnceLock::new();

/// Engine context for the COM objects. Set once, right after `Ctx` is created.
pub fn set_context(ctx: CtxRef) {
    let _ = CONTEXT.set(ctx);
}

/// Route from `Invoke` to the engine. Set once, before the class objects are registered.
pub fn set_command_sender(tx: UnboundedSender<TrayCmd>) {
    let _ = COMMANDS.set(tx);
}

fn context() -> Option<&'static CtxRef> {
    CONTEXT.get()
}

fn send(cmd: TrayCmd) -> bool {
    COMMANDS.get().is_some_and(|tx| tx.send(cmd).is_ok())
}

/// Started by COM (`-Embedding`, the `com:ExeServer` Arguments) rather than by the user.
pub fn is_embedding() -> bool {
    std::env::args().skip(1).any(|a| is_embedding_flag(&a))
}

fn is_embedding_flag(arg: &str) -> bool {
    arg.eq_ignore_ascii_case("-embedding") || arg.eq_ignore_ascii_case("/embedding")
}

// ---------------------------------------------------------------------- path -> item

/// Components of `path` below `root` (the engine's own rule, `sync::rel_components`). Empty
/// for the root itself, None outside it or for an empty root.
pub fn components_below(root: &Path, path: &Path) -> Option<Vec<String>> {
    if root.as_os_str().is_empty() {
        return None;
    }
    crate::sync::rel_components(root, path)
}

/// A DB name shown on disk as `local` (the engine's naming rules).
fn same_name(name: &str, local: &str) -> bool {
    crate::sync::naming::matches_local(name, local)
}

/// The project and item (project root, folder or file) a path under the sync root refers to.
/// Hub dirs and the root itself resolve to None. Reads the placeholder identity (metadata
/// only, never hydrates) and falls back to matching names level by level.
pub fn resolve_path(ctx: &CtxRef, path: &Path) -> Option<(Project, Item)> {
    resolve_in(&ctx.store, &ctx.paths.sync_root, path)
}

fn resolve_in(store: &Store, root: &Path, path: &Path) -> Option<(Project, Item)> {
    let comps = components_below(root, path)?;
    if comps.len() < 2 {
        return None;
    }
    by_identity(store, path, &comps).or_else(|| by_name(store, &comps))
}

fn project_dir_matches(p: &Project, comps: &[String]) -> bool {
    p.enabled
        && comps.first().is_some_and(|h| same_name(&p.hub_name, h))
        && comps.get(1).is_some_and(|n| same_name(&p.name, n))
}

fn by_identity(store: &Store, path: &Path, comps: &[String]) -> Option<(Project, Item)> {
    let info = ph::placeholder_info(path).ok()??;
    let (kind, pid, id) = ph::decode_identity(&info.identity)?;
    if kind == Kind::Hub {
        return None;
    }
    let p = store.project(pid).ok()??;
    let it = store.item(pid, &id).ok()??;
    // A stale identity (renamed elsewhere, rekeyed) must not win over the name walk.
    let leaf_ok = match comps.len() {
        2 => it.kind == Kind::ProjectRoot,
        _ => comps.last().is_some_and(|leaf| same_name(&it.name, leaf)),
    };
    (project_dir_matches(&p, comps) && leaf_ok && it.local_state != LocalState::Deleted)
        .then_some((p, it))
}

fn by_name(store: &Store, comps: &[String]) -> Option<(Project, Item)> {
    let p = store
        .projects()
        .ok()?
        .into_iter()
        .find(|p| project_dir_matches(p, comps))?;
    let mut cur = store.item(p.pid, p.root_id()).ok()??;
    for c in comps.iter().skip(2) {
        if !cur.kind.is_dir() {
            return None;
        }
        cur = store
            .children_named(p.pid, &cur.id, c)
            .ok()?
            .into_iter()
            .find(|it| it.local_state != LocalState::Deleted)?;
    }
    Some((p, cur))
}

/// Enabled projects at or below `path`: all for the sync root, the hub's for a hub dir, the
/// one project for anything inside a project.
pub fn projects_at(ctx: &CtxRef, path: &Path) -> Vec<Project> {
    let Some(comps) = components_below(&ctx.paths.sync_root, path) else {
        return Vec::new();
    };
    projects_matching(ctx.store.projects().unwrap_or_default(), &comps)
}

fn projects_matching(projects: Vec<Project>, comps: &[String]) -> Vec<Project> {
    projects
        .into_iter()
        .filter(|p| {
            p.enabled
                && comps.first().is_none_or(|h| same_name(&p.hub_name, h))
                && comps.get(1).is_none_or(|n| same_name(&p.name, n))
        })
        .collect()
}

// ---------------------------------------------------------------------- shell helpers

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(once(0)).collect()
}

/// Ask Explorer to re-read one item (its custom states and columns). Non-blocking.
pub fn refresh_item(path: &Path) {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_UPDATEITEM, SHCNF_PATHW};
    let w = wide(path.as_os_str());
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEITEM,
            SHCNF_PATHW,
            Some(w.as_ptr() as *const c_void),
            None,
        )
    };
}

/// Open an https URL in the default browser, from a short-lived STA thread (ShellExecute may
/// use COM). Anything else is refused: the URL comes from the network.
pub fn open_url(url: &str) {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    if !url.starts_with("https://") {
        tracing::warn!("öppnar inte länk som inte är https");
        return;
    }
    let url = HSTRING::from(url);
    std::thread::spawn(move || unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok();
        let r = ShellExecuteW(
            None,
            w!("open"),
            &url,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if (r.0 as isize) <= 32 {
            tracing::warn!("ShellExecuteW: {}", r.0 as isize);
        }
        if com {
            CoUninitialize();
        }
    });
}

/// Put text on the clipboard (CF_UNICODETEXT) from a short-lived thread with its own
/// message-only owner window.
pub fn copy_text(text: String) {
    std::thread::spawn(move || {
        if let Err(e) = set_clipboard(&text) {
            tracing::warn!("urklipp: {e}");
        }
    });
}

fn set_clipboard(text: &str) -> windows::core::Result<()> {
    use windows::core::w;
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };

    let data: Vec<u16> = text.encode_utf16().chain(once(0)).collect();
    unsafe {
        // EmptyClipboard with a NULL owner leaves the clipboard ownerless; give it a window.
        let owner = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
        .ok();
        let mut opened = OpenClipboard(owner);
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            // Another program holds the clipboard; it lets go within milliseconds.
            std::thread::sleep(std::time::Duration::from_millis(25));
            opened = OpenClipboard(owner);
        }
        let result = opened.and_then(|()| {
            let r = (|| {
                EmptyClipboard()?;
                let mem = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2)?;
                let dst = GlobalLock(mem) as *mut u16;
                if dst.is_null() {
                    let _ = GlobalFree(Some(mem));
                    return Err(windows::core::Error::from_thread());
                }
                std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                let _ = GlobalUnlock(mem);
                // On success the clipboard owns the memory.
                if let Err(e) = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0))) {
                    let _ = GlobalFree(Some(mem));
                    return Err(e);
                }
                Ok(())
            })();
            let _ = CloseClipboard();
            r
        });
        if let Some(hwnd) = owner {
            let _ = DestroyWindow(hwnd);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChangeSourceKind, TopFolder, PROJECT_ROOT_ID};
    use std::path::PathBuf;

    const MANIFEST: &str = include_str!("../../ui/Package.appxmanifest");

    #[test]
    fn manifest_registers_com_classes_and_native_cloud_files() {
        let manifest = MANIFEST.to_ascii_uppercase();
        let mut all = vec![CLSID_CUSTOM_STATES];
        all.extend(ShellVerb::ALL.iter().map(|v| v.clsid()));
        for clsid in &all {
            let s = format!("{clsid:?}");
            assert_eq!(s.len(), 36, "{s}");
            assert_eq!(
                manifest.matches(&s).count(),
                1,
                "{s} must be registered as one COM class"
            );
        }
        let unique: std::collections::HashSet<_> = all.iter().map(|g| format!("{g:?}")).collect();
        assert_eq!(unique.len(), all.len(), "CLSIDs are distinct");
        assert!(MANIFEST.contains(r#"Executable="engine\synka-engine.exe" Arguments="-Embedding""#));
        assert!(MANIFEST.contains("<desktop3:CloudFiles />"));
        assert!(!MANIFEST.contains("<desktop3:CloudFilesContextMenus>"));
        assert!(!MANIFEST.contains("<desktop3:CustomStateHandler"));
    }

    #[test]
    fn embedding_flag() {
        assert!(is_embedding_flag("-Embedding"));
        assert!(is_embedding_flag("/EMBEDDING"));
        assert!(!is_embedding_flag("-Embeddings"));
        assert!(!is_embedding_flag("synka://x"));
    }

    #[test]
    fn components() {
        let root = Path::new(r"C:\Users\Jöns\Synka");
        assert_eq!(
            components_below(root, Path::new(r"C:\Users\Jöns\Synka")),
            Some(vec![])
        );
        assert_eq!(
            components_below(root, Path::new(r"c:\users\JÖNS\Synka\Hub\Proj\a.rvt")),
            Some(vec![
                "Hub".to_string(),
                "Proj".to_string(),
                "a.rvt".to_string()
            ])
        );
        assert_eq!(
            components_below(root, Path::new(r"\\?\C:\Users\Jöns\Synka\Hub\")),
            Some(vec!["Hub".to_string()])
        );
        assert_eq!(
            components_below(root, Path::new(r"C:\Users\Jöns\Synka2\Hub")),
            None
        );
        assert_eq!(components_below(root, Path::new(r"C:\Users\Jöns")), None);
        assert_eq!(components_below(Path::new(""), Path::new(r"C:\x")), None);
    }

    struct TempDb(PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn project(project_id: &str, hub: &str, name: &str, root: &str) -> Project {
        Project {
            pid: 0,
            hub_id: format!("b.{hub}"),
            hub_name: hub.into(),
            project_id: project_id.into(),
            name: name.into(),
            root_folder_urn: format!("urn:adsk.wipemea:fs.folder:{root}"),
            urn_ns: "wipemea".into(),
            tops: vec![TopFolder {
                id: root.into(),
                name: "Project Files".into(),
            }],
            oss_scopes: None,
            change_source: ChangeSourceKind::Unknown,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: crate::model::Provider::Acc,
            meta: None,
        }
    }

    fn item(pid: i64, id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Item {
        Item {
            pid,
            id: id.into(),
            parent: parent.map(Into::into),
            name: name.into(),
            kind,
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
            populated: true,
        }
    }

    #[test]
    fn resolves_by_name() {
        let db =
            TempDb(std::env::temp_dir().join(format!("synka-shell-{}.db", std::process::id())));
        let store = Store::open(&db.0).unwrap();
        let pid = store
            .upsert_project(&project(
                "b.p1",
                "Byggstyrning AB",
                "Köpmannen: A/B",
                "co.root",
            ))
            .unwrap();
        let other = store
            .upsert_project(&project("b.p2", "Byggstyrning AB", "Annat", "co.r2"))
            .unwrap();
        store
            .upsert_item(&item(
                pid,
                PROJECT_ROOT_ID,
                None,
                "Köpmannen: A/B",
                Kind::ProjectRoot,
            ))
            .unwrap();
        store
            .upsert_item(&item(
                pid,
                "co.root",
                Some(PROJECT_ROOT_ID),
                "Project Files",
                Kind::Folder,
            ))
            .unwrap();
        store
            .upsert_item(&item(
                pid,
                "co.f",
                Some("co.root"),
                "Ritningar?",
                Kind::Folder,
            ))
            .unwrap();
        store
            .upsert_item(&item(pid, "lin1", Some("co.f"), "Plan.RVT", Kind::File))
            .unwrap();
        let mut gone = item(pid, "lin2", Some("co.f"), "old.dwg", Kind::File);
        gone.local_state = LocalState::Deleted;
        store.upsert_item(&gone).unwrap();

        let root = Path::new(r"C:\Users\u\Synka");
        let at =
            |rel: &str| resolve_in(&store, root, &root.join(rel)).map(|(p, it)| (p.pid, it.id));
        assert_eq!(
            at(r"Byggstyrning AB\Köpmannen_ A_B"),
            Some((pid, PROJECT_ROOT_ID.into()))
        );
        assert_eq!(
            at(r"Byggstyrning AB\Köpmannen_ A_B\project files"),
            Some((pid, "co.root".into()))
        );
        assert_eq!(
            at(r"byggstyrning ab\KÖPMANNEN_ A_B\Project Files\ritningar_"),
            Some((pid, "co.f".into()))
        );
        assert_eq!(
            at(r"Byggstyrning AB\Köpmannen_ A_B\Project Files\Ritningar_\plan.rvt"),
            Some((pid, "lin1".into()))
        );
        assert_eq!(
            at(r"Byggstyrning AB\Köpmannen_ A_B\Project Files\Ritningar_\old.dwg"),
            None,
            "deleted rows are skipped"
        );
        assert_eq!(
            at(r"Byggstyrning AB\Köpmannen_ A_B\Project Files\Ritningar_\Plan.RVT\x"),
            None,
            "below a file"
        );
        assert_eq!(
            at(r"Byggstyrning AB\Köpmannen_ A_B\Ritningar_"),
            None,
            "not directly in the project directory"
        );
        assert_eq!(at(r"Byggstyrning AB\Köpmannen_ A_B\Saknas"), None);
        assert_eq!(at(r"Byggstyrning AB"), None, "hub dir");
        assert_eq!(at(""), None, "sync root");
        assert_eq!(
            at(r"Byggstyrning AB\Annat"),
            None,
            "project root row missing"
        );
        assert!(resolve_in(
            &store,
            root,
            Path::new(r"D:\elsewhere\Byggstyrning AB\Annat")
        )
        .is_none());

        let all = store.projects().unwrap();
        let pids = |comps: &[&str]| -> Vec<i64> {
            let comps: Vec<String> = comps.iter().map(|s| s.to_string()).collect();
            projects_matching(all.clone(), &comps)
                .iter()
                .map(|p| p.pid)
                .collect()
        };
        assert_eq!(pids(&[]), vec![pid, other]);
        assert_eq!(pids(&["BYGGSTYRNING AB"]), vec![pid, other]);
        assert_eq!(pids(&["Byggstyrning AB", "Annat", "x.dwg"]), vec![other]);
        assert_eq!(pids(&["Annan hub"]), Vec::<i64>::new());
        drop(store);
    }
}

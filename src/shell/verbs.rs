//! Context-menu verbs (`IExplorerCommand`). `GetState` decides from the path alone, plus the
//! DB for Lås/Lås upp when Explorer says slow work is fine. `Invoke` copies the selected paths,
//! sends `TrayCmd::Shell` and returns at once.

use super::ShellVerb;
use crate::app::TrayCmd;
use crate::cf::placeholder::{self as ph, PinState};
use crate::model::Kind;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::core::{implement, Ref, BOOL, GUID, HSTRING, PWSTR};
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Storage::FileSystem::{
    GetFileAttributesW, FILE_ATTRIBUTE_DIRECTORY, INVALID_FILE_ATTRIBUTES,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, SHStrDupW,
    _EXPCMDSTATE, ECF_DEFAULT, ECS_ENABLED, ECS_HIDDEN, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

/// Most paths one Invoke hands to the engine.
const MAX_SELECTION: u32 = 1000;
/// Most items GetState looks up in the DB when deciding between Lås and Lås upp.
const MAX_STATE_LOOKUPS: u32 = 32;

#[implement(IExplorerCommand)]
pub(super) struct Verb(pub(super) ShellVerb);

impl IExplorerCommand_Impl for Verb_Impl {
    fn GetTitle(&self, _items: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        unsafe { SHStrDupW(&HSTRING::from(self.0.title())) }
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        match (self.0, app_icon()) {
            (ShellVerb::OpenInAcc, Some(icon)) => unsafe { SHStrDupW(icon) },
            _ => Err(E_NOTIMPL.into()),
        }
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        let text = match self.0 {
            ShellVerb::Pin => "Hämta och behåll filen lokalt. En mapp gäller även dess innehåll.",
            ShellVerb::Unpin => {
                "Tillåt Windows att frigöra lokalt utrymme efter synkning. ACC-kopian finns kvar."
            }
            _ => return Err(E_NOTIMPL.into()),
        };
        unsafe { SHStrDupW(&HSTRING::from(text)) }
    }

    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(self.0.clsid())
    }

    fn GetState(
        &self,
        items: Ref<IShellItemArray>,
        ok_to_be_slow: BOOL,
    ) -> windows::core::Result<u32> {
        let state = catch_unwind(AssertUnwindSafe(|| {
            state(self.0, items.as_ref(), ok_to_be_slow.as_bool())
        }))
        .unwrap_or(ECS_HIDDEN);
        Ok(state.0 as u32)
    }

    fn Invoke(
        &self,
        items: Ref<IShellItemArray>,
        _bind: Ref<IBindCtx>,
    ) -> windows::core::Result<()> {
        let items = items.ok()?;
        let limit = if matches!(self.0, ShellVerb::OpenInAcc | ShellVerb::Reset) {
            1
        } else {
            MAX_SELECTION
        };
        let paths = selection(items, limit);
        if paths.is_empty() {
            return Ok(());
        }
        if self.0 == ShellVerb::OpenInAcc {
            // Pass Explorer's foreground right on to the browser we start a moment later.
            unsafe {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
            }
        }
        if self.0 == ShellVerb::Reset {
            confirm_reset(paths.into_iter().next().unwrap_or_default());
            return Ok(());
        }
        if !super::send(TrayCmd::Shell {
            verb: self.0,
            paths,
        }) {
            tracing::warn!("{}: motorn tar inte emot kommandon", self.0.title());
        }
        Ok(())
    }

    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

/// Ask before rebuilding, on a thread of its own so Explorer's call returns at once.
fn confirm_reset(path: PathBuf) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO,
    };
    std::thread::spawn(move || {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = format!(
            "Bygga om \"{name}\" från ACC?\n\nMappen hämtas på nytt från ACC och filerna laddas ner igen när du öppnar dem. \
             Lokala ändringar som inte har synkats sparas först i mappen \"Synka - återställda filer\" i din användarmapp."
        );
        let answer = unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(text),
                &HSTRING::from(crate::sync::APP_NAME),
                MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
            )
        };
        if answer == IDYES
            && !super::send(TrayCmd::Reset {
                project_id: None,
                path: Some(path),
            })
        {
            tracing::warn!("återställning: motorn tar inte emot kommandon");
        }
    });
}

/// "<engine exe>,0" (the Synka icon), computed once.
fn app_icon() -> Option<&'static HSTRING> {
    static ICON: OnceLock<Option<HSTRING>> = OnceLock::new();
    ICON.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .map(|exe| HSTRING::from(format!("{},0", exe.display())))
    })
    .as_ref()
}

fn state(verb: ShellVerb, items: Option<&IShellItemArray>, ok_to_be_slow: bool) -> _EXPCMDSTATE {
    let Some(items) = items else {
        return ECS_HIDDEN;
    };
    let count = unsafe { items.GetCount() }.unwrap_or(0);
    let Some(first) = item_path(items, 0) else {
        return ECS_HIDDEN;
    };
    let Some(ctx) = super::context() else {
        return ECS_HIDDEN;
    };
    let Some(comps) = super::components_below(&ctx.paths.sync_root, &first) else {
        return ECS_HIDDEN;
    };
    if !visible(verb, comps.len(), is_directory(&first), count) {
        return ECS_HIDDEN;
    }
    match verb {
        ShellVerb::Pin | ShellVerb::Unpin => {
            let any = (0..count.min(MAX_STATE_LOOKUPS))
                .filter_map(|i| item_path(items, i))
                .filter_map(|path| {
                    ph::placeholder_info(&path)
                        .ok()
                        .flatten()
                        .map(|info| (path, info.pin_state))
                })
                .any(|(path, pin_state)| {
                    let pinned = match pin_state {
                        PinState::Pinned => true,
                        PinState::Inherit => {
                            if is_directory(&path) {
                                ph::dir_pinned(&path)
                            } else {
                                path.parent().is_some_and(ph::dir_pinned)
                            }
                        }
                        _ => false,
                    };
                    pinned == (verb == ShellVerb::Unpin)
                });
            if any {
                ECS_ENABLED
            } else {
                ECS_HIDDEN
            }
        }
        // Without permission for slow work, show both and let the engine sort it out.
        ShellVerb::Lock | ShellVerb::Unlock if ok_to_be_slow => {
            let want_locked = verb == ShellVerb::Unlock;
            let any = (0..count.min(MAX_STATE_LOOKUPS))
                .filter_map(|i| item_path(items, i))
                .filter_map(|p| super::resolve_path(ctx, &p))
                .any(|(_, it)| {
                    it.kind == Kind::File
                        && !crate::model::is_local_id(&it.id)
                        && it.locked_by.is_some() == want_locked
                });
            if any {
                ECS_ENABLED
            } else {
                ECS_HIDDEN
            }
        }
        _ => ECS_ENABLED,
    }
}

/// Visibility from the first selected path. `depth` counts components below the sync root:
/// 0 = the root, 1 = a hub dir, 2 = a project dir, 3+ = folders and files in a project.
fn visible(verb: ShellVerb, depth: usize, is_dir: bool, count: u32) -> bool {
    match verb {
        ShellVerb::OpenInAcc => count == 1 && depth >= 2,
        ShellVerb::CopyLink => count >= 1 && depth >= 2,
        ShellVerb::Lock | ShellVerb::Unlock => count >= 1 && depth >= 3 && !is_dir,
        ShellVerb::SyncNow => count >= 1,
        ShellVerb::Pin | ShellVerb::Unpin => count >= 1 && depth >= 2,
        ShellVerb::Reset => count == 1 && depth >= 2 && is_dir,
    }
}

/// File attributes only: never opens the file, so a placeholder is not hydrated.
fn is_directory(path: &Path) -> bool {
    let attrs = unsafe { GetFileAttributesW(&HSTRING::from(path)) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0
}

fn item_path(items: &IShellItemArray, index: u32) -> Option<PathBuf> {
    unsafe {
        let item = items.GetItemAt(index).ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.filter(|p| !p.is_empty()).map(PathBuf::from)
    }
}

/// The selection as file system paths. The array is only valid during the call.
fn selection(items: &IShellItemArray, limit: u32) -> Vec<PathBuf> {
    let count = unsafe { items.GetCount() }.unwrap_or(0).min(limit);
    (0..count).filter_map(|i| item_path(items, i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_by_depth_and_kind() {
        use ShellVerb::*;
        // sync root, hub, project, folder, file
        assert!(!visible(OpenInAcc, 0, true, 1));
        assert!(!visible(OpenInAcc, 1, true, 1));
        assert!(visible(OpenInAcc, 2, true, 1));
        assert!(visible(OpenInAcc, 4, false, 1));
        assert!(!visible(OpenInAcc, 4, false, 2), "one item at a time");
        assert!(visible(CopyLink, 4, false, 3));
        assert!(!visible(CopyLink, 1, true, 1));
        for v in [Lock, Unlock] {
            assert!(visible(v, 3, false, 1));
            assert!(visible(v, 5, false, 7));
            assert!(!visible(v, 3, true, 1), "folders cannot be locked");
            assert!(!visible(v, 2, true, 1));
        }
        assert!(visible(SyncNow, 0, true, 1));
        assert!(visible(SyncNow, 1, true, 1));
        assert!(visible(SyncNow, 4, false, 9));
        assert!(!visible(SyncNow, 4, false, 0));
        for v in [Pin, Unpin] {
            assert!(
                visible(v, 2, true, 1),
                "project folders can be kept offline"
            );
            assert!(visible(v, 4, false, 9), "files can be kept offline");
            assert!(!visible(v, 1, true, 1), "hub folders are not selectable");
        }
        assert!(visible(Reset, 2, true, 1), "a project folder");
        assert!(visible(Reset, 5, true, 1), "a folder inside it");
        assert!(!visible(Reset, 5, false, 1), "not a file");
        assert!(!visible(Reset, 1, true, 1), "not a hub dir");
        assert!(!visible(Reset, 4, true, 2), "one folder at a time");
    }

    #[test]
    fn titles_and_clsids_are_distinct() {
        let titles: std::collections::HashSet<_> =
            ShellVerb::ALL.iter().map(|v| v.title()).collect();
        assert_eq!(titles.len(), ShellVerb::ALL.len());
        assert_eq!(ShellVerb::Unlock.title(), "Lås upp");
        let _: fn() -> IExplorerCommand = || Verb(ShellVerb::Lock).into();
    }
}

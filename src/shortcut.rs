//! Desktop shortcut on first run (Koordinatverktyget DesktopShortcut.cs pattern): if kv
//! "desktop_shortcut_offered" is unset, create "%USERPROFILE%\Desktop\Synka.lnk" with
//! IShellLinkW + IPersistFile. Target: explorer.exe with argument "shell:AppsFolder\<PFN>!App"
//! when packaged (so it starts the packaged app), else the current exe. Icon = current exe,0.
//! Then set the kv flag. Do not recreate if the user deleted it. Also a second shortcut is
//! NOT created for the sync folder (Explorer shows the sync root in the navigation pane).

use crate::paths::Paths;
use crate::store::Store;
use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT,
};

const KV_FLAG: &str = "desktop_shortcut_offered";

/// Returns the created path, or None if already offered / failed (logged).
pub fn ensure_once(store: &Store, paths: &Paths) -> Option<PathBuf> {
    match store.kv_get(KV_FLAG) {
        Ok(Some(_)) => return None,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("desktop shortcut: kv read failed: {e:#}");
            return None;
        }
    }

    // Balanced init: only uninitialise when this call actually changed/added a reference.
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let res = create(paths);
    if init.is_ok() {
        unsafe { CoUninitialize() };
    }

    match res {
        Ok(path) => {
            if let Err(e) = store.kv_set(KV_FLAG, Some("1")) {
                tracing::warn!("desktop shortcut: could not persist offer flag: {e:#}");
            }
            tracing::info!("desktop shortcut created at {}", path.display());
            Some(path)
        }
        Err(e) => {
            tracing::warn!("desktop shortcut not created: {e:#}");
            None
        }
    }
}

fn create(paths: &Paths) -> Result<PathBuf> {
    let desktop = desktop_folder()?;
    let link_path = desktop.join("Synka.lnk");
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy().into_owned();

    let (target, args, workdir) = if paths.packaged {
        let pfn = paths
            .package_family_name
            .as_deref()
            .ok_or_else(|| anyhow!("packaged but no package family name"))?;
        let explorer = std::env::var("SystemRoot")
            .map(|r| format!("{r}\\explorer.exe"))
            .unwrap_or_else(|_| "C:\\Windows\\explorer.exe".to_string());
        (explorer, format!("shell:AppsFolder\\{pfn}!App"), None)
    } else {
        (
            exe_str.clone(),
            String::new(),
            exe.parent().map(|p| p.to_string_lossy().into_owned()),
        )
    };

    unsafe {
        let link: IShellLinkW = CoCreateInstance(
            &ShellLink,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )?;
        link.SetPath(&HSTRING::from(target.as_str()))?;
        link.SetArguments(&HSTRING::from(args.as_str()))?;
        link.SetIconLocation(&HSTRING::from(exe_str.as_str()), 0)?;
        link.SetDescription(&HSTRING::from("Synka"))?;
        if let Some(wd) = workdir {
            link.SetWorkingDirectory(&HSTRING::from(wd.as_str()))?;
        }
        let persist: IPersistFile = link.cast()?;
        let link_str = link_path.to_string_lossy().into_owned();
        persist.Save(&HSTRING::from(link_str.as_str()), true)?;
    }
    Ok(link_path)
}

fn desktop_folder() -> Result<PathBuf> {
    unsafe {
        let pwstr = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None)?;
        let s = pwstr.to_string();
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        Ok(PathBuf::from(
            s.map_err(|e| anyhow!("desktop path not UTF-16: {e}"))?,
        ))
    }
}

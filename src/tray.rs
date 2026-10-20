//! Tray icon + menu on the main thread (tray-icon crate, its re-exported `menu` = muda).
//! Swedish UI text. The window (Synka.exe, WinUI 3) holds the rest of the UI; see docs/IPC.md.
//!
//! Left click (single) opens the window:
//!   packaged   -> explorer.exe shell:AppsFolder\<PFN>!App
//!   unpackaged -> env SYNKA_UI, else the first Synka.exe under the dev ui folder (searched
//!                 once), else the sync folder.
//! Menu:
//!   <status line, disabled>     e.g. "Synkad · jonatan@…" / "Logga in"
//!   Öppna Synka               -> open the window
//!   Öppna mappen                -> TrayCmd::OpenFolder
//!   Synka nu                    -> TrayCmd::SyncNow (enabled when signed in)
//!   ──
//!   Avsluta                     -> TrayCmd::Quit (then the loop exits after the engine stops)
//! Tooltip: "Synka – <status text>". Icon: embedded resource id 1 (app.ico, see build.rs).
//!
//! Notifications: when a window is connected it shows them (InfoBar). Otherwise a Windows toast
//! (packaged; clicking it opens the window), at most 3 per 10 s with one "more in Synka"
//! summary after that, so a burst of conflicts never floods the screen. Unpackaged dev runs
//! fall back to one message box at a time.
//!
//! Message loop: GetMessageW/TranslateMessage/DispatchMessageW. On WM_APP_UI drain the UiMsg
//! receiver. Poll MenuEvent / TrayIconEvent receivers after every message. Exit on WM_QUIT
//! (posted by main when the engine finished).

use crate::app::{Severity, Status, TrayCmd, UiMsg, WM_APP_UI};
use crate::ipc::IpcHub;
use crate::paths::Paths;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetSystemMetrics, LoadImageW, SendMessageW, TranslateMessage,
    ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, MSG, SM_CXICON, SM_CXSMICON, SM_CYICON,
    SM_CYSMICON, WM_SETICON,
};

/// Dev build of the window (unpackaged runs). Searched once.
const UI_DEV_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), r"\ui\bin\x64\Debug");
const TOAST_WINDOW: Duration = Duration::from_secs(10);
const TOASTS_PER_WINDOW: usize = 3;
/// Permanent Windows notification-icon identity. Never change this between releases.
/// The packaged engine is Authenticode-signed before packaging so Windows can
/// retain the user's visibility preference when its versioned install path moves.
const TRAY_ICON_GUID: u128 = 0x6e1b0f02_70be_4768_a154_f30c67ca1912;
const ACC_FILE_TYPES_HELP: &str =
    "https://help.autodesk.com/cloudhelp/ENU/Docs-Files/files/files-upload/Supported_Files_Docs.html";

struct Tray {
    tray: TrayIcon,
    status_item: MenuItem,
    sync_item: MenuItem,
    open_ui_id: MenuId,
    open_folder_id: MenuId,
    sync_id: MenuId,
    quit_id: MenuId,
    cmd_tx: UnboundedSender<TrayCmd>,
    hub: Arc<IpcHub>,
    paths: Paths,
    signed_in: bool,
    /// Times of recent toasts (burst limit) and whether the "more" summary was shown.
    recent_toasts: Vec<Instant>,
    summarized: bool,
}

pub fn run_message_loop(
    ui_rx: std::sync::mpsc::Receiver<UiMsg>,
    cmd_tx: UnboundedSender<TrayCmd>,
    hub: Arc<IpcHub>,
    paths: Paths,
) -> anyhow::Result<()> {
    let mut tray = Tray::build(cmd_tx, hub, paths)?;
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // hwnd = None: thread messages, including tray-icon's own hidden-window messages.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 {
            break; // WM_QUIT
        }
        if r.0 == -1 {
            anyhow::bail!("GetMessageW failed");
        }
        if msg.message == WM_APP_UI {
            while let Ok(m) = ui_rx.try_recv() {
                tray.on_ui(m);
            }
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        tray.poll_events();
    }
    Ok(())
}

/// The notification icon does not assign an icon to tray-icon's hidden HWND.
/// Give Windows the same embedded icon for native window/process grouping.
fn set_native_window_icons(hwnd: HWND) -> windows::core::Result<()> {
    // SAFETY: this HWND belongs to the tray on the current thread. Resource 1 is
    // embedded by build.rs; LR_SHARED keeps its handles valid for the process lifetime.
    unsafe {
        let instance = GetModuleHandleW(PCWSTR::null())?.into();
        for (kind, width, height) in [
            (ICON_SMALL, SM_CXSMICON, SM_CYSMICON),
            (ICON_BIG, SM_CXICON, SM_CYICON),
        ] {
            let icon = LoadImageW(
                Some(instance),
                PCWSTR(std::ptr::without_provenance::<u16>(1)),
                IMAGE_ICON,
                GetSystemMetrics(width),
                GetSystemMetrics(height),
                LR_SHARED,
            )?;
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(kind as usize)),
                Some(LPARAM(icon.0 as isize)),
            );
        }
    }
    Ok(())
}
impl Tray {
    fn build(
        cmd_tx: UnboundedSender<TrayCmd>,
        hub: Arc<IpcHub>,
        paths: Paths,
    ) -> anyhow::Result<Tray> {
        let status_item = MenuItem::new("Logga in", false, None);
        let open_ui = MenuItem::new("Öppna Synka", true, None);
        let open_folder = MenuItem::new("Öppna mappen", true, None);
        let sync_item = MenuItem::new("Synka nu", false, None);
        let quit_item = MenuItem::new("Avsluta", true, None);

        let menu = Menu::new();
        menu.append(&status_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&open_ui)?;
        menu.append(&open_folder)?;
        menu.append(&sync_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit_item)?;

        let icon = Icon::from_resource(1, None)
            .map_err(|e| anyhow::anyhow!("app icon (resource 1): {e:?}"))?;
        let mut builder = TrayIconBuilder::new()
            .with_tooltip("Synka – Logga in")
            .with_icon(icon)
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(false);
        if paths.packaged && !cfg!(debug_assertions) {
            builder = builder.with_guid(TRAY_ICON_GUID);
        }
        // Unpackaged and debug builds must not claim the production identity.
        // Release packaging signs the engine before it receives this GUID.
        let tray = builder.build()?;
        if let Err(error) = set_native_window_icons(HWND(tray.window_handle())) {
            tracing::warn!("kunde inte sätta motorns fönsterikon: {error}");
        }

        Ok(Tray {
            tray,
            status_item,
            open_ui_id: open_ui.id().clone(),
            open_folder_id: open_folder.id().clone(),
            sync_id: sync_item.id().clone(),
            sync_item,
            quit_id: quit_item.id().clone(),
            cmd_tx,
            hub,
            paths,
            signed_in: false,
            recent_toasts: Vec::new(),
            summarized: false,
        })
    }

    fn send(&self, cmd: TrayCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    fn on_ui(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Status(s) => {
                self.apply_status(&s);
                crate::shell::flyout::status_changed();
            }
            UiMsg::Notify {
                title,
                body,
                severity,
            } => {
                tracing::info!("{title}: {body}");
                // The window shows notifications itself.
                if self.hub.client_count() == 0 {
                    self.notify(&title, &body, severity);
                }
            }
            // Projects, updates, settings and activity are shown by the window only.
            UiMsg::Projects { .. }
            | UiMsg::Update { .. }
            | UiMsg::Settings(_)
            | UiMsg::Activity(_)
            | UiMsg::ApsClientIdValidation { .. } => {}
        }
    }

    fn apply_status(&mut self, s: &Status) {
        self.signed_in = s.user.is_some();
        let user_label = s.user.as_ref().map(|u| {
            if u.email.is_empty() {
                u.name.clone()
            } else {
                u.email.clone()
            }
        });
        let text = if s.text.trim().is_empty() {
            if self.signed_in {
                "Synkad".to_string()
            } else {
                "Logga in".to_string()
            }
        } else {
            s.text.clone()
        };
        let line = match &user_label {
            Some(who) => format!("{text} · {who}"),
            None => text.clone(),
        };
        self.status_item.set_text(&line);
        let _ = self.tray.set_tooltip(Some(format!("Synka – {text}")));
        self.sync_item.set_enabled(self.signed_in);
    }

    fn notify(&mut self, title: &str, body: &str, severity: Severity) {
        let (title, body, severity, file_types_help) = friendly_notification(title, body, severity);
        let Some(pfn) = self.paths.package_family_name.clone() else {
            message_box(&title, &body, severity);
            return;
        };
        let now = Instant::now();
        self.recent_toasts
            .retain(|t| now.duration_since(*t) < TOAST_WINDOW);
        if self.recent_toasts.is_empty() {
            self.summarized = false;
        }
        let (title, body, file_types_help) = if self.recent_toasts.len() < TOASTS_PER_WINDOW {
            (title, body, file_types_help)
        } else if !self.summarized {
            self.summarized = true;
            (
                title,
                "Fler händelser finns under Aktivitet i Synka.".to_string(),
                false,
            )
        } else {
            return;
        };
        self.recent_toasts.push(now);
        if let Err(e) = show_toast(&pfn, &title, &body, file_types_help) {
            tracing::debug!("toast: {e}");
            message_box(&title, &body, severity);
        }
    }

    fn poll_events(&mut self) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.open_ui_id {
                open_ui(&self.paths);
            } else if ev.id == self.open_folder_id {
                self.send(TrayCmd::OpenFolder);
            } else if ev.id == self.sync_id {
                self.send(TrayCmd::SyncNow);
            } else if ev.id == self.quit_id {
                self.send(TrayCmd::Quit);
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                open_ui(&self.paths);
            }
        }
    }
}

/// Only the log and Activity retain ACC's response. Windows notifications need
/// short, useful text even when the app window is closed.
fn friendly_notification(
    title: &str,
    body: &str,
    severity: Severity,
) -> (String, String, Severity, bool) {
    if let Some(extension) = unsupported_extension(body) {
        return (
            "Filtyp stöds inte i ACC".into(),
            format!("ACC tar inte emot {extension}-filer. Filen finns kvar på datorn."),
            Severity::Info,
            true,
        );
    }
    if body.contains("HTTP ") || body.contains("{\"jsonapi\"") {
        return (
            title.into(),
            "Synkningen gick inte att slutföra. Se Aktivitet för mer information.".into(),
            severity,
            false,
        );
    }
    (title.into(), body.into(), severity, false)
}

fn unsupported_extension(body: &str) -> Option<String> {
    const PREFIX: &str = "File with extension ";
    const SUFFIX: &str = " is not allowed to upload";
    let start = body.find(PREFIX)? + PREFIX.len();
    let end = body[start..].find(SUFFIX)? + start;
    let extension = body[start..end].trim().trim_start_matches('.');
    if extension.is_empty()
        || extension.len() > 12
        || !extension.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(format!(".{}", extension.to_ascii_lowercase()))
}

/// A toast in the name of the window app (same package), so clicking it opens the window.
fn show_toast(
    pfn: &str,
    title: &str,
    body: &str,
    file_types_help: bool,
) -> windows::core::Result<()> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    let action = if file_types_help {
        format!(
            "<actions><action content=\"Filtyper i ACC\" arguments=\"{}\" activationType=\"protocol\" /></actions>",
            xml_escape(ACC_FILE_TYPES_HELP)
        )
    } else {
        String::new()
    };
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>{action}</toast>",
        xml_escape(title),
        xml_escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(format!("{pfn}!App")))?
        .Show(&toast)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Unpackaged fallback: one message box at a time on its own thread (the loop keeps pumping);
/// while one is open, further notices go to the log only.
fn message_box(title: &str, body: &str, severity: Severity) {
    static OPEN: AtomicBool = AtomicBool::new(false);
    if OPEN.swap(true, Ordering::SeqCst) {
        return;
    }
    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND,
            MB_TOPMOST,
        };
        let icon = match severity {
            Severity::Info => MB_ICONINFORMATION,
            Severity::Warning => MB_ICONWARNING,
            Severity::Error => MB_ICONERROR,
        };
        unsafe {
            MessageBoxW(
                None,
                &windows::core::HSTRING::from(body),
                &windows::core::HSTRING::from(format!("Synka – {title}")),
                MB_OK | icon | MB_TOPMOST | MB_SETFOREGROUND,
            );
        }
        OPEN.store(false, Ordering::SeqCst);
    });
}

/// Opens the Synka window (packaged: via its app id; unpackaged: a dev build).
fn open_ui(paths: &Paths) {
    if let Some(pfn) = &paths.package_family_name {
        let _ = Command::new("explorer.exe")
            .arg(format!("shell:AppsFolder\\{pfn}!App"))
            .spawn();
        return;
    }
    static UI_EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    if let Some(exe) = UI_EXE.get_or_init(find_ui_exe) {
        let _ = Command::new(exe).spawn();
        return;
    }
    let _ = Command::new("explorer.exe").arg(&paths.sync_root).spawn();
}

/// SYNKA_UI, else the first Synka.exe under UI_DEV_DIR (up to 4 levels deep).
fn find_ui_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYNKA_UI").map(PathBuf::from) {
        if p.is_file() {
            return Some(p);
        }
    }
    search(Path::new(UI_DEV_DIR), 4)
}

fn search(dir: &Path, depth: u32) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    if let Some(f) = entries.iter().find(|p| {
        p.is_file()
            && p.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("Synka.exe"))
    }) {
        return Some(f.clone());
    }
    if depth == 0 {
        return None;
    }
    entries
        .iter()
        .filter(|p| p.is_dir())
        .find_map(|d| search(d, depth - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_refusal_text_reaches_the_user_unchanged() {
        // A provider's own "not offered" text is user-facing already: no HTTP or ACC rewording.
        let raw = "Kunde inte synkronisera rapport.pdf: Interaxo stöds inte ännu";
        let (title, body, severity, help) = friendly_notification("Synka", raw, Severity::Info);
        assert_eq!(title, "Synka");
        assert_eq!(body, raw);
        assert_eq!(severity, Severity::Info);
        assert!(!help);
    }

    #[test]
    fn unsupported_acc_error_is_calm_and_links_to_help() {
        let raw = r#"Kunde inte synkronisera unsupported-1736.svg: HTTP 403: {"jsonapi":{"version":"1.0"},"errors":[{"code":"ERR_NOT_ALLOWED","detail":"File with extension svg is not allowed to upload."}]}"#;
        let (title, body, severity, help) = friendly_notification("Synka", raw, Severity::Error);
        assert_eq!(title, "Filtyp stöds inte i ACC");
        assert_eq!(
            body,
            "ACC tar inte emot .svg-filer. Filen finns kvar på datorn."
        );
        assert_eq!(severity, Severity::Info);
        assert!(help);
        assert!(!body.contains("HTTP 403"));
        assert!(!body.contains("jsonapi"));
    }

    #[test]
    fn other_server_errors_keep_raw_detail_out_of_toast() {
        let (title, body, severity, help) = friendly_notification(
            "Synka",
            "Kunde inte synkronisera ritning.rvt: HTTP 403: {\"jsonapi\":{}}",
            Severity::Error,
        );
        assert_eq!(title, "Synka");
        assert_eq!(
            body,
            "Synkningen gick inte att slutföra. Se Aktivitet för mer information."
        );
        assert_eq!(severity, Severity::Error);
        assert!(!help);
    }

    #[test]
    fn unrelated_notification_is_unchanged() {
        let (title, body, severity, help) =
            friendly_notification("Synka", "Synkningen är klar.", Severity::Info);
        assert_eq!(title, "Synka");
        assert_eq!(body, "Synkningen är klar.");
        assert_eq!(severity, Severity::Info);
        assert!(!help);
    }
}

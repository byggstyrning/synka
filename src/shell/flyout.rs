//! Windows 11 cloud status flyout (`cloudFiles2:StorageProviderStatusUISourceFactory`).
//!
//! GetStatusUI builds the flyout from the IpcHub status cache (the same JSON the window gets),
//! so it never touches the network. The tray loop calls `status_changed` on the main STA
//! thread (where the class objects and Explorer's handlers live) for every status event; when
//! the status JSON changed it raises StatusUIChanged for every subscribed Explorer handler.

use super::APP_ICON_URI;
use serde_json::Value;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard};
use windows::core::{implement, IInspectable, Ref, Result, HSTRING};
use windows::Foundation::{TypedEventHandler, Uri};
use windows::Storage::Provider::{
    IStorageProviderStatusUISource, IStorageProviderStatusUISourceFactory,
    IStorageProviderStatusUISourceFactory_Impl, IStorageProviderStatusUISource_Impl,
    IStorageProviderUICommand, IStorageProviderUICommand_Impl, StorageProviderMoreInfoUI,
    StorageProviderState, StorageProviderStatusUI, StorageProviderUICommandState,
};

#[implement(IStorageProviderStatusUISourceFactory)]
pub struct StatusFactory;

impl IStorageProviderStatusUISourceFactory_Impl for StatusFactory_Impl {
    fn GetStatusUISource(&self, _sync_root_id: &HSTRING) -> Result<IStorageProviderStatusUISource> {
        Ok(StatusSource.into())
    }
}

#[implement(IStorageProviderStatusUISource)]
pub struct StatusSource;

impl IStorageProviderStatusUISource_Impl for StatusSource_Impl {
    fn GetStatusUI(&self) -> Result<StorageProviderStatusUI> {
        build_ui()
    }

    fn StatusUIChanged(
        &self,
        handler: Ref<TypedEventHandler<IStorageProviderStatusUISource, IInspectable>>,
    ) -> Result<i64> {
        let handler = handler.ok()?.clone();
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        subs().push(Sub { token, handler });
        Ok(token)
    }

    fn RemoveStatusUIChanged(&self, token: i64) -> Result<()> {
        subs().retain(|s| s.token != token);
        Ok(())
    }
}

struct Sub {
    token: i64,
    handler: TypedEventHandler<IStorageProviderStatusUISource, IInspectable>,
}

// SAFETY: the handler is an interface pointer that Explorer gave us. It is only used for
// Invoke, from the COM threads, and COM marshals calls to it as its apartment requires.
unsafe impl Send for Sub {}

static NEXT_TOKEN: AtomicI64 = AtomicI64::new(1);
static SUBS: Mutex<Vec<Sub>> = Mutex::new(Vec::new());
static LAST_SENT: Mutex<String> = Mutex::new(String::new());

fn subs() -> MutexGuard<'static, Vec<Sub>> {
    SUBS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The engine's current `status` event as JSON (from the IpcHub cache). Null before the first.
fn current_status() -> Value {
    super::context()
        .and_then(|ctx| ctx.ui.hub.snapshot().into_iter().next())
        .and_then(|line| serde_json::from_str::<Value>(&line).ok())
        .unwrap_or(Value::Null)
}

/// Maps the engine's `status.state` to the flyout's provider state.
pub fn state_of(state: &str) -> StorageProviderState {
    match state {
        "synced" => StorageProviderState::InSync,
        "syncing" => StorageProviderState::Syncing,
        // Queued jobs may be deferred; they are not active transfers.
        "waiting" => StorageProviderState::Paused,
        "attention" => StorageProviderState::Error,
        "offline" => StorageProviderState::Offline,
        "error" => StorageProviderState::Error,
        // signed_out and anything unknown
        _ => StorageProviderState::Paused,
    }
}

fn build_ui() -> Result<StorageProviderStatusUI> {
    let status = current_status();
    let state = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("signed_out");
    let text = status
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("Logga in");

    let ui = StorageProviderStatusUI::new()?;
    ui.SetProviderState(state_of(state))?;
    ui.SetProviderStateLabel(&HSTRING::from(text))?;
    ui.SetProviderStateIcon(&Uri::CreateUri(&HSTRING::from(APP_ICON_URI))?)?;

    let open: IStorageProviderUICommand = OpenAppCommand.into();
    ui.SetSyncStatusCommand(&open)?;

    let more = StorageProviderMoreInfoUI::new()?;
    more.SetMessage(&HSTRING::from(more_text(&status)))?;
    ui.SetMoreInfoUI(&more)?;
    Ok(ui)
}

/// Swedish detail text for the flyout's "more info" area.
fn more_text(status: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(err) = status.get("error").and_then(Value::as_str) {
        lines.push(format!("Fel: {err}"));
    }
    let pending = status
        .get("pending_jobs")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let failed = status
        .get("failed_jobs")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    lines.push(format!("Väntande jobb: {pending}"));
    if failed > 0 {
        lines.push(format!("Misslyckade jobb: {failed}"));
    }
    if let Some(transfers) = status.get("transfers").and_then(Value::as_array) {
        for t in transfers.iter().take(5) {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            let done = t.get("done").and_then(Value::as_u64).unwrap_or(0);
            let total = t.get("total").and_then(Value::as_u64).unwrap_or(0);
            let verb = match t.get("direction").and_then(Value::as_str) {
                Some("up") => "Laddar upp",
                _ => "Hämtar",
            };
            let pct = (done * 100).checked_div(total).unwrap_or(0);
            lines.push(format!("{verb}: {name} ({pct} %)"));
        }
    }
    lines.join("\n")
}

/// "Öppna Synka" in the flyout: starts the window through its AppsFolder id.
#[implement(IStorageProviderUICommand)]
pub struct OpenAppCommand;

impl IStorageProviderUICommand_Impl for OpenAppCommand_Impl {
    fn Label(&self) -> Result<HSTRING> {
        Ok(HSTRING::from(format!("Öppna {}", crate::sync::APP_NAME)))
    }

    fn Description(&self) -> Result<HSTRING> {
        Ok(HSTRING::from(format!(
            "Öppna fönstret för {}",
            crate::sync::APP_NAME
        )))
    }

    fn Icon(&self) -> Result<Uri> {
        Uri::CreateUri(&HSTRING::from(APP_ICON_URI))
    }

    fn State(&self) -> Result<StorageProviderUICommandState> {
        Ok(StorageProviderUICommandState::Enabled)
    }

    fn Invoke(&self) -> Result<()> {
        open_app();
        Ok(())
    }
}

/// Starts `shell:AppsFolder\<package family name>!App`. Spawning returns at once.
pub fn open_app() {
    let Some(pfn) = super::context().and_then(|ctx| ctx.paths.package_family_name.clone()) else {
        tracing::warn!("open_app: not running packaged, no AppsFolder id");
        return;
    };
    let target = format!("shell:AppsFolder\\{pfn}!App");
    if let Err(e) = std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn()
    {
        tracing::warn!("open_app: {e}");
    }
}

/// Called on the main STA thread for every status event. Raises StatusUIChanged when the
/// status changed since the last time.
pub fn status_changed() {
    if subs().is_empty() {
        return;
    }
    let now = current_status().to_string();
    {
        let mut last = LAST_SENT.lock().unwrap_or_else(|e| e.into_inner());
        if *last == now {
            return;
        }
        *last = now;
    }
    let handlers: Vec<TypedEventHandler<IStorageProviderStatusUISource, IInspectable>> =
        subs().iter().map(|s| s.handler.clone()).collect();
    let source: IStorageProviderStatusUISource = StatusSource.into();
    for h in handlers {
        if let Err(e) = h.Invoke(&source, None::<&IInspectable>) {
            tracing::debug!("StatusUIChanged handler: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_mapping() {
        assert_eq!(state_of("synced").0, StorageProviderState::InSync.0);
        assert_eq!(state_of("syncing").0, StorageProviderState::Syncing.0);
        assert_eq!(state_of("waiting").0, StorageProviderState::Paused.0);
        assert_eq!(state_of("attention").0, StorageProviderState::Error.0);
        assert_eq!(state_of("offline").0, StorageProviderState::Offline.0);
        assert_eq!(state_of("error").0, StorageProviderState::Error.0);
        assert_eq!(state_of("signed_out").0, StorageProviderState::Paused.0);
        assert_eq!(state_of("bogus").0, StorageProviderState::Paused.0);
    }

    #[test]
    fn more_text_lists_jobs_and_transfers() {
        let v = serde_json::json!({
            "pending_jobs": 3, "failed_jobs": 1, "error": null,
            "transfers": [{"name": "a.rvt", "direction": "down", "done": 50, "total": 200}]
        });
        let t = more_text(&v);
        assert!(t.contains("Väntande jobb: 3"));
        assert!(t.contains("Misslyckade jobb: 1"));
        assert!(t.contains("Hämtar: a.rvt (25 %)"));
    }
}

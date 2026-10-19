//! Windows Cloud Files API layer (behaviour: docs/specs/cloud-files.md). This module knows
//! nothing about Autodesk.
//!
//! Registration (WinRT `Windows::Storage::Provider::StorageProviderSyncRootManager::Register`):
//!   Id = "synka!" + base64(user_id) + "!" + <current user SID string> + "!User"
//!   Path = StorageFolder::GetFolderFromPathAsync(root), DisplayNameResource = display name,
//!   IconResource = "<exe path>,0", Version = crate version, ProviderId = PROVIDER_ID,
//!   HydrationPolicy = Full, HydrationPolicyModifier = AutoDehydrationAllowed,
//!   PopulationPolicy = Full, InSyncPolicy = FileLastWriteTime | DirectoryLastWriteTime,
//!   HardlinkPolicy = None, ShowSiblingsAsGroup = false, AllowPinning = true,
//!   ProtectionMode = Unknown (ACC contains business files), Context = buffer of the id string,
//!   StorageProviderItemPropertyDefinitions = shell::PROPERTY_DEFINITIONS (custom state columns).
//!   `register` is idempotent: if a root with the same Id is registered at the same path, ok.
//!   Definitions are only read at Register time, so when the stored ones differ the stored
//!   registration is registered again with the new list (no Unregister: placeholders stay).
//!
//! Connect: CfConnectSyncRoot with callbacks FETCH_DATA, CANCEL_FETCH_DATA, FETCH_PLACEHOLDERS,
//!   CANCEL_FETCH_PLACEHOLDERS, NOTIFY_DELETE, NOTIFY_RENAME, then CF_CALLBACK_REGISTRATION_END.
//!   Flags: CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH | CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION.
//!   Callbacks are `extern "system"` fns that COPY everything they need out of
//!   CF_CALLBACK_INFO / CF_CALLBACK_PARAMETERS into a `CfEvent` and `send` it on the channel
//!   stored in a process-global (OnceLock). They must never block or call into tokio.
//!   The request objects carry the keys needed to complete the operation later with
//!   CfExecute from any thread.
//!
//! Call `expose_placeholders()` once at startup: RtlSetProcessPlaceholderCompatibilityMode
//!   (ntdll, PHCM_EXPOSE_PLACEHOLDERS = 2) so attributes are not disguised for us.

pub mod callbacks;
pub mod placeholder;

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::c_void;
use std::future::{Future, IntoFuture};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Poll, Wake, Waker};
use tokio::sync::mpsc::UnboundedSender;
use windows::core::{GUID, HSTRING, PCWSTR, PWSTR};
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Storage::Provider::{
    StorageProviderHardlinkPolicy, StorageProviderHydrationPolicy,
    StorageProviderHydrationPolicyModifier, StorageProviderInSyncPolicy,
    StorageProviderItemPropertyDefinition, StorageProviderPopulationPolicy,
    StorageProviderProtectionMode, StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
};
use windows::Storage::StorageFolder;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::Storage::CloudFilters::{
    CfConnectSyncRoot, CfDisconnectSyncRoot, CfUpdateSyncProviderStatus, CF_CALLBACK_REGISTRATION,
    CF_CALLBACK_TYPE_CANCEL_FETCH_DATA, CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
    CF_CALLBACK_TYPE_FETCH_DATA, CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS, CF_CALLBACK_TYPE_NONE,
    CF_CALLBACK_TYPE_NOTIFY_DELETE, CF_CALLBACK_TYPE_NOTIFY_RENAME, CF_CONNECTION_KEY,
    CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION, CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
    CF_PROVIDER_STATUS_CONNECTIVITY_LOST, CF_PROVIDER_STATUS_ERROR, CF_PROVIDER_STATUS_IDLE,
    CF_PROVIDER_STATUS_SYNC_INCREMENTAL, CF_SYNC_PROVIDER_STATUS,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Stable provider GUID of the sync provider.
pub const PROVIDER_ID: &str = "6f1d7c1e-3b2a-4c8e-9a51-fa57adc00001";

/// NTSTATUS codes for failing callbacks.
pub const STATUS_CLOUD_FILE_UNSUCCESSFUL: i32 = 0xC000_CF0Fu32 as i32;
pub const STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE: i32 = 0xC000_CF07u32 as i32;
pub const STATUS_CLOUD_FILE_ACCESS_DENIED: i32 = 0xC000_CF18u32 as i32;

/// RtlSetProcessPlaceholderCompatibilityMode argument: show real attributes and reparse tags.
const PHCM_EXPOSE_PLACEHOLDERS: u8 = 2;

/// Keys to complete a callback with CfExecute later. Plain integers so it is Send + Sync.
#[derive(Debug, Clone, Copy)]
pub struct OpKeys {
    pub connection_key: i64,
    pub transfer_key: i64,
    pub request_key: i64,
    /// CF_CALLBACK_INFO.CorrelationVector is not needed; FileId kept for dedup.
    pub file_id: i64,
}

#[derive(Debug, Clone)]
pub struct FetchDataReq {
    pub keys: OpKeys,
    /// Full path (\\?\ prefix stripped, volume-absolute "C:\..." form).
    pub path: PathBuf,
    pub identity: Vec<u8>,
    pub file_size: u64,
    pub required_offset: u64,
    pub required_length: u64,
}

#[derive(Debug, Clone)]
pub struct FetchPlaceholdersReq {
    pub keys: OpKeys,
    pub path: PathBuf,
    pub identity: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DeleteReq {
    pub keys: OpKeys,
    pub path: PathBuf,
    pub identity: Vec<u8>,
    /// The delete was taken back (CF_CALLBACK_DELETE_FLAG_IS_UNDELETE): the file stays.
    pub undelete: bool,
}

#[derive(Debug, Clone)]
pub struct RenameReq {
    pub keys: OpKeys,
    pub path: PathBuf,
    pub target_path: PathBuf,
    pub identity: Vec<u8>,
    /// False when the target is outside the sync root (treat as delete).
    pub target_in_scope: bool,
}

#[derive(Debug, Clone)]
pub enum CfEvent {
    FetchData(FetchDataReq),
    CancelFetchData { file_id: i64, transfer_key: i64 },
    FetchPlaceholders(FetchPlaceholdersReq),
    CancelFetchPlaceholders,
    Delete(DeleteReq),
    Rename(RenameReq),
}

/// Process-global route from the cfapi callback threads to the engine.
static EVENT_TX: OnceLock<Mutex<Option<UnboundedSender<CfEvent>>>> = OnceLock::new();

fn event_slot() -> &'static Mutex<Option<UnboundedSender<CfEvent>>> {
    EVENT_TX.get_or_init(|| Mutex::new(None))
}

fn set_event_sink(tx: Option<UnboundedSender<CfEvent>>) {
    *event_slot().lock().unwrap_or_else(|p| p.into_inner()) = tx;
}

/// Hand an event to the engine. Non-blocking. Returns false when nothing is connected, in
/// which case the caller must complete the cfapi operation with a failure itself.
pub(crate) fn send_event(ev: CfEvent) -> bool {
    let guard = event_slot().lock().unwrap_or_else(|p| p.into_inner());
    match guard.as_ref() {
        Some(tx) => tx.send(ev).is_ok(),
        None => false,
    }
}

pub fn expose_placeholders() {
    type SetCompatMode = unsafe extern "system" fn(u8) -> u8;
    unsafe {
        let Ok(ntdll) = GetModuleHandleW(windows::core::w!("ntdll.dll")) else {
            return;
        };
        let Some(proc) = GetProcAddress(
            ntdll,
            windows::core::s!("RtlSetProcessPlaceholderCompatibilityMode"),
        ) else {
            return;
        };
        let set: SetCompatMode = std::mem::transmute(proc);
        let previous = set(PHCM_EXPOSE_PLACEHOLDERS);
        tracing::debug!(previous, "placeholder compatibility mode set to expose");
    }
}

/// Register the sync root (idempotent). Creates `root` if missing.
pub fn register(root: &Path, user_id: &str, display_name: &str, version: &str) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create sync root {}", root.display()))?;
    let root_str = root
        .to_str()
        .context("sync root path is not valid UTF-8")?
        .to_string();
    let icon = format!("{},0", std::env::current_exe()?.display());
    let user_id = user_id.to_string();
    let display_name = display_name.to_string();
    let version = version.to_string();
    on_mta_thread(move || register_winrt(&root_str, &user_id, &display_name, &version, &icon))
}

fn register_winrt(
    root: &str,
    user_id: &str,
    display_name: &str,
    version: &str,
    icon: &str,
) -> Result<()> {
    let id = sync_root_id(user_id)?;
    if let Some(existing) = find_sync_root(&id)? {
        if same_path(&existing.Path()?.Path()?.to_string(), root) {
            if !same_definitions(
                &stored_definitions(&existing)?,
                &crate::shell::PROPERTY_DEFINITIONS,
            ) || existing.ProtectionMode()? != StorageProviderProtectionMode::Unknown
                || existing.IconResource()? != icon
                || existing.DisplayNameResource()? != display_name
                || existing.Version()? != version
            {
                // Register the stored info again with the new list (Register with an existing id
                // updates it in place, see docs/specs/cloud-files.md); if its list is read-only, a
                // fresh info with the same id and path. Never Unregister here. A failure only
                // costs the columns.
                // VERIFY on Windows 11: Register overwrites an existing id in place and the
                // placeholders keep working.
                let updated = set_definitions(&existing)
                    .and_then(|()| {
                        existing
                            .SetProtectionMode(StorageProviderProtectionMode::Unknown)
                            .map_err(Into::into)
                    })
                    .and_then(|()| {
                        existing
                            .SetIconResource(&HSTRING::from(icon))
                            .map_err(Into::into)
                    })
                    .and_then(|()| {
                        existing
                            .SetDisplayNameResource(&HSTRING::from(display_name))
                            .map_err(Into::into)
                    })
                    .and_then(|()| {
                        existing
                            .SetVersion(&HSTRING::from(version))
                            .map_err(Into::into)
                    })
                    .and_then(|()| {
                        StorageProviderSyncRootManager::Register(&existing).map_err(Into::into)
                    })
                    .or_else(|e| {
                        tracing::debug!("lagrad registrering gick inte att uppdatera: {e:#}");
                        let info = new_info(root, &id, display_name, version, icon)?;
                        StorageProviderSyncRootManager::Register(&info).map_err(anyhow::Error::from)
                    });
                match updated {
                    Ok(()) => tracing::info!("synkrotens kolumner uppdaterade"),
                    Err(e) => tracing::warn!("kunde inte uppdatera synkrotens kolumner: {e:#}"),
                }
            }
            return Ok(());
        }
        // Same id at another location: our own stale registration, replace it.
        StorageProviderSyncRootManager::Unregister(&HSTRING::from(&id))?;
    }

    let info = new_info(root, &id, display_name, version, icon)?;
    match StorageProviderSyncRootManager::Register(&info) {
        Ok(()) => {
            // Register can return success even when Explorer cannot resolve the root.
            // Keep syncing, but record the WinRT lookup result for diagnosis.
            match StorageProviderSyncRootManager::GetSyncRootInformationForId(&HSTRING::from(&id)) {
                Ok(_) => tracing::info!("WinRT can resolve newly registered sync root"),
                Err(e) => {
                    tracing::warn!(hresult = ?e.code(), "WinRT cannot resolve newly registered sync root")
                }
            }
            Ok(())
        }
        // Lost a race or registered by an earlier run: that is fine as long as it is there.
        Err(e) => {
            tracing::warn!(hresult = ?e.code(), "WinRT sync root registration returned an error");
            match find_sync_root_path(&id) {
                Ok(Some(path)) if same_path(&path, root) => Ok(()),
                _ => Err(e.into()),
            }
        }
    }
}

fn new_info(
    root: &str,
    id: &str,
    display_name: &str,
    version: &str,
    icon: &str,
) -> Result<StorageProviderSyncRootInfo> {
    let folder =
        block_on(StorageFolder::GetFolderFromPathAsync(&HSTRING::from(root))?.into_future())?;
    let info = StorageProviderSyncRootInfo::new()?;
    info.SetId(&HSTRING::from(id))?;
    info.SetPath(&folder)?;
    info.SetDisplayNameResource(&HSTRING::from(display_name))?;
    info.SetIconResource(&HSTRING::from(icon))?;
    info.SetVersion(&HSTRING::from(version))?;
    info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full)?;
    info.SetHydrationPolicyModifier(
        StorageProviderHydrationPolicyModifier::AutoDehydrationAllowed,
    )?;
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::Full)?;
    info.SetInSyncPolicy(StorageProviderInSyncPolicy(
        StorageProviderInSyncPolicy::FileLastWriteTime.0
            | StorageProviderInSyncPolicy::DirectoryLastWriteTime.0,
    ))?;
    info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
    info.SetShowSiblingsAsGroup(false)?;
    info.SetAllowPinning(true)?;
    info.SetProtectionMode(StorageProviderProtectionMode::Unknown)?;
    info.SetProviderId(GUID::try_from(PROVIDER_ID).map_err(|e| anyhow!("PROVIDER_ID: {e}"))?)?;
    let context = CryptographicBuffer::CreateFromByteArray(id.as_bytes())?;
    info.SetContext(&context)?;
    set_definitions(&info)?;
    Ok(info)
}

/// Unregister the sync root registered by `register` for this user (StorageProviderSyncRootManager::Unregister(id)).
pub fn unregister(user_id: &str) -> Result<()> {
    let user_id = user_id.to_string();
    on_mta_thread(move || {
        let id = sync_root_id(&user_id)?;
        StorageProviderSyncRootManager::Unregister(&HSTRING::from(&id))?;
        Ok(())
    })
}

fn sync_root_id(user_id: &str) -> Result<String> {
    let sid = current_user_sid()?;
    Ok(format_sync_root_id(user_id, &sid))
}

fn format_sync_root_id(user_id: &str, sid: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(user_id.as_bytes());
    format!("synka!{encoded}!{sid}!User")
}

/// The registered sync root with this id, if any.
fn find_sync_root(id: &str) -> Result<Option<StorageProviderSyncRootInfo>> {
    let roots = StorageProviderSyncRootManager::GetCurrentSyncRoots()?;
    for i in 0..roots.Size()? {
        let info = roots.GetAt(i)?;
        if info.Id()? == id {
            return Ok(Some(info));
        }
    }
    Ok(None)
}

/// Other cloud providers' registered sync roots (OneDrive, Dropbox, Desktop Connector...), as
/// (path, display name). A sync root cannot sit inside another one.
pub fn other_sync_roots() -> Result<Vec<(String, String)>> {
    on_mta_thread(|| {
        let roots = StorageProviderSyncRootManager::GetCurrentSyncRoots()?;
        let mut out = Vec::new();
        for i in 0..roots.Size()? {
            let info = roots.GetAt(i)?;
            if info.Id()?.to_string().starts_with("synka!") {
                continue;
            }
            let Ok(path) = info.Path().and_then(|f| f.Path()) else {
                continue;
            };
            let name = info
                .DisplayNameResource()
                .map(|n| n.to_string())
                .unwrap_or_default();
            out.push((path.to_string(), name));
        }
        Ok(out)
    })
}

/// Path of the registered sync root with this id, if any.
fn find_sync_root_path(id: &str) -> Result<Option<String>> {
    match find_sync_root(id)? {
        Some(info) => Ok(Some(info.Path()?.Path()?.to_string())),
        None => Ok(None),
    }
}

fn stored_definitions(info: &StorageProviderSyncRootInfo) -> Result<Vec<(i32, String)>> {
    let defs = info.StorageProviderItemPropertyDefinitions()?;
    let mut out = Vec::new();
    for i in 0..defs.Size()? {
        let d = defs.GetAt(i)?;
        out.push((d.Id()?, d.DisplayNameResource()?.to_string()));
    }
    Ok(out)
}

/// Replace the info's custom state definitions with `shell::PROPERTY_DEFINITIONS`.
fn set_definitions(info: &StorageProviderSyncRootInfo) -> Result<()> {
    let defs = info.StorageProviderItemPropertyDefinitions()?;
    defs.Clear()?;
    for (id, name) in crate::shell::PROPERTY_DEFINITIONS {
        let d = StorageProviderItemPropertyDefinition::new()?;
        d.SetId(id)?;
        d.SetDisplayNameResource(&HSTRING::from(name))?;
        defs.Append(&d)?;
    }
    Ok(())
}

/// Same ids with the same names, in any order.
fn same_definitions(stored: &[(i32, String)], wanted: &[(i32, &str)]) -> bool {
    stored.len() == wanted.len()
        && wanted
            .iter()
            .all(|(id, name)| stored.iter().any(|(sid, sname)| sid == id && sname == name))
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// Owned kernel handle closed on drop.
struct SafeHandle(HANDLE);

impl Drop for SafeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// SID string of the current process user, e.g. "S-1-12-1-...".
pub(crate) fn current_user_sid() -> Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("OpenProcessToken")?;
        let token = SafeHandle(token);

        let mut needed = 0u32;
        // Expected to fail with ERROR_INSUFFICIENT_BUFFER; it only reports the size.
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            bail!("GetTokenInformation(TokenUser) reported no size");
        }
        // u64 backing store keeps the TOKEN_USER pointer alignment.
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            needed,
            &mut needed,
        )
        .context("GetTokenInformation(TokenUser)")?;

        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_str = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid_str).context("ConvertSidToStringSidW")?;
        let sid = sid_str.to_string().unwrap_or_default();
        let _ = LocalFree(Some(HLOCAL(sid_str.0.cast::<c_void>())));
        if sid.is_empty() {
            bail!("empty SID string");
        }
        Ok(sid)
    }
}

/// Run a WinRT call on a fresh MTA thread. WinRT async results are awaited with `block_on`,
/// which must not run on an STA thread that has to pump messages.
fn on_mta_thread<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    // Keep the process MTA alive for the whole process lifetime. Without this the MTA is torn
    // down when this short-lived worker exits, and windows-rs' cached activation factories
    // then point into a dead apartment: the next WinRT call crashes with an access violation
    // (reproduced: failed Unregister on one worker, then GetCurrentSyncRoots on the next).
    static MTA: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    MTA.get_or_init(|| unsafe {
        let _ = windows::Win32::System::Com::CoIncrementMTAUsage();
    });
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        f()
    })
    .join()
    .map_err(|_| anyhow!("WinRT worker thread panicked"))?
}

/// Minimal blocking executor for a WinRT IAsyncOperation (no `futures` dependency).
fn block_on<F: Future>(fut: F) -> F::Output {
    struct Unparker(std::thread::Thread);
    impl Wake for Unparker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unparker(std::thread::current())));
    let mut cx = TaskContext::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

/// Live connection; disconnects (CfDisconnectSyncRoot) on drop.
pub struct Connection {
    pub connection_key: i64,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Stop routing first so requests arriving during teardown fail fast.
        set_event_sink(None);
        if let Err(e) = unsafe { CfDisconnectSyncRoot(CF_CONNECTION_KEY(self.connection_key)) } {
            tracing::warn!("CfDisconnectSyncRoot: {e}");
        }
    }
}

/// Connect callbacks and route events to `tx`.
pub fn connect(root: &Path, tx: UnboundedSender<CfEvent>) -> Result<Connection> {
    let wide_root: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let table = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(callbacks::on_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            Callback: Some(callbacks::on_cancel_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(callbacks::on_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
            Callback: Some(callbacks::on_cancel_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(callbacks::on_notify_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(callbacks::on_notify_rename),
        },
        // CF_CALLBACK_REGISTRATION_END
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    let flags =
        CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH | CF_CONNECT_FLAG_BLOCK_SELF_IMPLICIT_HYDRATION;

    set_event_sink(Some(tx));
    let key = unsafe { CfConnectSyncRoot(PCWSTR(wide_root.as_ptr()), table.as_ptr(), None, flags) };
    match key {
        Ok(k) => Ok(Connection {
            connection_key: k.0,
        }),
        Err(e) => {
            set_event_sink(None);
            Err(anyhow!(e).context(format!("CfConnectSyncRoot {}", root.display())))
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ProviderStatus {
    Idle,
    Syncing,
    ConnectivityLost,
    Error,
}

/// CfUpdateSyncProviderStatus.
pub fn set_provider_status(conn: &Connection, s: ProviderStatus) {
    let status: CF_SYNC_PROVIDER_STATUS = match s {
        ProviderStatus::Idle => CF_PROVIDER_STATUS_IDLE,
        ProviderStatus::Syncing => CF_PROVIDER_STATUS_SYNC_INCREMENTAL,
        ProviderStatus::ConnectivityLost => CF_PROVIDER_STATUS_CONNECTIVITY_LOST,
        ProviderStatus::Error => CF_PROVIDER_STATUS_ERROR,
    };
    if let Err(e) =
        unsafe { CfUpdateSyncProviderStatus(CF_CONNECTION_KEY(conn.connection_key), status) }
    {
        tracing::warn!("CfUpdateSyncProviderStatus: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cf::placeholder::{
        decode_identity, encode_identity, ms_to_filetime, PinState, PlaceholderSpec,
    };
    use crate::model::Kind;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn sync_root_id_has_synka_prefix() {
        let id = format_sync_root_id("ACC-user-42", "S-1-12-1-123-456-789-101");
        let components: Vec<_> = id.split('!').collect();
        assert_eq!(
            components,
            [
                "synka",
                "QUNDLXVzZXItNDI=",
                "S-1-12-1-123-456-789-101",
                "User"
            ]
        );
    }

    fn spec(name: &str, is_dir: bool, size: u64, identity: Vec<u8>) -> PlaceholderSpec {
        PlaceholderSpec {
            name: name.to_string(),
            is_dir,
            size,
            ctime_ms: 1_700_000_000_000,
            mtime_ms: 1_700_000_000_000,
            identity,
            in_sync: true,
            on_demand: true,
            read_only: false,
        }
    }

    #[test]
    fn identity_round_trip() {
        let blob = encode_identity(Kind::File, 1, "abc");
        assert_eq!(blob.len(), 6 + 3);
        assert_eq!(blob[0], 1);
        assert_eq!(
            decode_identity(&blob),
            Some((Kind::File, 1, "abc".to_string()))
        );

        let blob = encode_identity(Kind::ProjectRoot, 0x0102_0304, "co.x");
        assert_eq!(
            decode_identity(&blob),
            Some((Kind::ProjectRoot, 0x0102_0304, "co.x".to_string()))
        );
        assert_eq!(decode_identity(&[]), None);
        assert_eq!(
            decode_identity(&[2, 4, 0, 0, 0, 0]),
            None,
            "unknown version"
        );
        assert_eq!(
            decode_identity(&[1, 9, 0, 0, 0, 0, b'a']),
            None,
            "unknown kind"
        );
    }

    #[test]
    fn definitions_compare() {
        let wanted = crate::shell::PROPERTY_DEFINITIONS;
        let mut stored: Vec<(i32, String)> = wanted
            .iter()
            .rev()
            .map(|(i, n)| (*i, n.to_string()))
            .collect();
        assert!(same_definitions(&stored, &wanted), "order does not matter");
        stored[0].1 = "Lock Status".into();
        assert!(!same_definitions(&stored, &wanted), "renamed column");
        assert!(
            !same_definitions(&[], &wanted),
            "registered before the columns existed"
        );
        stored.truncate(2);
        assert!(!same_definitions(&stored, &wanted));
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(ms_to_filetime(0), 116_444_736_000_000_000);
        assert_eq!(ms_to_filetime(1_000), 116_444_736_010_000_000);
    }

    /// Full cfapi round trip against a throwaway sync root in %TEMP%. Only touches the
    /// "test" user id and `%TEMP%\synka-cftest`. Run with:
    /// `cargo test cf:: -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn integration_roundtrip() {
        let root = std::env::temp_dir().join("synka-cftest");
        expose_placeholders();
        let _ = unregister("test");
        let _ = std::fs::remove_dir_all(&root);

        register(&root, "test", "synka test", "0.0.0-test").expect("register");
        let (tx, mut rx) = mpsc::unbounded_channel::<CfEvent>();
        let conn = connect(&root, tx).expect("connect");

        let file_id = encode_identity(Kind::File, 1, "abc");
        let edited_id = encode_identity(Kind::File, 1, "edited");
        let grown_id = encode_identity(Kind::File, 1, "grown-from-empty");
        let dir_id = encode_identity(Kind::Folder, 1, "co.sub");
        create_placeholders_checked(
            &root,
            &[
                spec("hello.bin", false, 10_000, file_id.clone()),
                spec("edited.bin", false, 89, edited_id.clone()),
                spec("grown.bin", false, 0, grown_id.clone()),
                spec("sub", true, 0, dir_id.clone()),
            ],
        );

        let info = placeholder::placeholder_info(&root.join("hello.bin"))
            .expect("placeholder_info")
            .expect("hello.bin is a placeholder");
        assert_eq!(info.identity, file_id, "identity round-trips");
        assert!(info.in_sync);
        assert!(!info.hydrated, "fresh placeholder has no local data");
        assert_eq!(info.pin_state, PinState::Unspecified);
        let dir_info = placeholder::placeholder_info(&root.join("sub"))
            .expect("placeholder_info dir")
            .expect("sub is a placeholder");
        assert_eq!(dir_info.identity, dir_id);
        let plain = root.join("plain.txt");
        std::fs::write(&plain, b"x").unwrap();
        assert!(
            placeholder::placeholder_info(&plain).unwrap().is_none(),
            "plain file is not a placeholder"
        );

        // Answer engine events on a worker thread, like the real engine does.
        let worker = std::thread::spawn(move || {
            let mut fetched = Vec::new();
            let mut listed = 0usize;
            while let Some(ev) = rx.blocking_recv() {
                match ev {
                    CfEvent::FetchData(req) => {
                        let byte = if req.identity == file_id {
                            assert_eq!(req.file_size, 10_000);
                            b'x'
                        } else if req.identity == edited_id {
                            assert!(matches!(req.file_size, 89 | 130));
                            if req.file_size == 89 {
                                b'a'
                            } else {
                                b'b'
                            }
                        } else if req.identity == grown_id {
                            assert_eq!(req.file_size, 20);
                            b'g'
                        } else {
                            panic!("unexpected FETCH_DATA identity: {:?}", req.identity);
                        };
                        let data = vec![byte; req.file_size as usize];
                        callbacks::transfer_data(&req.keys, 0, &data).expect("transfer_data");
                        fetched.push(req.file_size);
                    }
                    CfEvent::FetchPlaceholders(req) => {
                        callbacks::transfer_placeholders(&req.keys, &[])
                            .expect("transfer_placeholders");
                        listed += 1;
                    }
                    CfEvent::Delete(req) => {
                        callbacks::ack_delete(&req.keys, 0).expect("ack_delete")
                    }
                    CfEvent::Rename(req) => {
                        callbacks::ack_rename(&req.keys, 0).expect("ack_rename")
                    }
                    CfEvent::CancelFetchData { .. } | CfEvent::CancelFetchPlaceholders => {}
                }
            }
            (fetched, listed)
        });

        let entries = std::fs::read_dir(root.join("sub"))
            .expect("read_dir sub")
            .count();
        assert_eq!(entries, 0, "directory placeholder lists empty");

        // The provider connects with BLOCK_SELF_IMPLICIT_HYDRATION, so this process may not
        // trigger hydration itself. Read through another process, like Explorer/Revit would.
        let copy = std::env::temp_dir().join("synka-cftest-copy.bin");
        let _ = std::fs::remove_file(&copy);
        let status = std::process::Command::new("cmd")
            .args(["/c", "copy", "/b", "/y"])
            .arg(root.join("hello.bin"))
            .arg(&copy)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("spawn copy");
        assert!(status.success(), "copy via cmd failed: {status}");
        let content = std::fs::read(&copy).expect("read copy");
        let _ = std::fs::remove_file(&copy);
        assert_eq!(content.len(), 10_000);
        assert!(
            content.iter().all(|&b| b == b'x'),
            "content from FETCH_DATA"
        );

        let info = placeholder::placeholder_info(&root.join("hello.bin"))
            .unwrap()
            .unwrap();
        assert!(info.hydrated, "file is hydrated after the read");

        // Regression: an uploaded edit of a hydrated placeholder can change its length.
        // Marking only the identity/in-sync bit leaves the old cloud length behind, which
        // becomes visible after Free up space and breaks the next FETCH_DATA callback.
        let edited = root.join("edited.bin");
        let copy = std::env::temp_dir().join("synka-cftest-edited-copy.bin");
        let _ = std::fs::remove_file(&copy);
        let status = std::process::Command::new("cmd")
            .args(["/c", "copy", "/b", "/y"])
            .arg(&edited)
            .arg(&copy)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("spawn initial edited copy");
        assert!(status.success(), "initial edited copy failed: {status}");
        assert_eq!(std::fs::read(&copy).unwrap(), vec![b'a'; 89]);

        let stale_usn = placeholder::file_usn(&edited)
            .unwrap()
            .expect("placeholder USN");
        std::fs::write(&edited, vec![b'b'; 130]).expect("edit hydrated placeholder");
        assert_eq!(std::fs::metadata(&edited).unwrap().len(), 130);
        assert!(
            placeholder::set_uploaded_file_guarded(
                &edited,
                &encode_identity(Kind::File, 1, "edited"),
                130,
                Some(stale_usn)
            )
            .is_err(),
            "a write after the upload snapshot must not be marked in sync"
        );
        assert!(
            !placeholder::placeholder_info(&edited)
                .unwrap()
                .unwrap()
                .in_sync
        );
        let uploaded_usn = placeholder::file_usn(&edited)
            .unwrap()
            .expect("edited placeholder USN");
        placeholder::set_uploaded_file_guarded(
            &edited,
            &encode_identity(Kind::File, 1, "edited"),
            130,
            Some(uploaded_usn),
        )
        .expect("mark uploaded file in sync with new size");
        assert!(
            placeholder::placeholder_info(&edited)
                .unwrap()
                .unwrap()
                .in_sync
        );
        placeholder::dehydrate(&edited).expect("Free up space");
        assert_eq!(
            std::fs::metadata(&edited).unwrap().len(),
            130,
            "new cloud length survives dehydration"
        );
        assert!(
            !placeholder::placeholder_info(&edited)
                .unwrap()
                .unwrap()
                .hydrated
        );
        let status = std::process::Command::new("cmd")
            .args(["/c", "copy", "/b", "/y"])
            .arg(&edited)
            .arg(&copy)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("spawn rehydrated copy");
        assert!(status.success(), "rehydrated copy failed: {status}");
        assert_eq!(std::fs::read(&copy).unwrap(), vec![b'b'; 130]);
        let _ = std::fs::remove_file(&copy);

        // A zero-byte cloud placeholder can grow on the local disk. After upload, freeing
        // space must invalidate that newly added range so the next read asks FETCH_DATA.
        let grown = root.join("grown.bin");
        let grown_copy = std::env::temp_dir().join("synka-cftest-grown-copy.bin");
        std::fs::write(&grown, vec![b'g'; 20]).expect("write zero-byte placeholder");
        assert_eq!(std::fs::read(&grown).unwrap(), vec![b'g'; 20]);
        let grown_usn = placeholder::file_usn(&grown)
            .unwrap()
            .expect("grown placeholder USN");
        placeholder::set_uploaded_file_guarded(
            &grown,
            &encode_identity(Kind::File, 1, "grown-from-empty"),
            20,
            Some(grown_usn),
        )
        .expect("mark grown placeholder in sync");
        let grown_info = placeholder::placeholder_info(&grown).unwrap().unwrap();
        assert!(grown_info.in_sync);
        assert_eq!(
            grown_info.modified_size, 0,
            "uploaded bytes must not remain a modified local range"
        );
        placeholder::dehydrate(&grown).expect("Free up space on grown placeholder");
        assert_eq!(std::fs::metadata(&grown).unwrap().len(), 20);
        let status = std::process::Command::new("cmd")
            .args(["/c", "copy", "/b", "/y"])
            .arg(&grown)
            .arg(&grown_copy)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("spawn grown placeholder copy");
        assert!(status.success(), "grown placeholder copy failed: {status}");
        assert_eq!(
            std::fs::read(&grown_copy).unwrap(),
            vec![b'g'; 20],
            "grown cloud bytes"
        );
        let _ = std::fs::remove_file(&grown_copy);

        set_provider_status(&conn, ProviderStatus::Idle);
        drop(conn);
        let (fetched, listed) = worker.join().expect("worker");
        assert_eq!(fetched, vec![10_000, 89, 130, 20]);
        assert_eq!(listed, 1);

        unregister("test").expect("unregister");
        std::thread::sleep(Duration::from_millis(200));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn create_placeholders_checked(root: &Path, specs: &[PlaceholderSpec]) {
        placeholder::create_placeholders(root, specs).expect("create_placeholders");
    }
}

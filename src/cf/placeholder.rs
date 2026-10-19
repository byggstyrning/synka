//! Placeholder helpers (CfCreatePlaceholders, CfUpdatePlaceholder, CfConvertToPlaceholder,
//! CfGetPlaceholderInfo, CfHydratePlaceholder,
//! CfDehydratePlaceholder) and the identity blob.
//!
//! Identity blob: [0]=1 (format version), [1]=Kind as u8, [2..6]=pid as u32 LE,
//! [6..]=item id UTF-8 bytes. Max 4 KiB by API; ours ~30 B.
//!
//! Open handles for placeholder ops with CfOpenFileWithOplock(path, CF_OPEN_FILE_FLAG_WRITE_ACCESS
//! or NONE) or CreateFileW with FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
//! (needed for directories). Never read file DATA in this module.
//!
//! Choice: operations that discard local file data use an exclusive oplock (fails while the
//! file is open elsewhere); other operations use a plain CreateFileW handle with share-all.

use crate::model::{Kind, Pid};
use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_FUNCTION, ERROR_JOURNAL_NOT_ACTIVE,
    ERROR_NOT_SUPPORTED, HANDLE,
};
use windows::Win32::Storage::CloudFilters::{
    CfCloseHandle, CfConvertToPlaceholder, CfCreatePlaceholders, CfDehydratePlaceholder,
    CfGetPlaceholderInfo, CfGetPlaceholderStateFromAttributeTag,
    CfGetWin32HandleFromProtectedHandle, CfHydratePlaceholder, CfOpenFileWithOplock,
    CfReferenceProtectedHandle, CfReleaseProtectedHandle, CfSetInSyncState, CfSetPinState,
    CfUpdatePlaceholder, CF_CONVERT_FLAG_MARK_IN_SYNC, CF_CONVERT_FLAG_NONE, CF_CREATE_FLAG_NONE,
    CF_DEHYDRATE_FLAG_NONE, CF_FILE_RANGE, CF_FS_METADATA, CF_HYDRATE_FLAG_NONE,
    CF_IN_SYNC_STATE_IN_SYNC, CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_OPEN_FILE_FLAGS,
    CF_OPEN_FILE_FLAG_EXCLUSIVE, CF_OPEN_FILE_FLAG_WRITE_ACCESS, CF_PIN_STATE_EXCLUDED,
    CF_PIN_STATE_INHERIT, CF_PIN_STATE_PINNED, CF_PIN_STATE_UNPINNED, CF_PIN_STATE_UNSPECIFIED,
    CF_PLACEHOLDER_CREATE_FLAGS, CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
    CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_INFO,
    CF_PLACEHOLDER_INFO_STANDARD, CF_PLACEHOLDER_STANDARD_INFO, CF_PLACEHOLDER_STATE_INVALID,
    CF_PLACEHOLDER_STATE_NO_STATES, CF_SET_IN_SYNC_FLAG_NONE, CF_SET_PIN_FLAG_NONE,
    CF_UPDATE_FLAGS, CF_UPDATE_FLAG_CLEAR_IN_SYNC, CF_UPDATE_FLAG_DEHYDRATE,
    CF_UPDATE_FLAG_MARK_IN_SYNC, CF_UPDATE_FLAG_VERIFY_IN_SYNC,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, GetFileInformationByHandleEx, GetFileSizeEx,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_READ_FILE_USN_DATA, USN_RECORD_V2, USN_RECORD_V3};
use windows::Win32::System::IO::DeviceIoControl;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE: data not local.
const ATTR_NOT_HYDRATED: u32 = 0x0040_0000 | 0x0000_1000;
/// Generous buffer for CF_PLACEHOLDER_STANDARD_INFO: header plus identity (max 4 KiB).
const PLACEHOLDER_INFO_BUF: usize = 4096 + 1024;
/// Milliseconds between 1601-01-01 and 1970-01-01.
const UNIX_TO_FILETIME_MS: i64 = 11_644_473_600_000;

#[derive(Debug, Clone)]
pub struct PlaceholderSpec {
    /// File or directory name (single path component).
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub ctime_ms: i64,
    pub mtime_ms: i64,
    pub identity: Vec<u8>,
    /// CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.
    pub in_sync: bool,
    /// Directories only: when false, CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION
    /// is set (children already created / none expected).
    pub on_demand: bool,
    /// Set FILE_ATTRIBUTE_READONLY (e.g. locked by someone else / no edit permission).
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    Unspecified,
    Pinned,
    Unpinned,
    Excluded,
    Inherit,
}

#[derive(Debug, Clone)]
pub struct PlaceholderInfo {
    pub identity: Vec<u8>,
    pub in_sync: bool,
    pub pin_state: PinState,
    /// Whole file present locally (no RECALL_ON_DATA_ACCESS / OFFLINE attribute).
    pub hydrated: bool,
    /// Local byte ranges that Cloud Files tracks as present on disk.
    pub on_disk_size: u64,
    /// Local edits that Cloud Files has not promoted to validated cloud data.
    pub modified_size: u64,
}

/// The file changed after its bytes were captured for upload; retry the upload.
#[derive(Debug, thiserror::Error)]
#[error("file changed after the upload snapshot")]
pub struct UsnMismatch;

/// CFAPI changed only placeholder metadata after the uploaded snapshot was
/// verified. The same cloud version can be finalized again using this USN;
/// the caller must also retain its original file size/write-time guard.
#[derive(Debug, thiserror::Error)]
#[error("retry local finalization with file USN {usn}")]
pub struct FinalizationRetryUsn {
    pub usn: i64,
}

pub fn encode_identity(kind: Kind, pid: Pid, id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + id.len());
    out.push(1);
    out.push(kind as u8);
    out.extend_from_slice(&(pid as u32).to_le_bytes());
    out.extend_from_slice(id.as_bytes());
    out
}

pub fn decode_identity(blob: &[u8]) -> Option<(Kind, Pid, String)> {
    if blob.len() < 6 || blob[0] != 1 {
        return None;
    }
    let kind = Kind::from_u8(blob[1])?;
    let pid = u32::from_le_bytes([blob[2], blob[3], blob[4], blob[5]]) as Pid;
    let id = std::str::from_utf8(&blob[6..]).ok()?.to_string();
    Some((kind, pid, id))
}

/// FILETIME conversions (100 ns since 1601) <-> unix ms.
pub fn ms_to_filetime(ms: i64) -> i64 {
    (ms + UNIX_TO_FILETIME_MS) * 10_000
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(once(0)).collect()
}

/// Owned handle from either CreateFileW (plain) or CfOpenFileWithOplock (protected).
enum Handle {
    Plain(HANDLE),
    Oplock(HANDLE),
}

impl Handle {
    fn win32(&self) -> HANDLE {
        match self {
            Handle::Plain(h) => *h,
            Handle::Oplock(p) => unsafe { CfGetWin32HandleFromProtectedHandle(*p) },
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            match self {
                Handle::Plain(h) => {
                    let _ = CloseHandle(*h);
                }
                Handle::Oplock(p) => {
                    CfReleaseProtectedHandle(*p);
                    CfCloseHandle(*p);
                }
            }
        }
    }
}

/// Plain handle that refers to the placeholder itself (never follows cloud reparse data).
fn open_plain(path: &Path, access: u32) -> Result<Handle> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .with_context(|| format!("open {}", path.display()))?;
    Ok(Handle::Plain(h))
}

/// A normal reparse-point handle that excludes concurrent writes and renames.
/// CfSetInSyncState on a protected oplock handle fails for modified placeholders
/// on real sync roots, so finalization uses this handle for both CFAPI calls.
fn open_plain_excluding_writers(path: &Path, access: u32) -> Result<Handle> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            access,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .with_context(|| format!("open without writer sharing {}", path.display()))?;
    Ok(Handle::Plain(h))
}

fn open_oplock(path: &Path, flags: CF_OPEN_FILE_FLAGS) -> Result<Handle> {
    let w = wide(path);
    let h = unsafe { CfOpenFileWithOplock(PCWSTR(w.as_ptr()), flags) }
        .with_context(|| format!("CfOpenFileWithOplock {}", path.display()))?;
    // CfGetWin32HandleFromProtectedHandle requires a matching reference while the
    // Win32 handle is in use. These handles live only for the synchronous CFAPI call.
    if !unsafe { CfReferenceProtectedHandle(h) } {
        unsafe { CfCloseHandle(h) };
        bail!("CfReferenceProtectedHandle {}", path.display());
    }
    Ok(Handle::Oplock(h))
}

fn fs_metadata(s: &PlaceholderSpec) -> CF_FS_METADATA {
    let attrs = if s.is_dir {
        FILE_ATTRIBUTE_DIRECTORY.0
            | if s.read_only {
                FILE_ATTRIBUTE_READONLY.0
            } else {
                0
            }
    } else if s.read_only {
        FILE_ATTRIBUTE_READONLY.0
    } else {
        FILE_ATTRIBUTE_NORMAL.0
    };
    let written = ms_to_filetime(s.mtime_ms);
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: ms_to_filetime(s.ctime_ms),
            LastAccessTime: written,
            LastWriteTime: written,
            ChangeTime: written,
            FileAttributes: attrs,
        },
        FileSize: if s.is_dir { 0 } else { s.size as i64 },
    }
}

fn create_flags(s: &PlaceholderSpec) -> CF_PLACEHOLDER_CREATE_FLAGS {
    let mut f: i32 = 0;
    if s.in_sync {
        f |= CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.0;
    }
    if s.is_dir && !s.on_demand {
        f |= CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION.0;
    }
    CF_PLACEHOLDER_CREATE_FLAGS(f)
}

/// CF_PLACEHOLDER_CREATE_INFO array with the backing name and identity buffers kept alive.
pub(crate) struct CreateBatch {
    _names: Vec<Vec<u16>>,
    _identities: Vec<Vec<u8>>,
    pub infos: Vec<CF_PLACEHOLDER_CREATE_INFO>,
}

pub(crate) fn create_batch(specs: &[PlaceholderSpec]) -> CreateBatch {
    let names: Vec<Vec<u16>> = specs
        .iter()
        .map(|s| s.name.encode_utf16().chain(once(0)).collect())
        .collect();
    let identities: Vec<Vec<u8>> = specs.iter().map(|s| s.identity.clone()).collect();
    let infos = specs
        .iter()
        .enumerate()
        .map(|(i, s)| CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(names[i].as_ptr()),
            FsMetadata: fs_metadata(s),
            FileIdentity: identities[i].as_ptr() as *const c_void,
            FileIdentityLength: identities[i].len() as u32,
            Flags: create_flags(s),
            Result: HRESULT(0),
            CreateUsn: 0,
        })
        .collect();
    CreateBatch {
        _names: names,
        _identities: identities,
        infos,
    }
}

/// CfCreatePlaceholders under `parent_dir`. Already-existing names are not an error.
pub fn create_placeholders(parent_dir: &Path, specs: &[PlaceholderSpec]) -> Result<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let base = wide(parent_dir);
    let mut batch = create_batch(specs);
    let mut processed = 0u32;
    unsafe {
        CfCreatePlaceholders(
            PCWSTR(base.as_ptr()),
            &mut batch.infos,
            CF_CREATE_FLAG_NONE,
            Some(&mut processed),
        )
    }
    .with_context(|| format!("CfCreatePlaceholders {}", parent_dir.display()))?;

    let already = HRESULT::from_win32(ERROR_ALREADY_EXISTS.0);
    for (spec, created) in specs.iter().zip(batch.infos.iter()) {
        if created.Result.0 != 0 && created.Result != already {
            bail!(
                "placeholder {} in {}: HRESULT 0x{:08X}",
                spec.name,
                parent_dir.display(),
                created.Result.0 as u32
            );
        }
    }
    Ok(())
}

/// CfUpdatePlaceholder: new metadata + identity. `dehydrate` = also drop local data
/// (CF_UPDATE_FLAG_DEHYDRATE, used when the remote version changed and the file is clean).
/// Marks in-sync (CF_UPDATE_FLAG_MARK_IN_SYNC). Fails if the file is open/dirty: return Err.
pub fn update_placeholder(path: &Path, spec: &PlaceholderSpec, dehydrate: bool) -> Result<()> {
    let handle = if spec.is_dir {
        open_plain(path, GENERIC_READ | GENERIC_WRITE)?
    } else {
        open_oplock(
            path,
            CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
        )?
    };
    let metadata = fs_metadata(spec);
    let mut flags = CF_UPDATE_FLAG_VERIFY_IN_SYNC.0 | CF_UPDATE_FLAG_MARK_IN_SYNC.0;
    let range = [CF_FILE_RANGE {
        StartingOffset: 0,
        Length: -1,
    }];
    let ranges: Option<&[CF_FILE_RANGE]> = if dehydrate {
        flags |= CF_UPDATE_FLAG_DEHYDRATE.0;
        Some(&range)
    } else {
        None
    };
    unsafe {
        CfUpdatePlaceholder(
            handle.win32(),
            Some(&metadata as *const CF_FS_METADATA),
            Some(spec.identity.as_ptr() as *const c_void),
            spec.identity.len() as u32,
            ranges,
            CF_UPDATE_FLAGS(flags),
            None,
            None,
        )
    }
    .with_context(|| format!("CfUpdatePlaceholder {}", path.display()))
}

/// Point an existing placeholder at another item (new identity blob), keeping its data and
/// metadata. `in_sync` marks it in sync (after an upload) or clears it (a local-only copy that
/// still has to be uploaded). Uses a share-all handle, so it works while the file is open.
pub fn set_identity(path: &Path, identity: &[u8], in_sync: bool) -> Result<()> {
    let handle = open_plain(path, GENERIC_READ | GENERIC_WRITE)?;
    let flags = if in_sync {
        CF_UPDATE_FLAG_MARK_IN_SYNC
    } else {
        CF_UPDATE_FLAG_CLEAR_IN_SYNC
    };
    unsafe {
        CfUpdatePlaceholder(
            handle.win32(),
            None,
            Some(identity.as_ptr() as *const c_void),
            identity.len() as u32,
            None,
            flags,
            None,
            None,
        )
    }
    .with_context(|| format!("CfUpdatePlaceholder (identity) {}", path.display()))
}

/// Read the last change-journal USN for a file without reading or hydrating its data.
/// The caller can capture this before uploading and pass it to
/// `set_uploaded_file_guarded` so a concurrent same-size, same-timestamp save cannot
/// be marked in sync. A zero USN cannot identify a stable uploaded snapshot.
pub fn file_usn(path: &Path) -> Result<Option<i64>> {
    let handle = open_plain(path, FILE_READ_ATTRIBUTES.0)?;
    file_usn_on_handle(handle.win32(), path)
}

fn file_usn_on_handle(handle: HANDLE, path: &Path) -> Result<Option<i64>> {
    // USN_RECORD_V2/V3 have variable-length names. 2 KiB comfortably holds the
    // fixed header and the maximum NTFS file-name component; u64 keeps it aligned.
    let mut buf = [0u64; 256];
    let mut returned = 0u32;
    let result = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_READ_FILE_USN_DATA,
            None,
            0,
            Some(buf.as_mut_ptr().cast()),
            std::mem::size_of_val(&buf) as u32,
            Some(&mut returned),
            None,
        )
    };
    match result {
        Ok(()) => {}
        Err(e)
            if [
                ERROR_INVALID_FUNCTION,
                ERROR_JOURNAL_NOT_ACTIVE,
                ERROR_NOT_SUPPORTED,
            ]
            .into_iter()
            .any(|code| e.code() == HRESULT::from_win32(code.0)) =>
        {
            tracing::debug!(
                "USN journal unavailable for {}; using file fingerprint checks",
                path.display()
            );
            return Ok(None);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("FSCTL_READ_FILE_USN_DATA {}", path.display()));
        }
    }
    if returned < 8 {
        bail!("short USN record for {}", path.display());
    }
    let ptr = buf.as_ptr().cast::<u8>();
    let record_len = unsafe { *(ptr.cast::<u32>()) } as usize;
    let major = unsafe { *(ptr.add(4).cast::<u16>()) };
    let usn = match major {
        2 if record_len >= std::mem::offset_of!(USN_RECORD_V2, FileName) => unsafe {
            (*(ptr.cast::<USN_RECORD_V2>())).Usn
        },
        3 if record_len >= std::mem::offset_of!(USN_RECORD_V3, FileName) => unsafe {
            (*(ptr.cast::<USN_RECORD_V3>())).Usn
        },
        _ => bail!(
            "unsupported USN record version {} for {}",
            major,
            path.display()
        ),
    };
    if record_len > returned as usize {
        bail!("incomplete USN record for {}", path.display());
    }
    Ok((usn != 0).then_some(usn))
}

fn file_size_on_handle(handle: HANDLE, path: &Path) -> Result<u64> {
    let mut size = 0i64;
    unsafe { GetFileSizeEx(handle, &mut size) }
        .with_context(|| format!("GetFileSizeEx {}", path.display()))?;
    u64::try_from(size).context("negative file size")
}

fn verify_expected_usn(handle: HANDLE, path: &Path, expected_usn: Option<i64>) -> Result<()> {
    let Some(expected) = expected_usn else {
        return Ok(());
    };
    match file_usn_on_handle(handle, path)? {
        Some(actual) if actual == expected => Ok(()),
        Some(_) => Err(UsnMismatch.into()),
        None => bail!("USN journal unavailable while verifying {}", path.display()),
    }
}

fn with_finalization_usn(
    handle: HANDLE,
    path: &Path,
    uploaded_size: u64,
    error: anyhow::Error,
) -> anyhow::Error {
    // This handle still excludes writers. CFAPI may have advanced the journal
    // while changing metadata, but no application could change the bytes.
    // A different logical size is unsafe to adopt as the uploaded snapshot.
    if file_size_on_handle(handle, path).is_ok_and(|size| size == uploaded_size) {
        if let Ok(Some(usn)) = file_usn_on_handle(handle, path) {
            return error.context(FinalizationRetryUsn { usn });
        }
    }
    error
}

fn rollback_uploaded_sync(
    handle: HANDLE,
    path: &Path,
    uploaded_size: u64,
    error: anyhow::Error,
) -> anyhow::Error {
    let error = match unsafe {
        CfSetInSyncState(
            handle,
            CF_IN_SYNC_STATE_NOT_IN_SYNC,
            CF_SET_IN_SYNC_FLAG_NONE,
            None,
        )
    } {
        Ok(()) => error,
        Err(rollback) => error.context(format!(
            "also could not clear in-sync state for {}: {rollback}",
            path.display()
        )),
    };
    with_finalization_usn(handle, path, uploaded_size, error)
}

/// Finalize an upload of an existing file placeholder without discarding its local data.
/// The file's current length may differ from the cloud length recorded when the placeholder
/// was created. If we only mark it in sync, dehydration can restore that old length and the
/// next FETCH_DATA will ask for the wrong number of bytes. `expected_usn` is captured before
/// the uploaded bytes were read and compared after acquiring a handle that excludes writers.
/// On some Windows builds, Cloud Files rejects a nonzero USN input for a locally modified
/// placeholder even when it matches. The held handle makes the subsequent unguarded CFAPI
/// calls safe from concurrent writes.
pub fn set_uploaded_file_guarded(
    path: &Path,
    identity: &[u8],
    size: u64,
    expected_usn: Option<i64>,
) -> Result<()> {
    let size = i64::try_from(size).context("uploaded file is too large for CF_FS_METADATA")?;
    let info = placeholder_info(path)?.context("uploaded file is no longer a placeholder")?;
    let metadata = CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: 0,
            LastAccessTime: 0,
            LastWriteTime: 0,
            ChangeTime: 0,
            FileAttributes: 0,
        },
        FileSize: size,
    };
    if info.modified_size > 0 && info.hydrated && info.on_disk_size == size as u64 {
        // The newly uploaded local ranges are complete and have reached ACC. Promote
        // them to validated cloud data before updating the cloud file length. A plain
        // reparse-point handle is required here: reverting a live placeholder through
        // a protected oplock fails with ERROR_REPARSE_TAG_MISMATCH on real sync roots.
        let handle = open_plain_excluding_writers(path, GENERIC_READ | GENERIC_WRITE)?;
        verify_expected_usn(handle.win32(), path, expected_usn)?;
        let set_sync = unsafe {
            CfSetInSyncState(
                handle.win32(),
                CF_IN_SYNC_STATE_IN_SYNC,
                CF_SET_IN_SYNC_FLAG_NONE,
                None,
            )
        };
        if let Err(e) = set_sync {
            let failure = anyhow::Error::new(e).context(format!(
                "CfSetInSyncState (uploaded file) {}",
                path.display()
            ));
            return Err(with_finalization_usn(
                handle.win32(),
                path,
                size as u64,
                failure,
            ));
        }
        let updated = unsafe {
            CfUpdatePlaceholder(
                handle.win32(),
                Some(&metadata as *const CF_FS_METADATA),
                Some(identity.as_ptr() as *const c_void),
                identity.len() as u32,
                None,
                CF_UPDATE_FLAG_MARK_IN_SYNC,
                None,
                None,
            )
        };
        if let Err(e) = updated {
            let failure = anyhow::Error::new(e).context(format!(
                "CfUpdatePlaceholder (uploaded file) {}",
                path.display()
            ));
            return Err(rollback_uploaded_sync(
                handle.win32(),
                path,
                size as u64,
                failure,
            ));
        }
        let after = match placeholder_info(path) {
            Ok(Some(after)) => after,
            Ok(None) => {
                let failure = anyhow::anyhow!("uploaded file did not remain a placeholder");
                return Err(rollback_uploaded_sync(
                    handle.win32(),
                    path,
                    size as u64,
                    failure,
                ));
            }
            Err(failure) => {
                return Err(rollback_uploaded_sync(
                    handle.win32(),
                    path,
                    size as u64,
                    failure.context("could not verify uploaded placeholder"),
                ));
            }
        };
        if !after.in_sync || after.modified_size != 0 || after.on_disk_size != size as u64 {
            let failure = anyhow::anyhow!(
                "uploaded placeholder still has {} modified bytes",
                after.modified_size
            );
            return Err(rollback_uploaded_sync(
                handle.win32(),
                path,
                size as u64,
                failure,
            ));
        }
        return Ok(());
    }
    let handle = open_plain_excluding_writers(path, GENERIC_READ | GENERIC_WRITE)?;
    verify_expected_usn(handle.win32(), path, expected_usn)?;
    let updated = unsafe {
        CfUpdatePlaceholder(
            handle.win32(),
            Some(&metadata as *const CF_FS_METADATA),
            Some(identity.as_ptr() as *const c_void),
            identity.len() as u32,
            None,
            CF_UPDATE_FLAG_MARK_IN_SYNC,
            None,
            None,
        )
    };
    updated.map_err(|e| {
        let failure = anyhow::Error::new(e).context(format!(
            "CfUpdatePlaceholder (uploaded file) {}",
            path.display()
        ));
        with_finalization_usn(handle.win32(), path, size as u64, failure)
    })
}

/// CfConvertToPlaceholder for a normal file/dir created locally (after upload),
/// with CF_CONVERT_FLAG_MARK_IN_SYNC when `mark_in_sync`.
pub fn convert_to_placeholder(path: &Path, identity: &[u8], mark_in_sync: bool) -> Result<()> {
    let handle = open_plain(path, GENERIC_READ | GENERIC_WRITE)?;
    let flags = if mark_in_sync {
        CF_CONVERT_FLAG_MARK_IN_SYNC
    } else {
        CF_CONVERT_FLAG_NONE
    };
    unsafe {
        CfConvertToPlaceholder(
            handle.win32(),
            Some(identity.as_ptr() as *const c_void),
            identity.len() as u32,
            flags,
            None,
            None,
        )
    }
    .with_context(|| format!("CfConvertToPlaceholder {}", path.display()))
}

/// Convert a newly uploaded normal file only if it still has the USN captured before
/// the upload. The exclusive CFAPI oplock holds off writers through the USN check,
/// conversion, and in-sync transition. If an editor already has the file open or
/// breaks the oplock, this fails and the upload finalizer can retry safely.
pub fn convert_to_placeholder_guarded(
    path: &Path,
    identity: &[u8],
    expected_usn: i64,
) -> Result<()> {
    anyhow::ensure!(
        expected_usn != 0,
        "zero USN cannot guard placeholder conversion"
    );
    let handle = open_oplock(
        path,
        CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
    )?;
    let actual_usn = file_usn_on_handle(handle.win32(), path)?.context("file has no usable USN")?;
    if actual_usn != expected_usn {
        return Err(UsnMismatch.into());
    }
    let uploaded_size = file_size_on_handle(handle.win32(), path)?;
    unsafe {
        CfConvertToPlaceholder(
            handle.win32(),
            Some(identity.as_ptr() as *const c_void),
            identity.len() as u32,
            CF_CONVERT_FLAG_NONE,
            None,
            None,
        )
    }
    .with_context(|| format!("CfConvertToPlaceholder (guarded) {}", path.display()))?;
    let set_sync = unsafe {
        CfSetInSyncState(
            handle.win32(),
            CF_IN_SYNC_STATE_IN_SYNC,
            CF_SET_IN_SYNC_FLAG_NONE,
            None,
        )
    };
    set_sync.map_err(|e| {
        let failure = anyhow::Error::new(e).context(format!(
            "CfSetInSyncState (converted file) {}",
            path.display()
        ));
        with_finalization_usn(handle.win32(), path, uploaded_size, failure)
    })
}

/// Ok(None) if `path` is not a placeholder (a normal file).
pub fn placeholder_info(path: &Path) -> Result<Option<PlaceholderInfo>> {
    let handle = open_plain(
        path,
        windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES.0,
    )?;
    // Normal files do not need a Cloud Files query. On volumes without an active
    // provider CfGetPlaceholderInfo can fail with ERROR_INVALID_FUNCTION instead
    // of ERROR_NOT_A_CLOUD_FILE. Classify the same open handle, without following
    // a replacement path or hiding errors from actual cloud placeholders.
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle.win32(),
            FileAttributeTagInfo,
            &mut attributes as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .with_context(|| format!("FileAttributeTagInfo {}", path.display()))?;
    let state = unsafe {
        CfGetPlaceholderStateFromAttributeTag(attributes.FileAttributes, attributes.ReparseTag)
    };
    if state == CF_PLACEHOLDER_STATE_NO_STATES {
        return Ok(None);
    }
    anyhow::ensure!(
        state != CF_PLACEHOLDER_STATE_INVALID,
        "invalid placeholder attributes {}",
        path.display()
    );
    // u64 backing store so the struct is aligned.
    let mut buf = vec![0u64; PLACEHOLDER_INFO_BUF / 8];
    let buf_len = (buf.len() * 8) as u32;
    let mut returned = 0u32;
    unsafe {
        CfGetPlaceholderInfo(
            handle.win32(),
            CF_PLACEHOLDER_INFO_STANDARD,
            buf.as_mut_ptr() as *mut c_void,
            buf_len,
            Some(&mut returned),
        )
    }
    .with_context(|| format!("CfGetPlaceholderInfo {}", path.display()))?;

    let base = buf.as_ptr() as *const u8;
    let (identity, pin_state, in_sync, on_disk_size, modified_size) = unsafe {
        let info = &*(base as *const CF_PLACEHOLDER_STANDARD_INFO);
        let id_len = info.FileIdentityLength as usize;
        let id_off = std::mem::offset_of!(CF_PLACEHOLDER_STANDARD_INFO, FileIdentity);
        if id_off + id_len > returned as usize {
            bail!(
                "CfGetPlaceholderInfo {}: identity exceeds returned size",
                path.display()
            );
        }
        let identity = std::slice::from_raw_parts(base.add(id_off), id_len).to_vec();
        let pin_state = match info.PinState {
            CF_PIN_STATE_UNSPECIFIED => PinState::Unspecified,
            CF_PIN_STATE_PINNED => PinState::Pinned,
            CF_PIN_STATE_UNPINNED => PinState::Unpinned,
            CF_PIN_STATE_EXCLUDED => PinState::Excluded,
            CF_PIN_STATE_INHERIT => PinState::Inherit,
            _ => PinState::Unspecified,
        };
        let in_sync = info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC;
        (
            identity,
            pin_state,
            in_sync,
            info.OnDiskDataSize.max(0) as u64,
            info.ModifiedDataSize.max(0) as u64,
        )
    };

    let attrs = std::fs::metadata(path).with_context(|| format!("metadata {}", path.display()))?;
    use std::os::windows::fs::MetadataExt;
    let hydrated = attrs.file_attributes() & ATTR_NOT_HYDRATED == 0;

    Ok(Some(PlaceholderInfo {
        identity,
        in_sync,
        pin_state,
        hydrated,
        on_disk_size,
        modified_size,
    }))
}

/// CfSetPinState(PINNED): "Always keep on this device" (new children inherit it).
pub fn pin(path: &Path) -> Result<()> {
    let handle = open_plain(path, GENERIC_READ)?;
    unsafe {
        CfSetPinState(
            handle.win32(),
            CF_PIN_STATE_PINNED,
            CF_SET_PIN_FLAG_NONE,
            None,
        )
    }
    .with_context(|| format!("CfSetPinState {}", path.display()))
}

/// CfSetPinState(UNPINNED): let the provider release local data for this file.
pub fn unpin(path: &Path) -> Result<()> {
    let handle = open_plain(path, GENERIC_READ)?;
    unsafe {
        CfSetPinState(
            handle.win32(),
            CF_PIN_STATE_UNPINNED,
            CF_SET_PIN_FLAG_NONE,
            None,
        )
    }
    .with_context(|| format!("CfSetPinState (unpin) {}", path.display()))
}

/// CfSetPinState(INHERIT): follow the parent's pin (a new entry in a pinned folder).
pub fn inherit_pin(path: &Path) -> Result<()> {
    let handle = open_plain(path, GENERIC_READ | GENERIC_WRITE)?;
    unsafe {
        CfSetPinState(
            handle.win32(),
            CF_PIN_STATE_INHERIT,
            CF_SET_PIN_FLAG_NONE,
            None,
        )
    }
    .with_context(|| format!("CfSetPinState {}", path.display()))
}

/// The directory is "Always keep on this device": pinned itself, or inheriting from a pinned
/// parent (looked up at most 64 levels, stopping at the first non-placeholder).
pub fn dir_pinned(dir: &Path) -> bool {
    let mut cur = Some(dir);
    for _ in 0..64 {
        let Some(d) = cur else { return false };
        match placeholder_info(d) {
            Ok(Some(i)) if i.pin_state == PinState::Pinned => return true,
            Ok(Some(i)) if i.pin_state == PinState::Inherit => cur = d.parent(),
            _ => return false,
        }
    }
    false
}

/// CfHydratePlaceholder over the whole file (blocks until done; call from spawn_blocking).
pub fn hydrate(path: &Path) -> Result<()> {
    let handle = open_plain(path, GENERIC_READ)?;
    unsafe { CfHydratePlaceholder(handle.win32(), 0, -1, CF_HYDRATE_FLAG_NONE, None) }
        .with_context(|| format!("CfHydratePlaceholder {}", path.display()))
}

/// CfDehydratePlaceholder over the whole file.
pub fn dehydrate(path: &Path) -> Result<()> {
    // Dehydration discards local ranges; the Cloud Files API requires an exclusive handle
    // so another writer cannot race that removal.
    let handle = open_oplock(
        path,
        CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
    )?;
    unsafe { CfDehydratePlaceholder(handle.win32(), 0, -1, CF_DEHYDRATE_FLAG_NONE, None) }
        .with_context(|| format!("CfDehydratePlaceholder {}", path.display()))
}

/// Set or clear FILE_ATTRIBUTE_READONLY without touching data.
pub fn set_read_only(path: &Path, read_only: bool) -> Result<()> {
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("metadata {}", path.display()))?
        .permissions();
    if perms.readonly() != read_only {
        perms.set_readonly(read_only);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("set readonly {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_files_and_directories_need_no_cloud_provider() {
        let directory = std::env::temp_dir().join(format!(
            "synka-plain-info-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            std::fs::create_dir_all(&directory)?;
            let file = directory.join("local.bin");
            std::fs::write(&file, b"unsynced bytes")?;
            assert!(placeholder_info(&file)?.is_none());
            assert!(placeholder_info(&directory)?.is_none());
            assert_eq!(std::fs::read(&file)?, b"unsynced bytes");
            std::fs::remove_file(&file)?;
            assert!(
                placeholder_info(&file).is_err(),
                "missing files remain errors"
            );
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(directory);
        result.expect("ordinary files must not require a registered cloud provider");
    }

    #[test]
    fn identity_encoding_layout() {
        let blob = encode_identity(Kind::Folder, 7, "co.z");
        assert_eq!(blob, vec![1, 3, 7, 0, 0, 0, b'c', b'o', b'.', b'z']);
        assert_eq!(
            decode_identity(&blob),
            Some((Kind::Folder, 7, "co.z".to_string()))
        );
    }

    #[test]
    fn filetime_from_unix_ms() {
        assert_eq!(ms_to_filetime(0), 116_444_736_000_000_000);
        assert_eq!(ms_to_filetime(1_726_000_123_456) % 10_000, 0);
    }

    #[test]
    fn create_batch_keeps_buffers() {
        let specs = vec![PlaceholderSpec {
            name: "a.dwg".into(),
            is_dir: false,
            size: 42,
            ctime_ms: 0,
            mtime_ms: 1_000,
            identity: vec![1, 4, 1, 0, 0, 0, b'x'],
            in_sync: true,
            on_demand: true,
            read_only: true,
        }];
        let batch = create_batch(&specs);
        let info = &batch.infos[0];
        assert_eq!(info.FileIdentityLength, 7);
        assert_eq!(info.FsMetadata.FileSize, 42);
        assert_eq!(
            info.FsMetadata.BasicInfo.FileAttributes,
            FILE_ATTRIBUTE_READONLY.0
        );
        assert_ne!(info.Flags.0 & CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.0, 0);
    }

    #[test]
    fn file_usn_changes_after_same_length_write() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "synka-usn-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"before")?;
            let first = file_usn(&path)?.context("test file has no usable USN")?;
            let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
            file.write_all(b"after!")?;
            file.sync_all()?;
            drop(file);
            let second = file_usn(&path)?.context("test file lost its USN")?;
            anyhow::ensure!(
                first != second,
                "same-length write did not advance file USN"
            );
            Ok(())
        })();
        let _ = std::fs::remove_file(path);
        result.expect("file USN should detect a same-length save");
    }

    #[test]
    fn plain_finalization_handle_excludes_writers() {
        let path = std::env::temp_dir().join(format!(
            "synka-writer-guard-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"uploaded bytes")?;
            let expected = file_usn(&path)?.context("test file has no usable USN")?;
            let handle = open_plain_excluding_writers(&path, GENERIC_READ | GENERIC_WRITE)?;
            verify_expected_usn(handle.win32(), &path, Some(expected))?;
            let stale = verify_expected_usn(handle.win32(), &path, Some(expected - 1))
                .expect_err("stale upload USN must be rejected");
            anyhow::ensure!(stale.is::<UsnMismatch>(), "wrong error: {stale:#}");
            let writer = std::fs::OpenOptions::new().write(true).open(&path);
            anyhow::ensure!(
                writer.is_err(),
                "concurrent writer opened through guarded handle"
            );
            let retry = with_finalization_usn(
                handle.win32(),
                &path,
                b"uploaded bytes".len() as u64,
                anyhow::anyhow!("simulated CFAPI metadata failure"),
            );
            anyhow::ensure!(
                retry
                    .downcast_ref::<FinalizationRetryUsn>()
                    .is_some_and(|guard| guard.usn == expected),
                "same-size metadata retry lost its journal guard"
            );
            anyhow::ensure!(
                format!("{retry:#}").contains("simulated CFAPI metadata failure"),
                "CFAPI failure was lost from the error chain"
            );
            let wrong_size = with_finalization_usn(
                handle.win32(),
                &path,
                b"uploaded bytes".len() as u64 + 1,
                anyhow::anyhow!("simulated size change"),
            );
            anyhow::ensure!(
                wrong_size.downcast_ref::<FinalizationRetryUsn>().is_none(),
                "a changed file size must not reuse the uploaded cloud version"
            );
            anyhow::ensure!(std::fs::read(&path)? == b"uploaded bytes", "bytes changed");
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result.expect("finalization handle must preserve the uploaded snapshot");
    }

    #[test]
    fn protected_conversion_handle_preserves_retry_usn() {
        let path = std::env::temp_dir().join(format!(
            "synka-converted-usn-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"unchanged uploaded bytes")?;
            let handle = open_oplock(
                &path,
                CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
            )?;
            let journal_usn = file_usn_on_handle(handle.win32(), &path)?
                .context("test file has no usable USN")?;
            let retry = with_finalization_usn(
                handle.win32(),
                &path,
                b"unchanged uploaded bytes".len() as u64,
                anyhow::anyhow!("simulated post-conversion failure"),
            );
            anyhow::ensure!(
                retry
                    .downcast_ref::<FinalizationRetryUsn>()
                    .is_some_and(|guard| guard.usn == journal_usn),
                "protected conversion handle did not preserve retry USN"
            );
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result.expect("conversion handle must preserve a valid retry guard");
    }

    #[test]
    fn guarded_conversion_rejects_stale_usn_without_touching_bytes() {
        let path = std::env::temp_dir().join(format!(
            "synka-stale-usn-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"old!")?;
            let stale = file_usn(&path)?.context("test file has no USN")?;
            std::fs::write(&path, b"new!")?;
            let identity = encode_identity(Kind::File, 1, "stale-test");
            let error = convert_to_placeholder_guarded(&path, &identity, stale).unwrap_err();
            anyhow::ensure!(error.is::<UsnMismatch>(), "wrong error: {error:#}");
            anyhow::ensure!(std::fs::read(&path)? == b"new!", "file bytes changed");
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result.expect("stale USN must reject conversion");
    }

    #[test]
    #[ignore]
    fn existing_placeholder_exposes_usn_without_hydration() {
        let path = std::path::PathBuf::from(
            std::env::var_os("SYNKA_USN_PLACEHOLDER").expect("set SYNKA_USN_PLACEHOLDER"),
        );
        let before = placeholder_info(&path).unwrap().unwrap();
        let usn = file_usn(&path).unwrap().expect("placeholder USN");
        let after = placeholder_info(&path).unwrap().unwrap();
        assert_ne!(usn, 0);
        assert_eq!(
            before.hydrated, after.hydrated,
            "reading USN must not hydrate the file"
        );
    }
}

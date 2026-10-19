//! Completing callbacks with CfExecute (any thread). The `extern "system"` callback
//! functions themselves also live here (registered by `cf::connect`).

use super::*;
use crate::cf::placeholder::{create_batch, PlaceholderSpec};
use anyhow::Result;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use windows::core::PCWSTR;
use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::Storage::CloudFilters::{
    CfExecute, CfReportProviderProgress, CF_CALLBACK_INFO, CF_CALLBACK_PARAMETERS,
    CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE, CF_CONNECTION_KEY, CF_OPERATION_ACK_DELETE_FLAG_NONE,
    CF_OPERATION_ACK_RENAME_FLAG_NONE, CF_OPERATION_INFO, CF_OPERATION_PARAMETERS,
    CF_OPERATION_PARAMETERS_0_0, CF_OPERATION_PARAMETERS_0_4, CF_OPERATION_PARAMETERS_0_6,
    CF_OPERATION_PARAMETERS_0_7, CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
    CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
    CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE, CF_OPERATION_TYPE, CF_OPERATION_TYPE_ACK_DELETE,
    CF_OPERATION_TYPE_ACK_RENAME, CF_OPERATION_TYPE_TRANSFER_DATA,
    CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS,
};

/// Operation info common to every CfExecute call made from this module.
fn op_info(keys: &OpKeys, ty: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
    CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: ty,
        ConnectionKey: CF_CONNECTION_KEY(keys.connection_key),
        TransferKey: keys.transfer_key,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: keys.request_key,
    }
}

/// The C macro CF_SIZE_OF_OP_PARAM(field): offset of the union plus the size of that arm.
fn op_param_size<T>() -> u32 {
    (std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + std::mem::size_of::<T>()) as u32
}

fn execute(info: &CF_OPERATION_INFO, params: &mut CF_OPERATION_PARAMETERS) -> Result<()> {
    unsafe { CfExecute(info, params) }.map_err(|e| anyhow::anyhow!("CfExecute: {e}"))
}

/// Copy the keys and identity/path out of CF_CALLBACK_INFO. Path is volume-absolute
/// (VolumeDosName + NormalizedPath).
unsafe fn keys_of(info: &CF_CALLBACK_INFO) -> OpKeys {
    OpKeys {
        connection_key: info.ConnectionKey.0,
        transfer_key: info.TransferKey,
        request_key: info.RequestKey,
        file_id: info.FileId,
    }
}

unsafe fn wide_ptr(p: PCWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        p.to_string().unwrap_or_default()
    }
}

unsafe fn full_path(info: &CF_CALLBACK_INFO, rel: PCWSTR) -> PathBuf {
    let vol = wide_ptr(info.VolumeDosName);
    let rel = wide_ptr(rel);
    PathBuf::from(format!("{vol}{rel}"))
}

unsafe fn identity_of(info: &CF_CALLBACK_INFO) -> Vec<u8> {
    if info.FileIdentity.is_null() || info.FileIdentityLength == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(
            info.FileIdentity as *const u8,
            info.FileIdentityLength as usize,
        )
        .to_vec()
    }
}

/// TRANSFER_DATA for `[offset, offset+data.len())`. `offset` must be 4 KiB aligned and
/// `data.len()` a multiple of 4096 unless it ends at end-of-file.
pub fn transfer_data(keys: &OpKeys, offset: u64, data: &[u8]) -> Result<()> {
    let info = op_info(keys, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut params: CF_OPERATION_PARAMETERS = unsafe { std::mem::zeroed() };
    params.ParamSize = op_param_size::<CF_OPERATION_PARAMETERS_0_0>();
    params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: NTSTATUS(0),
        Buffer: data.as_ptr() as *const c_void,
        Offset: offset as i64,
        Length: data.len() as i64,
    };
    execute(&info, &mut params)
}

/// Fail the requested range so pending reads overlapping it complete with an error.
pub fn fail_data(keys: &OpKeys, offset: u64, length: u64, status: i32) {
    let info = op_info(keys, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut params: CF_OPERATION_PARAMETERS = unsafe { std::mem::zeroed() };
    params.ParamSize = op_param_size::<CF_OPERATION_PARAMETERS_0_0>();
    params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: NTSTATUS(status),
        Buffer: std::ptr::null(),
        Offset: offset as i64,
        Length: length as i64,
    };
    if let Err(e) = execute(&info, &mut params) {
        tracing::warn!("fail_data: {e}");
    }
}

/// CfReportProviderProgress (call at most a few times per second).
pub fn report_progress(keys: &OpKeys, total: u64, completed: u64) {
    let res = unsafe {
        CfReportProviderProgress(
            CF_CONNECTION_KEY(keys.connection_key),
            keys.transfer_key,
            total as i64,
            completed as i64,
        )
    };
    if let Err(e) = res {
        tracing::debug!("CfReportProviderProgress: {e}");
    }
}

/// TRANSFER_PLACEHOLDERS with all children of the directory. Sets
/// CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION and
/// PlaceholderTotalCount = entries.len(). Entries whose name already exists on disk must be
/// skipped by the caller (or the API reports ERROR_ALREADY_EXISTS per entry; treat as ok).
/// Returns whether all children succeeded and the indices that were newly created. Existing
/// names are omitted: their backing version may differ from the current listing.
pub fn transfer_placeholders(
    keys: &OpKeys,
    entries: &[PlaceholderSpec],
) -> Result<(bool, Vec<usize>)> {
    let mut batch = create_batch(entries);
    let info = op_info(keys, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    let mut params: CF_OPERATION_PARAMETERS = unsafe { std::mem::zeroed() };
    params.ParamSize = op_param_size::<CF_OPERATION_PARAMETERS_0_4>();
    params.Anonymous.TransferPlaceholders = CF_OPERATION_PARAMETERS_0_4 {
        Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CompletionStatus: NTSTATUS(0),
        PlaceholderTotalCount: entries.len() as i64,
        // An empty Vec's pointer is dangling, not null; cldflt rejects that.
        PlaceholderArray: if entries.is_empty() {
            std::ptr::null_mut()
        } else {
            batch.infos.as_mut_ptr()
        },
        PlaceholderCount: entries.len() as u32,
        EntriesProcessed: 0,
    };
    execute(&info, &mut params)?;

    let already =
        windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_ALREADY_EXISTS.0).0;
    let mut complete = true;
    let mut newly_created = Vec::new();
    for (index, (spec, created)) in entries.iter().zip(batch.infos.iter()).enumerate() {
        let hr = created.Result.0;
        if hr == 0 {
            newly_created.push(index);
        }
        if hr != 0 && hr != already {
            tracing::warn!(name = %spec.name, hr = format_args!("0x{:08X}", hr as u32), "placeholder not created");
            complete = false;
        }
    }
    Ok((complete, newly_created))
}

/// Fail FETCH_PLACEHOLDERS.
pub fn fail_placeholders(keys: &OpKeys, status: i32) {
    let info = op_info(keys, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    let mut params: CF_OPERATION_PARAMETERS = unsafe { std::mem::zeroed() };
    params.ParamSize = op_param_size::<CF_OPERATION_PARAMETERS_0_4>();
    params.Anonymous.TransferPlaceholders = CF_OPERATION_PARAMETERS_0_4 {
        Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE,
        CompletionStatus: NTSTATUS(status),
        PlaceholderTotalCount: 0,
        PlaceholderArray: std::ptr::null_mut(),
        PlaceholderCount: 0,
        EntriesProcessed: 0,
    };
    if let Err(e) = execute(&info, &mut params) {
        tracing::warn!("fail_placeholders: {e}");
    }
}

/// ACK_DELETE (status 0 = allow, else deny with that NTSTATUS).
pub fn ack_delete(keys: &OpKeys, status: i32) -> Result<()> {
    let info = op_info(keys, CF_OPERATION_TYPE_ACK_DELETE);
    let mut params: CF_OPERATION_PARAMETERS = unsafe { std::mem::zeroed() };
    params.ParamSize = op_param_size::<CF_OPERATION_PARAMETERS_0_7>();
    params.Anonymous.AckDelete = CF_OPERATION_PARAMETERS_0_7 {
        Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
        CompletionStatus: NTSTATUS(status),
    };
    execute(&info, &mut params)
}

/// ACK_RENAME (status 0 = allow).
pub fn ack_rename(keys: &OpKeys, status: i32) -> Result<()> {
    let info = op_info(keys, CF_OPERATION_TYPE_ACK_RENAME);
    let mut params: CF_OPERATION_PARAMETERS = unsafe { std::mem::zeroed() };
    params.ParamSize = op_param_size::<CF_OPERATION_PARAMETERS_0_6>();
    params.Anonymous.AckRename = CF_OPERATION_PARAMETERS_0_6 {
        Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
        CompletionStatus: NTSTATUS(status),
    };
    execute(&info, &mut params)
}

// ---------------------------------------------------------------------------------------------
// Callbacks registered by `cf::connect`. They run on cldflt threads: copy, send, return.
// A panic must never unwind into the kernel-facing caller, so every body is wrapped.
// ---------------------------------------------------------------------------------------------

/// FETCH_DATA: copy the request and queue it. If nothing is listening, fail it right away.
pub unsafe extern "system" fn on_fetch_data(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let fd = (*params).Anonymous.FetchData;
    let keys = keys_of(info);
    let offset = fd.RequiredFileOffset.max(0) as u64;
    let length = fd.RequiredLength.max(0) as u64;
    let accepted = catch_unwind(AssertUnwindSafe(|| {
        let req = FetchDataReq {
            keys,
            path: full_path(info, info.NormalizedPath),
            identity: identity_of(info),
            file_size: info.FileSize.max(0) as u64,
            required_offset: offset,
            required_length: length,
        };
        send_event(CfEvent::FetchData(req))
    }))
    .unwrap_or(false);
    if !accepted {
        fail_data(&keys, offset, length, STATUS_CLOUD_FILE_UNSUCCESSFUL);
    }
}

/// CANCEL_FETCH_DATA: informational, the engine drops its download for this transfer.
pub unsafe extern "system" fn on_cancel_fetch_data(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let (file_id, transfer_key) = (info.FileId, info.TransferKey);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        send_event(CfEvent::CancelFetchData {
            file_id,
            transfer_key,
        });
    }));
}

/// FETCH_PLACEHOLDERS: directory listing request, answered from the engine's index.
pub unsafe extern "system" fn on_fetch_placeholders(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let keys = keys_of(info);
    let accepted = catch_unwind(AssertUnwindSafe(|| {
        let req = FetchPlaceholdersReq {
            keys,
            path: full_path(info, info.NormalizedPath),
            identity: identity_of(info),
        };
        send_event(CfEvent::FetchPlaceholders(req))
    }))
    .unwrap_or(false);
    if !accepted {
        fail_placeholders(&keys, STATUS_CLOUD_FILE_UNSUCCESSFUL);
    }
}

/// CANCEL_FETCH_PLACEHOLDERS.
pub unsafe extern "system" fn on_cancel_fetch_placeholders(
    _info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        send_event(CfEvent::CancelFetchPlaceholders);
    }));
}

/// NOTIFY_DELETE: the engine must answer with `ack_delete`. Denied if nothing is listening.
pub unsafe extern "system" fn on_notify_delete(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let keys = keys_of(info);
    let accepted = catch_unwind(AssertUnwindSafe(|| {
        let flags = if params.is_null() {
            0
        } else {
            (*params).Anonymous.Delete.Flags.0
        };
        let req = DeleteReq {
            keys,
            path: full_path(info, info.NormalizedPath),
            identity: identity_of(info),
            undelete: flags
                & windows::Win32::Storage::CloudFilters::CF_CALLBACK_DELETE_FLAG_IS_UNDELETE.0
                != 0,
        };
        send_event(CfEvent::Delete(req))
    }))
    .unwrap_or(false);
    if !accepted {
        if let Err(e) = ack_delete(&keys, STATUS_CLOUD_FILE_UNSUCCESSFUL) {
            tracing::warn!("ack_delete (no engine): {e}");
        }
    }
}

/// NOTIFY_RENAME: the engine must answer with `ack_rename`. Allowed to be vetoed by the engine.
pub unsafe extern "system" fn on_notify_rename(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let rn = (*params).Anonymous.Rename;
    let keys = keys_of(info);
    let accepted = catch_unwind(AssertUnwindSafe(|| {
        let req = RenameReq {
            keys,
            path: full_path(info, info.NormalizedPath),
            target_path: full_path(info, rn.TargetPath),
            identity: identity_of(info),
            target_in_scope: rn.Flags.0 & CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE.0 != 0,
        };
        send_event(CfEvent::Rename(req))
    }))
    .unwrap_or(false);
    if !accepted {
        if let Err(e) = ack_rename(&keys, STATUS_CLOUD_FILE_UNSUCCESSFUL) {
            tracing::warn!("ack_rename (no engine): {e}");
        }
    }
}

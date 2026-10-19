# Windows Cloud Files integration

Clean-room behavioural specification of how Synka uses the Windows Cloud Files API. It
describes Synka's own calls and their documented meaning. Public sources are Microsoft Learn
pages on the Cloud Files API (cfapi) and the Windows.Storage.Provider WinRT API. Anything not
confirmed there is marked `undocumented — review`.

Public sources:
- Build a cloud sync engine that supports placeholder files:
  https://learn.microsoft.com/en-us/windows/win32/cfapi/build-a-cloud-file-sync-engine
- StorageProviderSyncRootManager class:
  https://learn.microsoft.com/en-us/uwp/api/windows.storage.provider.storageprovidersyncrootmanager
- Cloud Files API reference (CfConnectSyncRoot, CfCreatePlaceholders, CfExecute, CfOpenFileWithOplock,
  CfDehydratePlaceholder, CfSetPinState and related functions): Windows Win32 `cfapi` reference section.

## Sync root registration

Synka registers one sync root per Windows user through
`StorageProviderSyncRootManager.Register` (WinRT). Registration is idempotent: a root with the
same id at the same path counts as already registered. When the stored custom-state definitions
differ from the wanted ones, the root is registered again with the new list. Placeholders are
kept; nothing is unregistered.

| Field | Value |
|---|---|
| Id | `synka!` + base64 of the ACC user id + `!` + current user SID + `!User` |
| Path | the sync folder, default `%USERPROFILE%\Synka` |
| DisplayNameResource | "Synka" |
| IconResource | the engine executable, index 0 |
| Version | the crate version |
| ProviderId | the fixed provider GUID (kept across releases) |
| HydrationPolicy | Full (the whole file is supplied on hydration) |
| HydrationPolicyModifier | AutoDehydrationAllowed (Windows may free space; pinned files are not evicted) |
| PopulationPolicy | Full |
| InSyncPolicy | FileLastWriteTime and DirectoryLastWriteTime |
| HardlinkPolicy | None |
| ShowSiblingsAsGroup | false |
| AllowPinning | true (Explorer "Always keep on this device" and "Free up space") |
| ProtectionMode | Unknown (ACC holds business files) |
| Context | the id string as a buffer |
| StorageProviderItemPropertyDefinitions | three custom columns, see below |

Public reference: StorageProviderSyncRootManager and sync root registration in the cfapi guide.
The exact meaning of ProtectionMode and ShowSiblingsAsGroup is `undocumented — review` beyond
the reference text.

When the user moves the sync folder, Synka refuses a target inside another sync root
(`other_sync_roots`, used by `sync/relocate.rs`). Registration itself does not check other roots.

## Connection and callbacks

Synka calls `CfConnectSyncRoot` with callbacks for FETCH_DATA, CANCEL_FETCH_DATA,
FETCH_PLACEHOLDERS, CANCEL_FETCH_PLACEHOLDERS, NOTIFY_DELETE and NOTIFY_RENAME, then
CF_CALLBACK_REGISTRATION_END. Flags: REQUIRE_FULL_FILE_PATH and BLOCK_SELF_IMPLICIT_HYDRATION.

- Each callback copies what it needs from CF_CALLBACK_INFO and CF_CALLBACK_PARAMETERS into an
  event and sends it on a channel. Callbacks never block and never call into the async runtime.
- Completion is done later with CfExecute, from any thread, using the connection key and
  transfer key copied from the callback.

## Placeholders and population

- Directories are created as placeholders without DISABLE_ON_DEMAND_POPULATION, so Explorer
  requests their children.
- FETCH_PLACEHOLDERS is answered from the local database in one CfExecute
  TRANSFER_PLACEHOLDERS call; on-demand population is then disabled for that directory.
- Placeholders are created with CfCreatePlaceholders, converted with CfConvertToPlaceholder and
  updated with CfUpdatePlaceholder. These are documented functions.
- Directory and file handles for these operations use CfOpenFileWithOplock (exclusive, when the
  operation discards local bytes) or a plain share-all handle (other operations). Directories
  are opened with backup semantics and open-reparse-point.
- Synka never reads file data in the placeholder module.

## Hydration

- FETCH_DATA is answered by streaming the file from the storage URL (see `aps-endpoints.md`)
  into TRANSFER_DATA in 1 MiB chunks, 4 KiB aligned, from one reusable buffer.
- The requested offset is honoured with an HTTP Range request.
- Progress is reported at most four times per second with CfReportProviderProgress.
- Concurrent fetches of the same file are de-duplicated.
- A failure is reported with CfExecute TRANSFER_DATA using a failure status; the placeholder
  stays dehydrated.
- Hydration policy Full means Windows asks for the whole file. Partial hydration is not used,
  because Revit and IFC readers need complete files.

## Identity blob

Each placeholder carries a short identity blob:

| Byte | Meaning |
|---|---|
| 0 | format version, 1 |
| 1 | item kind (folder, file or other) |
| 2 to 5 | project id, unsigned 32-bit little-endian |
| 6 onward | item id as UTF-8 (lineage id for files, folder id for folders) |

The blob is about 30 bytes. Everything else is looked up in the database. The Cloud Files API
allows up to 4 KiB for the identity; that bound is documented.

## Dehydration and free space

`free up space` dehydrates only files whose Cloud Files metadata and the database both say they
are in sync, inside populated folders. Pinned files are not dehydrated. The call is
CfDehydratePlaceholder, documented. Files with pending local edits keep their bytes.

## Custom states (columns)

Three custom columns are defined on the sync root: `Låst av` (locked by), `Version` and
`Ändrad av` (modified by). They are answered from the database by the item-property source
inside the engine's COM server. The lock state can show an icon from a file in the package,
referenced by absolute path. The icon path form is `undocumented — review`; no system icon
resource ids are used.

Explorer cloud-status column and flyout, thumbnails and banners are not implemented.

## Notifications and deletes

- NOTIFY_DELETE must be answered with an acknowledgement (documented). When no engine is
  listening, Synka denies the delete. NOTIFY_RENAME can be vetoed by the engine.
- A delete from Explorer is not sent to ACC at once: it waits (see `docs/IPC.md`, mass deletes),
  and the engine checks it again before sending. Windows "undelete" cancels a pending delete.

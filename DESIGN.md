# Synka — design

Synka syncs ACC Docs, StreamBIM and Interaxo files to a local folder (Cloud Files API). Each
platform is a *provider* behind one dispatch module (`src/remote/`); see `docs/PROVIDERS.md` for
the provider contract, ids, versions and what each API cannot do. It can run alongside Autodesk
Desktop Connector. It is shipped as a sideloaded MSIX tray app, deployed the same way as
[Koordinatverktyget](https://github.com/byggstyrning/Koordinatverktyget).

Clean-room note: the behaviour specs for the APS endpoints, the Cloud Files integration and the
save patterns are in `docs/specs/`. They cite public sources only.

## Goals and budgets

| | budget |
|---|---|
| Working set at idle | < 25 MB, < 12 threads |
| Idle CPU | ~0 % (timers only) |
| Idle poll | zero writes, zero log lines when nothing changed |
| Log volume | < 1 MB / day, capped files |
| Placeholders | on-demand FETCH_PLACEHOLDERS from SQLite |
| Identity blob | ≤ 40 B binary |
| Local state | one SQLite file (WAL, 2 MB cache cap) |
| Hydration memory | one reusable 1 MiB buffer per transfer |

## Shape

Two processes in one MSIX, the same split as Koordinatverktyget (WinUI front end + engine):

- `Synka.exe`: C# WinUI 3 window in Koordinatverktyget's style (Mica, TitleBar,
  NavigationView, SettingsCards, Swedish/English, theme). It runs only while open and exits
  on close, so it costs nothing at idle. Pages: Översikt, Projekt, Aktivitet, Inställningar, Om.
- `engine\synka-engine.exe`: the Rust engine below. It always runs, starts at sign-in and
  owns the tray icon. A left click on the tray icon opens the window.
- They talk over a per-user named pipe; see `docs/IPC.md`.

The engine, `synka-engine.exe` (`windows_subsystem = "windows"`):

```
main thread:  Win32 message loop + tray icon/menu (tray-icon + muda)
tokio (2 workers): auth, HTTP, scheduler, uploads, downloads
cldflt threads: cfapi callbacks -> copy info -> mpsc -> tokio  (never block in a callback)
```

```
src/
  main.rs        startup, single-instance mutex, logging, wiring, message loop
  paths.rs       LocalState dir (MSIX: ApplicationData), sync root dir, log dir
  config.rs      settings.json in LocalState (client id override, sync root path, flags)
  model.rs       shared types (ids, Item, Project, Delta, errors)        [owned by architect]
  auth.rs        APS 3-legged OAuth, Authorization Code + PKCE, loopback redirect,
                 DPAPI-protected refresh token, auto refresh
  http.rs        one reqwest client (rustls, gzip, HTTP/2), retry/backoff, Retry-After
  api/dm.rs      project/v1 hubs/projects/topFolders, data/v1 folders/items/versions/storage
  api/syncfeed.rs  dm/v3 desktop-sync delta feed (undocumented; off by default)
  api/oss.rs     signeds3download (streamed), signeds3upload (multipart, streamed)
  store.rs       SQLite schema + queries (projects, items, outbox, kv)
  cf/mod.rs      sync root registration (WinRT StorageProviderSyncRootManager), connect
  cf/callbacks.rs  FETCH_DATA, FETCH_PLACEHOLDERS, NOTIFY_DELETE/RENAME, cancel
  cf/placeholder.rs  create/update/convert/in-sync helpers, identity blob encode/decode
  sync/mod.rs    engine: per-project scheduler, remote apply, local change processing
  sync/watcher.rs  one ReadDirectoryChangesW on the sync root, debounce/coalesce
  sync/upload.rs   upload queue (create item / new version), conflict copies
  sync/ignore.rs   compiled ignore rules (Office ~$, .tmp, .bak, .dwl, Revit backups…)
  sync/shell_cmds.rs  Explorer verbs on the engine side (open/copy ACC link, lock, sync now)
  shell/         out-of-proc COM server in the engine: CLSIDs, path -> item resolver,
                 class factories (com.rs), IExplorerCommand verbs (verbs.rs),
                 custom state handler for the Explorer columns (states.rs)
  tray.rs        tray icon + menu: status, open folder, sign in/out, projects ▸, sync now,
                 updates, quit
  update.rs      optional release channel check (off by default) -> PackageManager.AddPackageAsync
  shortcut.rs    offer a desktop shortcut on first run (shell:AppsFolder\PFN!App)
packaging/       AppxManifest, build_msix.ps1, new_release.ps1, make_delivery.ps1,
                 share_release.ps1, delivery/Install.cmd + Install.ps1
```

## Key decisions

1. **Own sync root**, never another product's. Id `synka!<b64(user id)>!<SID>!User`, path
   `%USERPROFILE%\Synka` (movable in Settings: a move is a full rebuild, `sync/relocate.rs`).
   Layout `Synka\<Hub>\<Project>\<top folder>\…` as in ACC: the project folder is ours
   (id `PROJECT_ROOT_ID`) and holds the project's top folders, "Project Files" or, for a user
   with partial access, each highest folder they reach (`topFolders?projectFilesOnly=true`).
   Both products can run side by side.
2. **Auth: own APS public-client app** "Synka" (Desktop/Mobile/SPA type,
   Authorization Code + PKCE, no secret), callback `http://localhost:47863/`, scopes
   `data:read data:write data:create user-profile:read`. The APS app must be renamed "Synka"
   in the developer portal; the client id does not change. Its client id is the built-in
   default (`config::DEFAULT_CLIENT_ID`), overridable via `SYNKA_APS_CLIENT_ID` at build time or
   `aps_client_id` in settings.json. It must be added as a Custom Integration in each ACC account.
   No token reuse from any other product.
3. **Change detection: documented data/v1 listing** of populated folders (tiers …). Optional:
   Autodesk's undocumented dm/v3 desktop-sync feed, **OFF by default** (`desktop_sync_feed` in
   settings.json only; not in the UI or IPC). Enabling it requires written approval from
   Autodesk. When off, the engine never calls dm/v3 and drops any stored feed cursor.
   - Default: public `data/v1 folders/{f}/contents`, but only for folders that are
     **populated** locally (on-demand population makes this small), with tiered intervals.
   - Tiers: 30 s while active (callbacks or file activity in the last 10 min), 120 s, 300 s,
     720 s when the user is idle; ±10 % jitter; immediate poll after a local mutation or menu "Sync now".
   - A walk lists 4 folders at a time and stops at the first failure.
   - StreamBIM lists per folder (`sync::hot`). Between full walks a poll lists only the folders
     where the user worked (opened, saved, renamed, deleted) or a poll found a remote change in
     the last 10 min. Every populated folder is still listed at least every 300 s, after
     "Sync now" and on the first poll. A poll that finds an item gone from an active folder
     lists the quiet folders too, so a move into one of them stays a move. Rendra publishes no
     limits but warns against high-frequency polling, and each StreamBIM folder costs at least
     two requests (subfolders, then documents).
   - HTTP 429 (after the request's own Retry-After retries) pauses the whole provider
     (`sync::throttle`): 60 s, doubled after each further 429 up to 15 min, reset by a good poll.
     Its projects do not poll, not even after "Sync now". Its queued jobs wait without spending
     an attempt. The status shows "<provider> begränsar antalet anrop" instead of an error.
     Autodesk counts its limits per client ID and endpoint across every install that shares
     the ID (folder contents 300/min, new items 50/min), so an organisation with many users
     should use its own `aps_client_id`.
   - An empty poll writes nothing and logs nothing.
   - Endpoints and mappings: `docs/specs/aps-endpoints.md`.
4. **On-demand population.** Directory placeholders are created *without*
   `DISABLE_ON_DEMAND_POPULATION`; FETCH_PLACEHOLDERS answers from SQLite in one
   `TRANSFER_PLACEHOLDERS` and then disables on-demand for that dir. Remote deltas for
   unpopulated folders touch only the DB.
5. **Identity blob** = `[ver=1][kind][pid u32 LE][id bytes]` where id is the URN suffix
   (lineage id for files, folder id for folders). Everything else is looked up in SQLite.
6. **Hydration** streams `signeds3download` URL → `CfExecute(TRANSFER_DATA)` in 1 MiB
   4 KiB-aligned chunks from one reusable buffer, honours the requested offset (HTTP Range),
   reports progress ≤ 4×/s, dedups concurrent fetches per file.
7. **Uploads** stream from disk in 16 MiB parts (one buffer per upload, ≤ 2 concurrent),
   then create item (`items:autodesk.bim360:File`) or version (`versions:autodesk.bim360:File`).
   Conflict (remote changed while local dirty) → upload local copy as
   `name (konflikt YYYY-MM-DD HHMM).ext`, then take remote.
8. **No** activity-log telemetry, FOS, WPF, shell extensions, minifilter, analytics.
9. **MSIX**: full-trust packaged desktop app, `windows.startupTask` (start at sign-in),
   self-signed `CN=FastADC` for sideload (certificate name kept; see `packaging/README.md`).
   Release flow copied from Koordinatverktyget (bump → tag → build → sign → zip with Install.cmd
   + .cer → GitHub release on this public repository; Byggstyrning's staff also get it through
   BoomerHubb and the OneDrive share). The in-app updater is off unless `update_channel` is set.
   The ACC-only build (`--features acc-only`, `docs/MARKETPLACE.md`) offers ACC alone.
   TODO(owner): a new trusted signing certificate, see `packaging/README.md`.
10. **No emulation of Autodesk local services.** Synka never binds a named pipe whose name
    contains `Autodesk` or `DesktopConnector` (`ipc::check_pipe_name`). Revit-link emulation
    (the local service that Revit talks to) is not to be implemented without written approval
    from Autodesk.

## Deep Windows integration (OneDrive-level)

Package identity (MSIX) unlocks the `cloudfiles:` / `desktop4/5:` / `com:` manifest
extensions. Everything runs as COM servers inside `synka-engine.exe` (out-of-proc `com:ExeServer`,
started with `-Embedding`), so there is still only one small process and no shell DLL
injected into Explorer.

| Feature | Mechanism |
|---|---|
| Navigation pane entry, cloud status column, "Always keep on this device" / "Free up space" | sync root registration (done in `cf`) |
| Explorer cloud flyout (Win11 header: synced, pending, signed-in user, quota-free) | `cloudfiles:StorageProviderStatusUISourceFactory` COM class |
| Custom state icons + columns: **Låst av X**, **Version N**, **Ändrad av** | `cloudFiles:CustomStateHandler` (`IStorageProviderItemPropertySource`, answered from SQLite; ids 2/4/5) + definitions on the sync-root registration; lock icons `Assets\LockedByMe.ico` / `LockedByOther.ico` (built) |
| Thumbnails for dehydrated .rvt/.dwg/.pdf/.ifc without downloading | `cloudfiles:ThumbnailProviderHandler` COM class (ACC/derivative thumbnail API, cached) |
| Context menu: **Visa i ACC**, **Lås / Lås upp**, **Kopiera ACC-länk**, **Versionshistorik**, **Synka nu** | `cloudFiles:CloudFilesContextMenus` (sync root only) → one `IExplorerCommand` COM class per verb (built, except Versionshistorik) |
| Banners (e.g. "Filen är låst av X", "Konflikt") | `BannersHandler` exists in the manifest schema but has no public COM interface — **deferred**; use toasts + custom states instead |
| Toast notifications (conflicts, upload done, sign-in needed) with actions | `Windows.UI.Notifications` (needs identity) + `desktop:ToastNotificationActivation` |
| Start at sign-in | `desktop:StartupTask` |
| `synka://` deep links from ACC web ("Öppna i Synka") | `uap:Protocol` |
| Implicit lock when Revit/AutoCAD opens a file for write | `NOTIFY_FILE_OPEN_COMPLETION` + sharing probe → `PATCH items reserved` |
| Explorer search on metadata | properties via `StorageProviderItemProperties` (indexed) |

Order: custom states + context menu + toasts + flyout first (highest daily value), then
thumbnails, banners, implicit locks, protocol.

## Persisted state (SQLite, `LocalState\synka.db`)

See `store.rs`. Cursor is written in the same transaction as the item changes it describes.

## Phase plan

- v0.1 (this pass): sign-in, project picker, read path (browse + hydrate on demand, pin/free up
  space via Explorer), upload new/modified files, rename/move, delete, conflicts as copies, MSIX.
- v0.2: implicit locks for Revit/AutoCAD, read-only for files locked by others.

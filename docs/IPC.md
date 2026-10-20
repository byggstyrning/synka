# Engine ⇄ UI protocol

Two processes in one MSIX package:

| Process | What | Lifetime |
|---|---|---|
| `synka-engine.exe` (Rust) | sync, Cloud Files provider, tray icon, (later) Explorer COM servers | always running, starts at sign-in |
| `Synka.exe` (C# WinUI 3) | the window, styled like Koordinatverktyget | only while open; exits on close |

The engine is the single source of truth. The UI holds no state of its own beyond view
preferences (language, theme, window size), which it keeps in
`ApplicationData.Current.LocalSettings`.

## Transport

- Named pipe `\\.\pipe\synka-ui-<user SID>` (SID string of the current user, e.g.
  `S-1-12-1-…`). Engine is the server and accepts several clients. The pipe ACL allows only
  the current user.
- UTF-8, **one JSON object per line** (`\n`-terminated), both directions.
- The UI connects on start, retries every 1 s for 10 s. If the engine isn't running, it
  starts it (packaged: `shell:AppsFolder\<PFN>!Engine` via `explorer.exe`; dev: env
  `SYNKA_ENGINE` or `..\target\release\synka-engine.exe`) and shows an InfoBar while waiting.
- On disconnect the UI shows a non-blocking InfoBar ("Tappade kontakten med synkmotorn")
  and reconnects.

## UI → engine (commands)

Every command has `"cmd"` and an optional client-chosen integer `"id"`. If `id` is given the
engine answers `{"type":"ack","id":<id>,"ok":true}` or `{"type":"ack","id":<id>,"ok":false,"error":"…"}`.

| cmd | fields | effect |
|---|---|---|
| `hello` | `version` (UI version string) | engine replies with `status`, `settings`, `projects` (cached, may be empty), `activity` |
| `sign_in` | `provider?` (`acc` default, `streambim`, `interaxo`); StreamBIM also `region` (subdomain), `username`, `password` | ACC/Interaxo: browser sign-in. StreamBIM: password login, may ask for an MFA code. Progress arrives as `sign_in_progress`, the result as `status` |
| `sign_in_mfa` | `provider` (`streambim`), `code` (6 digits) | answers a `sign_in_progress` `mfa_required` |
| `cancel_sign_in` | `provider` | cancels a pending browser sign-in or MFA wait → `sign_in_progress` `cancelled` |
| `sign_out` | `provider?` | signs out of that provider; without `provider`, of every provider |
| `streambim_regions` | | → `streambim_regions` event (live list, built-in fallback) |
| `sync_now` | | |
| `open_folder` | `path?` (absolute; default sync root) | opens Explorer |
| `open_log` | | opens the log folder |
| `refresh_projects` | | reloads hubs/projects from every signed-in provider → `projects` event |
| `set_project` | `hub_id`, `project_id`, `subscribe` (bool) | subscribe/unsubscribe → `projects` + `status` |
| `set_setting` | `key`, `value` | keys: `propagate_deletes` (bool), `max_downloads` (1–8), `max_uploads` (1–4), `poll_active_secs` (15–300), `log_level` ("info"/"debug"), `start_at_login` (bool, StartupTask), `auto_update` (bool), `aps_client_id` (string, blank clears the optional override), `interaxo_client_id` (string, blank clears), `interaxo_redirect_uri` (string, blank = default), `interaxo_client_secret` (string, blank clears; never echoed); `desktop_sync_feed` is not settable (settings.json only) → `settings` |
| `test_aps_client_id` | `client_id` (nonblank public ID), `request_id` (UUID string) | validates fresh public PKCE sign-in before saving → `aps_client_id_validation`; saved changed ID restarts the engine |
| `cancel_aps_client_id_test` | `request_id` (UUID string) | cancels that test; the correlated terminal event decides a completion/cancellation race |
| `free_up_space` | `project_id?` | dehydrates clean local files and clears offline pinning for synced files and folders in the selected project, or all enabled projects when `project_id` is omitted |
| `check_updates` | | → `update` event |
| `install_update` | | installs the available update now (closes the window; the engine restarts on the new version) |
| `retry_failed` | | revives failed (dead) jobs, i.e. `failed_jobs` → pending again |
| `clear_activity` | | clears the recent activity list for every connected window; does not change sync jobs or files |
| `reset` | `project_id?`, `path?` (absolute folder path inside a project) | rebuilds the project (or that folder) from ACC: unsynced local changes are first copied to `%USERPROFILE%\Synka - återställda filer\<name> <date>\`, then the folder is replaced by a fresh placeholder. Refused (as `notify`) while a file inside is open → `activity_item` kind `reset`, `notify` |
| `confirm_deletes` | `project_id`, `confirm` bool | answers held mass deletes: `true` sends them to ACC, `false` brings the items back → `status`, `activity_item` kind `restore` |
| `reset_all` | | `reset` for every subscribed project, one after the other |
| `move_sync_root` | `path` (absolute: the new sync root itself, e.g. `D:\Projekt\Synka`) | moves the sync root and rebuilds every project there from ACC, as a full reset: unsynced files are saved to `Synka - återställda filer` next to the old root first. The ack is `ok:false` with a Swedish reason when the place cannot work (not a new or empty folder, inside or around the current root, inside another provider's folder such as OneDrive, a system folder, not a local NTFS drive). After `ok`: `status` text "Flyttar synkmappen…", a `notify`, then the engine restarts itself and the pipe closes; reconnect and send `hello` (the new `sync_root` is in `status` and `settings`). A file open in another program cancels the move with a `notify` and nothing changes |
| `export_diagnostics` | | zips the recent logs and a sanitized state summary (no tokens, no user name/e-mail, no file contents) to the Desktop and shows it in Explorer → `notify` |
| `quit_engine` | | stops sync and exits the engine (tray "Avsluta" does the same) |

## Engine → UI (events)

| type | fields |
|---|---|
| `status` | `signed_in` bool, `user` `{name,email}` or null, `state` one of `signed_out`,`synced`,`syncing`,`waiting`,`attention`,`offline`,`error`, `text` (Swedish one-liner), `error` string or null, `pending_jobs`, `failed_jobs`, `transfers` `[{name, project, direction:"down"\|"up", done, total}]` (bytes), `sync_root` (abs path), `held_deletes` `[{project_id, project, count}]` (delete bursts waiting for `confirm_deletes`; a folder counts its contents), `local_bytes` (bytes of file data on disk across projects, null until first counted), `engine_version`, `accounts` `[{provider, label, configured, signed_in, signing_in, user:{name,email}\|null, host}]` (always `acc`, `streambim`, `interaxo` in that order) |
| `projects` | `loading` bool, `hubs` `[{id, name, provider, projects:[{id, name, subscribed, local_path}]}]` (sorted by name) |
| `settings` | `propagate_deletes`, `max_downloads`, `max_uploads`, `poll_active_secs`, `log_level`, `start_at_login`, `auto_update`, `sync_root`, `client_id_configured`, `aps_client_id` (string or null), `aps_client_id_override_supported` (bool), `aps_client_id_validation_supported` (bool), `update_channel_configured` (bool; false = no update channel, the window hides update controls), `providers` (`["acc","streambim","interaxo"]`), `interaxo_client_id` (string or null), `interaxo_redirect_uri` (effective), `interaxo_redirect_uri_default`, `interaxo_client_secret_set` (bool) |
| `sign_in_progress` | `provider`, `state` (`browser`\|`mfa_required`\|`signed_in`\|`failed`\|`cancelled`), `error` (Swedish text or null) |
| `streambim_regions` | `regions` `[{subdomain, title, is_default}]` |
| `aps_client_id_validation` | `request_id`, `state` (`testing`\|`saved`\|`failed`\|`cancelled`), `error` (safe text or null). Recent per-request outcomes replay on hello/reconnect |
| `activity` | `items` `[{ts_ms, kind, name, project, detail}]` newest first, at most 200. `kind`: `download`,`upload`,`new_version`,`rename`,`move`,`delete`,`conflict`,`error`,`unsupported` (ACC rejected a file type and will not retry),`remote_change`,`free_up`,`reset`,`restore`,`lock`,`unlock` (Explorer verbs Lås / Lås upp). Sent in full on `hello`, then as `activity_item` |
| `activity_item` | `item` = one item as above, i.e. `{"type":"activity_item","item":{ts_ms,kind,name,project,detail}}` |
| `notify` | `title`, `body`, `severity` (`info`\|`warning`\|`error`) — UI shows an InfoBar |
| `update` | `available` bool, `version` or null, `error` or null, `installing` bool |
| `ack` | see above |

`status` is pushed on every change (throttled to 2/s); `transfers` progress rides on it.

## Providers and accounts

See `docs/PROVIDERS.md`. Clarifications:

- **One account per provider.** `status.signed_in` is true when any provider is signed in;
  `status.user` is the first signed-in account in the order `acc`, `streambim`, `interaxo`.
  `accounts[].configured` is false when the provider cannot sign in yet (ACC without a client id,
  Interaxo without `interaxo_client_id`). `host` is the StreamBIM region host or null.
- **`sign_in`.** A new `sign_in` for a provider replaces a pending attempt for the same provider
  (as the ACC rule: clicks within one second share it). StreamBIM requires `region`, `username`
  and `password`; missing fields are refused in the ack. The password is used once and never
  stored or logged. `sign_in_progress` reports `browser` (ACC/Interaxo, waiting for the browser),
  `mfa_required` (StreamBIM, send `sign_in_mfa` within five minutes), then `signed_in`, `failed`
  (with `error`) or `cancelled`. A failed MFA code gives `failed`; the user starts over.
- **`sign_out`.** Signing out of one provider stops polling and uploading its projects; their
  folders stay. The sync root is unregistered only when no provider is signed in.
- **Interaxo settings.** `interaxo_client_id`, `interaxo_redirect_uri` and
  `interaxo_client_secret` can change only while Interaxo is signed out; otherwise the engine
  sends an error `notify` and republishes `settings`. The redirect URI must be
  `http://localhost:<port>/<path>` or `http://127.0.0.1:<port>/<path>` (default
  `http://localhost:47864/`); it must match the URI registered with Interaxo exactly. The
  secret is optional, stored DPAPI-protected and reported only as `interaxo_client_secret_set`.
- **`projects`.** `hubs[].provider` is `acc`, `streambim` or `interaxo`. Hub ids and project ids
  are unique across providers (`docs/PROVIDERS.md`).

## Language

The engine's `text`/`detail`/`notify` strings are Swedish. The UI translates the `state`
enum itself, and shows engine strings as they are.

## Engine-side decisions (clarifications)

These pin down details the table above left open. The engine implements them as written here.

- **Ack.** A command with an `id` gets `{"type":"ack","id":N,"ok":true}` once it is accepted
  (validated and handed to the engine). The engine's work finishes later and shows up as
  `status`, `projects`, `settings`, `activity_item` or `notify`. Without an `id`, a success is
  silent. A failure is always acked: `{"type":"ack","ok":false,"error":"…"}`, with `"id"` only when
  the request had one. This covers invalid JSON, unknown `cmd`, and bad values.
- **Validation.** `set_setting` ranges and types are checked in the engine's IPC layer (see
  `config::check_setting`). `open_folder` with `path` requires an absolute path.
- **`hello`.** Reply order: ack (if `id`), then `status`, `settings` (when known), `projects`
  (empty listing before the first refresh), `activity`. The reply comes from the engine's caches
  and does not wait for the engine. `hello` is never forwarded to the engine.
- **Lag.** If a client falls behind the event queue, the engine resends `status`, `settings`,
  `projects`, `activity` in full, so the client converges without a restart.
- **`settings.client_id_configured`.** `true` when an effective APS client id is available,
  including the built-in id. Nonblank settings and build-time overrides take precedence.
  Diagnostics also report `client_id_source` as `settings`, `build_env` or `built_in`,
  without exposing the id. Availability does not validate Autodesk's app registration.
- **Optional APS Client ID.** New engines advertise `aps_client_id_override_supported: true`.
  `aps_client_id` contains only the saved optional override, or null for the default;
  it does not expose the built-in or build-time ID. Older engines omit the capability,
  so clients hide this editor. Blank `set_setting` values clear the override;
  nonblank values require `test_aps_client_id`. Client IDs are public
  identifiers; the value is a Client ID and no client secret is used. Effective-ID changes are
  applied only while signed out and idle. A pending browser sign-in is cancelled and
  awaited; any leftover credentials/profile are cleared before saving. Successful
  changes publish `settings`, then restart the engine to create Auth with the new ID.
  Reconnect, send `hello` and sign in again. A normalized edit that keeps the same
  effective ID saves without restarting after a successful signed-out test. Refused changes and
  save failures send an error `notify` and republish the actual settings. The early
  command ack means accepted for processing; it does not mean the setting was saved.
- **Test and save.** `aps_client_id_validation_supported: true` enables the staged
  `test_aps_client_id` flow. Nonblank overrides through generic `set_setting` are
  rejected; blank resets remain supported. The candidate stays in memory until a
  fresh browser PKCE code exchange and validated user profile succeed. Candidate
  tokens are discarded. Failure, cancellation, stale completion, a busy session,
  changed starting configuration or persistence failure cannot apply the candidate.
  The correlated `saved` event confirms completion; an unchanged settings snapshot
  cannot prove that a same-ID test passed. A changed saved snapshot can recover a
  completion lost during restart. The engine retains at most 16 recent operation
  results for reconnects. The UI keeps the draft and offers cancellation while the
  browser waits (five minutes plus bounded token/profile requests).
- **Settings apply.** `propagate_deletes`, `poll_active_secs` and `auto_update` take effect at once.
  `max_downloads`, `max_uploads` and `log_level` are saved now and take effect after the engine
  restarts. `start_at_login` is the StartupTask state (`SynkaStartup`). It is always `false` for
  unpackaged runs, and setting it there is ignored.
- **`set_project`.** Subscribing or unsubscribing is applied to the listing from the last
  `refresh_projects`. An unknown `hub_id`/`project_id` gives a `notify` (severity `warning`). The
  result comes as `projects` and `status`.
- **`free_up_space`.** Dehydrates only files whose Cloud Files metadata and database state both
  confirm they are in sync, in populated folders. Both the app-wide and project-specific commands
  clear offline pins from these files, then from their visible folder hierarchy; files with pending
  local edits retain their bytes and pin. A project-specific command leaves the shared hub and
  other projects untouched. If that shared hub is pinned, its inherited pin can prevent a
  project-specific command from releasing bytes; use the app-wide command to clear that pin.
  An unknown or unsubscribed `project_id` gives an error `notify` instead of
  reporting zero files freed. Only one free-up may run at a time; a concurrent request gets a
  warning `notify`. Unopened cloud folders are not enumerated. It emits activity
  `kind: "free_up"` (name `"N filer"`, project set when a single project was selected) and a
  `notify` (severity `info`).
- **Activity names.** `remote_change` has `name` `"N ändringar från <ACC|StreamBIM|Interaxo>"` and `project` set to
  the project name. `rename` and `move` carry the new name and `detail` `"från <old name>"`.
  `conflict` has `name` = the original file, `detail` = the name of the kept local copy. `error`
  has `name` = the file (or job id) and `detail` = the error text.
- **`status`.** `state` is one of `signed_out`, `synced`, `syncing`, `waiting`, `attention`, `offline`, `error`; `waiting` means jobs are queued but no poll or transfer is running; `attention` means a job failed and no work remains active or queued; `error`
  is the last engine error or null. `transfers` lists downloads (`direction` `"down"`) and uploads
  in progress (`"up"`); `done`/`total` are bytes. `local_bytes` counts file data present on disk
  in folders that have been opened (no file is opened to count it). It is recounted at most every
  2 minutes while a window is connected, on `hello`, and after `free_up_space`.
- **`projects`.** Hubs and projects are sorted by name (case-insensitive). `local_path` is set only
  for subscribed projects, and is `sync_root\<hub>\<project>` with the names sanitized the same
  way as on disk.
- **`update`.** Sent for every check, not only when an update exists: `available` false with
  `error` null means no update, `error` set means the check failed (or, with `available` true,
  that installing failed). `installing` is true while the package is handed to Windows. With
  `update_channel` set and `auto_update` on (default off), the engine installs a found update by
  itself once nothing is syncing and no window is connected; Windows restarts the engine afterwards.
  With no `update_channel` (the default) the engine never checks: `check_updates` and
  `install_update` answer with `update`, `available` false and an `error` text saying that updates are off.
- **Mass deletes.** Local deletes wait 15 s before they are sent. When a project's waiting deletes
  reach 25 (a folder counts what it holds; a folder never listed always counts as 25), all of them
  are held (`held_deletes`) until `confirm_deletes`, and the user gets one `notify`.
- **`notify` without a window.** Shown as a Windows toast (click opens the window), at most 3 per
  10 s plus one summary.
- **`notify`.** `title` is always `"Synka"`.
- **Shell open.** The tray's left click opens the window. It is not a `TrayCmd` in the protocol.

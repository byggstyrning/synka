# Synka roadmap

Handoff document for the next agent. Read `DESIGN.md`, `docs/IPC.md` and `docs/specs/` first.
Everything marked **verified** was measured on this machine against the real ACC project
*Köpmannen* (sync root `%USERPROFILE%\Synka`).

## Where things stand (2026-10-16, after the hardening pass, and the 2026-10 legal review)

| Area | State |
|---|---|
| Engine (`synka-engine.exe`, Rust) | Working. Sign-in (PKCE, DPAPI token), delta polling by the documented data/v1 listing (the dm/v3 desktop-sync feed exists but is off by default), placeholders on demand, downloads, uploads, rename, delete, dirty-state tracking. Hardened: see "Fixed in the hardening pass" below. |
| Upload round trip | **Verified** in ACC before the hardening pass: create folder + file, new version, rename, delete. Re-verify after it (checklist below). |
| Downloads | **Verified** byte-identical (sha256 match) for a 77 MB file, single stream and 4 parallel ranges. |
| IPC | Named pipe `\\.\pipe\synka-ui-<SID>`, protocol in `docs/IPC.md` (now with `retry_failed`, `export_diagnostics`, `reset`, `confirm_deletes`, `held_deletes`, `auto_update`). |
| WinUI window (`ui\`) | Reviewed against the contract; C# type-checks against the real reference assemblies; the XAML is compiled only in CI. |
| Explorer integration | **Verified on the live sync root**: native Cloud Files availability/status and right-click *Behåll alltid på denna enhet* / *Frigör utrymme*. Custom COM verbs, columns and status flyout are built but not attached to the shipped Cloud Files extension. |
| Packaging and release | CI on pull requests and main (`.github/workflows/ci.yml`); one-click release (`release.yml`, Actions → Release → Run workflow). Needs the secrets in `packaging/README.md`. |
| Release channel | Releases are published on this repository's GitHub releases (the repository is public). The Design and Make Marketplace listing is pending review (`docs/MARKETPLACE.md`). Byggstyrning's staff also update through BoomerHubb and the OneDrive share. The in-app check stays in code, off unless `update_channel` is set. |
| Builds | The default build syncs ACC, StreamBIM and Interaxo; the ACC-only build (`--features acc-only`, `build_msix.ps1 -AccOnly`) syncs ACC alone (`docs/MARKETPLACE.md`). |
| Name | The product is **Synka** (package `Byggstyrning.Synka`). Earlier builds of the app used other package identities (listed in `packaging/delivery/Install.ps1`); such a package must be uninstalled first (Install.cmd offers it). The signing certificate is still `CN=FastADC` (TODO, see `packaging/README.md`). |

## Multi-provider: StreamBIM and Interaxo (2026-10-20, not yet verified live)

Architecture and contract: `docs/PROVIDERS.md`; protocol: `docs/IPC.md` "Providers and
accounts"; API research with every gap: `docs/specs/streambim-endpoints.md`,
`docs/specs/interaxo-endpoints.md`.

| Area | State |
|---|---|
| Engine | One account per provider (ACC, StreamBIM, Interaxo). The sync root stays registered while any account is signed in (its id is kept in kv `sync_root_uid`; an existing ACC install keeps its id). Signing out of one provider pauses only its projects. Every API call goes through `src/remote/`. |
| StreamBIM | One account per server: regions from `regions.json` (built-in fallback) or a custom https address; e-mail + password, TOTP MFA. Listing of populated folders, downloads via `downloadlink`, uploads via upload ticket → S3 PUT → document / document-revision. Folder create, rename, move and soft delete. No locks, no SSO-only accounts, no uploads over 5 GB. |
| Interaxo | Bring-your-own OIDC client (Auth0, PKCE, optional secret, configurable loopback redirect). Communities on `api.interaxo.com` and `api.interaxo.se`, rooms as projects, simple folders and files. Download (pinned to the listed version), new files, new versions, delete. Folder create, rename and move stay local with a notice (no API). |
| Window | A Konton page: every account with its server, status, synced projects and sign-out; sign-in dialogs (StreamBIM server + MFA code, browser sign-in for ACC and Interaxo); APS and Interaxo client settings under Avancerat. Projects show the provider per hub and can be filtered to one account. |
| Tests | Loopback tests for both clients, multi-account engine and IPC tests, the IPC audit scenario and the resilience lab Quick profile. Nothing has run against a real StreamBIM or Interaxo tenant yet. |

### Verify against real accounts (first thing for these providers)

StreamBIM (one account with MFA, one without; one region other than `app`):
1. Login and MFA: does `mfa/verify` return a `refreshToken`? (If not, MFA users re-enter a code
   when the id token expires.) Does `refresh` accept a not-yet-expired id token? Wrong-code and
   wrong-password status codes.
2. Profile: shape of `/mgw/api/v2/user-profiles?filter[profile]=true` (name shown in Synka).
3. Listing: `folders?filter[parent]=` and `documents?filter[folderId]=` return direct children
   only; `meta.total` present; folder and document ids may collide (Synka prefixes documents
   with `d`).
4. Download link: which response variant; redirect to S3; whether a revision other than the
   current one can be fetched.
5. Upload: ticket `type` `attachments` accepted for documents; S3 PUT with a 5-minute presigned
   URL for a large file; `POST documents` and `POST document-revisions` response bodies;
   duplicate filenames in one folder.
6. PATCH with the full object (rename, move, `is-deleted`); whether a deleted folder cascades.

Interaxo (a test client from Interaxo support):
1. Native client with PKCE and refresh tokens; consent screen on the loopback redirect; refresh
   token rotation and lifetime; whether `/communities` on the "wrong" host answers 401, 403 or an
   empty list.
2. Paging: the real maximum `limit` on communities, rooms and children.
3. Upload: multipart POST response (`version`, renamed name on a conflict); `versions?major=true`.
4. Download with `?version=`; whether DELETE is soft or hard and whether it recurses.
5. `lock_info` owner shape; the unofficial `ix.interaxo.*/{community}/{room}/documentLibrary/{id}` link.

## Fixed in the hardening pass

- Safe saves (Office, AutoCAD, Revit) keep the version history: deletes wait 15 s, a new file
  or a rename back under the same name continues the item; a rename to a temp/backup name is a
  pending delete; stale copies that still carry an identity never touch ACC.
- Conflicts keep both: the local copy becomes `name (konflikt YYYY-MM-DD HHMM).ext` and is
  uploaded; the original takes the cloud version (both on upload and on remote change).
- Edits whose ACC item or folder disappeared are kept and uploaded again (was roadmap #3).
- Moves between projects are refused with a message (was #4).
- Locked files are deferred 10 s without spending an attempt; the outbox sleeps until the next
  job is due (was #2).
- Mass-delete protection: 25+ deletes (folders count their contents) wait for confirmation.
- One-click reset of a project or folder in the window, with
  unsynced changes saved to `%USERPROFILE%\Synka - återställda filer` first.
- Idle-to-active transition polls at once (was #6); `local_bytes` is real (was #7); startup
  rescan catches edits made while the engine was stopped.
- One naming module (Windows/Unicode case rules), indexed name lookups (schema v2).
- A remote change blocked by an open file is retried; a stale placeholder is never filled with
  a newer version's bytes.
- Seamless updates: auto-install when quiet, Windows restarts the engine
  (`RegisterApplicationRestart`). Toasts instead of message boxes. Diagnostics export.
- Uploads: 8 MiB parts, three in flight, zero-copy retries.
- Roadmap #5 (FETCH_DATA to EOF) is **by design** with `HydrationPolicy::Full`: Revit and IFC
  readers need whole files, and the platform asks for the whole file anyway.

## Since the hardening pass

- **Real project layout**: the project folder holds the project's top folders as in ACC:
  `Köpmannen\Project Files\...`. A user with access to only some folders
  inside Project Files (a consultant's discipline folders) sees each of them in the project
  folder. Top folders come from `topFolders?projectFilesOnly=true`, are refreshed every 6 h,
  cannot be deleted/renamed from Explorer, and nothing can be created or moved directly into
  the project folder (it stays local and the user is told). "Plans" is left out.
  Projects synced before this are rebuilt once at startup (unsynced work rescued).
- **Move the sync folder** (Settings → Synkmapp → Flytta…): checked first (new or empty folder,
  local NTFS, not inside OneDrive or another provider, not a system folder), then a full
  rebuild at the new place; the engine restarts itself.
- **Rebuild all projects** (Settings) in place.
- MSIX manifest: verb ids must be letters and digits only (MakeAppx refused a verb id containing a dot),
  and `cloudFiles:CloudFiles` needs all five handler elements. `tools/validate-manifest.py`
  checks the manifest against MakeAppx's own schemas on Linux.
- From the sync best-practices audit (internal), Top 10:
  - #1 an edit saved during its upload is uploaded again (never a torn version); changed rows
    that lost their job are queued again;
  - #2 offline or signed out is a pause: no attempts used, the queue waits;
  - #3 deletes are checked when they run (file back → kept; newer version in ACC → it wins;
    Windows "undelete" cancels);
  - #4 a reconcile pass at start finds deletes and renames made while the engine was stopped;
  - #5 the tip version is asked for right before a new version (conflicts are not buried);
  - #6 pinned folders keep new files; hydrations queue on two threads;
  - #7 (part) a remote rename of an open file waits instead of creating a second copy;
  - #8 25+ items removed in ACC at once: local copies are saved to the rescue folder first;
  - #9 (part) `.url/.lnk/.wsh` ignored, COM¹–³/LPT¹–³/CONIN$/CONOUT$ reserved;
  - rescue folders get a `LÄS MIG.txt`.
  Still open from it: #10 (USN-guarded "mark in sync"), a "not synced" reason column, a
  non-destructive "Reparera" step before "Bygg om", sign-out through the same rescue (in part:
  Windows turns populated placeholder folders into plain ones when the sync root is unregistered,
  so at the next connect a project whose folder is plain is rebuilt with the rescue before the
  reconcile pass, plain hub folders become placeholders again, and folders no synced project has
  are set aside and removed with their files saved).

## Verify on a real Windows 11 machine (first thing next session)

1. CI is green (engine tests, clippy `-D warnings`, MSIX build). Then install the CI artifact.
2. Uninstall any package of the app under an earlier package identity (Install.cmd offers it);
   install; sign in; subscribe *Köpmannen*; browse, open, edit, save from Word, AutoCAD and
   Revit: each save is a **new version**, not a new file.
3. Conflict: edit a file locally while someone uploads a new version → both files exist.
4. Delete 30 files in Explorer → the window asks; "Återställ" brings them back.
5. Rebuild a folder from the window: with a file open you get the list of open files;
   with an unsynced edit it lands in `Synka - återställda filer`.
6. Explorer: native *Behåll alltid på denna enhet* and *Frigör utrymme* work on files and
   folders inside `%USERPROFILE%\Synka`; availability and status render after pin/unpin.
7. Updates: with `update_channel` unset, the window shows updates as off and the engine never
   checks. With a test channel set, a signed build installs and the engine comes back.
8. Layout: Köpmannen shows `Project Files` as in ACC. With a consultant account
   that only reaches some folders, each appears in the project folder. A file saved directly
   in the project folder stays local with a notice; deleting or renaming `Project Files`
   is refused.
9. Settings → Synkmapp → Flytta… to `D:\Synka` (or another drive): refused inside OneDrive;
   with a file open, nothing changes; otherwise every project comes back under the new folder,
   an unsynced edit lands in `Synka - återställda filer`, the old folder is gone. Move back
   with "Återställ standardplats".

## Still open

1. **Sign-in verification**: complete a real browser sign-in on clean Windows 10/11 accounts.
   The APS app registers exactly `http://localhost:47863/` (`auth::REDIRECT_PORT`). Check
   busy-port errors, retry after closing the browser, and sign-out during a pending attempt.
2. **Implicit locks** for Revit/AutoCAD (lock on open-for-write, unlock on close).
   Needs `NOTIFY_FILE_OPEN_COMPLETION` plus a sharing probe; the lock API exists
   (`dm::set_reserved`). Confirm the public status of the reserved PATCH first
   (`docs/specs/aps-endpoints.md`).
3. Thumbnails for dehydrated files and `synka://` deep links: not started. Custom Explorer verbs,
   columns and the Windows 11 status flyout need a Cloud Files handler configuration that keeps
   the sync root visible to Explorer.
4. The sync best-practices audit (internal) compares the engine with other sync clients;
   its open items are listed above.

## Product ideas (ordered by daily value)

1. **Custom columns** Låst av / Version / Ändrad av: built, not attached to the shipped manifest.
2. **"Frigör utrymme" per folder**, not only per project, plus an automatic policy: dehydrate
   clean files not opened for N days. Respect pins.
3. **Offline pin**: Explorer's own *Behåll alltid på denna enhet* verb is available through
   `AllowPinning`; keep its file and folder behavior covered by live smoke tests.
4. **Notifications on remote change** to a file you have open or pinned, with an action to
   open it.
5. **Lock-aware read-only**: done (files locked by someone else are read-only; your own
   locks stay writable). Verify with a colleague's lock.
6. **Revit/AutoCAD friendliness**: keep `.rvt` temp and backup files out of uploads (ignore
   rules exist; add tests with real Revit backup names from `Kopmannen`).
7. **Bandwidth limit** setting for uploads and downloads (token bucket in the HTTP layer).
8. **Multiple accounts / hubs** in one sync root: today the root is per Windows user; the
   sync-root id already contains the user id, so extend the hub layer, not the registration.
9. **Diagnostics export**: done (`export_diagnostics`).
10. **Auto-update via an update channel**: kept in code, **off by default** (`update_channel`
    empty). Auto-install when quiet and restart by Windows are built; a channel and a real
    signed release are needed to test them.

## Engineering debt

- Coverage: unit tests exist for ignore rules, identity, store, delta mapping, upload naming,
  watcher. Missing: an end-to-end test that runs against a throwaway ACC project (needs a
  dedicated test folder and a test account, never a colleague's data).
- Warnings: none; CI runs clippy with `-D warnings`. Keep it that way (remove or wire dead
  code, never silence it).
- Local checks without Windows: `cargo clippy --target x86_64-pc-windows-gnu`, and the unit
  tests under Wine with a stub `cldapi.dll` (see `README.md`). The engine behaviour tests in
  `src/sync/mod.rs` (`fixture()`) run against a real temp folder and DB.
- The UI test harness is manual (screenshots). Add a fake-engine pipe test runner.
- Never commit any token, key or `.pfx`.

## Release checklist (before the next release)

1. `git` remote exists (`byggstyrning/synka`, public; the repository of an earlier package identity is archived). Done.
2. Export the `CN=FastADC` certificate with its private key to a `.pfx` kept outside git and
   OneDrive, and store it as the repository secrets `SIGNING_PFX_BASE64` and
   `SIGNING_PFX_PASSWORD`. Steps in `packaging/README.md`.
3. Check that CI (`.github/workflows/ci.yml`) is green on `main`; its first run also proves
   the MSIX builds on a GitHub runner.
4. Actions -> Release -> Run workflow, with Swedish release notes in `notes`.
   (`packaging\new_release.ps1 -DryRun` still works locally and hands over to the workflow.)
5. Copy the signed zip to the OneDrive share for Byggstyrning's staff (`packaging\share_release.ps1`).
6. Add the release rule to `.claude\CLAUDE.md` the same way Koordinatverktyget does.

## Migration from an earlier package identity

For a PC that has the app installed under an earlier package identity:

1. Check that the old app shows synced with nothing pending.
2. Run the new `Install.cmd`. It asks to remove the old package first, then installs Synka.
3. Sign in again and choose the projects again.
4. Delete `%USERPROFILE%\BDC` after verifying that the new folder has the files.

The steps match `packaging/delivery/README.txt`.

## Decisions taken (do not reopen without the user)

- Own APS app "Synka" (PKCE, public client). Client id is a public value.
- Own sync root `%USERPROFILE%\Synka`, never the other product's `ACCDocs` root.
- Auto-dehydration is allowed (Windows may free space; pinned files are never evicted).
- Sideload MSIX, same deployment model as Koordinatverktyget. Not the Store.
- Swedish UI by default, English toggle.
- Autodesk Desktop Connector is not used or required at runtime. Synka can run alongside it.
- Releases are public: this repository's GitHub releases and, once approved, the Design and Make
  Marketplace. The in-app update channel stays off by default.

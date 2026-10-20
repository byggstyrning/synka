# Synka

Synka syncs construction documents from **ACC Docs** (Autodesk Construction Cloud / Autodesk
Forma), **StreamBIM** and **Interaxo** to an ordinary folder in Explorer
(`%USERPROFILE%\Synka\<hub>\<project>\<top folder>`). For ACC the top folder is `Project Files`
(a user who only reaches some folders sees each of them instead); StreamBIM projects and Interaxo
rooms have one top folder, `Dokument`. Files download when you open them, and your saves go back
as new versions. Synka can run alongside Autodesk Desktop Connector without touching it.

- **Light:** one Rust engine (tray icon, sync, Explorer integration); the window runs only while
  it is open. Idle polling writes nothing and logs nothing.
- **Native Windows sync:** the Cloud Files API (the same as OneDrive): placeholders, Explorer
  availability/status, and the built-in right-click commands *Always keep on this device* and
  *Free up space* (shown in Swedish on a Swedish Windows installation).
- **Safe by default:** safe saves from Office, AutoCAD and Revit become new versions; conflicts
  keep both copies; a mass delete waits for your confirmation; a reset never throws away
  unsynced work (it is saved to `%USERPROFILE%\Synka - återställda filer` first).
- **Starts at sign-in:** the engine starts at sign-in and Windows restarts it after a crash.
  New versions are shared internally; there is no online update check.
- **Easy to start over:** in Settings, rebuild every project or move the sync folder to another
  drive (a move is a full rebuild, with the same rescue of unsynced work).

Swedish UI by default, English toggle.

## Use and data (read first)

- Internal use by Byggstyrning staff only.
- Synka never sends file content anywhere except between the PC and the platform the file
  belongs to: Autodesk (the APS endpoints and Autodesk's file storage), StreamBIM (the signed-in
  region's servers and the S3 storage they hand out upload links for) or Interaxo (its API hosts).
  Synka has no telemetry.
- The update check is off by default (`update_channel` is empty). Staff update from the internal
  OneDrive share.
- Each ACC hub's account admin must approve the Client ID as a Custom Integration, including
  client-owned hubs.
- The undocumented dm/v3 desktop-sync feed is off by default (`desktop_sync_feed`, settings.json
  only; enabling it requires written approval from Autodesk). Changes are found with the
  documented data/v1 folder listing.
- Synka never binds Autodesk's named pipes and never emulates Autodesk Desktop Connector services.
- StreamBIM sign-in uses the user's e-mail and password (plus an authenticator code when the
  account has MFA). The password goes once to StreamBIM and is never stored or logged.
- Interaxo needs the organisation's own registered client (bring your own Client ID); see
  [Platforms](#platforms).

## Install (users)

Download `Synka-<version>-sidladdning.zip` from the internal share, unpack all of it and
double-click `Install.cmd`. See `packaging/delivery/README.txt`.

## Repository

| Path | What |
|---|---|
| `src/` | The engine `synka-engine.exe` (Rust). Start at `src/main.rs`; the sync engine is `src/sync/`. |
| `src/sync/mod.rs` | Scheduler, remote apply, local changes, conflicts, deletes and renames. |
| `src/sync/upload.rs` | The outbox: uploads, mkdir, rename, move, delete, with retries. |
| `src/sync/reset.rs` | Rebuilding a project or folder from ACC without losing work. |
| `src/sync/relocate.rs` | Moving the sync folder (a full rebuild at the new place). |
| `src/sync/naming.rs` | How ACC names map to Windows names (one place). |
| `src/cf/` | Cloud Files API: sync root, callbacks, placeholders. |
| `src/shell/` | Packaged Explorer COM command/state implementations; native Cloud Files controls are active. |
| `src/remote/` | Provider dispatch: the one door from the engine to ACC, StreamBIM and Interaxo. |
| `src/api/` | Provider clients: ACC/APS (`dm.rs`, `oss.rs`, optional desktop-sync feed, off), `streambim.rs`, `interaxo.rs`. |
| `src/auth.rs`, `src/streambim_auth.rs` | Sign-in: OAuth + PKCE for ACC and Interaxo; StreamBIM password + MFA. |
| `src/store.rs` | SQLite state (one file, WAL). |
| `ui/` | The window `Synka.exe` (C#, WinUI 3) and the MSIX manifest. |
| `packaging/` | MSIX build, signing, sideload zip, release scripts. |
| `.github/workflows/` | CI (pull requests and main) and Release (one click). |
| `docs/` | `PROVIDERS.md` (platforms and the provider contract), `IPC.md` (engine ⇄ window protocol), `ROADMAP.md` (state and next steps), `RESILIENCE.md` (test laboratory). |
| `docs/specs/` | Clean-room behavioural specs: APS, StreamBIM and Interaxo endpoints, Cloud Files, save patterns and the ignore list. |
| `DESIGN.md` | Architecture and key decisions. |

## Platforms

Sign in on the **Konton** page: one ACC account, one Interaxo account, and one StreamBIM
account per StreamBIM server (a region such as Europe or Japan, or a custom `https://` address).
The page lists every account with its server, status and synced projects, with sign-out next to
each. Projects of every signed-in account appear on the Projects page, grouped by hub. Architecture and limits:
[docs/PROVIDERS.md](docs/PROVIDERS.md).

| Platform | Sign-in | What syncs | Not supported |
|---|---|---|---|
| ACC | Autodesk browser sign-in (PKCE), built-in or own APS Client ID | everything in Docs | — |
| StreamBIM | server (region or custom https address), e-mail, password, authenticator code if MFA is on; several servers at once | the project's document library (folders and documents, with revisions) | SSO-only accounts; files over 5 GB; locks |
| Interaxo | Interaxo browser sign-in (PKCE) with the organisation's own Client ID | simple folders and files of each room you are a member of | creating folders, renaming and moving (the public API has none: such changes stay on this PC with a notice); registers, entries and links; locks |

### Set up Interaxo (bring your own Client ID)

Interaxo has no self-service app registration. Ask Interaxo support for a **Native** (public)
client with Authorization Code + PKCE and refresh tokens, access to the API audience
`https://api.interaxo.com/v1/` with offline access, and the redirect URI
`http://localhost:47864/` registered exactly (or another loopback URI you then enter in Synka).
Enter the Client ID (and the client secret only if Interaxo insists on a confidential client)
under **Konton → Interaxo → Avancerat**, save, and sign in. The secret is stored
DPAPI-protected and never shown again.

## Build

On Windows with Rust (stable, MSVC) and the .NET 8 SDK:

    cargo test
    cargo build --release
    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests            # unsigned MSIX
    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests -Sign -Install

Details, signing and the certificate: `packaging/README.md`.

### Autodesk sign-in

The built-in APS client uses Authorization Code + PKCE with the registered callback
`http://localhost:47863/`. Keep the host, port and trailing slash identical in the
Autodesk app registration. If the port is unavailable, Synka reports it instead of
substituting an unregistered callback. Clicking **Logga in** again replaces a pending
attempt; rapid clicks within one second share the same attempt.

### Use your organization's APS app

Synka works with its built-in Autodesk app by default. To use an organization's own
Client ID, open **Settings → Autodesk APS app**, sign out, enter the public Client ID
and choose **Test and save**. Complete the fresh browser sign-in: Synka verifies the
public PKCE token exchange and profile before saving. Failed or cancelled tests
keep the previous setting and your draft. Test credentials are discarded. The
engine restarts when the effective ID changes; then sign in to Synka to sync.
**Use Synka default** clears the optional override and restores the default.
Project selections and local files stay in place.

The organization's app must support Authorization Code + PKCE as a public
**Desktop, Mobile, Single-Page App**, with the callback registered exactly as
`http://localhost:47863/`. **Traditional Web App is not supported** because it
requires a client secret. The test proves APS sign-in compatibility; ACC access
still depends on the hub's integration and your permissions.
Enable the Autodesk APIs Synka uses, and have the hub
administrator provision that Client ID as a custom integration for the hubs it
should access. Existing user and project
permissions still apply. Synka asks for the Client ID only; no client secret is used.
See Autodesk's [PKCE guide](https://aps.autodesk.com/en/docs/oauth/v2/tutorials/get-3-legged-token-pkce/get-3-legged-token-pkce/)
and [custom integration setup](https://help.autodesk.com/cloudhelp/ENG/Docs-Admin/files/hub-administration/Custom_Integrations.html).

The setting is optional and applies to this Windows user's Synka installation.
An empty or whitespace-only value uses the build-time `SYNKA_APS_CLIENT_ID` when
configured, then Synka's built-in ID. Invalid values and failed settings saves leave
the current override unchanged.
Changing the effective ID requires a signed-out session with no active sync work. The UI
shows a reason and lets you try again after resolving it.

Failed activity rows show the specific safe error reason on hover or keyboard
focus. Revit cloud worksharing models cannot be overwritten by ordinary file
uploads; synchronize and publish those models from Revit. Ordinary file updates
preserve the existing remote MIME type.
Queue IDs are persisted and never reused after cancellation, preventing a late
worker response from changing a replacement job.

### Stress and failure testing

Run the isolated resilience laboratory on Windows (PowerShell 7):

    pwsh -NoProfile -File tools/test-resilience.ps1 -Profile Stress -Seed 42

It simulates network failures, rapid file changes and IPC command bursts, checks recovery
and data preservation, and saves reproduction logs plus JSON/JUnit reports. Quick runs in
CI; Stress and Soak are available locally or through the **Resilience laboratory** workflow.
The .NET 8 SDK is required for the real C# protocol-client audit over private Windows pipes.
See [docs/RESILIENCE.md](docs/RESILIENCE.md) for profiles, coverage and limitations.

### Checking the engine without Windows

The engine cross-compiles from Linux, and its unit tests run under Wine:

    rustup target add x86_64-pc-windows-gnu      # plus mingw-w64 and wine64 from the distro
    cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
    tools/test-under-wine.sh

The MSIX manifest can be checked against the schemas MakeAppx uses (downloaded once from
NuGet; needs `python3-lxml`):

    tools/validate-manifest.py

Wine has no Cloud Files API; the script builds a stub `cldapi.dll`, so under Wine every file
looks like a plain file. The engine behaviour tests in `src/sync/mod.rs` (`fixture()`) run
against a real temporary folder and database that way. Everything that needs real placeholders
is covered by the `#[ignore]` tests on Windows and by CI.

## Release

Actions → **Release** → *Run workflow* (choose patch/minor/major, write the notes). The workflow
bumps the version, tags, tests, builds and signs, then publishes the release to the private
repository. The signed package is then shared internally (OneDrive, `share_release.ps1`). Setup
(secrets, signing): `packaging/README.md`, section "Release from GitHub Actions".

The version has one source: `[package] version` in `Cargo.toml`.

## Trademarks

Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma and Revit are registered
trademarks or trademarks of Autodesk, Inc., and/or its subsidiaries and/or affiliates. Synka is
an independent product of Byggstyrning and is not affiliated with, endorsed by or sponsored by
Autodesk.

## Rules of the codebase

- No warnings: CI runs clippy with `-D warnings`. Dead code is removed or wired, not silenced.
- User-facing text is Swedish; code and comments are English.
- Never log tokens or file contents. The diagnostics export contains neither.
- Never change the COM CLSIDs, the custom-state ids, the provider GUID or the notification-icon GUID after a release.

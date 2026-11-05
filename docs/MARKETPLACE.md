# Synka in the ACC App gallery

ACC account admins install apps from **Account Admin → Apps** (the *App gallery*). Apps reach the
gallery by being published on Autodesk's **Design and Make Marketplace** with an APS Client ID.
Until Synka is published there, admins add it under **Custom integrations** instead (below).

## State (4 November 2026)

| Step | State |
|---|---|
| APS app | Done. Developer Hub *Byggstyrning*, app **Synka**; Client ID `tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA` (`config::DEFAULT_CLIENT_ID`); Desktop/Mobile/SPA, PKCE, callback `http://localhost:47863/` |
| Publisher profile | Done. Publisher Corner (https://apps.autodesk.com/en/MyUploads): Jonatan Jacobsson, byggstyrning.se, support `info@byggstyrning.se`; My Page: company *Byggstyrning* |
| Publisher Agreement | Accepted 3 November 2026 (version of 3 May 2026) |
| Privacy policy | `docs/PRIVACY.md`, published at https://github.com/byggstyrning/synka/blob/main/docs/PRIVACY.md once merged to main; the About page links to it from 0.3.6. Still a draft until counsel has read it |
| Release | [v0.3.6](https://github.com/byggstyrning/synka/releases/tag/v0.3.6), a pre-release with `Synka-0.3.6.zip`. **Unsigned** (the release key was not available), so it cannot be installed; v0.3.5 stays the latest signed release |
| Listing | App ID `3397524952584336133`, status **Pending review**. Platform is set to **Web**; it should be **Windows 64-bit** (owner changes it in the portal). The earlier draft, App ID `1478764073777710332`, no longer exists |
| Listing file | Which file and which build (default or ACC-only, below) the listing points to: to be confirmed by the owner. The earlier draft's File link was the unsigned `Synka-0.3.6.zip` above |
| Listing values | As entered on the earlier draft: price **Free**; compatibility **Autodesk Construction Cloud → ACC**; categories **Integration**, **Construction Management**; stores US, EMEA, Australia; Forge Client Id `tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA`; data permission **Docs** only (not Account Administration) |

### Which build: default or ACC-only

The listing is for ACC (compatibility ACC, data permission Docs), while the default build also
syncs StreamBIM and Interaxo. The
ACC-only build is the same app with ACC as its one provider: the engine never signs in to, lists,
polls or contacts StreamBIM or Interaxo (not even the StreamBIM region list), ignores
`interaxo_enabled`, and leaves any earlier StreamBIM or Interaxo session file on disk untouched.
The window hides the StreamBIM and Interaxo sections of the Konton page and names ACC alone on
the About page. The owner decides which build is submitted.

| | Default build | ACC-only build |
|---|---|---|
| Engine | `cargo build --release` | `cargo build --release --features acc-only` |
| Package | `build_msix.ps1` | `build_msix.ps1 -AccOnly` |
| `settings.build_providers` (`docs/IPC.md`) | `["acc","streambim","interaxo"]` | `["acc"]` |

Both builds have the same package identity (`Byggstyrning.Synka`) and take their version from
`Cargo.toml`, so an ACC-only package and a default package of the same version cannot be told
apart by their file names, and `Install.cmd` skips a version that is already installed. Keep the
two apart: build the one that is submitted, upload it, and do not publish the other under the
same version.

### Building the signed Marketplace zip

The build must be signed with the release key (`CN=FastADC`, thumbprint
`F1085485D4950C413AFE6A6C1E1F9F63A16D54FE`, see `packaging/README.md`), so it runs on the release
machine or on a machine where that key has been restored. With `<ver>` the version in `Cargo.toml`:

    powershell -NoProfile -File packaging\build_msix.ps1 -Sign -AccOnly     # without -AccOnly: the default build
    powershell -NoProfile -File packaging\make_delivery.ps1 -SkipBuild
    Copy-Item dist\Synka-<ver>-sidladdning.zip dist\Synka-<ver>.zip

The Marketplace gets the same content as the sideload zip, named without `-sidladdning`. Upload
it where the listing's File link points (`gh release upload v<ver> dist\Synka-<ver>.zip
--clobber` for a file on a GitHub release), once the owner has confirmed that link.

Autodesk's review checklist (shown in the form): description and usage instructions of at least
1500 characters each, at least 4 screenshots, company URL and privacy policy URL, a binary app
file, a three-part version number, no unhandled exceptions, and uninstall removes all files.

## Publishing checklist

1. (Done) Accept the Design and Make Marketplace Publisher Agreement. Key terms, version of
   3 May 2026: free apps pay 0 % commission (§20.11);
   Byggstyrning is solely responsible for the app, support and security incidents (§4.1, §5.2) and
   indemnifies Autodesk (§18); Autodesk gets a perpetual, royalty-free licence to distribute the app
   and use our name and marks to market it (§14.3); Autodesk may remove the app at any time (§2)
   and change the agreement unilaterally (§19.1); no public statements about the Marketplace
   without Autodesk's approval (§19.7); California law, San Francisco courts (§19.5); the
   Independent Controller Data Protection Addendum (Exhibit A) applies.
2. Link `docs/PRIVACY.md` from the listing. Whether it is also published on byggstyrning.se is the
   owner's decision.
3. (Done in 0.3.6) Link the privacy policy from inside Synka (agreement §5.1): the About page.
4. Decide the end-user licence: Autodesk's Standard EULA (https://apps.autodesk.com/en/Public/EULA)
   applies unless we supply our own (§4.3).
5. Fill in the listing:
   - Title: Synka
   - Type: desktop app for Windows; compatible with Autodesk Construction Cloud / Forma (Docs)
   - APS Client ID: `tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA`
   - Price: free
   - Short description: Sync ACC Docs projects to an ordinary folder in Windows Explorer.
     Files download when you open them; your saves go back as new versions.
   - Icon: `ui/Assets/Square150x150Logo.scale-400.png` (check the Marketplace icon guidelines)
   - Screenshots: the Projects page, Explorer with the sync folder, the Konton page
   - Support: info@byggstyrning.se; privacy policy URL from step 2
   - Help page: the Client ID, the app name and the admin steps below (BIM 360 publisher
     guideline "HTML help page")
6. Upload the product as one ZIP (the sideload zip from `packaging/make_delivery.ps1`, of the build
   the owner chose above). Autodesk reviews it (up to two weeks), then builds the final package
   for us to approve.

Before submitting, check against the Marketplace product guidelines and the Forma / BIM 360
publisher guidelines: only documented production APS APIs (the dm/v3 feed stays off), 3-legged
OAuth with the Autodesk sign-in page, no public or shareable links to project data.

## For ACC account admins

### From the App gallery (once Synka is published)

1. Go to **Account Admin → Apps → Featured Apps** and open **Synka**.
2. Choose **Install** and confirm the access Synka asks for.
3. Users then sign in to Synka with their Autodesk account. Their ACC project permissions still
   decide what they can see and change.

### As a custom integration (works today)

1. Go to **Account Admin → Custom integrations → Add custom integration**.
2. Enter Client ID `tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA`, name *Synka*, and a
   description that includes the Client ID (ACC does not show it again later).
3. Save. Repeat for every ACC account (hub) that should sync, including client-owned hubs.

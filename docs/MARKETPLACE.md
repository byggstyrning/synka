# Synka in the ACC App gallery

ACC account admins install apps from **Account Admin → Apps** (the *App gallery*). Apps reach the
gallery by being published on Autodesk's **Design and Make Marketplace** with an APS Client ID.
Until Synka is published there, admins add it under **Custom integrations** instead (below).

## State (3 November 2026)

| Step | State |
|---|---|
| APS app | Done. Developer Hub *Byggstyrning*, app renamed from "Better Desktop Connector" to **Synka**; Client ID unchanged (`tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA`, `config::DEFAULT_CLIENT_ID`); Desktop/Mobile/SPA, PKCE, callback `http://localhost:47863/` |
| Publisher profile | Done. Publisher Corner (https://apps.autodesk.com/en/MyUploads): Jonatan Jacobsson, byggstyrning.se, support `info@byggstyrning.se`; My Page: company *Byggstyrning* |
| Publisher Agreement | Accepted 3 November 2026 (version of 3 May 2026) |
| Privacy policy | `docs/PRIVACY.md`, published at https://github.com/byggstyrning/synka/blob/main/docs/PRIVACY.md; the About page links to it from 0.3.6 |
| Release | [v0.3.6](https://github.com/byggstyrning/synka/releases/tag/v0.3.6), a pre-release with `Synka-0.3.6.zip` (top folder `Synka-0.3.6/`, installed with `Install.cmd`). Signed with the **earlier** `CN=FastADC` key `03F6B767F3CFC690AB55333AAE6093AC0C8CF233` (valid to 2027-10-16), because the current release key was not available; the zip carries that certificate. The release gate and the BoomerHubb channel did not run |
| Listing type | **Browser-based App**, App ID `3397524952584336133` (English, free). A browser-based listing has no app file: the Marketplace links to our download page and the ACC App gallery only grants access to the Client ID. Desktop sync tools in the gallery (odrive, Graitec FILE SYNC, CDE Sync) are listed the same way |
| Listing, step 1 | Saved. Name, short and long description (1,758 characters of text), privacy policy URL, version 0.3.6, Web Service Url `https://github.com/byggstyrning/synka/releases`, icon, 5 captioned screenshots (Overview, Projects, Explorer, Settings, About), the video below, usage instructions (with installing and uninstalling, since this type has no install field), support, additional info, known issues, Learn More URL |
| Listing, step 2 | Saved: login type **Sign in with Autodesk account**; price **Free**; compatibility **Autodesk Construction Cloud → ACC**; categories **Integration**, **Construction Management**; stores US, EMEA, Australia; Forge Client Id `tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA`; data permission **Docs** only (not Account Administration); workflow video https://www.youtube.com/watch?v=mmGS4ut3zKE (unlisted, 4:13), time stamp 2026-11-03 09:00:38 UTC (the Autodesk sign-in in Synka's log) |
| Status | *Ready to submit*; **Submit** not pressed yet |
| Earlier draft | App ID `1478764073777710332` (Desktop-based, Win64) stays at *Info incomplete* because a desktop listing needs an uploaded app file that Autodesk repackages. It is superseded by the browser-based listing and can be deleted |

### Building the download zip

The listing points at the GitHub releases page, so a new version needs no Marketplace upload,
only a new version number and description in the listing. Normally the build is signed with the
release key (`CN=FastADC`, thumbprint `F1085485D4950C413AFE6A6C1E1F9F63A16D54FE`, see
`packaging/README.md`), on the release machine or on a machine where that key has been restored:

    powershell -NoProfile -File packaging\build_msix.ps1 -Sign
    powershell -NoProfile -File packaging\make_delivery.ps1 -SkipBuild

The public download has the same content as the sideload zip, named `Synka-<version>.zip` with
the top folder `Synka-<version>/` instead of `-sidladdning`. Upload it to the release
(`gh release upload v<version> dist\Synka-<version>.zip --clobber`).

v0.3.6 was built on a machine without the release key: `packaging\FastADC.cer` was replaced, in
the working tree only, by the public certificate of the earlier key, so `build_msix.ps1 -Sign
-SkipEngineBuild` and `make_delivery.ps1 -SkipBuild` signed and checked with that key; the file was
restored afterwards. The engine had been built with the committed certificate, so the updater's
pinned certificate is unchanged.

Autodesk's review checklist (shown in the form): description and usage instructions of at least
1500 characters each, at least 4 screenshots, company URL and privacy policy URL, a binary app
file, a three-part version number, no unhandled exceptions, and uninstall removes all files.

The step 2 time stamp field takes **local** time as `MM/DD/YYYY HH:mm:ss` and converts it to UTC
when saved; text in any other format is replaced by the current time and the save fails with
"Missing Video Stamp".

## Publishing checklist

1. (Done) Accept the Design and Make Marketplace Publisher Agreement. Key terms, version of
   3 May 2026: free apps pay 0 % commission (§20.11);
   Byggstyrning is solely responsible for the app, support and security incidents (§4.1, §5.2) and
   indemnifies Autodesk (§18); Autodesk gets a perpetual, royalty-free licence to distribute the app
   and use our name and marks to market it (§14.3); Autodesk may remove the app at any time (§2)
   and change the agreement unilaterally (§19.1); no public statements about the Marketplace
   without Autodesk's approval (§19.7); California law, San Francisco courts (§19.5); the
   Independent Controller Data Protection Addendum (Exhibit A) applies.
2. Publish `docs/PRIVACY.md` on byggstyrning.se and link it from the listing.
3. Link the privacy policy from inside Synka (agreement §5.1): a link in Settings or Konton.
4. Decide the end-user licence: Autodesk's Standard EULA (https://apps.autodesk.com/en/Public/EULA)
   applies unless we supply our own (§4.3).
5. Fill in the listing:
   - Title: Synka
   - Type: Browser-based App (Synka itself is a Windows desktop app downloaded from GitHub);
     compatible with Autodesk Construction Cloud / Forma (Docs)
   - APS Client ID: `tFf8sYIHNWSrUiahpJPPxVujQTC3LJfAAEY3GZK3hEeLQ6jA`
   - Price: free
   - Short description: Sync ACC Docs projects to an ordinary folder in Windows Explorer.
     Files download when you open them; your saves go back as new versions.
   - Icon: `ui/Assets/Square150x150Logo.scale-400.png` (check the Marketplace icon guidelines)
   - Screenshots: the Projects page, Explorer with the sync folder, the Konton page
   - Support: info@byggstyrning.se; privacy policy URL from step 2
   - Help page: the Client ID, the app name and the admin steps below (BIM 360 publisher
     guideline "HTML help page")
6. Submit the browser-based listing. Autodesk reviews it (up to two weeks); once published, Synka
   appears in the ACC App gallery.

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

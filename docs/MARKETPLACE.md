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
| Privacy policy | `docs/PRIVACY.md`, published at https://github.com/byggstyrning/synka/blob/main/docs/PRIVACY.md once merged to main; the About page links to it from 0.3.6 |
| Listing | Draft saved, App ID `1478764073777710332` (English, Win64, free), status *Info incomplete*. Filled in: name, short and long description, usage, install/uninstall, support, additional info, known issues, privacy policy URL, version 0.3.6, icon, Learn More URL |
| Still missing before **Submit** | App file `Synka-0.3.6.zip` (signed build), at least 4 screenshots, then step 2 (price Free, product compatibility, Forge Client ID) |

### Building the Marketplace zip

The build must be signed with the release key (`CN=FastADC`, thumbprint
`F1085485D4950C413AFE6A6C1E1F9F63A16D54FE`, see `packaging/README.md`), so it runs on the release
machine or on a machine where that key has been restored:

    powershell -NoProfile -File packaging\build_msix.ps1 -Sign
    powershell -NoProfile -File packaging\make_delivery.ps1 -SkipBuild
    Copy-Item dist\Synka-0.3.6-sidladdning.zip dist\Synka-0.3.6.zip

The Marketplace gets the same content as the sideload zip, named without `-sidladdning`.
Upload it in the listing with **Choose File** (max 600 MB), or share it from cloud storage
under **File link**.

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
2. Publish `docs/PRIVACY.md` on byggstyrning.se and link it from the listing.
3. Link the privacy policy from inside Synka (agreement §5.1): a link in Settings or Konton.
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
6. Upload the product as one ZIP (the sideload zip from `packaging/make_delivery.ps1`). Autodesk
   reviews it (up to two weeks), then builds the final package for us to approve.

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

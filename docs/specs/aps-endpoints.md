# APS endpoints used by Synka

Clean-room behavioural specification. It describes what Synka calls, in what order, and what
it reads back. It is written from Synka's own code and from public Autodesk Platform Services
(APS) documentation. Public reference groups are named where they are known. Where a call
could not be matched to a public section, the row says `undocumented — review`.

Base URL: `https://developer.api.autodesk.com` (all calls except userinfo). Every call except
the presigned S3 URLs carries `Authorization: Bearer <access token>`.

## Authentication

| Step | Request | Public reference |
|---|---|---|
| Authorize | Browser to `/authentication/v2/authorize` with `response_type=code`, `client_id`, `redirect_uri=http://localhost:47863/`, `scope`, `code_challenge` (S256), `state` | Authentication v2, PKCE for desktop and mobile apps |
| Token | `POST /authentication/v2/token`, form `grant_type=authorization_code` with `code_verifier` | Authentication v2 |
| Refresh | `POST /authentication/v2/token`, form `grant_type=refresh_token`; the new refresh token replaces the old one | Authentication v2 |
| User | `GET https://api.userprofile.autodesk.com/userinfo` (`sub`, `name`, `email`) | OpenID Connect userinfo, APS user profile |

Scopes requested: `data:read data:write data:create user-profile:read`. The app is a public
client (no secret). `data:search` and `account:read` are not used by any call below and are
not requested. The registration must be Desktop, Mobile, Single-Page App with callback
`http://localhost:47863/`.

## Hubs and projects (Project API v1)

| Call | Path and query | Fields read |
|---|---|---|
| List hubs | `GET /project/v1/hubs?filter[extension.type]=hubs:autodesk.bim360:Account` | `data[].id`, `attributes.name`, `attributes.region` |
| List projects | `GET /project/v1/hubs/{hub}/projects?page[limit]=200`, all pages via `links.next.href` | `id`, `attributes.name`, `attributes.scopes`, root folder relationship. Projects without a root folder are skipped. |
| Get project | `GET /project/v1/hubs/{hub}/projects/{project}` | `attributes.scopes`, root folder |
| Top folders | `GET /project/v1/hubs/{hub}/projects/{project}/topFolders?projectFilesOnly=true&excludeDeleted=true` | `data[].id`, `attributes.name` or `displayName`. Hidden entries are dropped. Only folder URNs are kept. |

Public reference: Project API v1 (hubs, projects, topFolders).

## Data Management (Data API v1)

Project paths are `/data/v1/projects/{project}`. URNs in path segments are percent-encoded
(RFC 3986 unreserved set kept, everything else `%XX`).

| Call | Method and path | Notes |
|---|---|---|
| Folder listing | `GET .../folders/{urn}/contents?page[limit]=200`, all pages | Used for every folder on each poll. An HTTP 200 error body, a repeated next link or a cyclic next link fails the listing; it is never read as an empty folder. |
| Item and tip | `GET .../items/{lineage}`, then `GET .../items/{lineage}/tip` if `included[]` lacks the tip | Read before an upload so that a new version is not based on stale metadata. |
| Web link | `links.webView.href` on item or folder | Used for "open in ACC" and "copy link". |
| Create folder | `POST .../folders` (`folders:autodesk.bim360:Folder`) | Parent is a folder URN. |
| Create storage | `POST .../storage` (`objects`, target folder) | Returns the storage URN used for the upload. |
| Create item | `POST .../items` (`items:autodesk.bim360:File`) | Uses the storage URN from the step above. |
| New version | `POST .../versions` (`versions:autodesk.bim360:File`) | Same storage flow. Revit cloud-workshared models are never overwritten this way. |
| Rename file | `PATCH .../versions/{tip version urn}` `attributes.name` | Renames the tip version. |
| Move file | `PATCH .../items/{lineage}` `relationships.parent` | |
| Rename or move folder | `PATCH .../folders/{urn}` `attributes.name` and/or `relationships.parent` | |
| Delete file | `POST .../versions` with extension `versions:autodesk.core:Deleted` | Creates a deleted version; the item is removed from the listing. |
| Delete folder | `PATCH .../folders/{urn}` `attributes.hidden = true` | |
| Lock or unlock | `PATCH .../items/{lineage}` `attributes.reserved` | `undocumented — review`. The reserved attribute is public (item responses include it), but the PATCH call was not confirmed in public reference text. Synka checks the response and fails if the value does not change. |

Public reference: Data Management v2 (items, versions, folders, storage, contents, tip).
Lock behaviour is described in the APS blog post on locked ACC files (2024), which says that a
reserved lineage returns `403 ERR_WIP_ERROR` for writes by other users.

### JSON:API to item mapping

Synka reads JSON:API documents and maps them to its own item type:

- **Folder:** id from the URN suffix of `data.id`; name from `attributes.name` (or
  `displayName`); parent from `relationships.parent`; create and modify times from
  `createTime` and `lastModifiedTime` (RFC 3339 to milliseconds). Hidden folders are dropped.
- **File:** id from the URN suffix (lineage). The tip version comes from `included[]` (type
  `versions`) matched by `relationships.tip`. Name, version number, storage size, storage URN,
  version URN, last modified user and reservation holder come from the tip. Hidden items and
  names ending `.url`, `.lnk` or `.wsh` are dropped.
- Missing or unparsable times become 0.

## File transfer (OSS v2)

| Call | Method and path | Notes |
|---|---|---|
| Download URL | `GET /oss/v2/buckets/{bucket}/objects/{key}/signeds3download?minutesExpiration=60` | Returns `url`, `size`, `status`. |
| Download bytes | `GET {presigned url}` with no `Authorization` header; `Range: bytes=<offset>-` when resuming | Bytes are never decoded. Content-Encoding is not applied. |
| Upload URLs | `GET .../signeds3upload?parts=N&firstPart=1&minutesExpiration=60` (up to 25 URLs per call; `uploadKey` on later calls) | Returns `uploadKey` and `urls[]`. |
| Part upload | `PUT` each part to its presigned URL, no `Authorization` header | Parts are 8 MiB, last part smaller. A part is retried up to 3 times; a 403 means the URL expired and is requested again. |
| Complete | `POST .../signeds3upload` with JSON `{"uploadKey", "size"}` | |

Public reference: OSS v2 (signed S3 download and upload). The `scopes=` query parameter that
Synka appends when a project carries an OSS scope override is `undocumented — review`.

## Desktop-sync feed (Data Management dm/v3)

`POST /dm/v3/projects/{guid}/api:desktop-sync-status` and the paged `folders:desktop-sync`
and `documents:desktop-sync` feeds. Opaque sync tokens; page size 1000.

- Status: `undocumented — review`. Not in public APS reference.
- Off by default. Synka calls these only when `desktop_sync_feed` is `true` in `settings.json`.
  The setting is not exposed in the window or the IPC protocol, and enabling it requires
  written approval from Autodesk. When it is off, the engine never calls dm/v3 and drops any
  stored feed cursor.
- The default change detection is the documented folder listing above, for populated folders
  only, with the polling tiers in `DESIGN.md`.

## Calls Synka does not make

No search endpoints, no account administration or hub administration endpoints, and no
Revit-link or local-service endpoints.

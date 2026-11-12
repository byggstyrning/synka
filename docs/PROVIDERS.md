# Document providers (ACC, StreamBIM, Interaxo)

Synka syncs documents from more than one platform. Each platform is a **provider**. This file is
the architecture and the contract every provider implements. Read `DESIGN.md` and `docs/IPC.md`
first.

| Provider | `Provider` | Sign-in | Client id | Hub (2nd path level) | Project (3rd level) |
|---|---|---|---|---|---|
| Autodesk ACC | `acc` | APS Authorization Code + PKCE (browser) | built-in, overridable (`aps_client_id`) | ACC account | ACC project |
| StreamBIM | `streambim` | server (region or custom https address) + e-mail + password, optional TOTP (MFA) code | none: StreamBIM has no third-party OAuth client registration | one hub per server account: `StreamBIM` (region `app`), `StreamBIM <Region title>`, `StreamBIM (<host>)` | StreamBIM project |
| Interaxo | `interaxo` | OIDC Authorization Code + PKCE (Auth0, browser) | **bring your own**: `interaxo_client_id` (+ optional secret, redirect URI) | `Interaxo – <community>` | room |

API research behind this file, with every undocumented point listed, is in `docs/specs/streambim-endpoints.md` and `docs/specs/interaxo-endpoints.md`;
the facts the code relies on are repeated in the provider sections below.

## Layout and ids

The on-disk layout is unchanged: `<sync root>\<hub>\<project>\<top folder>\…`. A provider
only decides what its hubs, projects and top folders are.

- **Hub ids** are globally unique strings. ACC keeps its DM ids (`b.…`). Others are prefixed:
  `streambim:<account key>` and `interaxo:<community_id>`. The StreamBIM account key is the region
  subdomain for `*.streambim.com` (`app`, `japan`) and the lowercase host for a custom address
  (`streambim.example.com`), so `streambim:app` is unchanged from the single-account version.
- **Project ids** (`project.project_id`, unique in SQLite) are globally unique strings. ACC keeps
  `b.…`. Others: `streambim:<account key>:<projectId>` and `interaxo:<community_id>:<room_id>`.
- **Hub names** of non-ACC providers carry the provider so they cannot collide with an ACC
  account folder: `StreamBIM` (region `app`, the default; unchanged so existing folders stay),
  `StreamBIM <Region title>` (e.g. `StreamBIM Japan`), `StreamBIM (<host>)` for a custom address,
  `Interaxo – <community name>`.
- **Top folders**: non-ACC projects have exactly one top folder named `Dokument`, whose id is
  the provider's root folder id (StreamBIM `0`, Interaxo `room.content.root_id`).
- **Item ids** are provider-native strings, unique per project, which is all the store needs
  (`PRIMARY KEY(pid, id)`): StreamBIM folder ids as the server gives them (`"7"`, root `"0"`)
  and document ids with a `d` prefix (`"d1002"`, because folders and documents are numbered
  separately), Interaxo content UUIDs. Temporary ids keep the `local-` prefix.
- The identity blob, sync root and Cloud Files code do not change.

## Data model additions (`src/model.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider { #[default] Acc, StreamBim /* "streambim" */, Interaxo }
impl Provider {
    pub const ALL: [Provider; 3];
    pub fn as_str(self) -> &'static str;          // "acc" | "streambim" | "interaxo"
    pub fn parse(s: &str) -> Option<Provider>;
    pub fn label(self) -> &'static str;           // "ACC" | "StreamBIM" | "Interaxo"
}
```

- `Project` gains `provider: Provider` and `meta: Option<String>` (provider JSON, see below).
- `HubInfo` gains `provider: Provider`; `ProjectInfo` gains `provider: Provider` and
  `meta: Option<String>`.
- For non-ACC projects: `root_folder_urn` = the root folder id, `urn_ns` = `""`,
  `oss_scopes` = `None`, `change_source` = `PublicWalk`.
- Store schema **v4**: `ALTER TABLE project ADD COLUMN provider TEXT NOT NULL DEFAULT 'acc'`
  and `ALTER TABLE project ADD COLUMN meta TEXT`. Existing rows are ACC.

`meta` JSON per provider:

| Provider | `meta` |
|---|---|
| ACC | `null` |
| StreamBIM | `{"region":"app","base":"https://app.streambim.com","project":"1552"}`; projects made by this version also carry `"account"` (the same key as `region`) |
| Interaxo | `{"base":"https://api.interaxo.se/v1","community":"example-corp","room":"my-room","root":"<uuid>"}` |

`base` is the API origin every request of that project goes to (tests point it at a loopback
server). Interaxo hubs carry the same `base` in `HubInfo.region`; StreamBIM hubs carry the
region subdomain there.

## Versions and content handles

The engine compares integer versions (`remote_version`, `base_version`) and treats
`storage_urn` as an opaque content handle. Providers fill them like this:

| Provider | `remote_version` | `storage_urn` | `version_urn` |
|---|---|---|---|
| ACC | DM `versionNumber` | OSS URN | DM version URN |
| StreamBIM | document `revision` | `streambim:<docId>:<revision>` | `None` |
| Interaxo | `version` `"M.m"` → `M * 1000 + m` | `interaxo:<id>:<version>` | `None` |

Folders have no version on any provider.

## The dispatch module (`src/remote/mod.rs`)

The engine never calls `api::dm`, `api::oss`, `api::streambim` or `api::interaxo` directly. It
calls `crate::remote::*`, which matches on `p.provider` (or the hub's provider) and calls the
provider module. The ACC arm calls the existing `dm`/`oss` function with **identical arguments**,
so ACC behaviour does not change.

Every function mirrors the `dm`/`oss` function it replaces, with `ctx: &Ctx` in place of
`api: &Api` (each arm passes its own client: `ctx.api`, `ctx.interaxo_api`, or the API of the
StreamBIM account of the project or hub):

```rust
pub struct Caps { pub mkdir: bool, pub rename: bool, pub move_items: bool, pub delete: bool,
                  pub lock: bool, pub web_link: bool, pub ranged_download: bool }
pub fn caps(provider: Provider) -> Caps;

/// Download handle. `bearer`: send `Authorization: Bearer <token>` (not for presigned URLs).
pub struct DownloadUrl { pub url: String, pub size: u64, pub bearer: Option<String>, pub ranges: bool }

pub struct ProjectSetup {            // what `subscribe` needs to build a `Project`
    pub root_folder_urn: String, pub urn_ns: String, pub oss_scopes: Option<String>,
    pub tops: Vec<(String, String)>, // (top folder id, name), ready for model::unique_tops
    pub meta: Option<String>,
}

pub async fn list_hubs(ctx, provider) -> Result<Vec<HubInfo>, ApiError>;
pub async fn list_projects(ctx, hub: &HubInfo) -> Result<Vec<ProjectInfo>, ApiError>;
pub async fn project_setup(ctx, hub: &HubInfo, project: &ProjectInfo) -> Result<ProjectSetup, ApiError>;
pub async fn top_folders(ctx, p) -> Result<Vec<(String, String)>, ApiError>;   // (id, name)
pub async fn folder_contents(ctx, p, folder_id) -> Result<Vec<Item>, ApiError>;
pub async fn download_url(ctx, p, item_id, storage_urn) -> Result<DownloadUrl, ApiError>;
pub async fn open_download(ctx, p, d: &DownloadUrl, offset) -> Result<reqwest::Response, ApiError>;
pub async fn open_range(ctx, p, d: &DownloadUrl, start, end) -> Result<reqwest::Response, ApiError>;
pub async fn get_item_upload_info(ctx, p, id) -> Result<(Item, dm::UploadInfo), ApiError>;
pub async fn create_storage(ctx, p, parent_id, name) -> Result<String, ApiError>;
pub async fn upload_file(ctx, p, storage, path, content_type, progress) -> Result<(), ApiError>;
pub async fn create_item(ctx, p, parent_id, name, storage, path, progress) -> Result<Item, ApiError>;
pub async fn create_version(ctx, p, id, name, storage, path, progress) -> Result<(i64, String), ApiError>;
pub async fn create_folder(ctx, p, parent_id, name) -> Result<Item, ApiError>;
pub async fn rename_file(ctx, p, id, version_urn: Option<&str>, new_name) -> Result<(), ApiError>;
pub async fn move_file(ctx, p, id, new_parent_id) -> Result<(), ApiError>;
pub async fn patch_folder(ctx, p, folder_id, new_name, new_parent_id) -> Result<(), ApiError>;
pub async fn delete_file(ctx, p, id) -> Result<(), ApiError>;
pub async fn delete_folder(ctx, p, folder_id) -> Result<(), ApiError>;
pub async fn web_link(ctx, p, item: &Item) -> Result<String, ApiError>;
pub async fn set_reserved(ctx, p, id, reserve) -> Result<Option<String>, ApiError>;
```

### Upload sequencing

`sync/upload.rs` keeps its ACC sequence: `create_storage` → `upload_file` → tip re-check →
persist intent → `create_version`/`create_item`. The ACC arms ignore the new `path`/`progress`
arguments of `create_item`/`create_version`.

Non-ACC providers cannot separate "bytes stored" from "item committed" the way OSS does, so:

- `create_storage` returns a marker `"<provider>:pending:<random>"`, unique per attempt, without
  a network call (`remote::intent_proves_commit` is false for these providers: the marker never
  counts as proof that a version was committed);
- `upload_file` is a no-op;
- `create_item` / `create_version` upload the bytes from `path` and commit, reporting
  `progress`, and return the new item / `(remote_version, storage_urn)`; the engine stores that
  handle in `storage_urn` and leaves `version_urn` empty.

A lost commit reply cannot be matched to the intent marker on these providers (the listing
never returns `<provider>:pending`), so a retry after a crash in that window can create one
extra version. That is accepted.

### Unsupported operations

`http::ApiError` gains `Unsupported(String)` (Swedish user text). A provider returns it for an
operation its API does not offer. `upload.rs` treats it like an ACC "file type not allowed"
reply: the job is parked as unsupported (`dead = 3`), the user gets one notice, and the change
stays local. `Caps` lets the engine skip work it knows will be refused.

| | ACC | StreamBIM | Interaxo |
|---|---|---|---|
| list / download | yes | yes | yes |
| new file / new version | yes | yes | yes |
| create folder | yes | yes | **no** (API has none) |
| rename / move | yes | yes | **no** (API has none) |
| delete | yes (soft) | yes (`is-deleted` PATCH, soft) | yes (`DELETE`); a folder holding registers, entries or links is refused |
| lock (reserve) | yes | no | no (read-only `lock_info`) |
| web link | yes | project link | room link (unofficial route) |
| ranged download | yes | no (undocumented) | no (undocumented) |

## Accounts and sign-in

`Ctx` holds one account per provider:

```rust
pub auth: Arc<Auth>,                 // ACC (unchanged)
pub api: Api,                        // ACC
pub streambim: Arc<StreamBimAccounts>,   // one StreamBimAuth + Api per StreamBIM server
pub interaxo: Arc<Auth>,             pub interaxo_api: Api,   // Auth with the Interaxo OIDC spec
impl Ctx {
    // No `api_for`: ACC and Interaxo use `api` / `interaxo_api`; a StreamBIM call uses the API of
    // the account of its project or hub (`streambim.for_project(p)` / `for_hub(hub)`), never another.
    pub fn signed_in(&self, p: Provider) -> bool;     // StreamBIM: any account
    pub fn project_connected(&self, p: &Project) -> bool;   // StreamBIM: the project's own account
    pub fn any_signed_in(&self) -> bool;
    pub fn user_for(&self, p: Provider) -> Option<UserInfo>;
}
```

`http::Api` takes a `TokenSource` (enum over `Arc<Auth>` and `Arc<StreamBimAuth>`) instead of a
concrete `Arc<Auth>`; `Api::new(client, Arc<Auth>)` keeps working for ACC and tests.

Token files in the state dir (all refresh material DPAPI-protected, user profile plain JSON):

| Provider | token | profile |
|---|---|---|
| ACC | `token.bin` (unchanged) | `user.json` (unchanged) |
| StreamBIM (per account key) | `token-streambim-<key>.bin` (JSON `{refresh_token, id_token, expires_at}`) | `user-streambim-<key>.json` (adds `region`, `base`) |
| Interaxo | `token-interaxo.bin` | `user-interaxo.json` |

**Sync root.** The sync root is registered while *any* provider is signed in and unregistered
when the last one signs out. Its id no longer depends on the account that happens to be signed
in: the uid used at first registration is stored in kv `sync_root_uid` (an existing ACC
registration keeps its APS user id, so upgrades re-register the same root). Signing out of one
provider while another stays signed in pauses that provider's projects (no polls, its outbox
jobs wait as if offline) and leaves its folders in place.

### StreamBIM

- **Several accounts**, one per StreamBIM server (account key: region subdomain, or the host of a
  custom address). `StreamBimAccounts` (in `src/streambim_auth.rs`) loads every
  `user-streambim-<key>.json` at start, hands out the account of a project (by the host of
  `Project.meta.base`) or of a hub (`HubInfo.region` = the key), and adds an account on its first
  sign-in. A single-account install's `token-streambim.bin` / `user-streambim.json` are renamed
  to the keyed names once at start. A custom address must be `https://<host>` (DNS name, no port,
  path, query or user info); its password goes only to that host, and so does its token.
- Regions: `GET https://global.streambim.com/regions.json` (no auth); fallback list `app`
  (Europe, default), `sweden`, `japan`, `australia`, `spf`. Host `https://<subdomain>.streambim.com`.
  Both `distinctiveLanguages` and the misspelled `disctintiveLanguages` are optional.
- Login: `POST /auth/v1/login {"username","password"}` → `result` `SUCCESS` (tokens) or
  `CHALLENGE_REQUESTED` (`session`). StreamBIM sends the challenge with **HTTP 401**, so a 401
  whose body says `CHALLENGE_REQUESTED` is the MFA step, not a wrong password (StreamBIM's web
  app reads it that way). MFA: `POST /auth/v1/mfa/verify {"username","session","code"}`; a wrong
  code answers `{"error": "invalidCode"}` and the same `session` takes another code (Synka: three
  tries, then a new password step).
- Refusals pass Cognito's error on as one string, "operation error Cognito Identity Provider:
  <op>, https response error StatusCode: <n>, RequestID: <id>, <Exception>: <message>". Synka
  shows `<Exception>: <message>` first; "Identity Provider" in it does not mean SSO.
- Bearer is the **`idToken`**. Refresh: `POST /auth/v1/refresh {"token": <last idToken>,
  "refreshToken"}`; it may also answer `CHALLENGE_REQUESTED` → the user must sign in again.
- The password is never stored or logged. It travels once over the per-user pipe.
- No logout endpoint: sign-out deletes local tokens.

### Interaxo

- **Off by default.** Interaxo is turned off unless `settings.json` has `"interaxo_enabled": true`; while it is off, `settings.providers` omits `interaxo`, the Interaxo sign-in and settings are refused, and its projects are not listed or polled (`docs/IPC.md`).
- Hosts `https://api.interaxo.com/v1` and `https://api.interaxo.se/v1`, one token for both. A
  community lives on one host: after sign-in the engine lists `/communities` on both and
  remembers the host per community (in `meta`). A 401 from the wrong host is not a session
  failure.
- OIDC (Auth0): authorize `https://login.collaboration-sso.com/authorize`, token
  `…/oauth/token`, userinfo `…/userinfo`, revoke `…/oauth/revoke`; scopes
  `openid profile email offline_access`; extra authorize parameter
  `audience=https://api.interaxo.com/v1/`.
- **Bring your own client id**: an organisation asks Interaxo support for a *Native* (public)
  client with Authorization Code + PKCE and refresh tokens, and registers the exact redirect URI
  Synka uses (default `http://localhost:47864/`, configurable). If Interaxo only issues a
  confidential client, its secret can be set too (stored DPAPI-protected, never shown again).

## IPC additions (summary; `docs/IPC.md` is normative)

- `status.accounts`: `[{provider, signed_in, signing_in, user, host}]`. `signed_in`/`user` keep
  their meaning (any account / the first signed-in account).
- `sign_in` takes `provider` (default `acc`); StreamBIM also needs `region`, `username`,
  `password`. `sign_in_mfa {provider, code}`. `sign_out {provider?}` (no provider: all).
- `streambim_regions` command → `streambim_regions` event.
- `sign_in_progress` event: `{provider, state: browser|mfa_required|signed_in|failed|cancelled, error}`.
- `projects.hubs[].provider`.
- Settings: `interaxo_client_id`, `interaxo_redirect_uri`, `interaxo_client_secret_set`,
  `providers` (`["acc","streambim","interaxo"]`).

Listings of both providers page until an empty page (StreamBIM: until `meta.total` when
present), drop repeated ids and stop at 1000 pages, so a server that caps the page size cannot
silently truncate a folder.

## What is not supported (yet)

- StreamBIM SSO-only users (no documented native-client path) and uploads above 5 GB
  (multipart signer undocumented).
- Interaxo folder create, rename and move (not in the public API); active/workflow folders,
  entries and links are not synced.
- Locks on StreamBIM and Interaxo; the Explorer verbs are not active on any provider yet.
- Delta feeds: both new providers use the listing walk of populated folders, like ACC.

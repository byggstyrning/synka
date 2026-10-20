//! Interaxo REST client: a room is the sync root and its document tree is the content. The
//! endpoints and the facts they rely on are in docs/specs/interaxo-endpoints.md; the provider
//! contract is docs/PROVIDERS.md "Interaxo".
//!
//! A room's calls go to its own API base (`Meta::base`, one of `HOSTS`). Bodies are plain
//! JSON, not JSON:API. Listings take `limit`, `skip` and `sort=id` and answer a bare array with
//! no total. A listing is read until an empty page: a server may cap `limit` below `PAGE`.
//!
//! Interaxo's public API has no folder create, rename or move, and no file locks. Those calls
//! return `ApiError::Unsupported`; the engine checks `remote::caps` before it asks.

use crate::api::dm::{self, UploadInfo};
use crate::http::{Api, ApiError};
use crate::model::{HubInfo, Item, Kind, LocalState, Pid, Project, ProjectInfo, Provider};
use crate::remote::{DownloadUrl, ProjectSetup};
use futures_util::{stream, StreamExt};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

/// The two API hosts. One token works on both, and a community lives on one of them.
const HOSTS: [&str; 2] = ["https://api.interaxo.com/v1", "https://api.interaxo.se/v1"];
/// Page size asked for in every listing. The server may send fewer; only an empty page ends one.
const PAGE: usize = 500;
/// Pages one listing may take before it is refused (at most `MAX_PAGES * PAGE` items).
const MAX_PAGES: usize = 1000;
/// Prefix of hub ids, project ids and storage handles.
const PREFIX: &str = "interaxo:";
/// What `create_storage` returns: the bytes travel with the commit (docs/PROVIDERS.md).
const PENDING: &str = "interaxo:pending";
/// The one top folder of a project. Its id is the room's root content id.
const TOP_NAME: &str = "Dokument";
/// Bytes per read while a file is sent; progress is reported once per read.
const UPLOAD_CHUNK: usize = 256 * 1024;
const META_ERROR: &str =
    "Interaxo-projektets uppgifter saknas eller är ogiltiga. Ta bort projektet och lägg till det igen.";

/// The `meta` JSON of a project (docs/PROVIDERS.md). `root` is None until it is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    base: String,
    community: String,
    room: String,
    root: Option<String>,
}

impl Meta {
    /// `<base>/<community>/rooms/<room>`: the room context every call of a project uses.
    fn room_url(&self) -> String {
        format!(
            "{}/{}/rooms/{}",
            self.base,
            dm::pct(&self.community),
            dm::pct(&self.room)
        )
    }

    fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

fn parse_meta(raw: Option<&str>) -> Result<Meta, ApiError> {
    let raw = raw.ok_or_else(|| ApiError::Message(META_ERROR.into()))?;
    serde_json::from_str(raw).map_err(|_| ApiError::Message(META_ERROR.into()))
}

fn meta_of(p: &Project) -> Result<Meta, ApiError> {
    parse_meta(p.meta.as_deref())
}

fn hub_base(hub: &HubInfo) -> Result<String, ApiError> {
    hub.region
        .clone()
        .filter(|base| !base.is_empty())
        .ok_or_else(|| {
            ApiError::Message(
                "Interaxo-gemenskapen saknar API-adress. Uppdatera listan över gemenskaper.".into(),
            )
        })
}

// ---- wire types (only the fields the sync uses) -------------------------------------------

#[derive(Deserialize)]
struct Community {
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct Room {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    content: Option<RoomContent>,
}

#[derive(Deserialize)]
struct RoomContent {
    #[serde(default)]
    root_id: Option<String>,
}

impl Room {
    fn root(&self) -> Option<String> {
        self.content
            .as_ref()
            .and_then(|content| content.root_id.clone())
            .filter(|root| !root.is_empty())
    }
}

/// A user object (`created_by`, `last_modified_by`, `lock_info.owner`).
#[derive(Deserialize)]
struct Person {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct LockInfo {
    #[serde(default)]
    owner: Option<Person>,
}

/// A content object of any type.
#[derive(Deserialize)]
struct Content {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    last_modified: Option<String>,
    #[serde(default)]
    last_modified_by: Option<Person>,
    /// "M.m" for a file.
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
    /// Present only while the file is locked.
    #[serde(default)]
    lock_info: Option<LockInfo>,
}

impl Listed for Community {
    fn id(&self) -> &str {
        &self.id
    }
}

impl Listed for Room {
    fn id(&self) -> &str {
        &self.id
    }
}

impl Listed for Content {
    fn id(&self) -> &str {
        &self.id
    }
}

/// A listed object, told apart from the others by its id.
trait Listed {
    fn id(&self) -> &str;
}

/// Every item of a collection, page by page (see `list_pages`).
async fn list_all<T: Listed + DeserializeOwned>(api: &Api, url: &str) -> Result<Vec<T>, ApiError> {
    list_pages(url, move |page_url: String| async move {
        api.json::<Vec<T>>(Method::GET, &page_url, None)
            .await
            .map_err(scrub)
    })
    .await
}

/// Reads a listing until a page comes back empty. A server may cap `limit` below `PAGE`, so a
/// short page is not the last one. `skip` counts the raw items the server has sent, which keeps
/// it on the server's offsets when an item repeats across pages; repeats are dropped by `id`.
async fn list_pages<T, F, Fut>(url: &str, fetch: F) -> Result<Vec<T>, ApiError>
where
    T: Listed,
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Vec<T>, ApiError>>,
{
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut skip = 0usize;
    for _ in 0..MAX_PAGES {
        let page = fetch(format!("{url}?limit={PAGE}&skip={skip}&sort=id")).await?;
        if page.is_empty() {
            return Ok(out);
        }
        skip += page.len();
        for item in page {
            if seen.insert(item.id().to_string()) {
                out.push(item);
            }
        }
    }
    Err(ApiError::Message(
        "Interaxo: för många sidor i listningen".into(),
    ))
}

/// Read timeout of one metadata call: the same as `http::REQUEST_TIMEOUT` (private there).
const METADATA_TIMEOUT: Duration = Duration::from_secs(100);

/// `list_all` through `get_no_reauth`, for the hub probe.
async fn probe_all<T: Listed + DeserializeOwned>(api: &Api, url: &str) -> Result<Vec<T>, ApiError> {
    list_pages(url, move |page_url: String| async move {
        get_no_reauth::<Vec<T>>(api, &page_url).await
    })
    .await
}

/// One GET with the current bearer token, and no sign-in refresh on 401. The hub probe uses it:
/// a host that does not know the user answers 401, and a refresh would not change that answer.
async fn get_no_reauth<T: DeserializeOwned>(api: &Api, url: &str) -> Result<T, ApiError> {
    let token = api.bearer().await?;
    let resp = api
        .client()
        .get(url)
        .bearer_auth(&token)
        .timeout(METADATA_TIMEOUT)
        .send()
        .await
        .map_err(transport)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ApiError::Status {
            status: status.as_u16(),
            body: crate::http::error_body(resp).await,
        });
    }
    let bytes = resp.bytes().await.map_err(transport)?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
}

/// Drops the URL from a reqwest error. Request URLs are the place a token could end up, so
/// nothing that reaches an error text or a log keeps one.
fn transport(e: reqwest::Error) -> ApiError {
    ApiError::Transport(e.without_url())
}

/// `transport` for an error that already is an `ApiError` (from `Api`, whose errors carry the URL).
fn scrub(e: ApiError) -> ApiError {
    match e {
        ApiError::Transport(e) => transport(e),
        other => other,
    }
}

/// The root content id of one room (`GET .../rooms/{room}`), if the room has one.
async fn room_root(
    api: &Api,
    base: &str,
    community: &str,
    room_id: &str,
) -> Result<Option<String>, ApiError> {
    let url = format!("{base}/{}/rooms/{}", dm::pct(community), dm::pct(room_id));
    let room: Room = api.json(Method::GET, &url, None).await?;
    Ok(room.root())
}

// ---- hubs and projects ------------------------------------------------------------------------

/// The user's communities on both hosts. A community is listed under the host it lives on.
/// A 403 or 404 from a host means that host does not know the community. A 401 means the host
/// refused the sign-in; when both hosts do, the sign-in has expired and the error says so.
pub async fn list_hubs(api: &Api) -> Result<Vec<HubInfo>, ApiError> {
    list_hubs_on(api, &HOSTS).await
}

/// `list_hubs` over the given API bases, in order: a community on two hosts is kept once,
/// under the first host. A host that answers 401 is not asked again with a refreshed token. When
/// every host answers 401, the sign-in has expired and that is the error.
async fn list_hubs_on(api: &Api, bases: &[&str]) -> Result<Vec<HubInfo>, ApiError> {
    let mut hubs = Vec::new();
    let mut seen = HashSet::new();
    let mut answered = false;
    let mut refused = 0usize;
    let mut failure = None;
    for base in bases {
        match probe_all::<Community>(api, &format!("{base}/communities")).await {
            Ok(communities) => {
                answered = true;
                for community in communities {
                    if !seen.insert(community.id.clone()) {
                        info!(community = %community.id, host = %base,
                            "gemenskapen finns på båda värdarna – använder den första");
                        continue;
                    }
                    let label = if community.name.trim().is_empty() {
                        community.id.clone()
                    } else {
                        community.name
                    };
                    hubs.push(HubInfo {
                        id: format!("{PREFIX}{}", community.id),
                        name: format!("Interaxo – {label}"),
                        region: Some(base.to_string()),
                        provider: Provider::Interaxo,
                    });
                }
            }
            Err(ApiError::Status { status: 401, .. }) => {
                refused += 1;
                debug!(host = %base, "inloggningen godtas inte på den här värden");
            }
            Err(ApiError::Status {
                status: 403 | 404, ..
            }) => debug!(host = %base, "inga gemenskaper på den här värden"),
            Err(e) => {
                warn!(host = %base, status = ?e.status(), "kunde inte lista gemenskaper");
                failure.get_or_insert(e);
            }
        }
    }
    match failure {
        Some(e) if !answered => Err(e),
        _ if !answered && refused > 0 && refused == bases.len() => Err(ApiError::Auth(
            anyhow::anyhow!("Interaxo-inloggningen har gått ut – logga in igen. (Session expired)"),
        )),
        _ => Ok(hubs),
    }
}

/// The rooms of a community. Each room is one project; its root content id comes with it, or
/// from the room itself when the listing leaves it out.
pub async fn list_projects(api: &Api, hub: &HubInfo) -> Result<Vec<ProjectInfo>, ApiError> {
    let base = hub_base(hub)?;
    let community = hub
        .id
        .strip_prefix(PREFIX)
        .ok_or_else(|| ApiError::Message(format!("okänd Interaxo-gemenskap: {}", hub.id)))?;
    let rooms: Vec<Room> = list_all(api, &format!("{base}/{}/rooms", dm::pct(community))).await?;
    let mut out = Vec::with_capacity(rooms.len());
    for room in rooms {
        let root = match room.root() {
            Some(root) => root,
            None => match room_root(api, &base, community, &room.id).await {
                Ok(Some(root)) => root,
                // A room the user may not read on its own is left out, not a failed listing.
                Ok(None)
                | Err(ApiError::Status {
                    status: 403 | 404, ..
                }) => {
                    warn!(room = %room.id, "rummet saknar rotmapp eller kan inte läsas – hoppar över");
                    continue;
                }
                Err(e) => return Err(e),
            },
        };
        let meta = Meta {
            base: base.clone(),
            community: community.to_string(),
            room: room.id.clone(),
            root: Some(root.clone()),
        };
        out.push(ProjectInfo {
            id: format!("{PREFIX}{community}:{}", room.id),
            hub_id: hub.id.clone(),
            name: if room.name.trim().is_empty() {
                room.id.clone()
            } else {
                room.name
            },
            root_folder_urn: Some(root),
            scopes: Vec::new(),
            provider: Provider::Interaxo,
            meta: Some(meta.json()),
        });
    }
    Ok(out)
}

/// Subscribing to a room: its one top folder is the root. The root is looked up only when the
/// listing did not carry it.
pub async fn project_setup(
    api: &Api,
    _hub: &HubInfo,
    project: &ProjectInfo,
) -> Result<ProjectSetup, ApiError> {
    let mut meta = parse_meta(project.meta.as_deref())?;
    let root = match meta.root.clone().filter(|root| !root.is_empty()) {
        Some(root) => root,
        None => {
            let root = room_root(api, &meta.base, &meta.community, &meta.room)
                .await?
                .ok_or_else(|| {
                    ApiError::Message(format!("{} har ingen rotmapp i Interaxo", project.name))
                })?;
            meta.root = Some(root.clone());
            root
        }
    };
    Ok(ProjectSetup {
        root_folder_urn: root.clone(),
        urn_ns: String::new(),
        oss_scopes: None,
        tops: vec![(root, TOP_NAME.to_string())],
        meta: Some(meta.json()),
    })
}

/// The one top folder of a room, from the stored meta (no request).
pub async fn top_folders(_api: &Api, p: &Project) -> Result<Vec<(String, String)>, ApiError> {
    let root = meta_of(p)?
        .root
        .filter(|root| !root.is_empty())
        .ok_or_else(|| ApiError::Message(format!("{} saknar rotmapp", p.name)))?;
    Ok(vec![(root, TOP_NAME.to_string())])
}

// ---- content ------------------------------------------------------------------------------------

/// The children of a folder: simple folders and files. Other types (active and workflow
/// folders, entries, links) are not synced, and Windows shortcut files are left out.
pub async fn folder_contents(
    api: &Api,
    p: &Project,
    folder_id: &str,
) -> Result<Vec<Item>, ApiError> {
    let meta = meta_of(p)?;
    let url = format!(
        "{}/content/{}/children",
        meta.room_url(),
        dm::pct(folder_id)
    );
    let children: Vec<Content> = list_all(api, &url).await?;
    Ok(children
        .into_iter()
        .filter_map(|c| content_item(p.pid, c, Some(folder_id.to_string())))
        .filter(listable)
        .collect())
}

/// What a listing keeps: named items, and no Windows shortcut files.
fn listable(item: &Item) -> bool {
    let shortcut = matches!(item.kind, Kind::File) && dm::skip_name(&item.name);
    !item.name.trim().is_empty() && !shortcut
}

/// Map a content object. None for every type the sync does not use.
fn content_item(pid: Pid, c: Content, parent: Option<String>) -> Option<Item> {
    let file = match c.kind.as_str() {
        "file" => true,
        "simple-folder" => false,
        _ => return None,
    };
    let ctime_ms = dm::ms(c.created.as_deref());
    let mtime_ms = dm::ms(c.last_modified.as_deref());
    let modified_by = person_label(c.last_modified_by);
    if !file {
        return Some(Item {
            pid,
            id: c.id,
            parent,
            name: c.name,
            kind: Kind::Folder,
            remote_version: None,
            base_version: None,
            size: 0,
            ctime_ms,
            mtime_ms,
            storage_urn: None,
            version_urn: None,
            modified_by,
            locked_by: None,
            local_state: LocalState::Clean,
            populated: false,
        });
    }
    let version = c.version.unwrap_or_default();
    let storage_urn = format!("{PREFIX}{}:{version}", c.id);
    Some(Item {
        pid,
        id: c.id,
        parent,
        name: c.name,
        kind: Kind::File,
        remote_version: Some(version_number(&version)),
        base_version: None,
        size: c.file_size.unwrap_or(0),
        ctime_ms,
        mtime_ms,
        storage_urn: Some(storage_urn),
        version_urn: None,
        modified_by,
        locked_by: c
            .lock_info
            .map(|lock| person_label(lock.owner).unwrap_or_default()),
        local_state: LocalState::Clean,
        populated: false,
    })
}

/// A user's display name, or the id when the name is missing.
fn person_label(person: Option<Person>) -> Option<String> {
    let person = person?;
    person
        .name
        .filter(|name| !name.trim().is_empty())
        .or(person.id)
        .filter(|label| !label.is_empty())
}

/// Interaxo's version label "M.m" as the integer the engine compares: M * 1000 + m ("M" is
/// "M.0"). Any other label gives 0.
fn version_number(label: &str) -> i64 {
    let mut parts = label.trim().split('.');
    let major = parts.next().and_then(|s| s.parse::<u32>().ok());
    let minor = match parts.next() {
        None => Some(0),
        Some(s) => s.parse::<u32>().ok(),
    };
    match (major, minor, parts.next()) {
        (Some(major), Some(minor), None) => i64::from(major) * 1000 + i64::from(minor),
        _ => 0,
    }
}

/// The metadata of one file as the upload path needs it.
pub(crate) async fn get_item_upload_info(
    api: &Api,
    p: &Project,
    id: &str,
) -> Result<(Item, UploadInfo), ApiError> {
    let meta = meta_of(p)?;
    let url = format!("{}/content/{}", meta.room_url(), dm::pct(id));
    let content: Content = api.json(Method::GET, &url, None).await?;
    let parent = content.parent_id.clone();
    let item = content_item(p.pid, content, parent)
        .filter(|item| matches!(item.kind, Kind::File))
        .ok_or_else(|| ApiError::Message("Det här objektet är inte en fil i Interaxo.".into()))?;
    Ok((item, UploadInfo::generic()))
}

// ---- download -----------------------------------------------------------------------------------

/// The bytes of a file's current version. The engine's `storage_urn` version is not pinned: the
/// endpoint takes no version here, and `?version=` is undocumented. The bearer is a snapshot of
/// the token now; `open_download` sends a fresh one.
pub async fn download_url(
    api: &Api,
    p: &Project,
    item_id: &str,
    storage_urn: &str,
) -> Result<DownloadUrl, ApiError> {
    let meta = meta_of(p)?;
    let mut url = format!("{}/files/{}/content", meta.room_url(), dm::pct(item_id));
    // Ask for exactly the listed version, so the bytes match `remote_version`.
    if let Some(version) = pinned_version(item_id, storage_urn) {
        url.push_str("?version=");
        url.push_str(&dm::pct(version));
    }
    Ok(DownloadUrl {
        url,
        size: 0,
        bearer: Some(api.bearer().await?),
        ranges: false,
    })
}

/// The version in a storage handle `interaxo:<id>:<version>` written for this item, if any.
fn pinned_version<'a>(item_id: &str, storage_urn: &'a str) -> Option<&'a str> {
    let rest = storage_urn.strip_prefix("interaxo:")?;
    let (id, version) = rest.rsplit_once(':')?;
    (id == item_id && !version.is_empty()).then_some(version)
}

/// Stream the file from its start. Range support is undocumented, so `ranges` is false and the
/// engine always asks for offset 0; any other offset is refused.
pub async fn open_download(
    api: &Api,
    _p: &Project,
    d: &DownloadUrl,
    offset: u64,
) -> Result<reqwest::Response, ApiError> {
    if offset != 0 {
        return Err(ApiError::Message(
            "Interaxo kan inte återuppta en nedladdning mitt i filen.".into(),
        ));
    }
    let url = d.url.as_str();
    api.send_custom(|client| Ok(client.get(url))).await
}

pub async fn open_range(
    _api: &Api,
    _p: &Project,
    _d: &DownloadUrl,
    _start: u64,
    _end: u64,
) -> Result<reqwest::Response, ApiError> {
    Err(ApiError::Unsupported(
        "Interaxo API:t stöder inte delvis hämtning av filer.".into(),
    ))
}

// ---- upload -------------------------------------------------------------------------------------

/// Nothing is reserved on Interaxo: the bytes are sent with the commit in `create_item` or
/// `create_version`.
pub async fn create_storage(
    _api: &Api,
    _p: &Project,
    _parent_id: &str,
    _name: &str,
) -> Result<String, ApiError> {
    Ok(PENDING.to_string())
}

pub async fn upload_file(
    _api: &Api,
    _p: &Project,
    _storage: &str,
    _path: &Path,
    _content_type: Option<&str>,
    _progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), ApiError> {
    Ok(())
}

/// A new file in `parent_id`: the bytes of `path` are sent as the multipart part `file`.
pub async fn create_item(
    api: &Api,
    p: &Project,
    parent_id: &str,
    name: &str,
    _storage: &str,
    path: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<Item, ApiError> {
    check_name(name)?;
    let meta = meta_of(p)?;
    let url = format!(
        "{}/content/{}/children",
        meta.room_url(),
        dm::pct(parent_id)
    );
    let created = upload(api, &url, name, path, progress).await?;
    if created.name != name {
        info!(sent = %name, stored = %created.name, "Interaxo gav filen ett annat namn");
    }
    content_item(p.pid, created, Some(parent_id.to_string()))
        .ok_or_else(|| ApiError::Decode("svaret från Interaxo är inte en fil".into()))
}

/// A new major version of file `id`. Returns the version number and the storage handle.
pub async fn create_version(
    api: &Api,
    p: &Project,
    id: &str,
    name: &str,
    _storage: &str,
    path: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(i64, String), ApiError> {
    check_name(name)?;
    let meta = meta_of(p)?;
    let url = format!(
        "{}/files/{}/versions?major=true",
        meta.room_url(),
        dm::pct(id)
    );
    let file = upload(api, &url, name, path, progress).await?;
    let label = file
        .version
        .filter(|label| !label.is_empty())
        .ok_or_else(|| ApiError::Message("Interaxo returnerade ingen version.".into()))?;
    Ok((version_number(&label), format!("{PREFIX}{id}:{label}")))
}

/// Interaxo's file-name rule: no leading dot, and none of ' ; < > " \ | / * ? :
fn check_name(name: &str) -> Result<(), ApiError> {
    let refused = name.is_empty()
        || name.starts_with('.')
        || name.chars().any(|c| {
            matches!(
                c,
                '\'' | ';' | '<' | '>' | '"' | '\\' | '|' | '/' | '*' | '?' | ':'
            )
        });
    if refused {
        return Err(ApiError::Unsupported(format!(
            "Interaxo tillåter inte filnamnet \"{name}\" (inga ' ; < > \" \\ | / * ? : och ingen inledande punkt)."
        )));
    }
    Ok(())
}

/// POST `path` as the multipart part `file` (filename `name`) to `url` and read the content
/// object that answers. `progress(sent, total)` runs while the bytes go out. The body is
/// streamed and built by hand (no reqwest `multipart` feature), with a known Content-Length.
async fn upload(
    api: &Api,
    url: &str,
    name: &str,
    path: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<Content, ApiError> {
    let total = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .map_err(|e| local_error(path, &e))?;
    let boundary = boundary()?;
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    );
    let tail = format!("\r\n--{boundary}--\r\n");
    let length = head.len() as u64 + total + tail.len() as u64;
    let content_type = format!("multipart/form-data; boundary={boundary}");

    let (sent_tx, mut sent_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let send = api.send_custom(|client| {
        let file = std::fs::File::open(path).map_err(|e| local_error(path, &e))?;
        let body = upload_body(head.clone(), file, tail.clone(), sent_tx.clone());
        Ok(client
            .post(url)
            .header(CONTENT_TYPE, content_type.as_str())
            .header(CONTENT_LENGTH, length.to_string())
            .body(body))
    });
    tokio::pin!(send);
    let resp = loop {
        tokio::select! {
            resp = &mut send => break resp?,
            Some(sent) = sent_rx.recv() => progress(sent, total),
        }
    };
    while let Ok(sent) = sent_rx.try_recv() {
        progress(sent, total);
    }
    progress(total, total);
    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::Decode(e.to_string()))
}

fn local_error(path: &Path, e: &std::io::Error) -> ApiError {
    ApiError::Message(format!("kunde inte läsa {}: {e}", path.display()))
}

/// A random multipart boundary (128 bits).
fn boundary() -> Result<String, ApiError> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw)
        .map_err(|e| ApiError::Message(format!("slumpgenerator misslyckades: {e}")))?;
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("synka{hex}"))
}

/// The request body: `head`, the bytes of `file` read in chunks, then `tail`. Each chunk's
/// running total goes to `sent`. Streamed, so a large file is never held in memory.
fn upload_body(
    head: String,
    file: std::fs::File,
    tail: String,
    sent: UnboundedSender<u64>,
) -> reqwest::Body {
    let file = tokio::fs::File::from_std(file);
    let chunks = stream::try_unfold((file, 0u64), move |(mut file, done)| {
        let sent = sent.clone();
        async move {
            let mut buf = vec![0u8; UPLOAD_CHUNK];
            let n = file.read(&mut buf).await?;
            if n == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            buf.truncate(n);
            let done = done + n as u64;
            let _ = sent.send(done);
            Ok::<_, std::io::Error>(Some((buf, (file, done))))
        }
    });
    let head = stream::once(async move { Ok::<Vec<u8>, std::io::Error>(head.into_bytes()) });
    let tail = stream::once(async move { Ok::<Vec<u8>, std::io::Error>(tail.into_bytes()) });
    reqwest::Body::wrap_stream(head.chain(chunks).chain(tail))
}

// ---- structure and deletion ---------------------------------------------------------------------

pub async fn create_folder(
    _api: &Api,
    _p: &Project,
    _parent_id: &str,
    _name: &str,
) -> Result<Item, ApiError> {
    Err(ApiError::Unsupported(
        "Interaxo API:t kan inte skapa mappar. Mappen finns bara på den här datorn.".into(),
    ))
}

pub async fn rename_file(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _version_urn: Option<&str>,
    _new_name: &str,
) -> Result<(), ApiError> {
    Err(ApiError::Unsupported(
        "Interaxo API:t kan inte byta namn på filer. Namnbytet finns bara på den här datorn."
            .into(),
    ))
}

pub async fn move_file(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _new_parent_id: &str,
) -> Result<(), ApiError> {
    Err(ApiError::Unsupported(
        "Interaxo API:t kan inte flytta filer. Flytten finns bara på den här datorn.".into(),
    ))
}

/// Folders cannot be renamed or moved on Interaxo. A call that changes nothing succeeds.
pub async fn patch_folder(
    _api: &Api,
    _p: &Project,
    _folder_id: &str,
    new_name: Option<&str>,
    new_parent_id: Option<&str>,
) -> Result<(), ApiError> {
    if new_name.is_none() && new_parent_id.is_none() {
        return Ok(());
    }
    Err(ApiError::Unsupported(
        "Interaxo API:t kan inte ändra mappar. Ändringen finns bara på den här datorn.".into(),
    ))
}

pub async fn delete_file(api: &Api, p: &Project, id: &str) -> Result<(), ApiError> {
    delete_content(api, p, id).await
}

pub async fn delete_folder(api: &Api, p: &Project, folder_id: &str) -> Result<(), ApiError> {
    delete_content(api, p, folder_id).await
}

/// `DELETE .../content/{id}` (204). A 404 means the item is already gone.
async fn delete_content(api: &Api, p: &Project, id: &str) -> Result<(), ApiError> {
    let meta = meta_of(p)?;
    let url = format!("{}/content/{}", meta.room_url(), dm::pct(id));
    match api.request(Method::DELETE, &url, None).await {
        Ok(_) => Ok(()),
        Err(ApiError::Status { status: 404, .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The room's document library, or the item inside it. The route is the web app's own
/// (`/{community}/{room}/documentLibrary/{id}`, docs/specs/interaxo-endpoints.md 9.2). It is not
/// a documented permalink and may change with the web app.
pub async fn web_link(_api: &Api, p: &Project, item: &Item) -> Result<String, ApiError> {
    let meta = meta_of(p)?;
    let host = web_host(&meta.base)?;
    let library = format!(
        "https://{host}/{}/{}/documentLibrary",
        dm::pct(&meta.community),
        dm::pct(&meta.room)
    );
    match item.kind {
        Kind::ProjectRoot => Ok(library),
        Kind::Hub => Err(ApiError::Message("ingen Interaxo-mapp".into())),
        Kind::Folder | Kind::File => Ok(format!("{library}/{}", dm::pct(&item.id))),
    }
}

/// The web host of an API base: `https://api.interaxo.se/v1` gives `ix.interaxo.se`.
fn web_host(base: &str) -> Result<String, ApiError> {
    url::Url::parse(base)
        .ok()
        .and_then(|url| {
            url.host_str()
                .and_then(|host| host.strip_prefix("api."))
                .map(|rest| format!("ix.{rest}"))
        })
        .ok_or_else(|| ApiError::Message("okänd Interaxo-adress".into()))
}

/// Interaxo has no file locks in its API: the engine does not ask, and a call is refused.
pub async fn set_reserved(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _reserve: bool,
) -> Result<Option<String>, ApiError> {
    Err(ApiError::Unsupported(
        "Interaxo API:t kan inte låsa filer.".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChangeSourceKind, PROJECT_ROOT_ID};
    use crate::test_support::{Fault, Request, Server};
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// The bearer the loopback tests receive (see `Auth::for_test`).
    const TOKEN: &str = "Bearer synka-loopback-test-token";

    fn api_for(origin: &str) -> Api {
        Api::for_test(origin, Duration::from_secs(5))
    }

    fn json_reply(value: Value) -> Fault {
        Fault::reply(200, serde_json::to_vec(&value).unwrap())
            .header("Content-Type", "application/json")
    }

    /// Room `room-1` of community `example-corp`, on the API base `base`.
    fn project(base: &str, root: Option<&str>) -> Project {
        let meta = Meta {
            base: base.into(),
            community: "example-corp".into(),
            room: "room-1".into(),
            root: root.map(str::to_string),
        };
        Project {
            pid: 1,
            hub_id: "interaxo:example-corp".into(),
            hub_name: "Interaxo – Example Corp".into(),
            project_id: "interaxo:example-corp:room-1".into(),
            name: "Room 1".into(),
            root_folder_urn: root.unwrap_or_default().into(),
            urn_ns: String::new(),
            tops: vec![],
            oss_scopes: None,
            change_source: ChangeSourceKind::PublicWalk,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: Provider::Interaxo,
            meta: Some(meta.json()),
        }
    }

    fn item(kind: Kind, id: &str) -> Item {
        Item {
            pid: 1,
            id: id.into(),
            parent: None,
            name: "x".into(),
            kind,
            remote_version: None,
            base_version: None,
            size: 0,
            ctime_ms: 0,
            mtime_ms: 0,
            storage_urn: None,
            version_urn: None,
            modified_by: None,
            locked_by: None,
            local_state: LocalState::Clean,
            populated: false,
        }
    }

    /// A temporary directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("synka-interaxo-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn assert_unsupported<T>(result: Result<T, ApiError>, text: &str) {
        match result {
            Err(ApiError::Unsupported(message)) => {
                assert!(message.starts_with(text), "{message}")
            }
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(_) => panic!("expected Unsupported, got Ok"),
        }
    }

    #[test]
    fn version_labels_map_to_ordered_numbers() {
        assert_eq!(version_number("1.0"), 1000);
        assert_eq!(version_number("2.0"), 2000);
        assert_eq!(version_number("1.1"), 1001);
        assert_eq!(version_number("3"), 3000);
        assert_eq!(version_number(" 4.2 "), 4002);
        assert!(version_number("1.9") < version_number("2.0"));
        for bad in ["", "x", "1.x", "-1.0", "1.2.3", "1..2"] {
            assert_eq!(version_number(bad), 0, "{bad:?}");
        }
    }

    #[test]
    fn web_host_replaces_the_api_label() {
        assert_eq!(
            web_host("https://api.interaxo.com/v1").unwrap(),
            "ix.interaxo.com"
        );
        assert_eq!(
            web_host("https://api.interaxo.se/v1").unwrap(),
            "ix.interaxo.se"
        );
        assert!(web_host("http://127.0.0.1:9/v1").is_err());
    }

    #[test]
    fn names_follow_interaxo_rule() {
        assert!(check_name("Ritning v2 (final).pdf").is_ok());
        assert!(check_name("~$Plan.docx").is_ok());
        for bad in [
            ".gitignore",
            "a:b",
            "it's.txt",
            "x;y",
            "q?",
            "a<b",
            "a>b",
            "a\"b",
            "a\\b",
            "a|b",
            "a/b",
            "a*b",
            "",
        ] {
            let err = check_name(bad).unwrap_err();
            assert!(matches!(err, ApiError::Unsupported(_)), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn list_hubs_skips_a_host_that_does_not_know_the_community() {
        let com = Server::scripted(vec![Fault::reply(401, "")]).await;
        let se = Server::scripted(vec![
            json_reply(json!([
                {"id": "example-corp", "name": "Example Corp"},
                {"id": "other", "name": "Other"}
            ])),
            json_reply(json!([])),
        ])
        .await;
        let (com_base, se_base) = (format!("{}/v1", com.origin), format!("{}/v1", se.origin));
        let hubs = list_hubs_on(
            &api_for(&com.origin),
            &[com_base.as_str(), se_base.as_str()],
        )
        .await
        .unwrap();

        assert_eq!(hubs.len(), 2);
        assert_eq!(hubs[0].id, "interaxo:example-corp");
        assert_eq!(hubs[0].name, "Interaxo – Example Corp");
        assert_eq!(hubs[0].region.as_deref(), Some(se_base.as_str()));
        assert_eq!(hubs[0].provider, Provider::Interaxo);
        assert_eq!(hubs[1].id, "interaxo:other");
        // A 401 is not retried with a refreshed token: it counts as "not on this host".
        assert_eq!(com.requests().len(), 1);
        let se_requests = se.requests();
        assert_eq!(se_requests.len(), 2);
        assert_eq!(
            se_requests[0].target,
            "/v1/communities?limit=500&skip=0&sort=id"
        );
        assert_eq!(se_requests[0].header("authorization"), Some(TOKEN));
    }

    #[tokio::test]
    async fn list_hubs_lists_a_community_on_both_hosts_once_under_the_first() {
        let com = Server::scripted(vec![
            json_reply(json!([{"id": "c1", "name": "Com"}])),
            json_reply(json!([])),
        ])
        .await;
        let se = Server::scripted(vec![
            json_reply(json!([{"id": "c1", "name": "Se"}])),
            json_reply(json!([])),
        ])
        .await;
        let (com_base, se_base) = (format!("{}/v1", com.origin), format!("{}/v1", se.origin));
        let hubs = list_hubs_on(
            &api_for(&com.origin),
            &[com_base.as_str(), se_base.as_str()],
        )
        .await
        .unwrap();
        assert_eq!(hubs.len(), 1);
        assert_eq!(hubs[0].name, "Interaxo – Com");
        assert_eq!(hubs[0].region.as_deref(), Some(com_base.as_str()));
    }

    #[tokio::test]
    async fn list_hubs_is_empty_when_no_host_knows_the_user_but_reports_a_real_failure() {
        let api = api_for("http://127.0.0.1:9");
        let a = Server::scripted(vec![Fault::reply(401, "")]).await;
        let b = Server::scripted(vec![Fault::reply(404, "")]).await;
        let (a_base, b_base) = (format!("{}/v1", a.origin), format!("{}/v1", b.origin));
        let hubs = list_hubs_on(&api, &[a_base.as_str(), b_base.as_str()])
            .await
            .unwrap();
        assert!(hubs.is_empty());

        let c = Server::scripted(vec![Fault::reply(400, "{\"code\":\"validation_error\"}")]).await;
        let d = Server::scripted(vec![Fault::reply(401, "")]).await;
        let (c_base, d_base) = (format!("{}/v1", c.origin), format!("{}/v1", d.origin));
        let err = list_hubs_on(&api, &[c_base.as_str(), d_base.as_str()])
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(400));
    }

    #[tokio::test]
    async fn list_hubs_asks_the_user_to_sign_in_again_when_every_host_refuses_the_token() {
        let api = api_for("http://127.0.0.1:9");
        let a = Server::scripted(vec![Fault::reply(401, "")]).await;
        let b = Server::scripted(vec![Fault::reply(401, "")]).await;
        let (a_base, b_base) = (format!("{}/v1", a.origin), format!("{}/v1", b.origin));
        let err = list_hubs_on(&api, &[a_base.as_str(), b_base.as_str()])
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Auth(_)), "{err:?}");
        // One request per host: no refresh and no second attempt.
        assert_eq!(a.count(), 1);
        assert_eq!(b.count(), 1);
    }

    #[tokio::test]
    async fn list_projects_pages_through_every_room_and_fills_a_missing_root() {
        let first: Vec<Value> = (0..PAGE)
            .map(|i| {
                json!({
                    "id": format!("r{i}"),
                    "name": format!("Room {i}"),
                    "content": {"root_id": format!("root-{i}")}
                })
            })
            .collect();
        let last = vec![
            json!({"id": "late", "name": "Late", "content": {"root_id": "root-late"}}),
            json!({"id": "nofix", "name": "No Fix"}),
        ];
        let first_body = serde_json::to_vec(&first).unwrap();
        let last_body = serde_json::to_vec(&last).unwrap();
        let server = Server::new(move |req: &Request, _| match req.target.as_str() {
            "/v1/example-corp/rooms?limit=500&skip=0&sort=id" => {
                Fault::reply(200, first_body.clone())
            }
            "/v1/example-corp/rooms?limit=500&skip=500&sort=id" => {
                Fault::reply(200, last_body.clone())
            }
            "/v1/example-corp/rooms?limit=500&skip=502&sort=id" => Fault::reply(200, "[]"),
            "/v1/example-corp/rooms/nofix" => json_reply(
                json!({"id": "nofix", "name": "No Fix", "content": {"root_id": "root-nofix"}}),
            ),
            _ => Fault::reply(404, ""),
        })
        .await;

        let base = format!("{}/v1", server.origin);
        let hub = HubInfo {
            id: "interaxo:example-corp".into(),
            name: "Interaxo – Example Corp".into(),
            region: Some(base.clone()),
            provider: Provider::Interaxo,
        };
        let projects = list_projects(&api_for(&server.origin), &hub).await.unwrap();

        assert_eq!(projects.len(), PAGE + 2);
        let r0 = &projects[0];
        assert_eq!(r0.id, "interaxo:example-corp:r0");
        assert_eq!(r0.hub_id, "interaxo:example-corp");
        assert_eq!(r0.root_folder_urn.as_deref(), Some("root-0"));
        assert_eq!(r0.provider, Provider::Interaxo);
        let meta: Meta = serde_json::from_str(r0.meta.as_deref().unwrap()).unwrap();
        assert_eq!(
            meta,
            Meta {
                base: base.clone(),
                community: "example-corp".into(),
                room: "r0".into(),
                root: Some("root-0".into()),
            }
        );
        let nofix = &projects[PAGE + 1];
        assert_eq!(nofix.id, "interaxo:example-corp:nofix");
        assert_eq!(nofix.name, "No Fix");
        assert_eq!(nofix.root_folder_urn.as_deref(), Some("root-nofix"));

        let targets: Vec<String> = server.requests().into_iter().map(|r| r.target).collect();
        assert_eq!(
            targets,
            [
                "/v1/example-corp/rooms?limit=500&skip=0&sort=id",
                "/v1/example-corp/rooms?limit=500&skip=500&sort=id",
                "/v1/example-corp/rooms?limit=500&skip=502&sort=id",
                "/v1/example-corp/rooms/nofix",
            ]
        );
    }

    #[tokio::test]
    async fn list_projects_leaves_out_a_room_it_cannot_read() {
        let rooms = json!([
            {"id": "open", "name": "Open", "content": {"root_id": "root-open"}},
            {"id": "closed", "name": "Closed"}
        ]);
        let server = Server::scripted(vec![
            json_reply(rooms),
            json_reply(json!([])),
            Fault::reply(403, "{\"code\":\"forbidden\"}"),
        ])
        .await;
        let hub = HubInfo {
            id: "interaxo:example-corp".into(),
            name: "Interaxo – Example Corp".into(),
            region: Some(format!("{}/v1", server.origin)),
            provider: Provider::Interaxo,
        };
        let projects = list_projects(&api_for(&server.origin), &hub).await.unwrap();
        let ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["interaxo:example-corp:open"]);
    }

    /// The number of query parameter `key` in a request target (`skip=100` gives 100).
    fn query_number(target: &str, key: &str) -> Option<usize> {
        let prefix = format!("{key}=");
        target
            .split(['?', '&'])
            .find_map(|kv| kv.strip_prefix(prefix.as_str())?.parse().ok())
    }

    /// The page a server answers for `target` when it caps `limit` at `cap`: from `skip`, at
    /// most `cap` items.
    fn capped_page(target: &str, items: &[Value], cap: usize) -> Fault {
        let skip = query_number(target, "skip").unwrap_or(0);
        let limit = query_number(target, "limit").unwrap_or(usize::MAX).min(cap);
        let page: Vec<Value> = items.iter().skip(skip).take(limit).cloned().collect();
        json_reply(Value::Array(page))
    }

    fn listed_room(id: &str) -> Value {
        json!({"id": id, "name": id, "content": {"root_id": format!("root-{id}")}})
    }

    fn hub_at(base: &str) -> HubInfo {
        HubInfo {
            id: "interaxo:example-corp".into(),
            name: "Interaxo – Example Corp".into(),
            region: Some(base.to_string()),
            provider: Provider::Interaxo,
        }
    }

    #[tokio::test]
    async fn listings_read_past_a_server_that_caps_the_page_size() {
        let rooms: Vec<Value> = (0..250).map(|i| listed_room(&format!("r{i}"))).collect();
        let server =
            Server::new(move |req: &Request, _| capped_page(&req.target, &rooms, 100)).await;
        let hub = hub_at(&format!("{}/v1", server.origin));
        let projects = list_projects(&api_for(&server.origin), &hub).await.unwrap();

        assert_eq!(projects.len(), 250);
        assert_eq!(projects[0].id, "interaxo:example-corp:r0");
        assert_eq!(projects[249].id, "interaxo:example-corp:r249");
        let targets: Vec<String> = server.requests().into_iter().map(|r| r.target).collect();
        assert_eq!(
            targets,
            [
                "/v1/example-corp/rooms?limit=500&skip=0&sort=id",
                "/v1/example-corp/rooms?limit=500&skip=100&sort=id",
                "/v1/example-corp/rooms?limit=500&skip=200&sort=id",
                "/v1/example-corp/rooms?limit=500&skip=250&sort=id",
            ]
        );
    }

    #[tokio::test]
    async fn listings_drop_an_item_repeated_across_pages() {
        let server = Server::new(|req: &Request, _| match req.target.as_str() {
            "/v1/example-corp/rooms?limit=500&skip=0&sort=id" => json_reply(json!([
                listed_room("r0"),
                listed_room("r1"),
                listed_room("r2")
            ])),
            // r2 moved from the end of the first page to the start of this one.
            "/v1/example-corp/rooms?limit=500&skip=3&sort=id" => {
                json_reply(json!([listed_room("r2"), listed_room("r3")]))
            }
            "/v1/example-corp/rooms?limit=500&skip=5&sort=id" => json_reply(json!([])),
            _ => Fault::reply(404, ""),
        })
        .await;
        let hub = hub_at(&format!("{}/v1", server.origin));
        let projects = list_projects(&api_for(&server.origin), &hub).await.unwrap();

        let ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "interaxo:example-corp:r0",
                "interaxo:example-corp:r1",
                "interaxo:example-corp:r2",
                "interaxo:example-corp:r3",
            ]
        );
        // The offset follows the raw items sent, so the repeat does not shift the next page.
        let targets: Vec<String> = server.requests().into_iter().map(|r| r.target).collect();
        assert_eq!(targets.len(), 3);
        assert!(targets[2].contains("skip=5&"), "{targets:?}");
    }

    #[tokio::test]
    async fn a_listing_that_never_ends_is_refused() {
        let page = json_reply(json!([listed_room("r0")]));
        let server = Server::new(move |_, _| page.clone()).await;
        let hub = hub_at(&format!("{}/v1", server.origin));
        let err = list_projects(&api_for(&server.origin), &hub)
            .await
            .unwrap_err();

        assert!(
            matches!(&err, ApiError::Message(text) if text == "Interaxo: för många sidor i listningen"),
            "{err:?}"
        );
        assert_eq!(server.count(), MAX_PAGES);
    }

    #[tokio::test]
    async fn project_setup_looks_up_a_root_the_listing_left_out() {
        let server = Server::scripted(vec![json_reply(
            json!({"id": "room-1", "name": "Room 1", "content": {"root_id": "root-1"}}),
        )])
        .await;
        let base = format!("{}/v1", server.origin);
        let hub = HubInfo {
            id: "interaxo:example-corp".into(),
            name: "Interaxo – Example Corp".into(),
            region: Some(base.clone()),
            provider: Provider::Interaxo,
        };
        let info = ProjectInfo {
            id: "interaxo:example-corp:room-1".into(),
            hub_id: hub.id.clone(),
            name: "Room 1".into(),
            root_folder_urn: None,
            scopes: vec![],
            provider: Provider::Interaxo,
            meta: Some(
                Meta {
                    base: base.clone(),
                    community: "example-corp".into(),
                    room: "room-1".into(),
                    root: None,
                }
                .json(),
            ),
        };
        let setup = project_setup(&api_for(&server.origin), &hub, &info)
            .await
            .unwrap();
        assert_eq!(setup.root_folder_urn, "root-1");
        assert_eq!(
            setup.tops,
            vec![("root-1".to_string(), "Dokument".to_string())]
        );
        assert!(setup.urn_ns.is_empty());
        assert!(setup.oss_scopes.is_none());
        let meta: Meta = serde_json::from_str(setup.meta.as_deref().unwrap()).unwrap();
        assert_eq!(meta.root.as_deref(), Some("root-1"));
        assert_eq!(server.requests()[0].target, "/v1/example-corp/rooms/room-1");
    }

    #[tokio::test]
    async fn top_folders_come_from_the_stored_root_without_a_request() {
        let p = project("http://127.0.0.1:9/v1", Some("root-1"));
        let folders = top_folders(&api_for("http://127.0.0.1:9"), &p)
            .await
            .unwrap();
        assert_eq!(
            folders,
            vec![("root-1".to_string(), "Dokument".to_string())]
        );
    }

    #[tokio::test]
    async fn a_project_without_usable_meta_gives_a_user_message() {
        let api = api_for("http://127.0.0.1:9");
        let mut p = project("http://127.0.0.1:9/v1", Some("root-1"));
        for meta in [None, Some("not json".to_string())] {
            p.meta = meta;
            let err = folder_contents(&api, &p, "root-1").await.unwrap_err();
            assert!(matches!(err, ApiError::Message(_)), "{err:?}");
        }
    }

    #[tokio::test]
    async fn folder_contents_keeps_folders_and_files_and_skips_other_types() {
        let children = json!([
            {"type": "simple-folder", "id": "f-1", "parent_id": "root-1", "name": "Ritningar",
             "created": "2026-01-02T10:00:00.000Z", "last_modified": "2026-02-03T11:00:00.000Z",
             "last_modified_by": {"id": "anna@example.com", "name": "Anna"}, "child_count": 2},
            {"type": "file", "id": "d-1", "parent_id": "root-1", "name": "Plan.pdf",
             "created": "2026-01-05T08:00:00.000Z", "last_modified": "2026-03-01T09:30:00.000Z",
             "last_modified_by": {"id": "bo@example.com", "name": "Bo"},
             "version": "2.1", "mime_type": "application/pdf", "file_size": 7,
             "lock_info": {"owner": {"id": "anna@example.com", "name": "Anna"},
                           "timestamp": "2026-03-02T09:30:00.000Z"}},
            {"type": "file", "id": "d-2", "parent_id": "root-1", "name": "Notes.txt",
             "version": "1.0", "file_size": 0},
            {"type": "file", "id": "d-3", "parent_id": "root-1", "name": "Lock.txt",
             "version": "1.0", "file_size": 3,
             "lock_info": {"owner": {"id": "bo@example.com"}}},
            {"type": "file", "id": "d-4", "parent_id": "root-1", "name": "shortcut.url",
             "version": "1.0", "file_size": 1},
            {"type": "active-folder", "id": "a-1", "parent_id": "root-1", "name": "Ärenden"},
            {"type": "workflow-folder", "id": "w-1", "parent_id": "root-1", "name": "Flöde"},
            {"type": "entry", "id": "e-1", "parent_id": "a-1", "name": "Post"},
            {"type": "internal-link", "id": "l-1", "parent_id": "root-1", "name": "Länk"},
            {"type": "external-link", "id": "l-2", "parent_id": "root-1", "name": "Extern",
             "url": "https://example.com"}
        ]);
        let server = Server::scripted(vec![json_reply(children), json_reply(json!([]))]).await;
        let p = project(&format!("{}/v1", server.origin), Some("root-1"));
        let items = folder_contents(&api_for(&server.origin), &p, "root-1")
            .await
            .unwrap();
        assert_eq!(server.count(), 2, "a listing ends on an empty page");

        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Ritningar", "Plan.pdf", "Notes.txt", "Lock.txt"]);

        let folder = &items[0];
        assert!(matches!(folder.kind, Kind::Folder));
        assert_eq!(folder.id, "f-1");
        assert_eq!(folder.parent.as_deref(), Some("root-1"));
        assert_eq!(folder.modified_by.as_deref(), Some("Anna"));
        assert_eq!(folder.remote_version, None);
        assert!(folder.storage_urn.is_none());
        assert_eq!(folder.mtime_ms, dm::ms(Some("2026-02-03T11:00:00.000Z")));

        let plan = &items[1];
        assert!(matches!(plan.kind, Kind::File));
        assert_eq!(plan.remote_version, Some(2001));
        assert_eq!(plan.size, 7);
        assert_eq!(plan.storage_urn.as_deref(), Some("interaxo:d-1:2.1"));
        assert_eq!(plan.locked_by.as_deref(), Some("Anna"));
        assert_eq!(plan.modified_by.as_deref(), Some("Bo"));
        assert_eq!(plan.parent.as_deref(), Some("root-1"));
        assert!(plan.version_urn.is_none());

        let notes = &items[2];
        assert_eq!(notes.remote_version, Some(1000));
        assert_eq!(notes.locked_by, None);

        // A lock with no name shows the owner's id.
        assert_eq!(items[3].locked_by.as_deref(), Some("bo@example.com"));

        assert_eq!(
            server.requests()[0].target,
            "/v1/example-corp/rooms/room-1/content/root-1/children?limit=500&skip=0&sort=id"
        );
    }

    #[test]
    fn downloads_pin_the_listed_version() {
        assert_eq!(pinned_version("d-1", "interaxo:d-1:2.1"), Some("2.1"));
        assert_eq!(pinned_version("d-1", "interaxo:other:2.1"), None);
        assert_eq!(pinned_version("d-1", "interaxo:pending"), None);
        assert_eq!(pinned_version("d-1", "interaxo:d-1:"), None);
    }

    #[tokio::test]
    async fn download_url_has_the_file_route_and_the_stream_sends_the_bearer() {
        let server = Server::scripted(vec![Fault::reply(200, "hello from interaxo")]).await;
        let base = format!("{}/v1", server.origin);
        let p = project(&base, Some("root-1"));
        let api = api_for(&server.origin);

        let d = download_url(&api, &p, "d-1", "interaxo:d-1:2.1")
            .await
            .unwrap();
        assert_eq!(
            d.url,
            format!("{base}/example-corp/rooms/room-1/files/d-1/content?version=2.1")
        );
        assert_eq!(d.size, 0);
        assert!(!d.ranges);
        assert_eq!(d.bearer.as_deref(), Some("synka-loopback-test-token"));
        assert!(
            server.requests().is_empty(),
            "the URL is built without a request"
        );

        let resp = open_download(&api, &p, &d, 0).await.unwrap();
        assert_eq!(
            resp.bytes().await.unwrap().to_vec(),
            b"hello from interaxo".to_vec()
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].target,
            "/v1/example-corp/rooms/room-1/files/d-1/content?version=2.1"
        );
        assert_eq!(requests[0].header("authorization"), Some(TOKEN));

        // Ranges are not offered: a resumed download is refused before any request.
        assert!(open_download(&api, &p, &d, 5).await.is_err());
        assert!(matches!(
            open_range(&api, &p, &d, 0, 10).await,
            Err(ApiError::Unsupported(_))
        ));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn create_item_posts_the_file_as_multipart_and_returns_the_item() {
        let dir = TempDir::new("create");
        let path = dir.0.join("abc.txt");
        std::fs::write(&path, b"hello interaxo").unwrap();
        let server = Server::scripted(vec![json_reply(json!({
            "type": "file", "id": "new-1", "parent_id": "folder-1", "name": "abc.txt",
            "original_name": "abc.txt", "created": "2026-04-01T10:00:00.000Z",
            "last_modified": "2026-04-01T10:00:00.000Z", "version": "1.0",
            "mime_type": "text/plain", "file_size": 14
        }))])
        .await;
        let p = project(&format!("{}/v1", server.origin), Some("root-1"));
        let calls = Mutex::new(Vec::new());
        let progress = |done: u64, total: u64| calls.lock().unwrap().push((done, total));

        let item = create_item(
            &api_for(&server.origin),
            &p,
            "folder-1",
            "abc.txt",
            "interaxo:pending",
            &path,
            &progress,
        )
        .await
        .unwrap();

        assert_eq!(item.id, "new-1");
        assert_eq!(item.name, "abc.txt");
        assert!(matches!(item.kind, Kind::File));
        assert_eq!(item.parent.as_deref(), Some("folder-1"));
        assert_eq!(item.remote_version, Some(1000));
        assert_eq!(item.storage_urn.as_deref(), Some("interaxo:new-1:1.0"));
        assert_eq!(item.size, 14);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.target,
            "/v1/example-corp/rooms/room-1/content/folder-1/children"
        );
        assert_eq!(request.header("authorization"), Some(TOKEN));
        let content_type = request.header("content-type").unwrap();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("multipart content type");
        let expected = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"abc.txt\"\r\n\
             Content-Type: application/octet-stream\r\n\r\nhello interaxo\r\n--{boundary}--\r\n"
        );
        assert_eq!(request.body, expected.as_bytes());
        let length = expected.len().to_string();
        assert_eq!(request.header("content-length"), Some(length.as_str()));

        let calls = calls.lock().unwrap();
        assert_eq!(calls.last(), Some(&(14, 14)));
        assert!(calls.iter().all(|&(done, total)| total == 14 && done <= 14));
    }

    #[tokio::test]
    async fn create_item_keeps_the_name_the_server_gave_the_file() {
        let dir = TempDir::new("renamed");
        let path = dir.0.join("abc.txt");
        std::fs::write(&path, b"x").unwrap();
        let server = Server::scripted(vec![json_reply(json!({
            "type": "file", "id": "new-2", "parent_id": "folder-1", "name": "abc (1).txt",
            "version": "1.0", "file_size": 1
        }))])
        .await;
        let p = project(&format!("{}/v1", server.origin), Some("root-1"));
        let item = create_item(
            &api_for(&server.origin),
            &p,
            "folder-1",
            "abc.txt",
            "interaxo:pending",
            &path,
            &|_: u64, _: u64| {},
        )
        .await
        .unwrap();
        assert_eq!(item.name, "abc (1).txt");
        assert_eq!(item.id, "new-2");
    }

    #[tokio::test]
    async fn create_version_asks_for_a_major_version_and_returns_its_number() {
        let dir = TempDir::new("version");
        let path = dir.0.join("Plan.pdf");
        std::fs::write(&path, b"new bytes").unwrap();
        let server = Server::scripted(vec![json_reply(json!({
            "type": "file", "id": "d-1", "name": "Plan.pdf", "version": "3.0", "file_size": 9
        }))])
        .await;
        let p = project(&format!("{}/v1", server.origin), Some("root-1"));
        let (number, storage) = create_version(
            &api_for(&server.origin),
            &p,
            "d-1",
            "Plan.pdf",
            "interaxo:d-1:2.1",
            &path,
            &|_: u64, _: u64| {},
        )
        .await
        .unwrap();
        assert_eq!(number, 3000);
        assert_eq!(storage, "interaxo:d-1:3.0");

        let requests = server.requests();
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].target,
            "/v1/example-corp/rooms/room-1/files/d-1/versions?major=true"
        );
        assert_eq!(requests[0].header("authorization"), Some(TOKEN));
        assert!(requests[0]
            .header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary="));
    }

    #[tokio::test]
    async fn refused_names_fail_before_any_request_is_sent() {
        let server = Server::new(|_, _| Fault::reply(599, "")).await;
        let p = project(&format!("{}/v1", server.origin), Some("root-1"));
        let api = api_for(&server.origin);
        let progress: &(dyn Fn(u64, u64) + Send + Sync) = &|_: u64, _: u64| {};
        for name in [
            ".hidden", "a:b.txt", "it's.txt", "x;y.txt", "q?.txt", "a<b.txt", "a\"b.txt",
            "a\\b.txt", "a|b.txt", "a/b.txt", "a*b.txt", "",
        ] {
            let err = create_item(
                &api,
                &p,
                "folder-1",
                name,
                "interaxo:pending",
                Path::new("unused"),
                progress,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ApiError::Unsupported(_)), "{name:?}: {err:?}");
            let err = create_version(
                &api,
                &p,
                "d-1",
                name,
                "interaxo:pending",
                Path::new("unused"),
                progress,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ApiError::Unsupported(_)), "{name:?}: {err:?}");
        }
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn folder_and_file_structure_changes_are_unsupported() {
        let api = api_for("http://127.0.0.1:9");
        let p = project("http://127.0.0.1:9/v1", Some("root-1"));
        assert_unsupported(
            create_folder(&api, &p, "root-1", "Ny").await,
            "Interaxo API:t kan inte skapa mappar",
        );
        assert_unsupported(
            rename_file(&api, &p, "d-1", None, "x.pdf").await,
            "Interaxo API:t kan inte byta namn på filer",
        );
        assert_unsupported(
            move_file(&api, &p, "d-1", "f-1").await,
            "Interaxo API:t kan inte flytta filer",
        );
        assert_unsupported(
            patch_folder(&api, &p, "f-1", Some("x"), None).await,
            "Interaxo API:t kan inte ändra mappar",
        );
        assert_unsupported(
            patch_folder(&api, &p, "f-1", None, Some("g-1")).await,
            "Interaxo API:t kan inte ändra mappar",
        );
        assert_unsupported(
            set_reserved(&api, &p, "d-1", true).await,
            "Interaxo API:t kan inte låsa filer",
        );
        assert!(patch_folder(&api, &p, "f-1", None, None).await.is_ok());
        assert_eq!(
            create_storage(&api, &p, "root-1", "x.pdf").await.unwrap(),
            "interaxo:pending"
        );
        upload_file(
            &api,
            &p,
            "interaxo:pending",
            Path::new("unused"),
            None,
            &|_, _| {},
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn delete_removes_the_item_and_a_missing_one_counts_as_deleted() {
        let server = Server::scripted(vec![Fault::reply(204, ""), Fault::reply(404, "")]).await;
        let p = project(&format!("{}/v1", server.origin), Some("root-1"));
        let api = api_for(&server.origin);
        delete_file(&api, &p, "d-1").await.unwrap();
        delete_folder(&api, &p, "f-1").await.unwrap();

        let requests = server.requests();
        assert_eq!(requests[0].method, "DELETE");
        assert_eq!(
            requests[0].target,
            "/v1/example-corp/rooms/room-1/content/d-1"
        );
        assert_eq!(requests[1].method, "DELETE");
        assert_eq!(
            requests[1].target,
            "/v1/example-corp/rooms/room-1/content/f-1"
        );
    }

    #[tokio::test]
    async fn upload_target_is_the_current_file_and_a_folder_is_refused() {
        let server = Server::scripted(vec![
            json_reply(json!({
                "type": "file", "id": "d-1", "parent_id": "f-1", "name": "Plan.pdf",
                "version": "2.1", "file_size": 7
            })),
            json_reply(json!({"type": "simple-folder", "id": "f-1", "name": "Ritningar"})),
        ])
        .await;
        let p = project(&format!("{}/v1", server.origin), Some("root-1"));
        let api = api_for(&server.origin);
        let (item, _info) = get_item_upload_info(&api, &p, "d-1").await.unwrap();
        assert_eq!(item.remote_version, Some(2001));
        assert_eq!(item.parent.as_deref(), Some("f-1"));
        assert!(get_item_upload_info(&api, &p, "f-1").await.is_err());
        assert_eq!(
            server.requests()[0].target,
            "/v1/example-corp/rooms/room-1/content/d-1"
        );
    }

    #[tokio::test]
    async fn web_links_point_at_the_room_on_the_matching_web_host() {
        let api = api_for("http://127.0.0.1:9");
        let se = project("https://api.interaxo.se/v1", Some("root-1"));
        let com = project("https://api.interaxo.com/v1", Some("root-1"));
        assert_eq!(
            web_link(&api, &se, &item(Kind::Folder, "f-1"))
                .await
                .unwrap(),
            "https://ix.interaxo.se/example-corp/room-1/documentLibrary/f-1"
        );
        assert_eq!(
            web_link(&api, &com, &item(Kind::File, "d-1"))
                .await
                .unwrap(),
            "https://ix.interaxo.com/example-corp/room-1/documentLibrary/d-1"
        );
        assert_eq!(
            web_link(&api, &com, &item(Kind::ProjectRoot, PROJECT_ROOT_ID))
                .await
                .unwrap(),
            "https://ix.interaxo.com/example-corp/room-1/documentLibrary"
        );
        assert!(
            web_link(&api, &com, &item(Kind::Hub, "interaxo:example-corp"))
                .await
                .is_err()
        );
        let loopback = project("http://127.0.0.1:9/v1", Some("root-1"));
        assert!(web_link(&api, &loopback, &item(Kind::File, "d-1"))
            .await
            .is_err());
    }
}

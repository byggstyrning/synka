//! StreamBIM REST client: JSON:API under `{base}/project-{id}/api/v1/v2`, the non-JSON:API
//! download link, and the upload tickets (presigned S3 PUT, then a document or revision POST).
//! The endpoints and the facts they rely on: docs/specs/streambim-endpoints.md. The provider
//! contract: docs/PROVIDERS.md "StreamBIM".
//!
//! Base URL: a project's calls use `Project.meta` (`{"region","base","project"}`). The project
//! listing and its setup use the hub's region. Production derives `https://<region>.streambim.com`.
//! Tests call the `*_at` functions with a loopback origin, so no global override exists.

use crate::api::dm::{self, UploadInfo};
use crate::http::{error_body, Api, ApiError, Body};
use crate::model::{now_ms, HubInfo, Item, Kind, LocalState, Pid, Project, ProjectInfo, Provider};
use crate::remote::{DownloadUrl, ProjectSetup};
use crate::streambim_auth::{region_base, valid_region, StreamBimAuth};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, RANGE};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

const REGIONS_URL: &str = "https://global.streambim.com/regions.json";
const REGIONS_TIMEOUT: Duration = Duration::from_secs(10);
const JSON_API: &str = "application/vnd.api+json";
/// `page[limit]` of the document listing. Paging ends at `meta.total`, or at an empty page.
const PAGE_LIMIT: usize = 500;
/// The most pages one folder listing reads. A listing that does not end before this is refused.
const MAX_DOC_PAGES: usize = 1000;
/// The largest file one presigned PUT takes here. The multipart signer is undocumented.
const MAX_UPLOAD_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const PROGRESS_TICK: Duration = Duration::from_millis(250);
/// The same whole-request limit as the OSS transfers (api::oss).
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const ROOT_ID: &str = "0";
const TOP_NAME: &str = "Dokument";
/// Engine ids of documents start with this letter: folders and documents have separate id
/// sequences on the server, so a bare number could name both (see `doc_item_id`).
const DOC_PREFIX: char = 'd';
/// The storage marker of a new file: its bytes go up inside `create_item`/`create_version`.
const PENDING_STORAGE: &str = "streambim:pending";
/// Document attributes a PATCH echoes back (see `full_object`).
const DOC_KEYS: &[&str] = &[
    "description",
    "filename",
    "scale",
    "external-id",
    "is-deleted",
];
const FOLDER_KEYS: &[&str] = &["name", "description", "external-id", "is-deleted"];
const META_MISSING: &str = "StreamBIM-projektet saknar anslutningsuppgifter.";
const META_BAD: &str = "StreamBIM-projektets anslutningsuppgifter är ogiltiga.";
/// Returned when a document was created but the reply has no usable id and the listing does not
/// show it. Not retryable: the file is picked up by the next sync.
const NOT_LISTED: &str = "StreamBIM tog emot filen men svarade utan id; den visas vid nästa synk.";

/// A StreamBIM region: the host is `https://<subdomain>.streambim.com`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Region {
    pub subdomain: String,
    pub title: String,
    pub is_default: bool,
}

/// The regions of `global.streambim.com/regions.json` as shipped with the app, used when the
/// list cannot be fetched. "app" (Europe) is the default.
pub fn fallback_regions() -> Vec<Region> {
    [
        ("app", "Europe", true),
        ("sweden", "Sweden", false),
        ("japan", "Japan", false),
        ("australia", "Australia", false),
        ("spf", "SPF", false),
    ]
    .into_iter()
    .map(|(subdomain, title, is_default)| Region {
        subdomain: subdomain.into(),
        title: title.into(),
        is_default,
    })
    .collect()
}

/// The regions offered at sign-in: `regions.json`, or the built-in list when that fails or is
/// empty.
pub async fn regions(client: &reqwest::Client) -> Vec<Region> {
    regions_from(client, REGIONS_URL).await
}

async fn regions_from(client: &reqwest::Client, url: &str) -> Vec<Region> {
    match fetch_regions(client, url).await {
        Ok(list) if !list.is_empty() => list,
        Ok(_) => {
            warn!("StreamBIM: regions.json saknar regioner; använder den inbyggda listan");
            fallback_regions()
        }
        Err(e) => {
            warn!(error = %e, "StreamBIM: regions.json kunde inte hämtas; använder den inbyggda listan");
            fallback_regions()
        }
    }
}

async fn fetch_regions(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<Region>> {
    let resp = client
        .get(url)
        .timeout(REGIONS_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.without_url())?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status().as_u16());
    }
    let doc: Value = resp.json().await.map_err(|e| e.without_url())?;
    let rows = doc
        .get("regions")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("regions saknas"))?;
    Ok(rows.iter().filter_map(region_from_json).collect())
}

/// One entry of `regions.json`. `distinctiveLanguages` and its misspelled twin are not read.
fn region_from_json(row: &Value) -> Option<Region> {
    let subdomain = row.get("subdomain")?.as_str()?.trim().to_string();
    if !valid_region(&subdomain) {
        return None;
    }
    let title = row
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(subdomain.as_str())
        .to_string();
    let is_default = row
        .get("isDefault")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(Region {
        subdomain,
        title,
        is_default,
    })
}

/// One StreamBIM hub for the signed-in account (its region). Nothing without a sign-in.
pub async fn list_hubs(_api: &Api, auth: &StreamBimAuth) -> Result<Vec<HubInfo>, ApiError> {
    let Some(region) = auth.region().filter(|_| auth.is_signed_in()) else {
        return Ok(Vec::new());
    };
    Ok(vec![HubInfo {
        id: format!("streambim:{region}"),
        name: "StreamBIM".into(),
        region: Some(region),
        provider: Provider::StreamBim,
    }])
}

/// Every project the account can see (`project-links`), under the hub's region.
pub async fn list_projects(api: &Api, hub: &HubInfo) -> Result<Vec<ProjectInfo>, ApiError> {
    let region = hub_region(hub)?;
    let base = region_base(&region).map_err(|e| user_error(e.to_string()))?;
    list_projects_at(api, &base, hub).await
}

async fn list_projects_at(
    api: &Api,
    base: &str,
    hub: &HubInfo,
) -> Result<Vec<ProjectInfo>, ApiError> {
    let region = hub_region(hub)?;
    let doc = get_json(api, &format!("{base}/mgw/api/v3/project-links")).await?;
    let mut out = Vec::new();
    for row in resource_list(&doc)? {
        let Some(id) = res_id(row) else { continue };
        let Ok(meta) = meta_checked(&region, base, &id) else {
            continue;
        };
        out.push(ProjectInfo {
            id: format!("streambim:{region}:{id}"),
            hub_id: hub.id.clone(),
            name: attr_text(row, "name").unwrap_or_else(|| id.clone()),
            root_folder_urn: Some(ROOT_ID.into()),
            scopes: Vec::new(),
            provider: Provider::StreamBim,
            meta: Some(meta_json(&meta)),
        });
    }
    Ok(out)
}

/// What subscribing needs. A stopped project is started (failure is only logged), and a failed
/// read of the project is not fatal either.
pub async fn project_setup(
    api: &Api,
    _hub: &HubInfo,
    project: &ProjectInfo,
) -> Result<ProjectSetup, ApiError> {
    let meta = parse_meta(project.meta.as_deref())?;
    let url = format!(
        "{}/mgw/api/v3/projects/{}",
        meta.base,
        dm::pct(&meta.project)
    );
    match get_json(api, &url).await {
        Ok(doc) => {
            if doc
                .pointer("/data/attributes/status")
                .and_then(Value::as_str)
                == Some("Stopped")
            {
                start_project(api, &meta).await;
            }
        }
        Err(e) => warn!(status = ?e.status(), "StreamBIM: projektet kunde inte läsas; fortsätter"),
    }
    Ok(ProjectSetup {
        root_folder_urn: ROOT_ID.into(),
        urn_ns: String::new(),
        oss_scopes: None,
        tops: vec![(ROOT_ID.into(), TOP_NAME.into())],
        meta: project.meta.clone(),
    })
}

async fn start_project(api: &Api, meta: &Meta) {
    let url = format!(
        "{}/mgw/api/v1/projects/{}/_starttrigger",
        meta.base,
        dm::pct(&meta.project)
    );
    match api.request(Method::GET, &url, None).await {
        Ok(_) => info!(project = %meta.project, "StreamBIM: projektet startas"),
        Err(e) => info!(
            project = %meta.project,
            status = ?e.status(),
            "StreamBIM: starttriggern misslyckades; fortsätter"
        ),
    }
}

/// StreamBIM has one top folder per project, the root (`0`), shown as "Dokument".
pub async fn top_folders(_api: &Api, _p: &Project) -> Result<Vec<(String, String)>, ApiError> {
    Ok(vec![(ROOT_ID.into(), TOP_NAME.into())])
}

/// Folders, then the documents, directly inside `folder_id`. Deleted rows, rows that point at
/// another folder, and shortcut names are dropped.
pub async fn folder_contents(
    api: &Api,
    p: &Project,
    folder_id: &str,
) -> Result<Vec<Item>, ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let root = jsonapi_root(&meta);
    let fid = dm::pct(folder_id);
    let mut out = Vec::new();

    let doc = get_json(
        api,
        &format!("{root}/folders?filter%5Bparent%5D={fid}&filter%5BisDeleted%5D=false"),
    )
    .await?;
    for row in resource_list(&doc)? {
        if attr_flag(row, "is-deleted") || moved_away(row, folder_id) {
            continue;
        }
        out.extend(folder_item(p.pid, row, folder_id, None));
    }

    // Pages run to `meta.total` when the reply has it, else to the first empty page. A row that
    // comes again on a later page is read once.
    let mut seen = HashSet::new();
    let mut skip = 0usize;
    for _ in 0..MAX_DOC_PAGES {
        let url = format!(
            "{root}/documents?filter%5BfolderId%5D={fid}&filter%5BisDeleted%5D=false\
             &page%5Bskip%5D={skip}&page%5Blimit%5D={PAGE_LIMIT}"
        );
        let doc = get_json(api, &url).await?;
        let rows = resource_list(&doc)?;
        for row in rows {
            if res_id(row).is_some_and(|id| !seen.insert(id)) {
                continue;
            }
            if attr_flag(row, "is-deleted") || moved_away(row, folder_id) {
                continue;
            }
            if let Some(item) = file_item(p.pid, row, Some(folder_id)) {
                if !dm::skip_name(&item.name) {
                    out.push(item);
                }
            }
        }
        skip += rows.len();
        let more = match doc.pointer("/meta/total").and_then(Value::as_u64) {
            Some(total) => !rows.is_empty() && (skip as u64) < total,
            None => !rows.is_empty(),
        };
        if !more {
            return Ok(out);
        }
    }
    Err(user_error(
        "StreamBIM-mappen har fler dokument än synken läser; listningen avbröts.",
    ))
}

/// A download link for the current revision. The link is relative to the project's v1 root,
/// or absolute. The bearer token goes only to a link on the StreamBIM origin that carries no
/// token of its own (a presigned link never gets it).
pub async fn download_url(
    api: &Api,
    p: &Project,
    item_id: &str,
    storage_urn: &str,
) -> Result<DownloadUrl, ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let doc = server_doc_id(item_id)?;
    let root = api_v1_root(&meta);
    let url = format!("{root}/documents/{}/downloadlink", dm::pct(doc));
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let resp = api
        .request_with_headers(Method::GET, &url, None, headers)
        .await?;
    let link = parse_download_link(&read_body(resp).await?)?;
    let full = join_download_link(&root, &link);
    // The engine asked for the revision in `storage_urn`. A link to another revision would stream
    // other bytes than the ones the engine is about to store, so the engine asks again instead.
    if let (Some(linked), Some(wanted)) =
        (revision_in_link(&full, doc)?, handle_revision(storage_urn))
    {
        if linked != wanted {
            return Err(ApiError::Status {
                status: 409,
                body: "revision changed".into(),
            });
        }
    }
    let bearer = if needs_bearer(&full, &meta.base)? {
        Some(api.bearer().await?)
    } else {
        None
    };
    Ok(DownloadUrl {
        url: full,
        size: 0,
        bearer,
        ranges: false,
    })
}

/// Streams the bytes of `d` from `offset`. Without `ranges` the engine only asks for offset 0.
pub async fn open_download(
    api: &Api,
    _p: &Project,
    d: &DownloadUrl,
    offset: u64,
) -> Result<reqwest::Response, ApiError> {
    let range = (offset > 0).then(|| format!("bytes={offset}-"));
    let resp = get_download(api, &d.url, d.bearer.as_deref(), range).await?;
    if offset > 0 && resp.status().as_u16() != 206 {
        return Err(user_error("StreamBIM svarade inte på delvis hämtning."));
    }
    Ok(resp)
}

/// StreamBIM answers no ranges (undocumented), so the engine never asks for one.
pub async fn open_range(
    _api: &Api,
    _p: &Project,
    _d: &DownloadUrl,
    _start: u64,
    _end: u64,
) -> Result<reqwest::Response, ApiError> {
    Err(ApiError::Unsupported(
        "StreamBIM stöder inte delvis hämtning.".into(),
    ))
}

pub(crate) async fn get_item_upload_info(
    api: &Api,
    p: &Project,
    id: &str,
) -> Result<(Item, UploadInfo), ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let doc = get_json(api, &object_url(&meta, "documents", server_doc_id(id)?)).await?;
    let row = doc
        .get("data")
        .ok_or_else(|| decode("dokumentet saknas i svaret"))?;
    if attr_flag(row, "is-deleted") {
        return Err(deleted_document());
    }
    let item =
        file_item(p.pid, row, None).ok_or_else(|| decode("dokumentet saknar namn eller id"))?;
    Ok((item, UploadInfo::generic()))
}

/// No storage is reserved on StreamBIM: `create_item` / `create_version` upload the bytes.
pub async fn create_storage(
    _api: &Api,
    _p: &Project,
    _parent_id: &str,
    _name: &str,
) -> Result<String, ApiError> {
    Ok(PENDING_STORAGE.into())
}

/// No-op: see `create_storage`.
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

/// Upload `path` as a new document in `parent_id`. Returns the document as the server has it.
pub async fn create_item(
    api: &Api,
    p: &Project,
    parent_id: &str,
    name: &str,
    _storage: &str,
    path: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<Item, ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let size = file_size(path).await?;
    ensure_uploadable(size)?;
    let ticket = create_ticket(api, &meta, name, size).await?;
    put_presigned(api, &ticket.url, path, size, progress).await?;
    let body = json!({"data": {
        "id": "0",
        "type": "documents",
        "attributes": {"description": "", "filename": name, "filesize": size},
        "relationships": {
            "labels": {"data": []},
            "parent": {"data": {"id": parent_id, "type": "folders"}},
            "upload-ticket": {"data": {"type": "upload-tickets", "id": ticket.id}},
        },
    }});
    // The POST answer is the commit: from here on the document exists, so no error below may be
    // retryable, or the engine would upload the file again. An unreadable answer counts as no
    // answer, and a failed read-back falls back to what the answer and the upload establish.
    let resp = api
        .request(
            Method::POST,
            &format!("{}/documents", jsonapi_root(&meta)),
            Some(Body::JsonApi(body)),
        )
        .await?;
    let reply = match read_body(resp).await {
        Ok(bytes) => parse_body(&bytes).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let Some(server_id) = reply.get("data").and_then(res_id).filter(|id| plain_id(id)) else {
        // No usable id: the newest file of that name and size in the folder is the one just made.
        return newest_file_named(api, p, parent_id, name, size)
            .await
            .ok_or_else(|| user_error(NOT_LISTED));
    };
    let usable = reply
        .get("data")
        .and_then(|row| file_item(p.pid, row, Some(parent_id)))
        .filter(|item| item.remote_version.is_some());
    if let Some(item) = usable {
        return Ok(item);
    }
    // The reply lacks the attributes (the revision, the name): read the document back.
    let fetched = get_json(api, &object_url(&meta, "documents", &server_id))
        .await
        .ok()
        .and_then(|doc| {
            doc.get("data")
                .and_then(|row| file_item(p.pid, row, Some(parent_id)))
        });
    Ok(fetched.unwrap_or_else(|| created_item(p.pid, &server_id, parent_id, name, size)))
}

/// Upload `path` as the next revision of document `id`. Returns (revision, storage handle).
/// The revision keeps the document's file name. A 400 or 409 from the server is returned as is.
/// A deleted document is refused with a 404 (the engine treats that as gone).
pub async fn create_version(
    api: &Api,
    p: &Project,
    id: &str,
    _name: &str,
    _storage: &str,
    path: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(i64, String), ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let doc = server_doc_id(id)?;
    let current = get_json(api, &object_url(&meta, "documents", doc)).await?;
    let row = current
        .get("data")
        .ok_or_else(|| decode("dokumentet saknas i svaret"))?;
    if attr_flag(row, "is-deleted") {
        return Err(deleted_document());
    }
    let revision = attr_int(row, "revision").ok_or_else(|| decode("dokumentet saknar revision"))?;
    let filename = attr_text(row, "filename").ok_or_else(|| decode("dokumentet saknar filnamn"))?;
    let size = file_size(path).await?;
    ensure_uploadable(size)?;
    let ticket = create_ticket(api, &meta, &filename, size).await?;
    put_presigned(api, &ticket.url, path, size, progress).await?;
    let next = revision + 1;
    let body = json!({"data": {
        "id": format!("{doc}-{next}"),
        "type": "document-revisions",
        "attributes": {"description": "", "filesize": size},
        "relationships": {
            "upload-ticket": {"data": {"type": "upload-tickets", "id": ticket.id}},
        },
    }});
    send_json(
        api,
        Method::POST,
        &format!("{}/document-revisions", jsonapi_root(&meta)),
        body,
    )
    .await?;
    Ok((next, storage_urn(doc, next)))
}

pub async fn create_folder(
    api: &Api,
    p: &Project,
    parent_id: &str,
    name: &str,
) -> Result<Item, ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let body = json!({"data": {
        "id": "new",
        "type": "folders",
        "attributes": {"name": name},
        "relationships": {
            "labels": {"data": []},
            "parent": {"data": {"id": parent_id, "type": "folders"}},
        },
    }});
    let doc = send_json(
        api,
        Method::POST,
        &format!("{}/folders", jsonapi_root(&meta)),
        body,
    )
    .await?;
    let row = doc
        .get("data")
        .ok_or_else(|| decode("create folder: svaret saknar data"))?;
    folder_item(p.pid, row, parent_id, Some(name))
        .ok_or_else(|| decode("create folder: inget id i svaret"))
}

/// Rename a document. The version argument is ACC's and is not needed here.
pub async fn rename_file(
    api: &Api,
    p: &Project,
    id: &str,
    _version_urn: Option<&str>,
    new_name: &str,
) -> Result<(), ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let id = server_doc_id(id)?;
    let res = current(api, &meta, "documents", id).await?;
    let body = full_object(
        "documents",
        id,
        &res,
        DOC_KEYS,
        vec![("filename", json!(new_name))],
        None,
    );
    patch(api, &meta, "documents", id, body).await
}

pub async fn move_file(
    api: &Api,
    p: &Project,
    id: &str,
    new_parent_id: &str,
) -> Result<(), ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let id = server_doc_id(id)?;
    let res = current(api, &meta, "documents", id).await?;
    let body = full_object(
        "documents",
        id,
        &res,
        DOC_KEYS,
        Vec::new(),
        Some(new_parent_id),
    );
    patch(api, &meta, "documents", id, body).await
}

/// Rename and/or move a folder. Neither given: nothing is sent.
pub async fn patch_folder(
    api: &Api,
    p: &Project,
    folder_id: &str,
    new_name: Option<&str>,
    new_parent_id: Option<&str>,
) -> Result<(), ApiError> {
    if new_name.is_none() && new_parent_id.is_none() {
        return Ok(());
    }
    let meta = parse_meta(p.meta.as_deref())?;
    let res = current(api, &meta, "folders", folder_id).await?;
    let changes: Vec<(&str, Value)> = new_name
        .map(|name| ("name", json!(name)))
        .into_iter()
        .collect();
    let body = full_object(
        "folders",
        folder_id,
        &res,
        FOLDER_KEYS,
        changes,
        new_parent_id,
    );
    patch(api, &meta, "folders", folder_id, body).await
}

/// Soft delete: `is-deleted` true.
pub async fn delete_file(api: &Api, p: &Project, id: &str) -> Result<(), ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let id = server_doc_id(id)?;
    let res = current(api, &meta, "documents", id).await?;
    let body = full_object(
        "documents",
        id,
        &res,
        DOC_KEYS,
        vec![("is-deleted", json!(true))],
        None,
    );
    patch(api, &meta, "documents", id, body).await
}

/// Soft delete: `is-deleted` true (no hard delete is documented).
pub async fn delete_folder(api: &Api, p: &Project, folder_id: &str) -> Result<(), ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    let res = current(api, &meta, "folders", folder_id).await?;
    let body = full_object(
        "folders",
        folder_id,
        &res,
        FOLDER_KEYS,
        vec![("is-deleted", json!(true))],
        None,
    );
    patch(api, &meta, "folders", folder_id, body).await
}

/// The project's page in the web app. StreamBIM has no documented link to one document or folder.
pub async fn web_link(_api: &Api, p: &Project, item: &Item) -> Result<String, ApiError> {
    let meta = parse_meta(p.meta.as_deref())?;
    if item.kind == Kind::File {
        server_doc_id(&item.id)?;
    }
    Ok(format!(
        "{}/webapp/default/#/?projectId={}",
        meta.base, meta.project
    ))
}

pub async fn set_reserved(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _reserve: bool,
) -> Result<Option<String>, ApiError> {
    Err(ApiError::Unsupported("StreamBIM har inga fillås.".into()))
}

// ---------- project base, metadata ----------

/// `Project.meta` of a StreamBIM project.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Meta {
    region: String,
    base: String,
    project: String,
}

#[derive(Deserialize)]
struct MetaJson {
    region: String,
    base: String,
    project: String,
}

fn parse_meta(raw: Option<&str>) -> Result<Meta, ApiError> {
    let raw = raw.ok_or_else(|| user_error(META_MISSING))?;
    let parsed: MetaJson = serde_json::from_str(raw).map_err(|_| user_error(META_BAD))?;
    meta_checked(&parsed.region, &parsed.base, &parsed.project)
}

/// The checks every meta goes through: a plain region, an http(s) base, a plain project id.
fn meta_checked(region: &str, base: &str, project: &str) -> Result<Meta, ApiError> {
    let base = base.trim_end_matches('/');
    let plain_base = base.starts_with("https://") || base.starts_with("http://");
    if !valid_region(region) || !plain_base || !plain_id(project) {
        return Err(user_error(META_BAD));
    }
    Ok(Meta {
        region: region.to_string(),
        base: base.to_string(),
        project: project.to_string(),
    })
}

fn meta_json(m: &Meta) -> String {
    json!({"region": m.region, "base": m.base, "project": m.project}).to_string()
}

fn hub_region(hub: &HubInfo) -> Result<String, ApiError> {
    hub.region
        .clone()
        .filter(|r| !r.is_empty())
        .ok_or_else(|| user_error("StreamBIM-hubben saknar region."))
}

/// A plain StreamBIM id: letters, digits, '-' and '_', nothing that changes a URL.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The engine's id of a document: `d` + the server id ("1002" gives "d1002"). Folders keep the
/// bare server id, so a folder "7" and a document "7" never share a key in the store.
fn doc_item_id(server_id: &str) -> String {
    format!("{DOC_PREFIX}{server_id}")
}

/// The server id of an engine document id: "d1002" gives "1002". An id without the prefix is not
/// a document of this provider (a folder id is the bare server id), so it is refused.
fn server_doc_id(item_id: &str) -> Result<&str, ApiError> {
    item_id
        .strip_prefix(DOC_PREFIX)
        .filter(|id| plain_id(id))
        .ok_or_else(|| user_error("Ogiltigt dokument-id för StreamBIM."))
}

/// The number of an engine document id ("d1002" gives 1002), 0 for anything else.
fn doc_number(item_id: &str) -> i64 {
    item_id
        .strip_prefix(DOC_PREFIX)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// `{base}/project-{id}/api/v1/v2`: the JSON:API root of a project.
fn jsonapi_root(m: &Meta) -> String {
    format!("{}/project-{}/api/v1/v2", m.base, m.project)
}

/// `{base}/project-{id}/api/v1`: the root of the non-JSON:API helpers (download links).
fn api_v1_root(m: &Meta) -> String {
    format!("{}/project-{}/api/v1", m.base, m.project)
}

fn object_url(m: &Meta, kind: &str, id: &str) -> String {
    format!("{}/{kind}/{}", jsonapi_root(m), dm::pct(id))
}

// ---------- HTTP helpers ----------

fn decode(msg: impl Into<String>) -> ApiError {
    ApiError::Decode(msg.into())
}

fn user_error(msg: impl Into<String>) -> ApiError {
    ApiError::Message(msg.into())
}

/// A document deleted in StreamBIM: uploads into it are refused, and the engine treats the 404
/// as "gone".
fn deleted_document() -> ApiError {
    ApiError::Status {
        status: 404,
        body: "document is deleted".into(),
    }
}

fn jsonapi_accept() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static(JSON_API));
    headers
}

fn parse_body(bytes: &[u8]) -> Result<Value, ApiError> {
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(bytes).map_err(|e| decode(format!("svaret från StreamBIM: {e}")))
}

/// A reqwest error as `ApiError::Transport`, with its URL removed: a presigned or `?token=` link
/// must not reach an error, a log or the user.
fn transport(e: reqwest::Error) -> ApiError {
    ApiError::Transport(e.without_url())
}

/// The whole body of a reply, read without the URL in any error.
async fn read_body(resp: reqwest::Response) -> Result<bytes::Bytes, ApiError> {
    resp.bytes().await.map_err(transport)
}

async fn get_json(api: &Api, url: &str) -> Result<Value, ApiError> {
    let resp = api
        .request_with_headers(Method::GET, url, None, jsonapi_accept())
        .await?;
    parse_body(&read_body(resp).await?)
}

/// POST, PATCH or DELETE a JSON:API body. The reply body, when there is one, is returned.
async fn send_json(api: &Api, method: Method, url: &str, body: Value) -> Result<Value, ApiError> {
    let resp = api.request(method, url, Some(Body::JsonApi(body))).await?;
    parse_body(&read_body(resp).await?)
}

fn resource_list(doc: &Value) -> Result<&Vec<Value>, ApiError> {
    doc.get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| decode("listan från StreamBIM saknar data"))
}

async fn get_download(
    api: &Api,
    url: &str,
    bearer: Option<&str>,
    range: Option<String>,
) -> Result<reqwest::Response, ApiError> {
    let mut req = api.transfer_client().get(url).timeout(TRANSFER_TIMEOUT);
    if let Some(token) = bearer {
        req = req.bearer_auth(token);
    }
    if let Some(range) = range {
        req = req.header(RANGE, range);
    }
    // The link may carry a token or a signature: its URL never reaches the error.
    let resp = req.send().await.map_err(transport)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ApiError::Status {
            status: status.as_u16(),
            body: error_body(resp).await,
        });
    }
    Ok(resp)
}

// ---------- resource mapping ----------

fn res_id(r: &Value) -> Option<String> {
    id_text(r.get("id")?)
}

/// JSON:API ids are strings in project resources and integers in the project list.
fn id_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn attr<'a>(r: &'a Value, key: &str) -> Option<&'a Value> {
    r.get("attributes")?.get(key)
}

fn attr_text(r: &Value, key: &str) -> Option<String> {
    attr(r, key)?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn attr_int(r: &Value, key: &str) -> Option<i64> {
    attr(r, key)?.as_i64()
}

fn attr_flag(r: &Value, key: &str) -> bool {
    attr(r, key).and_then(Value::as_bool) == Some(true)
}

fn rel_id(r: &Value, key: &str) -> Option<String> {
    id_text(r.get("relationships")?.get(key)?.get("data")?.get("id")?)
}

/// True when the row's parent is another folder than the one that was listed.
fn moved_away(row: &Value, folder_id: &str) -> bool {
    rel_id(row, "parent").is_some_and(|parent| parent != folder_id)
}

/// The content handle of one revision (opaque to the engine, see docs/PROVIDERS.md).
fn storage_urn(id: &str, revision: i64) -> String {
    format!("streambim:{id}:{revision}")
}

/// A folder resource. `parent` is the folder that was listed (or created in). `fallback_name`
/// covers a reply without a name. None without a name or an id.
fn folder_item(pid: Pid, r: &Value, parent: &str, fallback_name: Option<&str>) -> Option<Item> {
    let name = attr_text(r, "name").or_else(|| fallback_name.map(str::to_string))?;
    Some(Item {
        pid,
        id: res_id(r)?,
        parent: Some(parent.to_string()),
        name,
        kind: Kind::Folder,
        remote_version: None,
        base_version: None,
        size: 0,
        ctime_ms: dm::ms(attr(r, "created-date").and_then(Value::as_str)),
        mtime_ms: dm::ms(attr(r, "last-modified").and_then(Value::as_str)),
        storage_urn: None,
        version_urn: None,
        modified_by: rel_id(r, "modified-by"),
        locked_by: None,
        local_state: LocalState::Clean,
        populated: false,
    })
}

/// A document resource. The parent is the relationship, else `parent_fallback`. The item id is
/// the prefixed engine id (`doc_item_id`); the storage handle keeps the server id.
fn file_item(pid: Pid, r: &Value, parent_fallback: Option<&str>) -> Option<Item> {
    let server_id = res_id(r)?;
    let name = attr_text(r, "filename")?;
    let revision = attr_int(r, "revision");
    Some(Item {
        pid,
        id: doc_item_id(&server_id),
        parent: rel_id(r, "parent").or_else(|| parent_fallback.map(str::to_string)),
        name,
        kind: Kind::File,
        remote_version: revision,
        base_version: None,
        size: attr_int(r, "filesize").unwrap_or(0),
        ctime_ms: dm::ms(attr(r, "uploaded-date").and_then(Value::as_str)),
        mtime_ms: dm::ms(attr(r, "last-modified").and_then(Value::as_str)),
        storage_urn: revision.map(|rev| storage_urn(&server_id, rev)),
        version_urn: None,
        modified_by: rel_id(r, "uploaded-by"),
        locked_by: None,
        local_state: LocalState::Clean,
        populated: false,
    })
}

/// The newest file called `name` and of `size` in the listing of `folder_id`. None when the
/// listing fails or does not show it. A file of the same name but another size is an older one.
async fn newest_file_named(
    api: &Api,
    p: &Project,
    folder_id: &str,
    name: &str,
    size: u64,
) -> Option<Item> {
    folder_contents(api, p, folder_id)
        .await
        .ok()?
        .into_iter()
        .filter(|item| item.kind == Kind::File && item.name == name && item.size == size as i64)
        .max_by_key(|item| doc_number(&item.id))
}

/// A document just created, as the upload knows it when the read-back failed. A new document is
/// revision 1, and its times are now.
fn created_item(pid: Pid, server_id: &str, parent: &str, name: &str, size: u64) -> Item {
    let now = now_ms();
    Item {
        pid,
        id: doc_item_id(server_id),
        parent: Some(parent.to_string()),
        name: name.to_string(),
        kind: Kind::File,
        remote_version: Some(1),
        base_version: None,
        size: size as i64,
        ctime_ms: now,
        mtime_ms: now,
        storage_urn: Some(storage_urn(server_id, 1)),
        version_urn: None,
        modified_by: None,
        locked_by: None,
        local_state: LocalState::Clean,
        populated: false,
    }
}

// ---------- downloads ----------

/// The link in a downloadlink reply: `{"data": "<link>"}`, or the bare text (quotes removed).
fn parse_download_link(body: &[u8]) -> Result<String, ApiError> {
    let link = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(obj)) => obj
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Ok(Value::String(s)) => s,
        _ => String::from_utf8_lossy(body).to_string(),
    };
    let link = link.trim().trim_matches('"').trim().to_string();
    if link.is_empty() {
        return Err(decode("downloadlink: svaret saknar en länk"));
    }
    Ok(link)
}

/// A relative link joins `api_v1` with exactly one '/'. An absolute link is used as it is.
fn join_download_link(api_v1: &str, link: &str) -> String {
    if link.starts_with("https://") || link.starts_with("http://") {
        return link.to_string();
    }
    format!(
        "{}/{}",
        api_v1.trim_end_matches('/'),
        link.trim_start_matches('/')
    )
}

/// The revision that a download link names, for `.../documents/{id}/{rev}/download...`. None for
/// the other link forms (no revision in the path) and for a link to another document.
fn revision_in_link(full: &str, doc: &str) -> Result<Option<i64>, ApiError> {
    let link = url::Url::parse(full).map_err(|_| decode("downloadlink: ogiltig länk"))?;
    let path: Vec<&str> = link.path().split('/').collect();
    let Some(at) = path.iter().position(|segment| *segment == "documents") else {
        return Ok(None);
    };
    if path.get(at + 1) != Some(&doc) {
        return Ok(None);
    }
    let revision = path.get(at + 2).and_then(|rev| rev.parse::<i64>().ok());
    let download = path
        .get(at + 3)
        .is_some_and(|seg| seg.starts_with("download"));
    Ok(revision.filter(|_| download))
}

/// The revision of a content handle `streambim:{id}:{revision}`. None for any other handle.
fn handle_revision(storage_urn: &str) -> Option<i64> {
    let (_, revision) = storage_urn.strip_prefix("streambim:")?.rsplit_once(':')?;
    revision.parse().ok()
}

/// False for another origin (a presigned S3 link) and for links with `token` or `microtoken`.
fn needs_bearer(full: &str, base: &str) -> Result<bool, ApiError> {
    let link = url::Url::parse(full).map_err(|_| decode("downloadlink: ogiltig länk"))?;
    let origin = url::Url::parse(base)
        .map_err(|_| decode("StreamBIM: ogiltig bas-URL"))?
        .origin();
    if link.origin() != origin {
        return Ok(false);
    }
    Ok(!link
        .query_pairs()
        .any(|(key, _)| key == "token" || key == "microtoken"))
}

// ---------- uploads ----------

/// A presigned upload target from `POST upload-tickets`.
struct Ticket {
    id: String,
    url: String,
}

/// Refuses files over 5 GiB, the limit of the single presigned PUT used here.
fn ensure_uploadable(size: u64) -> Result<(), ApiError> {
    if size > MAX_UPLOAD_BYTES {
        return Err(ApiError::Unsupported(
            "StreamBIM: filer över 5 GB kan inte laddas upp ännu.".into(),
        ));
    }
    Ok(())
}

async fn file_size(path: &Path) -> Result<u64, ApiError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| decode(format!("local file: {e}")))?;
    Ok(meta.len())
}

async fn create_ticket(
    api: &Api,
    meta: &Meta,
    filename: &str,
    size: u64,
) -> Result<Ticket, ApiError> {
    let body = json!({"data": {
        "id": "0",
        "type": "attachments",
        "attributes": {"filename": filename, "file-size": size},
    }});
    let doc = send_json(
        api,
        Method::POST,
        &format!("{}/upload-tickets", jsonapi_root(meta)),
        body,
    )
    .await?;
    let data = doc
        .get("data")
        .ok_or_else(|| decode("upload-ticket: data saknas"))?;
    let id = res_id(data).ok_or_else(|| decode("upload-ticket: id saknas"))?;
    let presigned = data
        .pointer("/attributes/s3presignedurl")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| decode("upload-ticket: presigned-URL saknas"))?;
    Ok(Ticket {
        id,
        url: presigned.to_string(),
    })
}

/// PUTs the file to its presigned S3 URL. The URL is signed, so no StreamBIM credentials are sent.
async fn put_presigned(
    api: &Api,
    url: &str,
    path: &Path,
    size: u64,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), ApiError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| decode(format!("local file: {e}")))?;
    let sent = Arc::new(AtomicU64::new(0));
    let body = reqwest::Body::wrap_stream(file_chunks(file, sent.clone()));
    let request = api
        .transfer_client()
        .put(url)
        .timeout(TRANSFER_TIMEOUT)
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(CONTENT_LENGTH, size.to_string())
        .body(body)
        .send();
    tokio::pin!(request);
    let mut tick = tokio::time::interval(PROGRESS_TICK);
    let resp = loop {
        tokio::select! {
            // The presigned URL is signed: its error is returned without the URL.
            reply = &mut request => break reply.map_err(transport)?,
            _ = tick.tick() => progress(sent.load(Ordering::Relaxed), size),
        }
    };
    let status = resp.status();
    if !status.is_success() {
        return Err(ApiError::Status {
            status: status.as_u16(),
            body: error_body(resp).await,
        });
    }
    progress(size, size);
    Ok(())
}

/// The file as a stream of chunks. `sent` counts the bytes handed to the request.
fn file_chunks(
    file: tokio::fs::File,
    sent: Arc<AtomicU64>,
) -> impl futures_util::stream::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static
{
    futures_util::stream::try_unfold((file, sent), |(mut file, sent)| async move {
        let mut buf = vec![0u8; CHUNK_BYTES];
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n);
        sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(Some((bytes::Bytes::from(buf), (file, sent))))
    })
}

// ---------- mutations ----------

/// The current JSON:API object of a document or folder (the `data` member).
async fn current(api: &Api, meta: &Meta, kind: &str, id: &str) -> Result<Value, ApiError> {
    let doc = get_json(api, &object_url(meta, kind, id)).await?;
    doc.get("data")
        .cloned()
        .ok_or_else(|| decode("objektet saknas i svaret"))
}

/// The full JSON:API object for a PATCH. PATCH may replace the whole object, so the current
/// values of `keys` (those the reply has), the labels and the parent go back with `changes`
/// applied. `parent` overrides the current parent.
fn full_object(
    kind: &str,
    id: &str,
    res: &Value,
    keys: &[&str],
    changes: Vec<(&str, Value)>,
    parent: Option<&str>,
) -> Value {
    let mut attributes = Map::new();
    for key in keys {
        if let Some(value) = attr(res, key) {
            attributes.insert((*key).to_string(), value.clone());
        }
    }
    for (key, value) in changes {
        attributes.insert(key.to_string(), value);
    }
    let labels = res
        .pointer("/relationships/labels/data")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let mut relationships = json!({ "labels": { "data": labels } });
    if let Some(parent_id) = parent.map(str::to_string).or_else(|| rel_id(res, "parent")) {
        relationships["parent"] = json!({ "data": { "id": parent_id, "type": "folders" } });
    }
    json!({ "data": {
        "id": id,
        "type": kind,
        "attributes": attributes,
        "relationships": relationships,
    }})
}

async fn patch(api: &Api, meta: &Meta, kind: &str, id: &str, body: Value) -> Result<(), ApiError> {
    send_json(api, Method::PATCH, &object_url(meta, kind, id), body)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChangeSourceKind, TopFolder};
    use crate::test_support::{Fault, Request, Server};
    use std::sync::atomic::AtomicU64 as Counter;
    use std::sync::Mutex;

    /// A state directory removed when dropped.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static N: Counter = Counter::new(0);
            let path = std::env::temp_dir().join(format!(
                "synka-streambim-api-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A local file removed when dropped.
    struct TestFile(std::path::PathBuf);

    impl TestFile {
        fn new(bytes: &[u8]) -> Self {
            static N: Counter = Counter::new(0);
            let path = std::env::temp_dir().join(format!(
                "synka-streambim-upload-{}-{}.bin",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn api_for(server: &Server) -> Api {
        Api::for_test(&server.origin, Duration::from_secs(2))
    }

    fn reply(status: u16, value: Value) -> Fault {
        Fault::reply(status, value.to_string()).header("Content-Type", "application/vnd.api+json")
    }

    fn body_of(req: &Request) -> Value {
        serde_json::from_slice(&req.body).unwrap()
    }

    /// Project 1552 on the loopback origin, as `list_projects_at` would have listed it.
    fn project(origin: &str) -> Project {
        let meta = meta_checked("app", origin, "1552").unwrap();
        Project {
            pid: 1,
            hub_id: "streambim:app".into(),
            hub_name: "StreamBIM".into(),
            project_id: "streambim:app:1552".into(),
            name: "Demo".into(),
            root_folder_urn: ROOT_ID.into(),
            urn_ns: String::new(),
            tops: vec![TopFolder {
                id: ROOT_ID.into(),
                name: TOP_NAME.into(),
            }],
            oss_scopes: None,
            change_source: ChangeSourceKind::PublicWalk,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: Provider::StreamBim,
            meta: Some(meta_json(&meta)),
        }
    }

    const LIVE_REGIONS: &str = r#"{"regions": [
        {"subdomain": "app", "title": "Europe", "distinctiveLanguages": null, "isDefault": true},
        {"subdomain": "sweden", "title": "Sweden", "distinctiveLanguages": ["sv"], "isDefault": false},
        {"subdomain": "japan", "title": "Japan", "distinctiveLanguages": ["ja", "jp"], "isDefault": false},
        {"subdomain": "australia", "title": "Australia", "disctintiveLanguages": ["en"], "isDefault": false},
        {"subdomain": "spf", "title": "SPF", "disctintiveLanguages": null, "isDefault": false}
    ]}"#;

    #[tokio::test]
    async fn regions_parse_the_live_shape_and_fall_back() {
        let ok = Server::scripted(vec![Fault::reply(200, LIVE_REGIONS)]).await;
        let found = regions_from(&http(), &format!("{}/regions.json", ok.origin)).await;
        assert_eq!(found.len(), 5);
        assert_eq!(
            found[0],
            Region {
                subdomain: "app".into(),
                title: "Europe".into(),
                is_default: true
            }
        );
        assert_eq!(found[4].subdomain, "spf");
        assert_eq!(ok.requests()[0].target, "/regions.json");

        for fault in [
            Fault::reply(500, "boom"),
            Fault::reply(200, r#"{"regions": []}"#),
            Fault::reply(200, "not json"),
            Fault::Disconnect,
        ] {
            let s = Server::scripted(vec![fault]).await;
            let found = regions_from(&http(), &format!("{}/regions.json", s.origin)).await;
            assert_eq!(found, fallback_regions());
        }
    }

    #[tokio::test]
    async fn hub_needs_a_sign_in_and_projects_come_from_project_links() {
        let dir = TestDir::new("hub");
        let server = Server::new(|req, _| match req.target.as_str() {
            "/auth/v1/login" => reply(200, json!({"result": "SUCCESS", "idToken": "id-1", "refreshToken": "rt", "expiresIn": 3600})),
            t if t.starts_with("/mgw/api/v2/user-profiles") => reply(200, json!({"data": {"attributes": {"name": "Ada"}}})),
            "/mgw/api/v3/project-links" => reply(200, json!({"data": [
                {"id": 1552, "type": "project-links", "attributes": {"name": "Hus A"}},
                {"id": "1553", "type": "project-links", "attributes": {"name": "Hus B"}},
                {"id": "bad id", "type": "project-links", "attributes": {"name": "skipped"}},
            ]})),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let auth = StreamBimAuth::for_test(http(), dir.0.as_path(), &server.origin);
        assert!(list_hubs(&api_for(&server), &auth)
            .await
            .unwrap()
            .is_empty());
        auth.login("app", "ada@example.com", "pw").await.unwrap();

        let hubs = list_hubs(&api_for(&server), &auth).await.unwrap();
        assert_eq!(hubs.len(), 1);
        assert_eq!(hubs[0].id, "streambim:app");
        assert_eq!(hubs[0].name, "StreamBIM");
        assert_eq!(hubs[0].region.as_deref(), Some("app"));
        assert_eq!(hubs[0].provider, Provider::StreamBim);

        let api = api_for(&server);
        let projects = list_projects_at(&api, &server.origin, &hubs[0])
            .await
            .unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].id, "streambim:app:1552");
        assert_eq!(projects[0].name, "Hus A");
        assert_eq!(projects[0].root_folder_urn.as_deref(), Some("0"));
        assert_eq!(projects[1].id, "streambim:app:1553");
        let meta = parse_meta(projects[1].meta.as_deref()).unwrap();
        assert_eq!(meta.base, server.origin);
        assert_eq!(meta.project, "1553");
        assert_eq!(meta.region, "app");
        let links = server.requests().into_iter().last().unwrap();
        assert_eq!(links.target, "/mgw/api/v3/project-links");
        assert_eq!(links.header("accept"), Some(JSON_API));
    }

    #[tokio::test]
    async fn project_setup_starts_a_stopped_project_and_tolerates_a_failed_read() {
        let stopped = Server::new(|req, _| match req.target.as_str() {
            "/mgw/api/v3/projects/1552" => reply(
                200,
                json!({"data": {"id": 1552, "attributes": {"status": "Stopped"}}}),
            ),
            "/mgw/api/v1/projects/1552/_starttrigger" => Fault::reply(200, "Project started"),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let hub = HubInfo {
            id: "streambim:app".into(),
            name: "StreamBIM".into(),
            region: Some("app".into()),
            provider: Provider::StreamBim,
        };
        let info = |origin: &str| ProjectInfo {
            id: "streambim:app:1552".into(),
            hub_id: "streambim:app".into(),
            name: "Demo".into(),
            root_folder_urn: Some("0".into()),
            scopes: vec![],
            provider: Provider::StreamBim,
            meta: Some(meta_json(&meta_checked("app", origin, "1552").unwrap())),
        };
        let setup = project_setup(&api_for(&stopped), &hub, &info(&stopped.origin))
            .await
            .unwrap();
        assert_eq!(setup.root_folder_urn, "0");
        assert_eq!(setup.urn_ns, "");
        assert_eq!(setup.oss_scopes, None);
        assert_eq!(setup.tops, vec![("0".to_string(), "Dokument".to_string())]);
        let targets: Vec<String> = stopped.requests().into_iter().map(|r| r.target).collect();
        assert_eq!(
            targets,
            [
                "/mgw/api/v3/projects/1552",
                "/mgw/api/v1/projects/1552/_starttrigger"
            ]
        );

        // A failed read is not fatal, and no start trigger follows it.
        let broken = Server::new(|_, _| Fault::reply(404, "gone")).await;
        let setup = project_setup(&api_for(&broken), &hub, &info(&broken.origin))
            .await
            .unwrap();
        assert_eq!(setup.tops.len(), 1);
        assert_eq!(broken.count(), 1, "no start trigger after a failed read");
    }

    #[tokio::test]
    async fn folder_contents_maps_folders_and_pages_documents() {
        let server = Server::new(|req, _| {
            let t = req.target.as_str();
            if t.starts_with("/project-1552/api/v1/v2/folders?") {
                reply(200, json!({"data": [
                    {"id": "8", "type": "folders", "attributes": {"name": "Ritningar", "created-date": "2021-06-10T13:44:56.50162Z", "last-modified": "2021-06-15T00:30:00.627941Z", "is-deleted": false},
                     "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}, "modified-by": {"data": {"id": "ada@example.com", "type": "users"}}}},
                    {"id": "9", "type": "folders", "attributes": {"name": "Borttagen", "is-deleted": true},
                     "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}}},
                ]}))
            } else if t.contains("page%5Bskip%5D=0&") {
                reply(200, json!({
                    "data": [
                        {"id": "1002", "type": "documents", "attributes": {"filename": "plan.pdf", "filesize": 406602, "revision": 2, "uploaded-date": "2021-06-11T09:50:52.489056Z", "last-modified": "2021-06-11T09:50:52.489056Z", "is-deleted": false},
                         "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}, "uploaded-by": {"data": {"id": "ada@example.com", "type": "users"}}}},
                        {"id": "1003", "type": "documents", "attributes": {"filename": "gammal.pdf", "is-deleted": true}, "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}}},
                        {"id": "1004", "type": "documents", "attributes": {"filename": "annan.pdf", "is-deleted": false}, "relationships": {"parent": {"data": {"id": "9", "type": "folders"}}}},
                    ],
                    "meta": {"total": 4, "timestamp": "1624541386399325605"},
                }))
            } else if t.contains("page%5Bskip%5D=3&") {
                reply(200, json!({
                    "data": [
                        {"id": "1005", "type": "documents", "attributes": {"filename": "Leverans.url", "is-deleted": false}, "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}}},
                        {"id": "1006", "type": "documents", "attributes": {"filename": "Snitt.dwg", "revision": 1, "filesize": 10, "is-deleted": false}, "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}}},
                    ],
                    "meta": {"total": 4},
                }))
            } else {
                Fault::reply(404, "unexpected")
            }
        })
        .await;
        let items = folder_contents(&api_for(&server), &project(&server.origin), "7")
            .await
            .unwrap();

        let names: Vec<(&str, Kind)> = items.iter().map(|i| (i.name.as_str(), i.kind)).collect();
        assert_eq!(
            names,
            [
                ("Ritningar", Kind::Folder),
                ("plan.pdf", Kind::File),
                ("Snitt.dwg", Kind::File)
            ]
        );
        let folder = &items[0];
        assert_eq!(folder.id, "8");
        assert_eq!(folder.parent.as_deref(), Some("7"));
        assert_eq!(folder.modified_by.as_deref(), Some("ada@example.com"));
        assert!(folder.ctime_ms > 0 && folder.mtime_ms > folder.ctime_ms);
        let plan = &items[1];
        assert_eq!(plan.id, "d1002");
        assert_eq!(plan.parent.as_deref(), Some("7"));
        assert_eq!(plan.size, 406602);
        assert_eq!(plan.remote_version, Some(2));
        assert_eq!(plan.storage_urn.as_deref(), Some("streambim:1002:2"));
        assert_eq!(plan.version_urn, None);
        assert_eq!(plan.modified_by.as_deref(), Some("ada@example.com"));
        assert!(plan.ctime_ms > 0);

        let docs: Vec<String> = server
            .requests()
            .into_iter()
            .map(|r| r.target)
            .filter(|t| t.contains("/documents?"))
            .collect();
        assert_eq!(docs.len(), 2, "a second page because total is 4");
        assert!(
            docs[0].contains("filter%5BfolderId%5D=7") && docs[0].contains("page%5Blimit%5D=500")
        );
        assert!(docs[1].contains("page%5Bskip%5D=3"));
    }

    #[tokio::test]
    async fn download_links_in_both_variants_and_the_bearer_decision() {
        let cases: [(&str, &str, Option<&str>); 4] = [
            (
                r#"{"data": "/documents/1002/1/download?token=abc"}"#,
                "/documents/1002/1/download?token=abc",
                None,
            ),
            (
                r#""documents/1002/download/plan.pdf""#,
                "/documents/1002/download/plan.pdf",
                Some("synka-loopback-test-token"),
            ),
            (
                r#"{"data": "/documents/1002/download?microtoken=mt"}"#,
                "/documents/1002/download?microtoken=mt",
                None,
            ),
            (
                "documents/1002/download/plan.pdf\n",
                "/documents/1002/download/plan.pdf",
                Some("synka-loopback-test-token"),
            ),
        ];
        for (body, path, bearer) in cases {
            let server = Server::scripted(vec![Fault::reply(200, body)]).await;
            let p = project(&server.origin);
            let d = download_url(&api_for(&server), &p, "d1002", "streambim:1002:1")
                .await
                .unwrap();
            assert_eq!(
                d.url,
                format!("{}/project-1552/api/v1{path}", server.origin)
            );
            assert_eq!(d.bearer.as_deref(), bearer, "{body}");
            assert_eq!(d.size, 0);
            assert!(!d.ranges);
            let request = &server.requests()[0];
            assert_eq!(
                request.target,
                "/project-1552/api/v1/documents/1002/downloadlink"
            );
            assert_eq!(request.header("accept"), Some("application/json"));
        }

        // A presigned link on another host never gets the bearer token.
        let server = Server::scripted(vec![Fault::reply(
            200,
            r#"{"data": "https://bucket.example.invalid/x?X-Amz-Signature=1"}"#,
        )])
        .await;
        let d = download_url(&api_for(&server), &project(&server.origin), "d1002", "x")
            .await
            .unwrap();
        assert_eq!(d.url, "https://bucket.example.invalid/x?X-Amz-Signature=1");
        assert_eq!(d.bearer, None);
    }

    #[tokio::test]
    async fn open_download_sends_the_bearer_only_when_the_link_has_one() {
        let server = Server::new(|req, _| match req.target.as_str() {
            "/project-1552/api/v1/documents/1002/download/plan.pdf" => {
                Fault::reply(200, "PDF-BYTES")
            }
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let api = api_for(&server);
        let d = DownloadUrl {
            url: format!(
                "{}/project-1552/api/v1/documents/1002/download/plan.pdf",
                server.origin
            ),
            size: 0,
            bearer: Some("synka-loopback-test-token".into()),
            ranges: false,
        };
        let resp = open_download(&api, &project(&server.origin), &d, 0)
            .await
            .unwrap();
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"PDF-BYTES");
        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("Bearer synka-loopback-test-token")
        );
    }

    /// Routes the upload calls: the ticket, the S3 PUT and the commit (`commit_path`).
    fn upload_fault(req: &Request, commit_path: &str, commit: &Fault) -> Option<Fault> {
        let host = req.header("host").unwrap_or_default();
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/project-1552/api/v1/v2/upload-tickets") => Some(reply(
                200,
                json!({"data": {
                    "id": "ticket-uuid",
                    "type": "upload-tickets",
                    "attributes": {"s3presignedurl": format!("http://{host}/s3/bucket/key?X-Amz-Expires=300")},
                }}),
            )),
            // The presigned URL carries a query string: match the path only.
            ("PUT", target) if target.starts_with("/s3/bucket/key") => Some(Fault::reply(200, "")),
            ("POST", target) if target == commit_path => Some(commit.clone()),
            _ => None,
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn create_item_uploads_to_s3_without_credentials_and_then_commits() {
        let bytes = pattern(300_000);
        let file = TestFile::new(&bytes);
        let commit = reply(
            200,
            json!({"data": {
                "id": "1002", "type": "documents",
                "attributes": {"filename": "plan.pdf", "filesize": 300000, "revision": 1,
                               "uploaded-date": "2021-06-11T09:50:52.489056Z", "last-modified": "2021-06-11T09:50:52.489056Z"},
                "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}, "uploaded-by": {"data": {"id": "ada@example.com", "type": "users"}}},
            }}),
        );
        let server = Server::new(move |req, _| {
            upload_fault(req, "/project-1552/api/v1/v2/documents", &commit)
                .unwrap_or_else(|| Fault::reply(404, "unexpected"))
        })
        .await;
        let last = Mutex::new((0u64, 0u64));
        let progress = |done: u64, total: u64| *last.lock().unwrap() = (done, total);
        let item = create_item(
            &api_for(&server),
            &project(&server.origin),
            "7",
            "plan.pdf",
            PENDING_STORAGE,
            &file.0,
            &progress,
        )
        .await
        .unwrap();

        assert_eq!(item.id, "d1002");
        assert_eq!(item.name, "plan.pdf");
        assert_eq!(item.parent.as_deref(), Some("7"));
        assert_eq!(item.remote_version, Some(1));
        assert_eq!(item.storage_urn.as_deref(), Some("streambim:1002:1"));
        assert_eq!(*last.lock().unwrap(), (300_000, 300_000));

        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let ticket = body_of(&requests[0]);
        assert_eq!(ticket["data"]["type"], "attachments");
        assert_eq!(ticket["data"]["attributes"]["filename"], "plan.pdf");
        assert_eq!(ticket["data"]["attributes"]["file-size"], 300_000);
        // The S3 PUT: no StreamBIM credentials, the raw bytes, and a declared length.
        let put = &requests[1];
        assert_eq!(put.method, "PUT");
        assert_eq!(put.header("authorization"), None);
        assert_eq!(put.header("content-type"), Some("application/octet-stream"));
        assert_eq!(put.body, bytes);
        // The commit names the ticket, the parent and the size.
        let commit = body_of(&requests[2]);
        assert_eq!(commit["data"]["attributes"]["filesize"], 300_000);
        assert_eq!(
            commit["data"]["relationships"]["upload-ticket"]["data"]["id"],
            "ticket-uuid"
        );
        assert_eq!(commit["data"]["relationships"]["parent"]["data"]["id"], "7");
        assert_eq!(
            commit["data"]["relationships"]["upload-ticket"]["data"]["type"],
            "upload-tickets"
        );
    }

    #[tokio::test]
    async fn create_version_uses_the_next_revision_id_and_the_document_filename() {
        let file = TestFile::new(&pattern(10));
        let commit = reply(
            200,
            json!({"data": {"id": "1002-5", "type": "document-revisions"}}),
        );
        let server = Server::new(move |req, _| {
            if req.method == "GET" && req.target == "/project-1552/api/v1/v2/documents/1002" {
                return reply(
                    200,
                    json!({"data": {"id": "1002", "type": "documents",
                    "attributes": {"filename": "plan.pdf", "revision": 4}}}),
                );
            }
            upload_fault(req, "/project-1552/api/v1/v2/document-revisions", &commit)
                .unwrap_or_else(|| Fault::reply(404, "unexpected"))
        })
        .await;
        let progress = |_: u64, _: u64| {};
        let (revision, storage) = create_version(
            &api_for(&server),
            &project(&server.origin),
            "d1002",
            "x",
            "y",
            &file.0,
            &progress,
        )
        .await
        .unwrap();
        assert_eq!((revision, storage.as_str()), (5, "streambim:1002:5"));
        let requests = server.requests();
        assert_eq!(
            body_of(&requests[1])["data"]["attributes"]["filename"],
            "plan.pdf"
        );
        let revision_body = body_of(requests.last().unwrap());
        assert_eq!(revision_body["data"]["id"], "1002-5");
        assert_eq!(revision_body["data"]["type"], "document-revisions");
        assert_eq!(
            revision_body["data"]["relationships"]["upload-ticket"]["data"]["id"],
            "ticket-uuid"
        );
    }

    #[tokio::test]
    async fn a_refused_revision_is_returned_as_is() {
        let file = TestFile::new(&pattern(10));
        let conflict = Fault::reply(
            409,
            r#"{"errors":[{"status":"409","code":"conflictError"}]}"#,
        );
        let server = Server::new(move |req, _| {
            if req.method == "GET" {
                return reply(200, json!({"data": {"id": "1002", "attributes": {"filename": "plan.pdf", "revision": 4}}}));
            }
            upload_fault(req, "/project-1552/api/v1/v2/document-revisions", &conflict)
                .unwrap_or_else(|| Fault::reply(404, "unexpected"))
        })
        .await;
        let progress = |_: u64, _: u64| {};
        let err = create_version(
            &api_for(&server),
            &project(&server.origin),
            "d1002",
            "x",
            "y",
            &file.0,
            &progress,
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), Some(409));
    }

    #[tokio::test]
    async fn rename_sends_the_full_object_with_the_new_name() {
        let server = Server::new(|req, _| match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/project-1552/api/v1/v2/documents/1002") => reply(200, json!({"data": {
                "id": "1002", "type": "documents",
                "attributes": {"filename": "plan.pdf", "description": "d", "scale": null,
                               "external-id": "x", "is-deleted": false, "num-revisions": 4, "revision": 4},
                "relationships": {"labels": {"data": [{"id": "1005", "type": "document-labels"}]},
                                  "parent": {"data": {"id": "7", "type": "folders"}}},
            }})),
            ("PATCH", "/project-1552/api/v1/v2/documents/1002") => reply(200, json!({"data": {"id": "1002"}})),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        rename_file(
            &api_for(&server),
            &project(&server.origin),
            "d1002",
            None,
            "renamed.pdf",
        )
        .await
        .unwrap();
        let patch = server.requests().pop().unwrap();
        assert_eq!(patch.method, "PATCH");
        let body = body_of(&patch);
        let attrs = body["data"]["attributes"].as_object().unwrap();
        let mut keys: Vec<&str> = attrs.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "description",
                "external-id",
                "filename",
                "is-deleted",
                "scale"
            ]
        );
        assert_eq!(attrs["filename"], "renamed.pdf");
        assert_eq!(attrs["description"], "d");
        assert_eq!(attrs["scale"], Value::Null);
        assert_eq!(body["data"]["id"], "1002");
        assert_eq!(
            body["data"]["relationships"]["labels"]["data"][0]["id"],
            "1005"
        );
        assert_eq!(body["data"]["relationships"]["parent"]["data"]["id"], "7");
    }

    #[tokio::test]
    async fn move_and_soft_delete_send_the_full_object_with_the_change() {
        let server = Server::new(|req, _| match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/project-1552/api/v1/v2/documents/1002") => reply(
                200,
                json!({"data": {
                    "id": "1002", "attributes": {"filename": "plan.pdf", "is-deleted": false},
                    "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}},
                }}),
            ),
            ("GET", "/project-1552/api/v1/v2/folders/8") => reply(
                200,
                json!({"data": {
                    "id": "8", "attributes": {"name": "Ritningar", "is-deleted": false},
                    "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}},
                }}),
            ),
            ("PATCH", _) => reply(200, json!({})),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let api = api_for(&server);
        let p = project(&server.origin);
        move_file(&api, &p, "d1002", "9").await.unwrap();
        delete_file(&api, &p, "d1002").await.unwrap();
        delete_folder(&api, &p, "8").await.unwrap();
        patch_folder(&api, &p, "8", None, None).await.unwrap();
        let patches: Vec<Value> = server
            .requests()
            .into_iter()
            .filter(|r| r.method == "PATCH")
            .map(|r| body_of(&r))
            .collect();
        assert_eq!(
            patches.len(),
            3,
            "patch_folder with nothing to change sends nothing"
        );
        assert_eq!(
            patches[0]["data"]["relationships"]["parent"]["data"]["id"],
            "9"
        );
        assert_eq!(patches[0]["data"]["attributes"]["filename"], "plan.pdf");
        assert_eq!(patches[1]["data"]["attributes"]["is-deleted"], true);
        assert_eq!(patches[2]["data"]["attributes"]["is-deleted"], true);
        assert_eq!(patches[2]["data"]["attributes"]["name"], "Ritningar");
    }

    #[tokio::test]
    async fn create_folder_posts_the_parent_and_returns_the_folder() {
        let server = Server::new(|req, _| match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/project-1552/api/v1/v2/folders") => reply(
                200,
                json!({"data": {
                    "id": "12", "type": "folders", "attributes": {"name": "Nytt"},
                    "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}},
                }}),
            ),
            _ => Fault::reply(404, "unexpected"),
        })
        .await;
        let item = create_folder(&api_for(&server), &project(&server.origin), "7", "Nytt")
            .await
            .unwrap();
        assert_eq!(
            (item.id.as_str(), item.name.as_str(), item.kind),
            ("12", "Nytt", Kind::Folder)
        );
        assert_eq!(item.parent.as_deref(), Some("7"));
        let body = body_of(&server.requests()[0]);
        assert_eq!(body["data"]["id"], "new");
        assert_eq!(body["data"]["attributes"]["name"], "Nytt");
        assert_eq!(body["data"]["relationships"]["parent"]["data"]["id"], "7");
    }

    /// The document listing of `folder 7` with `docs` as its rows; no folders.
    fn listing(req: &Request, docs: Value) -> Fault {
        if req.target.contains("/folders?") {
            reply(200, json!({"data": []}))
        } else if req.target.contains("/documents?") {
            // One page holding every row, with the total, so the listing ends after it.
            let total = docs.as_array().map_or(0, Vec::len);
            reply(200, json!({"data": docs, "meta": {"total": total}}))
        } else {
            Fault::reply(404, "unexpected")
        }
    }

    #[tokio::test]
    async fn a_failed_read_back_after_the_post_still_returns_the_document() {
        let file = TestFile::new(&pattern(300_000));
        let commit = reply(200, json!({"data": {"id": "1002", "type": "documents"}}));
        // The read-back of the new document fails: the answer to the POST is all there is.
        let server = Server::new(move |req, _| {
            upload_fault(req, "/project-1552/api/v1/v2/documents", &commit)
                .unwrap_or(Fault::Disconnect)
        })
        .await;
        let progress = |_: u64, _: u64| {};
        let item = create_item(
            &api_for(&server),
            &project(&server.origin),
            "7",
            "plan.pdf",
            PENDING_STORAGE,
            &file.0,
            &progress,
        )
        .await
        .unwrap();

        assert_eq!(item.id, "d1002");
        assert_eq!(item.name, "plan.pdf");
        assert_eq!(item.parent.as_deref(), Some("7"));
        assert_eq!(item.size, 300_000);
        assert_eq!(item.remote_version, Some(1));
        assert_eq!(item.storage_urn.as_deref(), Some("streambim:1002:1"));
        let commits = server
            .requests()
            .iter()
            .filter(|r| r.method == "POST" && r.target == "/project-1552/api/v1/v2/documents")
            .count();
        assert_eq!(commits, 1, "one commit, no second upload");
    }

    #[tokio::test]
    async fn a_reply_without_a_usable_id_is_read_from_the_listing_or_is_a_message() {
        let file = TestFile::new(&pattern(300_000));
        let older = json!({"id": "990", "type": "documents", "attributes": {"filename": "plan.pdf", "filesize": 5, "revision": 1, "is-deleted": false}, "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}}});
        let created = json!({"id": "1002", "type": "documents", "attributes": {"filename": "plan.pdf", "filesize": 300000, "revision": 1, "is-deleted": false}, "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}}});
        // Listed: the newest file of that name and size is the one just made.
        for (commit, docs, expect) in [
            (
                reply(200, json!({})),
                json!([older.clone(), created.clone()]),
                Some("d1002"),
            ),
            (
                Fault::reply(200, "not json"),
                json!([created.clone()]),
                Some("d1002"),
            ),
            // Not listed (the older file has another size): a message, never a retry.
            (reply(200, json!({})), json!([older.clone()]), None),
            (Fault::reply(200, "not json"), json!([]), None),
        ] {
            let server = Server::new(move |req, _| {
                upload_fault(req, "/project-1552/api/v1/v2/documents", &commit)
                    .unwrap_or_else(|| listing(req, docs.clone()))
            })
            .await;
            let progress = |_: u64, _: u64| {};
            let result = create_item(
                &api_for(&server),
                &project(&server.origin),
                "7",
                "plan.pdf",
                PENDING_STORAGE,
                &file.0,
                &progress,
            )
            .await;
            match expect {
                Some(id) => assert_eq!(result.unwrap().id, id),
                None => {
                    let err = result.unwrap_err();
                    assert!(
                        matches!(&err, ApiError::Message(m) if m == NOT_LISTED),
                        "{err:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn uploads_into_a_deleted_document_are_refused_with_a_404() {
        let file = TestFile::new(&pattern(10));
        let deleted = json!({"data": {"id": "1002", "type": "documents",
                              "attributes": {"filename": "plan.pdf", "revision": 4, "is-deleted": true}}});
        let server = Server::new(move |req, _| {
            if req.method == "GET" && req.target == "/project-1552/api/v1/v2/documents/1002" {
                return reply(200, deleted.clone());
            }
            Fault::reply(404, "unexpected")
        })
        .await;
        let api = api_for(&server);
        let p = project(&server.origin);
        let progress = |_: u64, _: u64| {};

        let err = create_version(&api, &p, "d1002", "x", "y", &file.0, &progress)
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(404));
        assert!(err.to_string().contains("document is deleted"), "{err}");
        let err = get_item_upload_info(&api, &p, "d1002").await.err().unwrap();
        assert_eq!(err.status(), Some(404));
        assert!(err.to_string().contains("document is deleted"), "{err}");
        // Only the two reads: no ticket and no upload.
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn a_failed_upload_never_shows_the_presigned_url() {
        let server = Server::new(|req, _| {
            let host = req.header("host").unwrap_or_default();
            match (req.method.as_str(), req.target.as_str()) {
                ("POST", "/project-1552/api/v1/v2/upload-tickets") => reply(
                    200,
                    json!({"data": {"id": "ticket-uuid", "type": "upload-tickets",
                        "attributes": {"s3presignedurl": format!("http://{host}/s3/bucket/key?X-Amz-Signature=secret-sig")}}}),
                ),
                // The storage hangs up on the PUT.
                ("PUT", _) => Fault::Disconnect,
                _ => Fault::reply(404, "unexpected"),
            }
        })
        .await;
        let file = TestFile::new(&pattern(10));
        let progress = |_: u64, _: u64| {};
        let err = create_item(
            &api_for(&server),
            &project(&server.origin),
            "7",
            "plan.pdf",
            PENDING_STORAGE,
            &file.0,
            &progress,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
        let shown = format!("{err} {err:?}");
        assert!(
            !shown.contains("secret-sig") && !shown.contains("X-Amz"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_failed_download_never_shows_the_token_link() {
        let server = Server::new(|_, _| Fault::Disconnect).await;
        let d = DownloadUrl {
            url: format!(
                "{}/project-1552/api/v1/documents/1002/download/plan.pdf?token=tok-secret",
                server.origin
            ),
            size: 0,
            bearer: None,
            ranges: false,
        };
        let err = open_download(&api_for(&server), &project(&server.origin), &d, 0)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
        let shown = format!("{err} {err:?}");
        assert!(
            !shown.contains("tok-secret") && !shown.contains("plan.pdf"),
            "{shown}"
        );
    }

    /// The `page[skip]` of a document listing request (`usize::MAX` when there is none).
    fn skip_of(target: &str) -> usize {
        target
            .split("page%5Bskip%5D=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(usize::MAX)
    }

    fn doc_row(n: u32) -> Value {
        json!({"id": n.to_string(), "type": "documents",
               "attributes": {"filename": format!("f{n}.pdf"), "filesize": 1, "revision": 1, "is-deleted": false},
               "relationships": {"parent": {"data": {"id": "7", "type": "folders"}}}})
    }

    #[tokio::test]
    async fn a_listing_without_a_total_reads_until_an_empty_page() {
        let server = Server::new(|req, _| {
            if req.target.contains("/folders?") {
                return reply(200, json!({"data": []}));
            }
            let ids = match skip_of(&req.target) {
                0 => 1..=500,
                500 => 500..=999, // 500 comes again: it is read once
                1000 => 1000..=1002,
                _ => return reply(200, json!({"data": []})),
            };
            reply(200, json!({"data": ids.map(doc_row).collect::<Vec<_>>()}))
        })
        .await;
        let items = folder_contents(&api_for(&server), &project(&server.origin), "7")
            .await
            .unwrap();
        assert_eq!(items.len(), 1002);
        let docs: Vec<String> = server
            .requests()
            .into_iter()
            .map(|r| r.target)
            .filter(|t| t.contains("/documents?"))
            .collect();
        assert_eq!(docs.len(), 4, "three pages, then the empty one");
        assert!(docs[3].contains("page%5Bskip%5D=1003&"), "{}", docs[3]);
    }

    #[tokio::test]
    async fn a_listing_that_never_ends_is_refused_after_the_page_limit() {
        let server = Server::new(|req, _| {
            if req.target.contains("/folders?") {
                return reply(200, json!({"data": []}));
            }
            reply(200, json!({"data": [doc_row(1)]}))
        })
        .await;
        let err = folder_contents(&api_for(&server), &project(&server.origin), "7")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Message(_)), "{err:?}");
        let pages = server
            .requests()
            .iter()
            .filter(|r| r.target.contains("/documents?"))
            .count();
        assert_eq!(pages, MAX_DOC_PAGES);
    }

    #[tokio::test]
    async fn a_link_to_another_revision_is_refused_with_a_409() {
        let server = Server::scripted(vec![Fault::reply(
            200,
            r#"{"data": "/documents/1002/2/download?token=abc"}"#,
        )])
        .await;
        let p = project(&server.origin);
        let err = download_url(&api_for(&server), &p, "d1002", "streambim:1002:1")
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(409));
        assert!(err.to_string().contains("revision changed"), "{err}");
    }

    #[tokio::test]
    async fn a_link_to_the_handle_revision_is_used() {
        let server = Server::scripted(vec![Fault::reply(
            200,
            r#"{"data": "/documents/1002/2/download?token=abc"}"#,
        )])
        .await;
        let d = download_url(
            &api_for(&server),
            &project(&server.origin),
            "d1002",
            "streambim:1002:2",
        )
        .await
        .unwrap();
        assert!(d.url.ends_with("/documents/1002/2/download?token=abc"));
    }

    #[test]
    fn the_revision_of_a_link_and_of_a_handle() {
        let base = "https://app.streambim.com/project-1552/api/v1";
        assert_eq!(
            revision_in_link(&format!("{base}/documents/1002/3/download?token=t"), "1002").unwrap(),
            Some(3)
        );
        // Other forms name no revision, or another document.
        for link in [
            format!("{base}/documents/1002/download/plan.pdf"),
            format!("{base}/documents/1003/3/download"),
            "https://bucket.example.invalid/x?X-Amz=1".to_string(),
        ] {
            assert_eq!(revision_in_link(&link, "1002").unwrap(), None, "{link}");
        }
        assert_eq!(handle_revision("streambim:1002:7"), Some(7));
        assert_eq!(handle_revision("streambim:pending"), None);
        assert_eq!(handle_revision("plan"), None);
    }

    #[test]
    fn uploads_above_five_gib_are_unsupported() {
        assert!(ensure_uploadable(MAX_UPLOAD_BYTES).is_ok());
        let err = ensure_uploadable(MAX_UPLOAD_BYTES + 1).unwrap_err();
        assert!(matches!(err, ApiError::Unsupported(_)), "{err:?}");
    }

    #[tokio::test]
    async fn reserve_is_unsupported_and_web_link_names_the_project() {
        let api = Api::for_test("http://127.0.0.1:9", Duration::from_secs(1));
        let p = project("http://127.0.0.1:9");
        assert!(matches!(
            set_reserved(&api, &p, "1002", true).await,
            Err(ApiError::Unsupported(_))
        ));
        let item = Item {
            pid: 1,
            id: "d1002".into(),
            parent: None,
            name: "plan.pdf".into(),
            kind: Kind::File,
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
        };
        assert_eq!(
            web_link(&api, &p, &item).await.unwrap(),
            "http://127.0.0.1:9/webapp/default/#/?projectId=1552"
        );
    }

    #[test]
    fn meta_must_name_a_plain_region_base_and_project() {
        let good = r#"{"region":"app","base":"https://app.streambim.com","project":"1552"}"#;
        assert_eq!(
            parse_meta(Some(good)).unwrap().base,
            "https://app.streambim.com"
        );
        assert!(parse_meta(None).is_err());
        assert!(parse_meta(Some("{}")).is_err());
        for bad in [
            r#"{"region":"App","base":"https://app.streambim.com","project":"1552"}"#,
            r#"{"region":"app","base":"ftp://app.streambim.com","project":"1552"}"#,
            r#"{"region":"app","base":"https://app.streambim.com","project":"15 52/x"}"#,
            r#"{"region":"app","base":"https://app.streambim.com","project":""}"#,
        ] {
            assert!(parse_meta(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn download_link_joins_and_bearer_rules() {
        let v1 = "https://app.streambim.com/project-1552/api/v1";
        assert_eq!(
            join_download_link(v1, "/documents/1/download"),
            format!("{v1}/documents/1/download")
        );
        assert_eq!(
            join_download_link(v1, "documents/1/download"),
            format!("{v1}/documents/1/download")
        );
        assert_eq!(
            join_download_link(v1, "https://x.example/y"),
            "https://x.example/y"
        );
        assert!(needs_bearer(
            &format!("{v1}/documents/1/download"),
            "https://app.streambim.com"
        )
        .unwrap());
        assert!(!needs_bearer(
            &format!("{v1}/documents/1/download?token=t"),
            "https://app.streambim.com"
        )
        .unwrap());
        assert!(!needs_bearer(
            "https://app.streambim.com/x?microtoken=m",
            "https://app.streambim.com"
        )
        .unwrap());
        assert!(!needs_bearer(
            "https://bucket.example/x?X-Amz=1",
            "https://app.streambim.com"
        )
        .unwrap());
        assert_eq!(
            parse_download_link(br#"{"data": 5}"#).unwrap_err().status(),
            None
        );
        assert!(parse_download_link(b"  \"\"  ").is_err());
    }

    #[test]
    fn folder_and_document_with_the_same_number_get_distinct_ids() {
        let folder_row = json!({"id": "7", "type": "folders", "attributes": {"name": "Ritningar"}});
        let doc_row = json!({"id": "7", "type": "documents",
                             "attributes": {"filename": "plan.pdf", "revision": 3}});
        let folder = folder_item(1, &folder_row, "0", None).unwrap();
        let doc = file_item(1, &doc_row, Some("0")).unwrap();
        assert_eq!(folder.id, "7");
        assert_eq!(doc.id, "d7");
        assert_ne!(folder.id, doc.id);
        // The storage handle keeps the server id and the revision.
        assert_eq!(doc.storage_urn.as_deref(), Some("streambim:7:3"));
        assert_eq!(server_doc_id(&doc.id).unwrap(), "7");
    }

    #[test]
    fn only_prefixed_ids_name_a_document() {
        assert_eq!(server_doc_id("d1002").unwrap(), "1002");
        for bad in ["1002", "d", "", "d1002/../x", "dd1002?token=x"] {
            assert!(
                matches!(server_doc_id(bad), Err(ApiError::Message(_))),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn engine_ids_without_the_document_prefix_are_refused_before_any_request() {
        let server = Server::new(|_, _| Fault::reply(404, "unexpected")).await;
        let api = api_for(&server);
        let p = project(&server.origin);
        let file = TestFile::new(&pattern(10));
        let progress = |_: u64, _: u64| {};
        let refused = [
            download_url(&api, &p, "1002", "streambim:1002:1")
                .await
                .err(),
            get_item_upload_info(&api, &p, "1002").await.err(),
            create_version(&api, &p, "1002", "x", "y", &file.0, &progress)
                .await
                .err(),
            rename_file(&api, &p, "1002", None, "x.pdf").await.err(),
            move_file(&api, &p, "1002", "9").await.err(),
            delete_file(&api, &p, "1002").await.err(),
        ];
        for err in refused {
            assert!(matches!(err, Some(ApiError::Message(_))), "{err:?}");
        }
        let item = Item {
            pid: 1,
            id: "1002".into(),
            parent: None,
            name: "plan.pdf".into(),
            kind: Kind::File,
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
        };
        assert!(matches!(
            web_link(&api, &p, &item).await,
            Err(ApiError::Message(_))
        ));
        assert_eq!(server.count(), 0);
    }

    #[test]
    fn id_and_timestamp_helpers() {
        assert_eq!(id_text(&json!(1552)).as_deref(), Some("1552"));
        assert_eq!(id_text(&json!("7")).as_deref(), Some("7"));
        assert_eq!(id_text(&json!("")), None);
        assert_eq!(id_text(&json!(null)), None);
        assert!(dm::ms(Some("2021-06-11T09:50:52.489056Z")) > 0);
        assert_eq!(storage_urn("1002", 3), "streambim:1002:3");
    }
}

//! Public Data Management API (project/v1, data/v1). JSON:API. Endpoints and fields: docs/specs/aps-endpoints.md.
//!
//! Path segments containing URNs must be percent-encoded (`urlencoding` style: encode ':' etc.).
//! Use `Body::JsonApi` for writes. Map JSON:API resources to `model::Item`:
//!   folder:  id = urn_suffix(data.id), name = attributes.name (or displayName),
//!            parent = urn_suffix(relationships.parent.data.id), kind Folder,
//!            ctime/mtime from createTime/lastModifiedTime (RFC3339 -> ms).
//!            Skip folders with attributes.hidden == true.
//!   item:    id = urn_suffix(data.id), name = tip version attributes.name (fallback displayName),
//!            remote_version = tip versionNumber, size = storageSize, storage_urn =
//!            tip relationships.storage.data.id, version_urn = tip id,
//!            modified_by = lastModifiedUserName, locked_by = reservedUserName if reserved.
//!            Skip hidden items and names ending .url/.lnk/.wsh.
//!   The tip version comes from `included[]` (type "versions") matched by
//!   relationships.tip.data.id.
//! Pagination: follow `links.next.href` (absolute URL) until absent. page[limit]=200 for contents.

use crate::http::{Api, ApiError, Body};
use crate::model::*;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fmt::Write as _;

/// Percent-encode everything except the RFC 3986 unreserved set (A-Z a-z 0-9 - _ . ~).
pub(crate) fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// RFC3339 -> unix ms; missing or unparsable -> 0.
pub(crate) fn ms(s: Option<&str>) -> i64 {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// True for names the sync layer never materialises (Windows shortcut files).
pub(crate) fn skip_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".url") || lower.ends_with(".lnk") || lower.ends_with(".wsh")
}

fn jsonapi(data: Value) -> Body {
    Body::JsonApi(json!({ "jsonapi": { "version": "1.0" }, "data": data }))
}

// ---------- wire types (only the fields we use) ----------

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Attrs {
    name: Option<String>,
    display_name: Option<String>,
    create_time: Option<String>,
    last_modified_time: Option<String>,
    last_modified_user_name: Option<String>,
    reserved: Option<bool>,
    reserved_user_name: Option<String>,
    hidden: Option<bool>,
    version_number: Option<i64>,
    storage_size: Option<i64>,
    mime_type: Option<String>,
    extension: Value,
    scopes: Option<Vec<String>>,
    region: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RelData {
    id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Rel {
    data: Option<RelData>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Rels {
    parent: Rel,
    tip: Rel,
    storage: Rel,
    root_folder: Rel,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Resource {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    attributes: Attrs,
    relationships: Rels,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Link {
    Obj { href: String },
    Str(String),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Links {
    next: Option<Link>,
}

impl Links {
    fn next_href(&self) -> Option<String> {
        match self.next.as_ref()? {
            Link::Obj { href } => Some(href.clone()),
            Link::Str(s) => Some(s.clone()),
        }
    }
}

/// A list response (data array + included + links).
#[derive(Deserialize, Default)]
struct Page {
    data: Vec<Resource>,
    #[serde(default)]
    included: Vec<Resource>,
    #[serde(default)]
    links: Links,
}

/// A single-resource response.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Single {
    data: Resource,
    included: Vec<Resource>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Many {
    data: Vec<Resource>,
}

/// Only `data.links.webView` of a single item or folder (D1/D2).
#[derive(Deserialize, Default)]
#[serde(default)]
struct WebViewDoc {
    data: WebViewData,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WebViewData {
    links: WebViewLinks,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct WebViewLinks {
    web_view: Option<Link>,
}

impl WebViewDoc {
    fn href(&self) -> Option<String> {
        let href = match self.data.links.web_view.as_ref()? {
            Link::Obj { href } => href,
            Link::Str(s) => s,
        };
        Some(href.clone()).filter(|h| !h.is_empty())
    }
}

fn decode(msg: impl Into<String>) -> ApiError {
    ApiError::Decode(msg.into())
}

// ---------- mapping ----------

fn tip_of<'a>(item: &Resource, included: &'a [Resource]) -> Option<&'a Resource> {
    let tip = item.relationships.tip.data.as_ref()?.id.as_str();
    included
        .iter()
        .find(|r| r.kind == "versions" && r.id == tip)
}

fn parent_of(r: &Resource, fallback: Option<&str>) -> Option<String> {
    r.relationships
        .parent
        .data
        .as_ref()
        .map(|d| urn_suffix(&d.id).to_string())
        .or_else(|| fallback.map(str::to_string))
}

/// Map a file item (+ its tip version from `included`). Does not filter.
fn file_item(pid: Pid, r: &Resource, included: &[Resource], parent_fallback: Option<&str>) -> Item {
    let tip = tip_of(r, included);
    let tv = tip.map(|t| &t.attributes);
    let name = tv
        .and_then(|a| a.name.clone())
        .or_else(|| r.attributes.display_name.clone())
        .unwrap_or_default();
    let mtime_src = tv
        .and_then(|a| a.last_modified_time.as_deref())
        .or(r.attributes.last_modified_time.as_deref());
    let locked_by = (r.attributes.reserved == Some(true))
        .then(|| r.attributes.reserved_user_name.clone().unwrap_or_default());
    Item {
        pid,
        id: urn_suffix(&r.id).to_string(),
        parent: parent_of(r, parent_fallback),
        name,
        kind: Kind::File,
        remote_version: tv.and_then(|a| a.version_number),
        base_version: None,
        size: tv.and_then(|a| a.storage_size).unwrap_or(0),
        ctime_ms: ms(r.attributes.create_time.as_deref()),
        mtime_ms: ms(mtime_src),
        storage_urn: tip
            .and_then(|t| t.relationships.storage.data.as_ref())
            .map(|d| d.id.clone()),
        version_urn: tip.map(|t| t.id.clone()),
        modified_by: r.attributes.last_modified_user_name.clone(),
        locked_by,
        local_state: LocalState::Clean,
        populated: false,
    }
}

/// Map a folder resource. None when it has no usable name.
fn folder_item(pid: Pid, r: &Resource, parent_fallback: Option<&str>) -> Option<Item> {
    let name = r
        .attributes
        .name
        .clone()
        .or_else(|| r.attributes.display_name.clone())
        .filter(|s| !s.is_empty())?;
    Some(Item {
        pid,
        id: urn_suffix(&r.id).to_string(),
        parent: parent_of(r, parent_fallback),
        name,
        kind: Kind::Folder,
        remote_version: None,
        base_version: None,
        size: 0,
        ctime_ms: ms(r.attributes.create_time.as_deref()),
        mtime_ms: ms(r.attributes.last_modified_time.as_deref()),
        storage_urn: None,
        version_urn: None,
        modified_by: None,
        locked_by: None,
        local_state: LocalState::Clean,
        populated: false,
    })
}

/// Map one folder-contents page (folders and items), applying the hidden/shortcut filters.
fn map_contents(pid: Pid, folder_id: &str, page: &Page, out: &mut Vec<Item>) {
    for r in &page.data {
        if r.attributes.hidden == Some(true) {
            continue;
        }
        match r.kind.as_str() {
            "folders" => {
                if let Some(it) = folder_item(pid, r, Some(folder_id)) {
                    out.push(it);
                }
            }
            "items" => {
                let it = file_item(pid, r, &page.included, Some(folder_id));
                if !skip_name(&it.name) {
                    out.push(it);
                }
            }
            _ => {}
        }
    }
}

/// Follow `links.next.href` from `first`, calling `f` for every page.
async fn for_each_page(api: &Api, first: String, mut f: impl FnMut(&Page)) -> Result<(), ApiError> {
    let mut next = Some(first);
    let mut visited = HashSet::new();
    while let Some(url) = next.take() {
        if !visited.insert(url.clone()) {
            return Err(decode(
                "pagination: repeated page; remote listing is incomplete",
            ));
        }
        let page: Page = api.json(Method::GET, &url, None).await?;
        let n = page.links.next_href();
        if n.as_ref().is_some_and(|next| visited.contains(next)) {
            return Err(decode(
                "pagination: repeated page; remote listing is incomplete",
            ));
        }
        f(&page);
        next = n;
    }
    Ok(())
}

fn project_base(p: &Project) -> String {
    format!("/data/v1/projects/{}", pct(&p.project_id))
}

// ---------- public API ----------

pub async fn list_hubs(api: &Api) -> Result<Vec<HubInfo>, ApiError> {
    // GET /project/v1/hubs?filter[extension.type]=hubs:autodesk.bim360:Account
    let url = format!(
        "/project/v1/hubs?filter[extension.type]={}",
        pct("hubs:autodesk.bim360:Account")
    );
    let mut out = Vec::new();
    for_each_page(api, url, |page| {
        for r in &page.data {
            out.push(HubInfo {
                id: r.id.clone(),
                name: r.attributes.name.clone().unwrap_or_default(),
                region: r.attributes.region.clone(),
            });
        }
    })
    .await?;
    Ok(out)
}

pub async fn list_projects(api: &Api, hub_id: &str) -> Result<Vec<ProjectInfo>, ApiError> {
    // GET /project/v1/hubs/{hub}/projects?page[limit]=200 ; skip projects without rootFolder
    let url = format!("/project/v1/hubs/{}/projects?page[limit]=200", pct(hub_id));
    let mut out = Vec::new();
    for_each_page(api, url, |page| {
        for r in &page.data {
            let Some(root) = r.relationships.root_folder.data.as_ref() else {
                continue;
            };
            out.push(ProjectInfo {
                id: r.id.clone(),
                hub_id: hub_id.to_string(),
                name: r.attributes.name.clone().unwrap_or_default(),
                root_folder_urn: Some(root.id.clone()),
                scopes: r.attributes.scopes.clone().unwrap_or_default(),
            });
        }
    })
    .await?;
    Ok(out)
}

pub async fn get_project(
    api: &Api,
    hub_id: &str,
    project_id: &str,
) -> Result<ProjectInfo, ApiError> {
    // GET /project/v1/hubs/{hub}/projects/{project}
    let url = format!(
        "/project/v1/hubs/{}/projects/{}",
        pct(hub_id),
        pct(project_id)
    );
    let s: Single = api.json(Method::GET, &url, None).await?;
    Ok(ProjectInfo {
        id: s.data.id.clone(),
        hub_id: hub_id.to_string(),
        name: s.data.attributes.name.clone().unwrap_or_default(),
        root_folder_urn: s
            .data
            .relationships
            .root_folder
            .data
            .as_ref()
            .map(|d| d.id.clone()),
        scopes: s.data.attributes.scopes.clone().unwrap_or_default(),
    })
}

/// The folders a project shows at its top, as (folder URN, name): GET .../topFolders?
/// projectFilesOnly=true. That is "Project Files" for a project admin or anyone with access to
/// it, and otherwise every highest folder the user can reach inside it (several for a
/// consultant who only sees their own discipline folders). "Plans" and the hidden system
/// folders are left out: Plans holds sheets, not files.
pub async fn top_folders(
    api: &Api,
    hub_id: &str,
    project_id: &str,
) -> Result<Vec<(String, String)>, ApiError> {
    let url = format!(
        "/project/v1/hubs/{}/projects/{}/topFolders?projectFilesOnly=true&excludeDeleted=true",
        pct(hub_id),
        pct(project_id)
    );
    let m: Many = api.json(Method::GET, &url, None).await?;
    Ok(top_folder_list(m))
}

fn top_folder_list(m: Many) -> Vec<(String, String)> {
    m.data
        .into_iter()
        .filter(|r| r.attributes.hidden != Some(true) && r.id.contains(FOLDER_URN_PREFIX))
        .filter_map(|r| {
            let name = r
                .attributes
                .name
                .clone()
                .or(r.attributes.display_name.clone())
                .filter(|n| !n.trim().is_empty())?;
            Some((r.id, name))
        })
        .collect()
}

/// All children (folders and files) of a folder, all pages.
pub async fn folder_contents(
    api: &Api,
    p: &Project,
    folder_id: &str,
) -> Result<Vec<Item>, ApiError> {
    // GET /data/v1/projects/{p}/folders/{urn}/contents?page[limit]=200
    let url = format!(
        "{}/folders/{}/contents?page[limit]=200",
        project_base(p),
        pct(&p.folder_urn(folder_id))
    );
    let mut out = Vec::new();
    for_each_page(api, url, |page| {
        map_contents(p.pid, folder_id, page, &mut out)
    })
    .await?;
    Ok(out)
}

/// D1 `links.webView.href`: the file in the ACC web UI.
pub async fn item_web_view(api: &Api, p: &Project, lineage_id: &str) -> Result<String, ApiError> {
    let url = format!(
        "{}/items/{}",
        project_base(p),
        pct(&p.lineage_urn(lineage_id))
    );
    web_view(api, &url).await
}

/// Upload eligibility belongs to the remote version, not its filename extension.
/// In particular, a published Revit cloud model cannot be replaced by an OSS file.
#[derive(Clone, Debug, Default)]
pub(crate) struct UploadInfo {
    mime_type: Option<String>,
    cloud_model: bool,
}

impl UploadInfo {
    fn from_version(version: &Resource) -> Self {
        let attrs = &version.attributes;
        let mime_type = attrs
            .mime_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let extension_type = attrs.extension.get("type").and_then(Value::as_str);
        let nested_mime = attrs
            .extension
            .pointer("/data/mimeType")
            .and_then(Value::as_str);
        Self {
            mime_type: mime_type.map(str::to_string),
            cloud_model: extension_type.is_some_and(|kind| kind.ends_with(":C4RModel"))
                || [mime_type, nested_mime].into_iter().flatten().any(|mime| {
                    mime.split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("application/vnd.autodesk.r360")
                }),
        }
    }

    pub(crate) fn ensure_writable(&self) -> Result<(), ApiError> {
        if self.cloud_model {
            return Err(decode("Revit cloud model (C4RModel): cannot replace it with an ordinary file upload. Open and synchronize the cloud model through Revit; local changes remain on this computer."));
        }
        if let Some(mime) = self.mime_type() {
            let essence = mime.split(';').next().unwrap_or_default().trim();
            let token = |part: &str| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            };
            if mime.len() > 512
                || !essence
                    .split_once('/')
                    .is_some_and(|(kind, subtype)| token(kind) && token(subtype))
                || reqwest::header::HeaderValue::from_str(mime).is_err()
            {
                return Err(decode("upload target has an invalid MIME type"));
            }
        }
        Ok(())
    }

    pub(crate) fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }
}

/// Read the same current tip used for conflict detection, retaining its upload metadata.
pub(crate) async fn get_item_upload_info(
    api: &Api,
    p: &Project,
    lineage_id: &str,
) -> Result<(Item, UploadInfo), ApiError> {
    let url = format!(
        "{}/items/{}",
        project_base(p),
        pct(&p.lineage_urn(lineage_id))
    );
    let mut doc: Single = api.json(Method::GET, &url, None).await?;
    if tip_of(&doc.data, &doc.included).is_none() {
        // Some responses omit included[]. Ask the documented tip endpoint instead
        // of treating missing metadata as permission to overwrite a cloud model.
        let tip: Single = api.json(Method::GET, &format!("{url}/tip"), None).await?;
        if tip.data.kind != "versions" || tip.data.id.is_empty() {
            return Err(decode("upload target: current version metadata is missing"));
        }
        let expected = doc
            .data
            .relationships
            .tip
            .data
            .as_ref()
            .map(|v| v.id.as_str());
        if expected.is_some_and(|id| id != tip.data.id) {
            return Err(decode(
                "upload target changed while reading its metadata; retry",
            ));
        }
        doc.data.relationships.tip.data = Some(RelData {
            id: tip.data.id.clone(),
        });
        doc.included.push(tip.data);
    }
    let tip = tip_of(&doc.data, &doc.included)
        .ok_or_else(|| decode("upload target: current version metadata is missing"))?;
    let info = UploadInfo::from_version(tip);
    Ok((file_item(p.pid, &doc.data, &doc.included, None), info))
}

/// D2 `links.webView.href`: the folder in the ACC web UI.
pub async fn folder_web_view(api: &Api, p: &Project, folder_id: &str) -> Result<String, ApiError> {
    let url = format!(
        "{}/folders/{}",
        project_base(p),
        pct(&p.folder_urn(folder_id))
    );
    web_view(api, &url).await
}

async fn web_view(api: &Api, url: &str) -> Result<String, ApiError> {
    let doc: WebViewDoc = api.json(Method::GET, url, None).await?;
    doc.href()
        .ok_or_else(|| decode("no links.webView in response"))
}

/// M15/M16. Reserve (lock) or release a file. Returns the reserving user's name when the
/// response reports one. An empty or unparsable response body counts as success; a body that
/// reports the opposite state is an error.
pub async fn set_reserved(
    api: &Api,
    p: &Project,
    lineage_id: &str,
    reserved: bool,
) -> Result<Option<String>, ApiError> {
    let lineage = p.lineage_urn(lineage_id);
    let body = jsonapi(json!({
        "type": "items",
        "id": lineage,
        "attributes": { "reserved": reserved }
    }));
    let url = format!("{}/items/{}", project_base(p), pct(&lineage));
    let resp = api.request(Method::PATCH, &url, Some(body)).await?;
    let bytes = resp.bytes().await?;
    let s: Single = serde_json::from_slice(&bytes).unwrap_or_default();
    reservation(&s.data, reserved)
}

fn reservation(r: &Resource, requested: bool) -> Result<Option<String>, ApiError> {
    match r.attributes.reserved {
        Some(actual) if actual != requested => Err(decode(format!("reserved is still {actual}"))),
        _ if requested => Ok(r
            .attributes
            .reserved_user_name
            .clone()
            .filter(|n| !n.is_empty())),
        _ => Ok(None),
    }
}

/// M1. Returns the created folder.
pub async fn create_folder(
    api: &Api,
    p: &Project,
    parent_id: &str,
    name: &str,
) -> Result<Item, ApiError> {
    let body = jsonapi(json!({
        "type": "folders",
        "attributes": {
            "name": name,
            "extension": { "type": "folders:autodesk.bim360:Folder", "version": "1.0" }
        },
        "relationships": {
            "parent": { "data": { "type": "folders", "id": p.folder_urn(parent_id) } }
        }
    }));
    let s: Single = api
        .json(
            Method::POST,
            &format!("{}/folders", project_base(p)),
            Some(body),
        )
        .await?;
    folder_item(p.pid, &s.data, Some(parent_id))
        .ok_or_else(|| decode("create folder: no name in response"))
}

/// M2. Returns the storage URN (data.id).
pub async fn create_storage(
    api: &Api,
    p: &Project,
    parent_id: &str,
    name: &str,
) -> Result<String, ApiError> {
    let body = jsonapi(json!({
        "type": "objects",
        "attributes": { "name": name },
        "relationships": {
            "target": { "data": { "type": "folders", "id": p.folder_urn(parent_id) } }
        }
    }));
    let s: Single = api
        .json(
            Method::POST,
            &format!("{}/storage", project_base(p)),
            Some(body),
        )
        .await?;
    if s.data.id.is_empty() {
        return Err(decode("create storage: no id in response"));
    }
    Ok(s.data.id)
}

/// M4. First version of a new file. Returns the created item (from response data + included).
pub async fn create_item(
    api: &Api,
    p: &Project,
    parent_id: &str,
    name: &str,
    storage_urn: &str,
) -> Result<Item, ApiError> {
    let body = jsonapi(json!({
        "type": "items",
        "attributes": {
            "displayName": name,
            "extension": { "type": "items:autodesk.bim360:File", "version": "1.0" }
        },
        "relationships": {
            "tip": { "data": { "type": "versions", "id": "1" } },
            "parent": { "data": { "type": "folders", "id": p.folder_urn(parent_id) } }
        }
    }));
    let body = match body {
        Body::JsonApi(mut v) => {
            v["included"] = json!([{
                "type": "versions",
                "id": "1",
                "attributes": {
                    "name": name,
                    "extension": { "type": "versions:autodesk.bim360:File", "version": "1.0" }
                },
                "relationships": {
                    "storage": { "data": { "type": "objects", "id": storage_urn } }
                }
            }]);
            Body::JsonApi(v)
        }
        other => other,
    };
    let s: Single = api
        .json(
            Method::POST,
            &format!("{}/items", project_base(p)),
            Some(body),
        )
        .await?;
    Ok(file_item(p.pid, &s.data, &s.included, Some(parent_id)))
}

/// M5. New version of an existing file. Returns the item as it is after the call (use the
/// response version resource: versionNumber, id, storage; keep parent/name from input).
pub async fn create_version(
    api: &Api,
    p: &Project,
    lineage_id: &str,
    name: &str,
    storage_urn: &str,
) -> Result<(i64, String), ApiError> {
    let body = jsonapi(json!({
        "type": "versions",
        "attributes": {
            "name": name,
            "extension": { "type": "versions:autodesk.bim360:File", "version": "1.0" }
        },
        "relationships": {
            "item": { "data": { "type": "items", "id": p.lineage_urn(lineage_id) } },
            "storage": { "data": { "type": "objects", "id": storage_urn } }
        }
    }));
    let s: Single = api
        .json(
            Method::POST,
            &format!("{}/versions", project_base(p)),
            Some(body),
        )
        .await?;
    let vn = s
        .data
        .attributes
        .version_number
        .ok_or_else(|| decode("create version: no versionNumber in response"))?;
    Ok((vn, s.data.id))
}

/// M7. Rename a file: PATCH /data/v1/projects/{p}/versions/{tip version urn} attributes.name.
pub async fn rename_file(
    api: &Api,
    p: &Project,
    version_urn: &str,
    new_name: &str,
) -> Result<(), ApiError> {
    let body = jsonapi(json!({
        "type": "versions",
        "id": version_urn,
        "attributes": { "name": new_name }
    }));
    let url = format!("{}/versions/{}", project_base(p), pct(version_urn));
    api.request(Method::PATCH, &url, Some(body)).await?;
    Ok(())
}

/// M8. Move a file to another folder: PATCH items/{urn} relationships.parent.
pub async fn move_file(
    api: &Api,
    p: &Project,
    lineage_id: &str,
    new_parent_id: &str,
) -> Result<(), ApiError> {
    let lineage = p.lineage_urn(lineage_id);
    let body = jsonapi(json!({
        "type": "items",
        "id": lineage,
        "relationships": {
            "parent": { "data": { "type": "folders", "id": p.folder_urn(new_parent_id) } }
        }
    }));
    let url = format!("{}/items/{}", project_base(p), pct(&lineage));
    api.request(Method::PATCH, &url, Some(body)).await?;
    Ok(())
}

/// M11/M12. Rename and/or move a folder: PATCH folders/{urn}.
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
    let urn = p.folder_urn(folder_id);
    let mut data = json!({ "type": "folders", "id": urn });
    if let Some(n) = new_name {
        data["attributes"] = json!({ "name": n });
    }
    if let Some(par) = new_parent_id {
        data["relationships"] = json!({
            "parent": { "data": { "type": "folders", "id": p.folder_urn(par) } }
        });
    }
    let url = format!("{}/folders/{}", project_base(p), pct(&urn));
    api.request(Method::PATCH, &url, Some(jsonapi(data)))
        .await?;
    Ok(())
}

/// M13. Delete a file: POST versions with extension versions:autodesk.core:Deleted.
pub async fn delete_file(api: &Api, p: &Project, lineage_id: &str) -> Result<(), ApiError> {
    let body = jsonapi(json!({
        "type": "versions",
        "attributes": {
            "extension": { "type": "versions:autodesk.core:Deleted", "version": "1.0" }
        },
        "relationships": {
            "item": { "data": { "type": "items", "id": p.lineage_urn(lineage_id) } }
        }
    }));
    api.request(
        Method::POST,
        &format!("{}/versions", project_base(p)),
        Some(body),
    )
    .await?;
    Ok(())
}

/// M14. Delete a folder: PATCH folders/{urn} attributes.hidden=true.
pub async fn delete_folder(api: &Api, p: &Project, folder_id: &str) -> Result<(), ApiError> {
    let urn = p.folder_urn(folder_id);
    let body = jsonapi(json!({
        "type": "folders",
        "id": urn,
        "attributes": { "hidden": true }
    }));
    let url = format!("{}/folders/{}", project_base(p), pct(&urn));
    api.request(Method::PATCH, &url, Some(body)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resilience_upload_metadata_distinguishes_cloud_models_from_ordinary_rvt() {
        for attrs in [
            json!({"name":"model.rvt", "extension":{"type":"versions:autodesk.bim360:C4RModel"}}),
            json!({"name":"model.rvt", "mimeType":"application/vnd.autodesk.r360"}),
            json!({"name":"model.rvt", "mimeType":" APPLICATION/VND.AUTODESK.R360; charset=binary "}),
            json!({"name":"model.rvt", "extension":{"data":{"mimeType":"application/vnd.autodesk.r360"}}}),
        ] {
            let version: Resource = serde_json::from_value(json!({"attributes":attrs})).unwrap();
            assert!(UploadInfo::from_version(&version)
                .ensure_writable()
                .is_err());
        }
        for mime in [
            None,
            Some("application/vnd.autodesk.revit"),
            Some("application/octet-stream"),
        ] {
            let version: Resource = serde_json::from_value(json!({"attributes":{
                "name":"model.rvt", "mimeType":mime,
                "extension":{"type":"versions:autodesk.bim360:File"}}}))
            .unwrap();
            let info = UploadInfo::from_version(&version);
            info.ensure_writable().unwrap();
            assert_eq!(info.mime_type(), mime);
        }
        let version: Resource = serde_json::from_value(json!({"attributes":{
            "mimeType":"text/plain\r\nAuthorization: credential"}}))
        .unwrap();
        assert!(UploadInfo::from_version(&version)
            .ensure_writable()
            .is_err());
    }

    #[tokio::test]
    async fn resilience_upload_metadata_missing_included_tip_is_fetched_and_checked() {
        use crate::test_support::{Fault, Server};
        let server = Server::scripted(vec![
            Fault::reply(
                200,
                json!({"data":{"type":"items","id":"LIN-target",
                "attributes":{"displayName":"model.rvt"}}})
                .to_string(),
            ),
            Fault::reply(
                200,
                json!({"data":{"type":"versions","id":"VERSION-3",
                "attributes":{"name":"model.rvt","versionNumber":3,
                    "mimeType":"application/vnd.autodesk.r360"}}})
                .to_string(),
            ),
        ])
        .await;
        let api = Api::for_test(&server.origin, std::time::Duration::from_secs(2));
        let (_, info) = get_item_upload_info(&api, &proj(), "LIN-target")
            .await
            .unwrap();
        assert!(info.ensure_writable().is_err());
        assert_eq!(server.count(), 2);
        assert!(server.requests()[1].target.ends_with("/tip"));
    }

    fn proj() -> Project {
        Project {
            pid: 7,
            hub_id: "b.hub-1".into(),
            hub_name: "Hub".into(),
            project_id: "b.c3745078-aaaa".into(),
            name: "Kopmannen".into(),
            root_folder_urn: "urn:adsk.wipemea:fs.folder:co.ROOT".into(),
            urn_ns: "wipemea".into(),
            tops: vec![],
            oss_scopes: Some("data:read,data:write".into()),
            change_source: ChangeSourceKind::Unknown,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
        }
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(
            pct("urn:adsk.wipemea:dm.lineage:AbC-1_2.~"),
            "urn%3Aadsk.wipemea%3Adm.lineage%3AAbC-1_2.~"
        );
        assert_eq!(pct("a b/ç"), "a%20b%2F%C3%A7");
        assert_eq!(pct("b.c3745078-aaaa"), "b.c3745078-aaaa");
    }

    #[test]
    fn top_folders_skip_hidden_and_unnamed() {
        let json = r#"{"data": [
            {"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.PF","attributes":{"name":"Project Files","hidden":false}},
            {"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.SYS","attributes":{"name":"quantification_x","hidden":true}},
            {"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.B","attributes":{"displayName":"04 BIM"}},
            {"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.N","attributes":{"name":" "}}
        ]}"#;
        let m: Many = serde_json::from_str(json).unwrap();
        assert_eq!(
            top_folder_list(m),
            vec![
                (
                    "urn:adsk.wipemea:fs.folder:co.PF".to_string(),
                    "Project Files".to_string()
                ),
                (
                    "urn:adsk.wipemea:fs.folder:co.B".to_string(),
                    "04 BIM".to_string()
                ),
            ]
        );
    }

    #[test]
    fn folder_contents_page() {
        let json = r#"{
          "data": [
            {"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.F1",
             "attributes":{"name":"Ritningar","createTime":"2026-01-02T10:00:00Z",
                           "lastModifiedTime":"2026-02-03T11:30:00.500Z","hidden":false},
             "relationships":{"parent":{"data":{"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.ROOT"}}}},
            {"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.H",
             "attributes":{"name":"Hidden","hidden":true}},
            {"type":"items","id":"urn:adsk.wipemea:dm.lineage:LIN1",
             "attributes":{"displayName":"plan.rvt","createTime":"2026-01-01T00:00:00Z",
                           "lastModifiedTime":"2026-01-05T00:00:00Z","lastModifiedUserName":"Anna",
                           "reserved":true,"reservedUserName":"Bo","hidden":false},
             "relationships":{"parent":{"data":{"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.F1"}},
                              "tip":{"data":{"type":"versions","id":"urn:adsk.wipemea:fs.file:vf.V2?version=2"}}}},
            {"type":"items","id":"urn:adsk.wipemea:dm.lineage:LIN2",
             "attributes":{"displayName":"shortcut.URL"},
             "relationships":{"tip":{"data":{"type":"versions","id":"urn:adsk.wipemea:fs.file:vf.X?version=1"}}}}
          ],
          "included": [
            {"type":"versions","id":"urn:adsk.wipemea:fs.file:vf.V2?version=2",
             "attributes":{"name":"plan.rvt","versionNumber":2,"storageSize":1234,
                           "lastModifiedTime":"2026-01-06T08:00:00Z"},
             "relationships":{"storage":{"data":{"type":"objects",
                 "id":"urn:adsk.objects:os.object:wip.dm.emea.2/abc.rvt"}}}}
          ],
          "links": {"next": {"href": "https://developer.api.autodesk.com/data/v1/x?page[offset]=200"}}
        }"#;
        let page: Page = serde_json::from_str(json).unwrap();
        assert_eq!(
            page.links.next_href().as_deref(),
            Some("https://developer.api.autodesk.com/data/v1/x?page[offset]=200")
        );
        let mut out = Vec::new();
        map_contents(7, "co.F1", &page, &mut out);
        assert_eq!(out.len(), 2, "hidden folder and .URL shortcut are dropped");

        let f = &out[0];
        assert_eq!(f.id, "co.F1");
        assert_eq!(f.kind, Kind::Folder);
        assert_eq!(f.parent.as_deref(), Some("co.ROOT"));
        assert_eq!(f.name, "Ritningar");
        assert_eq!(f.ctime_ms, 1_767_348_000_000); // 2026-01-02T10:00:00Z

        let file = &out[1];
        assert_eq!(file.id, "LIN1");
        assert_eq!(file.kind, Kind::File);
        assert_eq!(file.parent.as_deref(), Some("co.F1"));
        assert_eq!(file.name, "plan.rvt");
        assert_eq!(file.remote_version, Some(2));
        assert_eq!(file.size, 1234);
        assert_eq!(
            file.storage_urn.as_deref(),
            Some("urn:adsk.objects:os.object:wip.dm.emea.2/abc.rvt")
        );
        assert_eq!(
            file.version_urn.as_deref(),
            Some("urn:adsk.wipemea:fs.file:vf.V2?version=2")
        );
        assert_eq!(file.modified_by.as_deref(), Some("Anna"));
        assert_eq!(file.locked_by.as_deref(), Some("Bo"));
        assert_eq!(file.mtime_ms, 1_767_686_400_000);
        assert_eq!(file.local_state, LocalState::Clean);
    }

    #[test]
    fn item_with_included_tip_single() {
        let json = r#"{
          "data": {"type":"items","id":"urn:adsk.wipemea:dm.lineage:LIN9",
            "attributes":{"displayName":"x.dwg","createTime":"2026-03-01T00:00:00Z"},
            "relationships":{"parent":{"data":{"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.P"}},
              "tip":{"data":{"type":"versions","id":"urn:adsk.wipemea:fs.file:vf.T?version=5"}}}},
          "included": [{"type":"versions","id":"urn:adsk.wipemea:fs.file:vf.T?version=5",
            "attributes":{"name":"x-v5.dwg","versionNumber":5,"storageSize":42},
            "relationships":{"storage":{"data":{"type":"objects","id":"urn:adsk.objects:os.object:b/k.dwg"}}}}]
        }"#;
        let s: Single = serde_json::from_str(json).unwrap();
        let it = file_item(7, &s.data, &s.included, None);
        assert_eq!(it.id, "LIN9");
        assert_eq!(it.name, "x-v5.dwg");
        assert_eq!(it.remote_version, Some(5));
        assert_eq!(it.size, 42);
        assert_eq!(
            it.storage_urn.as_deref(),
            Some("urn:adsk.objects:os.object:b/k.dwg")
        );
        assert_eq!(it.parent.as_deref(), Some("co.P"));
        assert_eq!(it.locked_by, None);
        assert_eq!(it.ctime_ms, 1_772_323_200_000);
        assert_eq!(it.mtime_ms, 0, "no lastModifiedTime -> 0");
    }

    #[test]
    fn urn_builders_encode() {
        let p = proj();
        assert_eq!(
            pct(&p.lineage_urn("ABC")),
            "urn%3Aadsk.wipemea%3Adm.lineage%3AABC"
        );
        assert_eq!(project_base(&p), "/data/v1/projects/b.c3745078-aaaa");
    }

    #[test]
    fn web_view_link() {
        let json = r#"{"data":{"type":"items","id":"urn:adsk.wipemea:dm.lineage:L",
            "links":{"self":{"href":"https://developer.api.autodesk.com/x"},
                     "webView":{"href":"https://acc.autodesk.eu/docs/files/projects/p?entityId=L"}}}}"#;
        let doc: WebViewDoc = serde_json::from_str(json).unwrap();
        assert_eq!(
            doc.href().as_deref(),
            Some("https://acc.autodesk.eu/docs/files/projects/p?entityId=L")
        );
        let doc: WebViewDoc =
            serde_json::from_str(r#"{"data":{"links":{"webView":"https://a/b"}}}"#).unwrap();
        assert_eq!(doc.href().as_deref(), Some("https://a/b"));
        let doc: WebViewDoc = serde_json::from_str(r#"{"data":{"links":{}}}"#).unwrap();
        assert_eq!(doc.href(), None);
    }

    #[test]
    fn reservation_response() {
        let locked: Single = serde_json::from_str(
            r#"{"data":{"type":"items","attributes":{"reserved":true,"reservedUserName":"Anna"}}}"#,
        )
        .unwrap();
        assert_eq!(
            reservation(&locked.data, true).unwrap().as_deref(),
            Some("Anna")
        );
        assert!(
            reservation(&locked.data, false).is_err(),
            "still reserved after unlock"
        );

        let free: Single =
            serde_json::from_str(r#"{"data":{"attributes":{"reserved":false}}}"#).unwrap();
        assert_eq!(reservation(&free.data, false).unwrap(), None);
        assert!(reservation(&free.data, true).is_err());

        let empty = Single::default();
        assert_eq!(
            reservation(&empty.data, true).unwrap(),
            None,
            "no body: trust the 2xx"
        );
        assert_eq!(reservation(&empty.data, false).unwrap(), None);
    }

    #[test]
    fn ms_parsing() {
        assert_eq!(ms(Some("1970-01-01T00:00:01Z")), 1000);
        assert_eq!(ms(Some("garbage")), 0);
        assert_eq!(ms(None), 0);
    }

    #[tokio::test]
    async fn resilience_realworld_listing_error_envelope_must_not_mean_empty_folder() {
        use crate::test_support::{Fault, Server};
        use std::time::Duration;

        let server = Server::scripted(vec![Fault::reply(
            200,
            r#"{"errors":[{"title":"upstream temporarily unavailable"}]}"#,
        )])
        .await;
        let api = Api::for_test(&server.origin, Duration::from_secs(2));
        let result = folder_contents(&api, &proj(), "co.ROOT").await;
        assert!(
            matches!(result, Err(ApiError::Decode(_))),
            "an unexpected successful JSON envelope must not become an empty remote listing: {result:?}"
        );
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn resilience_realworld_bad_second_page_never_returns_partial_success() {
        use crate::test_support::{Fault, Server};
        use std::time::Duration;

        let server = Server::scripted(vec![
            Fault::reply(
                200,
                json!({
                    "data": [{"type": "folders", "id": "co.A", "attributes": {"name": "A"}}],
                    "links": {"next": "/second-page"}
                })
                .to_string(),
            ),
            Fault::reply(200, r#"{"message":"gateway temporarily unavailable"}"#),
        ])
        .await;
        let api = Api::for_test(&server.origin, Duration::from_secs(2));
        assert!(matches!(
            folder_contents(&api, &proj(), "co.ROOT").await,
            Err(ApiError::Decode(_))
        ));
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn resilience_realworld_valid_pagination_still_keeps_all_pages_and_empty_folders() {
        use crate::test_support::{Fault, Server};
        use std::time::Duration;

        let server = Server::scripted(vec![
            Fault::reply(
                200,
                json!({
                    "data": [{"type": "folders", "id": "co.A", "attributes": {"name": "A"}}],
                    "links": {"next": {"href": "/second-page"}}
                })
                .to_string(),
            ),
            Fault::reply(
                200,
                json!({
                    "data": [{"type": "folders", "id": "co.B", "attributes": {"name": "B"}}]
                })
                .to_string(),
            ),
            Fault::reply(200, r#"{"data":[]}"#),
        ])
        .await;
        let api = Api::for_test(&server.origin, Duration::from_secs(2));
        let folders = folder_contents(&api, &proj(), "co.ROOT").await.unwrap();
        assert_eq!(
            folders
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert!(folder_contents(&api, &proj(), "co.EMPTY")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(server.count(), 3);
    }

    #[tokio::test]
    async fn resilience_realworld_self_repeating_page_is_not_a_complete_listing() {
        use crate::test_support::{Fault, Server};
        use std::time::Duration;

        let server = Server::new(|request, _| {
            Fault::reply(
                200,
                json!({"data": [], "links": {"next": request.target}}).to_string(),
            )
        })
        .await;
        let api = Api::for_test(&server.origin, Duration::from_secs(2));
        let result = folder_contents(&api, &proj(), "co.ROOT").await;
        assert!(
            matches!(result, Err(ApiError::Decode(_))),
            "a repeated next page must fail instead of accepting a partial folder listing: {result:?}"
        );
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn resilience_realworld_pagination_cycle_stops_without_request_storm() {
        use crate::test_support::{Fault, Server};
        use std::time::Duration;

        let server = Server::new(|request, _| {
            let next = if request.target == "/page-b" {
                "/page-a"
            } else {
                "/page-b"
            };
            Fault::reply(
                200,
                json!({"data": [], "links": {"next": next}}).to_string(),
            )
        })
        .await;
        let api = Api::for_test(&server.origin, Duration::from_secs(2));
        let outcome = tokio::time::timeout(
            Duration::from_millis(500),
            folder_contents(&api, &proj(), "co.ROOT"),
        )
        .await;
        assert!(
            outcome.is_ok(),
            "cyclic next links made {} requests without stopping",
            server.count()
        );
        assert!(matches!(outcome.unwrap(), Err(ApiError::Decode(_))));
        assert!(server.count() <= 3);
    }
}

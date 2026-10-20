//! Autodesk's undocumented dm/v3 desktop-sync delta feed. OFF by default: called only when
//! Config::desktop_sync_feed is true, which requires written approval from Autodesk. Do not call
//! from anywhere except Engine::poll_project.
//!
//! Cursor persisted in `Project::cursor` as JSON `{"f": "<folders token>", "d": "<documents token>"}`.
//! Tokens are opaque strings: never parse them.
//!
//! poll():
//!   1. If cursor is None or either token empty -> full enumeration: page S1 then S2 with
//!      empty syncToken until moreData == false (follow nextSyncToken). Result: Delta with
//!      `reset = true`, all current folders+files as upserts.
//!   2. Else POST S0 `api:desktop-sync-status` with
//!      `[{"syncToken":F,"referenceId":"folders"},{"syncToken":D,"referenceId":"documents"}]`.
//!      For each result with moreData == true (or missing), page that feed from its token.
//!      overwrite == true on any -> drop cursor, do full enumeration (reset=true).
//!      Nothing to do -> return an empty Delta with new_cursor None (caller writes nothing).
//!   3. Paging: POST S1/S2 `{"syncToken":T,"limit":1000}` -> `current.data`, `deleted.data`,
//!      `moreData`, `overwrite`, `nextSyncToken`; loop while moreData && !overwrite.
//!      new_cursor = JSON with the last nextSyncToken per feed (unchanged feeds keep their token).
//!   Mapping:
//!     folder: id = urn_suffix(urn), parent = urn_suffix(parentUrn), name; drop if name or
//!       parent missing. A top folder (`Project::tops`, e.g. "Project Files") is a child of
//!       the project directory (parent PROJECT_ROOT_ID) under its top-folder name.
//!       Skip hidden == true (treat as delete).
//!     document: id = urn_suffix(urn) (lineage), parent = urn_suffix(parentFolderUrn),
//!       name = includedVersion.name, remote_version = includedVersion.versionNumber,
//!       version_urn = includedVersion.urn, storage_urn = includedVersion.storageUrn,
//!       size = includedVersion.storageSize, mtime = includedVersion.updatedAt (or updatedAt),
//!       ctime = createdAt, modified_by = updatedByName, locked_by = reservedByName if reserved.
//!       Drop docs without parentFolderUrn or includedVersion; drop names ending .url/.lnk/.wsh;
//!       hidden == true -> delete.
//!     deleted.data -> Delta.deletes (urn_suffix).
//!   Items outside the top folders' subtrees are kept (the applier ignores orphans): the
//!   real ACC root above "Project Files", "Plans", system folders.
//!   Use Body::Json, Content-Type application/json. The project guid is `p.project_guid()`.
//!   Stream-friendly: deserialize each page into small serde structs with only the used
//!   fields (#[serde(default)] everywhere); do not keep raw JSON.

use super::dm::{ms, skip_name};
use crate::http::{Api, ApiError, Body};
use crate::model::*;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    /// 401 after refresh, 403 or 404 on the dm/v3 endpoints: this token/app cannot use the
    /// private feed. Caller switches the project to ChangeSourceKind::PublicWalk.
    #[error("desktop-sync feed not available (HTTP {0})")]
    NotSupported(u16),
    #[error(transparent)]
    Api(#[from] ApiError),
}

const LIMIT: u32 = 1000;

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Cursor {
    f: String,
    d: String,
}

// ---------- wire types ----------

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Feed {
    current: Bucket,
    deleted: Bucket,
    more_data: Option<bool>,
    overwrite: Option<bool>,
    next_sync_token: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Bucket {
    data: Vec<Obj>,
}

/// Folder or document object (only the fields we use).
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Obj {
    urn: String,
    name: Option<String>,
    parent_urn: Option<String>,
    parent_folder_urn: Option<String>,
    hidden: Option<bool>,
    reserved: Option<bool>,
    reserved_by_name: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    updated_by_name: Option<String>,
    included_version: Option<Version>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Version {
    urn: String,
    name: Option<String>,
    version_number: Option<i64>,
    storage_urn: Option<String>,
    storage_size: Option<i64>,
    updated_at: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StatusResp {
    results: Vec<StatusResult>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StatusResult {
    reference_id: String,
    more_data: Option<bool>,
    overwrite: Option<bool>,
}

// ---------- mapping ----------

/// Last change per item id wins, first-seen order kept. `None` means delete.
#[derive(Default)]
struct Changes {
    slots: Vec<(String, Option<Item>)>,
    index: HashMap<String, usize>,
}

impl Changes {
    fn set(&mut self, id: String, change: Option<Item>) {
        match self.index.get(&id) {
            Some(&i) => self.slots[i].1 = change,
            None => {
                self.index.insert(id.clone(), self.slots.len());
                self.slots.push((id, change));
            }
        }
    }

    fn into_delta(self, reset: bool) -> Delta {
        let mut d = Delta {
            reset,
            ..Default::default()
        };
        for (id, change) in self.slots {
            match change {
                Some(item) => d.upserts.push(item),
                None => d.deletes.push(id),
            }
        }
        d
    }
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

/// Folder object -> (id, change). None = ignore the object.
fn folder_change(p: &Project, o: &Obj) -> Option<(String, Option<Item>)> {
    let id = urn_suffix(&o.urn).to_string();
    if o.hidden == Some(true) {
        return Some((id, None));
    }
    let base = |name: String, parent: Option<String>, kind: Kind| Item {
        pid: p.pid,
        id: id.clone(),
        parent,
        name,
        kind,
        remote_version: None,
        base_version: None,
        size: 0,
        ctime_ms: ms(o.created_at.as_deref()),
        mtime_ms: ms(o.updated_at.as_deref()),
        storage_urn: None,
        version_urn: None,
        modified_by: None,
        locked_by: None,
        local_state: LocalState::Clean,
        populated: false,
    };
    if let Some(top) = p.top(&id) {
        return Some((
            id.clone(),
            Some(base(
                top.name.clone(),
                Some(PROJECT_ROOT_ID.to_string()),
                Kind::Folder,
            )),
        ));
    }
    let name = nonempty(&o.name)?.to_string();
    let parent = nonempty(&o.parent_urn).map(|u| urn_suffix(u).to_string())?;
    Some((id.clone(), Some(base(name, Some(parent), Kind::Folder))))
}

/// Document object -> (id, change). None = ignore the object.
fn document_change(p: &Project, o: &Obj) -> Option<(String, Option<Item>)> {
    let id = urn_suffix(&o.urn).to_string();
    if o.hidden == Some(true) {
        return Some((id, None));
    }
    let parent = nonempty(&o.parent_folder_urn).map(|u| urn_suffix(u).to_string())?;
    let v = o.included_version.as_ref()?;
    let name = v
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| o.name.clone())
        .unwrap_or_default();
    if skip_name(&name) {
        return None;
    }
    let item = Item {
        pid: p.pid,
        id: id.clone(),
        parent: Some(parent),
        name,
        kind: Kind::File,
        remote_version: v.version_number,
        base_version: None,
        size: v.storage_size.unwrap_or(0),
        ctime_ms: ms(o.created_at.as_deref()),
        mtime_ms: ms(v.updated_at.as_deref().or(o.updated_at.as_deref())),
        storage_urn: v.storage_urn.clone(),
        version_urn: Some(v.urn.clone()),
        modified_by: o.updated_by_name.clone(),
        locked_by: (o.reserved == Some(true))
            .then(|| o.reserved_by_name.clone().unwrap_or_default()),
        local_state: LocalState::Clean,
        populated: false,
    };
    Some((id, Some(item)))
}

fn change_for(p: &Project, o: &Obj, is_folders: bool) -> Option<(String, Option<Item>)> {
    if is_folders {
        folder_change(p, o)
    } else {
        document_change(p, o)
    }
}

// ---------- requests ----------

fn feed_err(e: ApiError) -> FeedError {
    match e.status() {
        Some(c @ (401 | 403 | 404)) => FeedError::NotSupported(c),
        _ => FeedError::Api(e),
    }
}

/// Page one feed (folders or documents) from `token` until moreData is false.
/// Returns the last token, or None if the server signalled `overwrite`.
async fn page_feed(
    api: &Api,
    p: &Project,
    is_folders: bool,
    mut token: String,
    ch: &mut Changes,
) -> Result<Option<String>, FeedError> {
    let path = if is_folders {
        "folders:desktop-sync"
    } else {
        "documents:desktop-sync"
    };
    let url = format!("/dm/v3/projects/{}/{path}", p.project_guid());
    loop {
        let body = Body::Json(json!({ "syncToken": token, "limit": LIMIT }));
        let feed: Feed = api
            .json(Method::POST, &url, Some(body))
            .await
            .map_err(feed_err)?;
        if feed.overwrite == Some(true) {
            return Ok(None);
        }
        for o in &feed.current.data {
            if let Some((id, change)) = change_for(p, o, is_folders) {
                ch.set(id, change);
            }
        }
        for o in &feed.deleted.data {
            ch.set(urn_suffix(&o.urn).to_string(), None);
        }
        let more = feed.more_data == Some(true);
        match feed.next_sync_token.filter(|t| !t.is_empty()) {
            Some(next) if more => token = next,
            Some(next) => return Ok(Some(next)),
            None => return Ok(Some(token)),
        }
    }
}

/// Full enumeration with empty tokens: reset = true.
async fn full(api: &Api, p: &Project) -> Result<Delta, FeedError> {
    let mut ch = Changes::default();
    let f = page_feed(api, p, true, String::new(), &mut ch)
        .await?
        .unwrap_or_default();
    let d = page_feed(api, p, false, String::new(), &mut ch)
        .await?
        .unwrap_or_default();
    let mut delta = ch.into_delta(true);
    delta.deletes.clear();
    delta.new_cursor = Some(json!({ "f": f, "d": d }).to_string());
    Ok(delta)
}

pub async fn poll(api: &Api, p: &Project) -> Result<Delta, FeedError> {
    let cursor: Option<Cursor> = p
        .cursor
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let cur = match cursor {
        Some(c) if !c.f.is_empty() && !c.d.is_empty() => c,
        _ => return full(api, p).await,
    };

    // S0: cheap probe.
    let url = format!(
        "/dm/v3/projects/{}/api:desktop-sync-status",
        p.project_guid()
    );
    let body = Body::Json(json!([
        { "syncToken": cur.f, "referenceId": "folders" },
        { "syncToken": cur.d, "referenceId": "documents" }
    ]));
    let status: StatusResp = api
        .json(Method::POST, &url, Some(body))
        .await
        .map_err(feed_err)?;

    let mut next = Cursor {
        f: cur.f.clone(),
        d: cur.d.clone(),
    };
    let mut paged = false;
    let mut ch = Changes::default();
    for r in &status.results {
        let is_folders = match r.reference_id.as_str() {
            "folders" => true,
            "documents" => false,
            _ => continue,
        };
        if r.overwrite == Some(true) {
            return full(api, p).await;
        }
        if !r.more_data.unwrap_or(true) {
            continue;
        }
        let from = if is_folders {
            cur.f.clone()
        } else {
            cur.d.clone()
        };
        match page_feed(api, p, is_folders, from, &mut ch).await? {
            Some(tok) => {
                paged = true;
                if is_folders {
                    next.f = tok;
                } else {
                    next.d = tok;
                }
            }
            None => return full(api, p).await,
        }
    }

    if !paged {
        return Ok(Delta::default());
    }
    let mut delta = ch.into_delta(false);
    delta.new_cursor = Some(json!({ "f": next.f, "d": next.d }).to_string());
    Ok(delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proj() -> Project {
        Project {
            pid: 3,
            hub_id: "b.hub-1".into(),
            hub_name: "Hub".into(),
            project_id: "b.c3745078-aaaa".into(),
            name: "Kopmannen".into(),
            root_folder_urn: "urn:adsk.wipemea:fs.folder:co.ROOT".into(),
            urn_ns: "wipemea".into(),
            tops: vec![TopFolder {
                id: "co.ROOT".into(),
                name: "Project Files".into(),
            }],
            oss_scopes: None,
            change_source: ChangeSourceKind::DesktopSync,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: Provider::Acc,
            meta: None,
        }
    }

    #[test]
    fn folder_page_maps_top_folders_and_drops_orphans() {
        let json = r#"{
          "current": {"data": [
            {"urn":"urn:adsk.wipemea:fs.folder:co.ROOT","name":"Project Files","isRoot":true,
             "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z"},
            {"urn":"urn:adsk.wipemea:fs.folder:co.F1","name":"Ritningar",
             "parentUrn":"urn:adsk.wipemea:fs.folder:co.ROOT","hidden":false,
             "createdAt":"2026-01-02T10:00:00Z"},
            {"urn":"urn:adsk.wipemea:fs.folder:co.NOPARENT","name":"Orphan"},
            {"urn":"urn:adsk.wipemea:fs.folder:co.GONE","name":"Gone",
             "parentUrn":"urn:adsk.wipemea:fs.folder:co.ROOT","hidden":true}
          ]},
          "deleted": {"data": [{"urn":"urn:adsk.wipemea:fs.folder:co.OLD"}]},
          "moreData": false,
          "overwrite": false,
          "nextSyncToken": "tok-f-2"
        }"#;
        let feed: Feed = serde_json::from_str(json).unwrap();
        let p = proj();
        let mut ch = Changes::default();
        for o in &feed.current.data {
            if let Some((id, c)) = change_for(&p, o, true) {
                ch.set(id, c);
            }
        }
        for o in &feed.deleted.data {
            ch.set(urn_suffix(&o.urn).to_string(), None);
        }
        let d = ch.into_delta(false);
        assert_eq!(d.upserts.len(), 2);
        let top = &d.upserts[0];
        assert_eq!(top.kind, Kind::Folder);
        assert_eq!(top.parent.as_deref(), Some(PROJECT_ROOT_ID));
        assert_eq!(top.name, "Project Files");
        let f1 = &d.upserts[1];
        assert_eq!(f1.id, "co.F1");
        assert_eq!(f1.kind, Kind::Folder);
        assert_eq!(f1.parent.as_deref(), Some("co.ROOT"));
        assert_eq!(f1.ctime_ms, 1_767_348_000_000);
        assert_eq!(d.deletes, vec!["co.GONE".to_string(), "co.OLD".to_string()]);
        assert_eq!(feed.next_sync_token.as_deref(), Some("tok-f-2"));
    }

    #[test]
    fn a_consultants_top_folders_are_children_of_the_project_directory() {
        // Access to two discipline folders deep inside Project Files, not to Project Files.
        let mut p = proj();
        p.tops = vec![
            TopFolder {
                id: "co.K".into(),
                name: "K - Konstruktion".into(),
            },
            TopFolder {
                id: "co.V".into(),
                name: "V - VVS".into(),
            },
        ];
        let json = r#"{"current": {"data": [
            {"urn":"urn:adsk.wipemea:fs.folder:co.K","name":"K - Konstruktion","parentUrn":"urn:adsk.wipemea:fs.folder:co.NOACCESS"},
            {"urn":"urn:adsk.wipemea:fs.folder:co.V","name":"V - VVS","parentUrn":"urn:adsk.wipemea:fs.folder:co.NOACCESS2"},
            {"urn":"urn:adsk.wipemea:fs.folder:co.K1","name":"Modeller","parentUrn":"urn:adsk.wipemea:fs.folder:co.K"}
        ]}, "deleted": {"data": []}, "moreData": false}"#;
        let feed: Feed = serde_json::from_str(json).unwrap();
        let mut ch = Changes::default();
        for o in &feed.current.data {
            if let Some((id, c)) = change_for(&p, o, true) {
                ch.set(id, c);
            }
        }
        let d = ch.into_delta(false);
        let got: Vec<(&str, Option<&str>)> = d
            .upserts
            .iter()
            .map(|u| (u.id.as_str(), u.parent.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("co.K", Some(PROJECT_ROOT_ID)),
                ("co.V", Some(PROJECT_ROOT_ID)),
                ("co.K1", Some("co.K"))
            ]
        );
    }

    #[test]
    fn documents_page_maps_files() {
        let json = r#"{
          "current": {"data": [
            {"urn":"urn:adsk.wipemea:dm.lineage:LIN1",
             "parentFolderUrn":"urn:adsk.wipemea:fs.folder:co.F1",
             "name":"plan.rvt","reserved":true,"reservedByName":"Bo",
             "createdAt":"2026-01-01T00:00:00Z","updatedByName":"Anna",
             "includedVersion":{"urn":"urn:adsk.wipemea:fs.file:vf.V2?version=2",
               "name":"plan-v2.rvt","versionNumber":2,
               "storageUrn":"urn:adsk.objects:os.object:wip.dm.emea.2/abc.rvt",
               "storageSize":1234,"updatedAt":"2026-01-06T08:00:00Z"}},
            {"urn":"urn:adsk.wipemea:dm.lineage:NOVER",
             "parentFolderUrn":"urn:adsk.wipemea:fs.folder:co.F1","name":"nover.pdf"},
            {"urn":"urn:adsk.wipemea:dm.lineage:LNK",
             "parentFolderUrn":"urn:adsk.wipemea:fs.folder:co.F1","name":"x.lnk",
             "includedVersion":{"urn":"urn:adsk.wipemea:fs.file:vf.L?version=1"}},
            {"urn":"urn:adsk.wipemea:dm.lineage:HID","hidden":true,
             "parentFolderUrn":"urn:adsk.wipemea:fs.folder:co.F1",
             "includedVersion":{"urn":"urn:adsk.wipemea:fs.file:vf.H?version=1"}}
          ]},
          "deleted": {"data": [{"urn":"urn:adsk.wipemea:dm.lineage:DEL"}]},
          "moreData": true,
          "overwrite": false,
          "nextSyncToken": "tok-d-2"
        }"#;
        let feed: Feed = serde_json::from_str(json).unwrap();
        let p = proj();
        let mut ch = Changes::default();
        for o in &feed.current.data {
            if let Some((id, c)) = change_for(&p, o, false) {
                ch.set(id, c);
            }
        }
        for o in &feed.deleted.data {
            ch.set(urn_suffix(&o.urn).to_string(), None);
        }
        let d = ch.into_delta(false);
        assert_eq!(d.upserts.len(), 1);
        let f = &d.upserts[0];
        assert_eq!(f.id, "LIN1");
        assert_eq!(f.kind, Kind::File);
        assert_eq!(f.parent.as_deref(), Some("co.F1"));
        assert_eq!(f.name, "plan-v2.rvt");
        assert_eq!(f.remote_version, Some(2));
        assert_eq!(f.size, 1234);
        assert_eq!(
            f.storage_urn.as_deref(),
            Some("urn:adsk.objects:os.object:wip.dm.emea.2/abc.rvt")
        );
        assert_eq!(
            f.version_urn.as_deref(),
            Some("urn:adsk.wipemea:fs.file:vf.V2?version=2")
        );
        assert_eq!(f.modified_by.as_deref(), Some("Anna"));
        assert_eq!(f.locked_by.as_deref(), Some("Bo"));
        assert_eq!(f.mtime_ms, 1_767_686_400_000);
        assert_eq!(d.deletes, vec!["HID".to_string(), "DEL".to_string()]);
    }

    #[test]
    fn last_change_per_id_wins() {
        let mut ch = Changes::default();
        ch.set("A".into(), None);
        ch.set("B".into(), None);
        ch.set(
            "A".into(),
            Some(Item {
                pid: 1,
                id: "A".into(),
                parent: None,
                name: "a".into(),
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
            }),
        );
        let d = ch.into_delta(false);
        assert_eq!(d.upserts.len(), 1);
        assert_eq!(d.deletes, vec!["B".to_string()]);
    }

    #[test]
    fn cursor_roundtrip_and_empty_tokens() {
        let c: Cursor = serde_json::from_str(r#"{"f":"F1","d":"D1"}"#).unwrap();
        assert_eq!((c.f.as_str(), c.d.as_str()), ("F1", "D1"));
        let s = json!({ "f": c.f, "d": c.d }).to_string();
        assert!(s.contains("\"f\":\"F1\"") && s.contains("\"d\":\"D1\""));
        let empty: Cursor = serde_json::from_str("{}").unwrap();
        assert!(empty.f.is_empty() && empty.d.is_empty());
    }
}

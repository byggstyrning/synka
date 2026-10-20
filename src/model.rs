//! Shared types. Owned by the architect: modules may ADD helpers in their own files but must
//! not change the shape of these types without coordinating (other modules depend on them).

use serde::{Deserialize, Serialize};

/// Local project key (SQLite rowid of `project`).
pub type Pid = i64;

/// Region-specific URN prefixes. ACC EMEA uses `wipemea`, US uses `wipprod`.
/// We store the full prefix per project (`Project::urn_ns`, e.g. "wipemea") and only the
/// suffix per item.
pub const FOLDER_URN_PREFIX: &str = "fs.folder:";
pub const LINEAGE_URN_PREFIX: &str = "dm.lineage:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Kind {
    /// Local-only grouping directory for a hub (account) under the sync root.
    Hub = 1,
    /// The project directory. A local container, not an ACC folder (id `PROJECT_ROOT_ID`):
    /// its children are the project's top folders (`Project::tops`).
    ProjectRoot = 2,
    Folder = 3,
    File = 4,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        match v {
            1 => Some(Kind::Hub),
            2 => Some(Kind::ProjectRoot),
            3 => Some(Kind::Folder),
            4 => Some(Kind::File),
            _ => None,
        }
    }
    pub fn is_dir(self) -> bool {
        !matches!(self, Kind::File)
    }
}

/// Local state of an item relative to its `base_version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum LocalState {
    Clean = 0,
    /// Content changed locally, upload pending.
    Dirty = 1,
    /// Exists locally only, create pending.
    New = 2,
    /// Deleted locally, remote delete pending.
    Deleted = 3,
    /// Upload failed permanently or conflict needs attention.
    Error = 4,
}

impl LocalState {
    pub fn from_u8(v: u8) -> LocalState {
        match v {
            1 => LocalState::Dirty,
            2 => LocalState::New,
            3 => LocalState::Deleted,
            4 => LocalState::Error,
            _ => LocalState::Clean,
        }
    }
}

/// Item id of every project directory (`Kind::ProjectRoot`). Not an ACC id: folder ids start
/// with "co.", lineage ids are base64url, temporary ids start with "local-".
pub const PROJECT_ROOT_ID: &str = "~project";

/// A folder shown directly in the project directory: "Project Files" for most users, or each
/// highest folder a user with limited access can reach (`topFolders?projectFilesOnly=true`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopFolder {
    /// Folder id (URN suffix, "co.xxx").
    pub id: String,
    /// Name in the project directory: the ACC name, made unique among the top folders.
    pub name: String,
}

/// The platform a hub, project and its files come from. Serialized as "acc", "streambim" or
/// "interaxo" (see docs/PROVIDERS.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Acc,
    #[serde(rename = "streambim")]
    StreamBim,
    Interaxo,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Acc, Provider::StreamBim, Provider::Interaxo];

    /// Stable name used in the database and on the IPC wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Acc => "acc",
            Provider::StreamBim => "streambim",
            Provider::Interaxo => "interaxo",
        }
    }

    /// Inverse of `as_str` (exact match).
    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// Name shown to the user (the UI shows it from the next step on).
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Provider::Acc => "ACC",
            Provider::StreamBim => "StreamBIM",
            Provider::Interaxo => "Interaxo",
        }
    }
}

/// A subscribed (synced) project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub pid: Pid,
    /// DM hub id, e.g. "b.4d68920b-...".
    pub hub_id: String,
    pub hub_name: String,
    /// DM project id, e.g. "b.c3745078-...".
    pub project_id: String,
    pub name: String,
    /// Full URN of the first top folder, e.g. "urn:adsk.wipemea:fs.folder:co.xxx" (where
    /// `urn_ns` comes from).
    pub root_folder_urn: String,
    /// URN namespace, e.g. "wipemea" (parsed from root_folder_urn).
    pub urn_ns: String,
    /// The top folders in the project directory. Empty until they are known (a project from
    /// before top folders, until its first poll).
    pub tops: Vec<TopFolder>,
    /// OSS scopes (project.attributes.scopes joined by ','), needed for signeds3* calls.
    pub oss_scopes: Option<String>,
    /// Which change source works for this project.
    pub change_source: ChangeSourceKind,
    /// Opaque desktop-sync cursors (only with Config::desktop_sync_feed), JSON {"f":..,"d":..,"p":..}; None before first full sync.
    pub cursor: Option<String>,
    pub enabled: bool,
    pub last_sync_ms: Option<i64>,
    /// Which platform this project comes from (`acc` for every project before schema v4).
    #[serde(default)]
    pub provider: Provider,
    /// Provider-specific JSON (region, host, community...). None for ACC.
    #[serde(default)]
    pub meta: Option<String>,
}

impl Project {
    /// Bare GUID used by dm/v3 endpoints (strip "b.").
    pub fn project_guid(&self) -> &str {
        self.project_id
            .strip_prefix("b.")
            .unwrap_or(&self.project_id)
    }
    pub fn folder_urn(&self, id: &str) -> String {
        format!("urn:adsk.{}:{}{}", self.urn_ns, FOLDER_URN_PREFIX, id)
    }
    pub fn lineage_urn(&self, id: &str) -> String {
        format!("urn:adsk.{}:{}{}", self.urn_ns, LINEAGE_URN_PREFIX, id)
    }
    /// The project directory's item id (`PROJECT_ROOT_ID`).
    pub fn root_id(&self) -> &str {
        PROJECT_ROOT_ID
    }
    /// The top folder with this id, if it is one.
    pub fn top(&self, id: &str) -> Option<&TopFolder> {
        self.tops.iter().find(|t| t.id == id)
    }
}

/// Top folders from ACC (id, name) -> `TopFolder`s with names unique in the project directory
/// (the same name twice gets " (2)", " (3)"...; Windows compares case-insensitively).
pub fn unique_tops(found: Vec<(String, String)>) -> Vec<TopFolder> {
    let mut out: Vec<TopFolder> = Vec::new();
    for (id, name) in found {
        if out.iter().any(|t| t.id == id) {
            continue;
        }
        let taken = |n: &str, out: &[TopFolder]| {
            out.iter()
                .any(|t| t.name.to_lowercase() == n.to_lowercase())
        };
        let mut unique = name.clone();
        let mut n = 2;
        while taken(&unique, &out) {
            unique = format!("{name} ({n})");
            n += 1;
        }
        out.push(TopFolder { id, name: unique });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ChangeSourceKind {
    /// Not yet probed.
    Unknown = 0,
    /// Private dm/v3 desktop-sync delta feed works with our token (only with Config::desktop_sync_feed).
    DesktopSync = 1,
    /// Public data/v1 listing of populated folders.
    PublicWalk = 2,
}

impl ChangeSourceKind {
    pub fn from_u8(v: u8) -> ChangeSourceKind {
        match v {
            1 => ChangeSourceKind::DesktopSync,
            2 => ChangeSourceKind::PublicWalk,
            _ => ChangeSourceKind::Unknown,
        }
    }
}

/// One remote or local item (folder or file) within a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub pid: Pid,
    /// URN suffix: "co.xxx" for folders, lineage id for files. Case-sensitive.
    pub id: String,
    /// Parent folder id (suffix). None only for the project root.
    pub parent: Option<String>,
    pub name: String,
    pub kind: Kind,
    /// Latest known remote version number (files only).
    pub remote_version: Option<i64>,
    /// Version materialised locally / agreed base (files only).
    pub base_version: Option<i64>,
    pub size: i64,
    pub ctime_ms: i64,
    pub mtime_ms: i64,
    /// Full OSS storage URN of the tip version, "urn:adsk.objects:os.object:<bucket>/<key>".
    pub storage_urn: Option<String>,
    /// Full version URN of the tip version (files only).
    pub version_urn: Option<String>,
    pub modified_by: Option<String>,
    /// Reserved (locked) by someone; Some(name) when locked.
    pub locked_by: Option<String>,
    pub local_state: LocalState,
    /// Directory placeholder has been populated on disk (FETCH_PLACEHOLDERS answered or
    /// created eagerly). Only populated dirs need their children kept on disk.
    pub populated: bool,
}

/// A change set produced by a ChangeSource for one project.
#[derive(Debug, Default, Clone)]
pub struct Delta {
    /// Upserted folders and files (full rows; `local_state`/`base_version`/`populated` are
    /// ignored by the applier and preserved from the DB).
    pub upserts: Vec<Item>,
    /// Removed item ids.
    pub deletes: Vec<String>,
    /// New cursor to persist together with the changes (DesktopSync only).
    pub new_cursor: Option<String>,
    /// Server said our cursor is invalid: wipe and re-enumerate.
    pub reset: bool,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.deletes.is_empty() && !self.reset
    }
}

/// Hub as listed by project/v1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubInfo {
    pub id: String,
    pub name: String,
    pub region: Option<String>,
    #[serde(default)]
    pub provider: Provider,
}

/// Project as listed by project/v1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    pub hub_id: String,
    pub name: String,
    pub root_folder_urn: Option<String>,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub provider: Provider,
    #[serde(default)]
    pub meta: Option<String>,
}

/// Return the id suffix of a folder or lineage URN (text after the last ':' that precedes
/// the id). "urn:adsk.wipemea:fs.folder:co.abc" -> "co.abc";
/// "urn:adsk.wipemea:dm.lineage:XYZ" -> "XYZ". Strips any "?version=" query.
pub fn urn_suffix(urn: &str) -> &str {
    let no_q = urn.split('?').next().unwrap_or(urn);
    for p in [FOLDER_URN_PREFIX, LINEAGE_URN_PREFIX] {
        if let Some(i) = no_q.find(p) {
            return &no_q[i + p.len()..];
        }
    }
    no_q.rsplit(':').next().unwrap_or(no_q)
}

/// "urn:adsk.wipemea:fs.folder:co.abc" -> Some("wipemea")
pub fn urn_namespace(urn: &str) -> Option<&str> {
    let rest = urn.strip_prefix("urn:adsk.")?;
    rest.split(':').next()
}

/// Prefix of temporary ids for items that exist only locally until their upload or mkdir job
/// gives them a remote id.
pub const LOCAL_ID_PREFIX: &str = "local-";

/// True for temporary ids of items that exist only locally so far.
pub fn is_local_id(id: &str) -> bool {
    id.starts_with(LOCAL_ID_PREFIX)
}

/// Parse "urn:adsk.objects:os.object:<bucket>/<key>" into (bucket, key).
pub fn parse_storage_urn(urn: &str) -> Option<(&str, &str)> {
    let rest = urn.strip_prefix("urn:adsk.objects:os.object:")?;
    rest.split_once('/')
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suffixes() {
        assert_eq!(urn_suffix("urn:adsk.wipemea:fs.folder:co.abc"), "co.abc");
        assert_eq!(urn_suffix("urn:adsk.wipemea:dm.lineage:XyZ_1"), "XyZ_1");
        assert_eq!(
            urn_namespace("urn:adsk.wipemea:fs.folder:co.abc"),
            Some("wipemea")
        );
        assert_eq!(
            parse_storage_urn("urn:adsk.objects:os.object:wip.dm.emea.2/abc-def.rvt"),
            Some(("wip.dm.emea.2", "abc-def.rvt"))
        );
    }

    #[test]
    fn provider_names_round_trip() {
        assert_eq!(Provider::default(), Provider::Acc);
        assert_eq!(Provider::ALL.len(), 3);
        for p in Provider::ALL {
            assert_eq!(Provider::parse(p.as_str()), Some(p));
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            assert_eq!(serde_json::from_str::<Provider>(&json).unwrap(), p);
        }
        assert_eq!(Provider::StreamBim.as_str(), "streambim");
        assert_eq!(Provider::parse("StreamBIM"), None, "names are exact");
        assert_eq!(Provider::parse(""), None);
        assert_eq!(Provider::Acc.label(), "ACC");
        assert_eq!(Provider::StreamBim.label(), "StreamBIM");
        assert_eq!(Provider::Interaxo.label(), "Interaxo");
    }

    #[test]
    fn projects_from_before_providers_parse_as_acc() {
        let project = Project {
            pid: 3,
            hub_id: "b.h".into(),
            hub_name: "Hub".into(),
            project_id: "b.p".into(),
            name: "Proj".into(),
            root_folder_urn: "urn:adsk.wipemea:fs.folder:co.R".into(),
            urn_ns: "wipemea".into(),
            tops: vec![],
            oss_scopes: None,
            change_source: ChangeSourceKind::Unknown,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: Provider::Acc,
            meta: None,
        };
        let mut value = serde_json::to_value(&project).unwrap();
        value.as_object_mut().unwrap().remove("provider");
        value.as_object_mut().unwrap().remove("meta");
        let old: Project = serde_json::from_value(value).unwrap();
        assert_eq!(old.provider, Provider::Acc);
        assert_eq!(old.meta, None);

        let mut streambim = project.clone();
        streambim.provider = Provider::StreamBim;
        streambim.meta = Some(r#"{"region":"app"}"#.into());
        let back: Project =
            serde_json::from_value(serde_json::to_value(&streambim).unwrap()).unwrap();
        assert_eq!(back.provider, Provider::StreamBim);
        assert_eq!(back.meta, streambim.meta);
    }

    #[test]
    fn top_folder_names_are_unique() {
        let tops = unique_tops(vec![
            ("co.a".into(), "Shared".into()),
            ("co.b".into(), "shared".into()),
            ("co.a".into(), "Shared".into()),
            ("co.c".into(), "Shared".into()),
            ("co.d".into(), "Ritningar".into()),
        ]);
        let names: Vec<&str> = tops.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Shared", "shared (2)", "Shared (3)", "Ritningar"]);
    }
}

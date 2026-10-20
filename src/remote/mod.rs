//! Provider dispatch: the functions the engine calls to reach a provider's API.
//!
//! Each function picks the provider from `p.provider` (or `hub.provider`) and calls that
//! provider's client: `api::dm` and `api::oss` for ACC, `api::streambim` and `api::interaxo`
//! for the others. The ACC arms pass the arguments the engine passed before, so ACC behaviour
//! does not change. See docs/PROVIDERS.md "The dispatch module".
//!
//! The engine (`sync`) reaches every provider only through these functions.

use crate::api::{dm, interaxo, oss, streambim};
use crate::app::Ctx;
use crate::http::{Api, ApiError};
use crate::model::{
    urn_namespace, urn_suffix, HubInfo, Item, Kind, Project, ProjectInfo, Provider,
};
use std::path::Path;

/// What a provider can do. The engine checks it to skip work the provider would refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub mkdir: bool,
    pub rename: bool,
    pub move_items: bool,
    pub delete: bool,
    pub lock: bool,
    pub web_link: bool,
    pub ranged_download: bool,
}

/// The operations each provider offers (docs/PROVIDERS.md "Unsupported operations").
pub fn caps(provider: Provider) -> Caps {
    match provider {
        Provider::Acc => Caps {
            mkdir: true,
            rename: true,
            move_items: true,
            delete: true,
            lock: true,
            web_link: true,
            ranged_download: true,
        },
        Provider::StreamBim => Caps {
            mkdir: true,
            rename: true,
            move_items: true,
            delete: true,
            lock: false,
            web_link: true,
            ranged_download: false,
        },
        Provider::Interaxo => Caps {
            mkdir: false,
            rename: false,
            move_items: false,
            delete: true,
            lock: false,
            web_link: true,
            ranged_download: false,
        },
    }
}

/// A download handle for one version: where to read its bytes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadUrl {
    pub url: String,
    pub size: u64,
    /// Send `Authorization: Bearer <token>` with the request. Never for presigned URLs.
    pub bearer: Option<String>,
    /// The server answers Range requests, so the download may run in parallel segments.
    pub ranges: bool,
}

/// What `subscribe` needs to build a `model::Project` from a listed project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSetup {
    pub root_folder_urn: String,
    pub urn_ns: String,
    pub oss_scopes: Option<String>,
    /// (top folder id, name) in the provider's order. Pass through `model::unique_tops`.
    pub tops: Vec<(String, String)>,
    pub meta: Option<String>,
}

/// Hubs of one provider: ACC's account hubs, StreamBIM's one hub, Interaxo's communities.
pub async fn list_hubs(ctx: &Ctx, provider: Provider) -> Result<Vec<HubInfo>, ApiError> {
    let api = ctx.api_for(provider);
    match provider {
        Provider::Acc => dm::list_hubs(api).await,
        Provider::StreamBim => streambim::list_hubs(api, &ctx.streambim).await,
        Provider::Interaxo => interaxo::list_hubs(api).await,
    }
}

/// Projects of one hub.
pub async fn list_projects(ctx: &Ctx, hub: &HubInfo) -> Result<Vec<ProjectInfo>, ApiError> {
    let api = ctx.api_for(hub.provider);
    match hub.provider {
        Provider::Acc => dm::list_projects(api, &hub.id).await,
        Provider::StreamBim => streambim::list_projects(api, hub).await,
        Provider::Interaxo => interaxo::list_projects(api, hub).await,
    }
}

/// What subscribing to a project needs (the ACC arm is the logic of `sync::subscribe`).
pub async fn project_setup(
    ctx: &Ctx,
    hub: &HubInfo,
    project: &ProjectInfo,
) -> Result<ProjectSetup, ApiError> {
    let api = ctx.api_for(hub.provider);
    match hub.provider {
        Provider::Acc => acc_project_setup(api, hub, project).await,
        Provider::StreamBim => streambim::project_setup(api, hub, project).await,
        Provider::Interaxo => interaxo::project_setup(api, hub, project).await,
    }
}

/// The ACC half of subscribing: the root and namespace come from the first top folder, the
/// OSS scopes from the project. Errors keep the wording of `subscribe`.
async fn acc_project_setup(
    api: &Api,
    hub: &HubInfo,
    project: &ProjectInfo,
) -> Result<ProjectSetup, ApiError> {
    let info = dm::get_project(api, &hub.id, &project.id).await?;
    let found = dm::top_folders(api, &hub.id, &project.id).await?;
    let root_folder_urn = found.first().map(|(urn, _)| urn.clone()).ok_or_else(|| {
        ApiError::Message(format!(
            "du har inte åtkomst till någon mapp i {}",
            project.name
        ))
    })?;
    let urn_ns = urn_namespace(&root_folder_urn)
        .ok_or_else(|| ApiError::Message(format!("okänd URN: {root_folder_urn}")))?
        .to_string();
    let tops = found
        .iter()
        .map(|(urn, name)| (urn_suffix(urn).to_string(), name.clone()))
        .collect();
    let oss_scopes = if info.scopes.is_empty() {
        None
    } else {
        Some(info.scopes.join(","))
    };
    Ok(ProjectSetup {
        root_folder_urn,
        urn_ns,
        oss_scopes,
        tops,
        meta: None,
    })
}

/// The top folders of a project as (folder id, name), before `model::unique_tops`.
pub async fn top_folders(ctx: &Ctx, p: &Project) -> Result<Vec<(String, String)>, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => {
            let found = dm::top_folders(api, &p.hub_id, &p.project_id).await?;
            Ok(found
                .into_iter()
                .map(|(urn, name)| (urn_suffix(&urn).to_string(), name))
                .collect())
        }
        Provider::StreamBim => streambim::top_folders(api, p).await,
        Provider::Interaxo => interaxo::top_folders(api, p).await,
    }
}

/// Children (folders and files) of one folder.
pub async fn folder_contents(
    ctx: &Ctx,
    p: &Project,
    folder_id: &str,
) -> Result<Vec<Item>, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::folder_contents(api, p, folder_id).await,
        Provider::StreamBim => streambim::folder_contents(api, p, folder_id).await,
        Provider::Interaxo => interaxo::folder_contents(api, p, folder_id).await,
    }
}

/// A download handle for the version stored at `storage_urn` of item `item_id`.
pub async fn download_url(
    ctx: &Ctx,
    p: &Project,
    item_id: &str,
    storage_urn: &str,
) -> Result<DownloadUrl, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        // A presigned S3 URL: no bearer token, and ranges are answered.
        Provider::Acc => {
            let (url, size) = oss::download_url(api, p, storage_urn).await?;
            Ok(DownloadUrl {
                url,
                size,
                bearer: None,
                ranges: true,
            })
        }
        Provider::StreamBim => streambim::download_url(api, p, item_id, storage_urn).await,
        Provider::Interaxo => interaxo::download_url(api, p, item_id, storage_urn).await,
    }
}

/// Start streaming the bytes of `d` from `offset`. `d.bearer`, when set, is sent as
/// `Authorization: Bearer`; ACC's presigned URLs never get one.
pub async fn open_download(
    ctx: &Ctx,
    p: &Project,
    d: &DownloadUrl,
    offset: u64,
) -> Result<reqwest::Response, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => oss::open_download_with(api, &d.url, offset, d.bearer.as_deref()).await,
        Provider::StreamBim => streambim::open_download(api, p, d, offset).await,
        Provider::Interaxo => interaxo::open_download(api, p, d, offset).await,
    }
}

/// Stream the byte range `[start, end)` of `d` (same bearer rule as `open_download`).
pub async fn open_range(
    ctx: &Ctx,
    p: &Project,
    d: &DownloadUrl,
    start: u64,
    end: u64,
) -> Result<reqwest::Response, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => oss::open_range_with(api, &d.url, start, end, d.bearer.as_deref()).await,
        Provider::StreamBim => streambim::open_range(api, p, d, start, end).await,
        Provider::Interaxo => interaxo::open_range(api, p, d, start, end).await,
    }
}

/// The current tip of a file and what its upload needs to know.
pub(crate) async fn get_item_upload_info(
    ctx: &Ctx,
    p: &Project,
    id: &str,
) -> Result<(Item, dm::UploadInfo), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::get_item_upload_info(api, p, id).await,
        Provider::StreamBim => streambim::get_item_upload_info(api, p, id).await,
        Provider::Interaxo => interaxo::get_item_upload_info(api, p, id).await,
    }
}

/// Reserve storage for the bytes of a new file. ACC answers a storage URN.
pub async fn create_storage(
    ctx: &Ctx,
    p: &Project,
    parent_id: &str,
    name: &str,
) -> Result<String, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::create_storage(api, p, parent_id, name).await,
        Provider::StreamBim => streambim::create_storage(api, p, parent_id, name).await,
        Provider::Interaxo => interaxo::create_storage(api, p, parent_id, name).await,
    }
}

/// Put the bytes of `path` into the storage from `create_storage` (ACC only; other providers
/// upload inside `create_item` / `create_version`).
pub async fn upload_file(
    ctx: &Ctx,
    p: &Project,
    storage: &str,
    path: &Path,
    content_type: Option<&str>,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => oss::upload_file(api, p, storage, path, content_type, progress).await,
        Provider::StreamBim => {
            streambim::upload_file(api, p, storage, path, content_type, progress).await
        }
        Provider::Interaxo => {
            interaxo::upload_file(api, p, storage, path, content_type, progress).await
        }
    }
}

/// Create a file in a folder. ACC uses `storage` (uploaded before); StreamBIM and Interaxo
/// upload `path` here and report `progress`.
pub async fn create_item(
    ctx: &Ctx,
    p: &Project,
    parent_id: &str,
    name: &str,
    storage: &str,
    path: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<Item, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::create_item(api, p, parent_id, name, storage).await,
        Provider::StreamBim => {
            streambim::create_item(api, p, parent_id, name, storage, path, progress).await
        }
        Provider::Interaxo => {
            interaxo::create_item(api, p, parent_id, name, storage, path, progress).await
        }
    }
}

/// Add a new version to an existing file. Same upload rules as `create_item`. Returns
/// (remote version number, storage URN of the new version).
pub async fn create_version(
    ctx: &Ctx,
    p: &Project,
    id: &str,
    name: &str,
    storage: &str,
    path: &Path,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(i64, String), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::create_version(api, p, id, name, storage).await,
        Provider::StreamBim => {
            streambim::create_version(api, p, id, name, storage, path, progress).await
        }
        Provider::Interaxo => {
            interaxo::create_version(api, p, id, name, storage, path, progress).await
        }
    }
}

/// Create a folder.
pub async fn create_folder(
    ctx: &Ctx,
    p: &Project,
    parent_id: &str,
    name: &str,
) -> Result<Item, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::create_folder(api, p, parent_id, name).await,
        Provider::StreamBim => streambim::create_folder(api, p, parent_id, name).await,
        Provider::Interaxo => interaxo::create_folder(api, p, parent_id, name).await,
    }
}

/// Rename a file. ACC renames its tip version, so it needs `version_urn`.
pub async fn rename_file(
    ctx: &Ctx,
    p: &Project,
    id: &str,
    version_urn: Option<&str>,
    new_name: &str,
) -> Result<(), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => {
            let vurn = version_urn
                .ok_or_else(|| ApiError::Message("file has no remote version yet".to_string()))?;
            dm::rename_file(api, p, vurn, new_name).await
        }
        Provider::StreamBim => streambim::rename_file(api, p, id, version_urn, new_name).await,
        Provider::Interaxo => interaxo::rename_file(api, p, id, version_urn, new_name).await,
    }
}

/// Move a file into another folder.
pub async fn move_file(
    ctx: &Ctx,
    p: &Project,
    id: &str,
    new_parent_id: &str,
) -> Result<(), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::move_file(api, p, id, new_parent_id).await,
        Provider::StreamBim => streambim::move_file(api, p, id, new_parent_id).await,
        Provider::Interaxo => interaxo::move_file(api, p, id, new_parent_id).await,
    }
}

/// Rename and/or move a folder (None leaves that part as it is).
pub async fn patch_folder(
    ctx: &Ctx,
    p: &Project,
    folder_id: &str,
    new_name: Option<&str>,
    new_parent_id: Option<&str>,
) -> Result<(), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::patch_folder(api, p, folder_id, new_name, new_parent_id).await,
        Provider::StreamBim => {
            streambim::patch_folder(api, p, folder_id, new_name, new_parent_id).await
        }
        Provider::Interaxo => {
            interaxo::patch_folder(api, p, folder_id, new_name, new_parent_id).await
        }
    }
}

/// Delete a file (soft delete on ACC and StreamBIM).
pub async fn delete_file(ctx: &Ctx, p: &Project, id: &str) -> Result<(), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::delete_file(api, p, id).await,
        Provider::StreamBim => streambim::delete_file(api, p, id).await,
        Provider::Interaxo => interaxo::delete_file(api, p, id).await,
    }
}

/// Delete a folder.
pub async fn delete_folder(ctx: &Ctx, p: &Project, folder_id: &str) -> Result<(), ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::delete_folder(api, p, folder_id).await,
        Provider::StreamBim => streambim::delete_folder(api, p, folder_id).await,
        Provider::Interaxo => interaxo::delete_folder(api, p, folder_id).await,
    }
}

/// The provider's web page for an item (the API part only: the engine refuses local-only
/// items and the hub itself before it asks).
pub async fn web_link(ctx: &Ctx, p: &Project, item: &Item) -> Result<String, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => acc_web_link(api, p, item).await,
        Provider::StreamBim => streambim::web_link(api, p, item).await,
        Provider::Interaxo => interaxo::web_link(api, p, item).await,
    }
}

async fn acc_web_link(api: &Api, p: &Project, item: &Item) -> Result<String, ApiError> {
    match item.kind {
        Kind::File => dm::item_web_view(api, p, &item.id).await,
        Kind::Folder => dm::folder_web_view(api, p, &item.id).await,
        // The project directory is ACC's first top folder.
        Kind::ProjectRoot => {
            let top = p.tops.first().ok_or_else(|| {
                ApiError::Message("projektets mappar är inte kända än".to_string())
            })?;
            dm::folder_web_view(api, p, &top.id).await
        }
        Kind::Hub => Err(ApiError::Message("ingen ACC-mapp".to_string())),
    }
}

/// Lock (`reserve` true) or unlock a file. Returns the name of the user holding the lock.
pub async fn set_reserved(
    ctx: &Ctx,
    p: &Project,
    id: &str,
    reserve: bool,
) -> Result<Option<String>, ApiError> {
    let api = ctx.api_for(p.provider);
    match p.provider {
        Provider::Acc => dm::set_reserved(api, p, id, reserve).await,
        Provider::StreamBim => streambim::set_reserved(api, p, id, reserve).await,
        Provider::Interaxo => interaxo::set_reserved(api, p, id, reserve).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;
    use crate::config::Config;
    use crate::ipc::IpcHub;
    use crate::model::{ChangeSourceKind, TopFolder};
    use crate::paths::Paths;
    use crate::store::Store;
    use crate::streambim_auth::StreamBimAuth;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, RwLock};

    /// Removes its directory when dropped. Declared after the `Ctx` in `Fixture`, so the
    /// database is closed first.
    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        ctx: Ctx,
        _dir: Dir,
    }

    fn fixture(tag: &str) -> Fixture {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("synka-remote-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let paths = Paths {
            state_dir: state.clone(),
            log_dir: state.join("logs"),
            db_file: state.join("synka.db"),
            sync_root: dir.join("Synka"),
            packaged: false,
            package_family_name: None,
        };
        let store = Arc::new(Store::open(&paths.db_file).unwrap());
        let http = crate::http::build_client();
        let auth = Arc::new(Auth::new(String::new(), http.clone(), &state));
        let streambim = Arc::new(StreamBimAuth::new(http.clone(), &state));
        let (tx, _rx) = std::sync::mpsc::channel();
        let ui = crate::app::UiSender {
            tx,
            main_thread_id: 0,
            hub: Arc::new(IpcHub::new(paths.sync_root.clone())),
        };
        let cfg = Config::default();
        let ctx = Ctx {
            settings: RwLock::new(cfg.clone()),
            cfg,
            paths,
            store,
            auth: auth.clone(),
            api: Api::new(http.clone(), auth.clone()),
            streambim: streambim.clone(),
            streambim_api: Api::with_source(
                http.clone(),
                crate::http::TokenSource::StreamBim(streambim),
            ),
            interaxo: auth.clone(),
            interaxo_api: Api::new(http, auth),
            ui,
        };
        Fixture {
            ctx,
            _dir: Dir(dir),
        }
    }

    fn project(provider: Provider) -> Project {
        Project {
            pid: 1,
            hub_id: "hub".into(),
            hub_name: "Hub".into(),
            project_id: "project".into(),
            name: "Proj".into(),
            root_folder_urn: "0".into(),
            urn_ns: String::new(),
            tops: vec![TopFolder {
                id: "0".into(),
                name: "Dokument".into(),
            }],
            oss_scopes: None,
            change_source: ChangeSourceKind::PublicWalk,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider,
            meta: None,
        }
    }

    #[test]
    fn caps_table_matches_the_contract() {
        assert_eq!(
            caps(Provider::Acc),
            Caps {
                mkdir: true,
                rename: true,
                move_items: true,
                delete: true,
                lock: true,
                web_link: true,
                ranged_download: true,
            }
        );
        assert_eq!(
            caps(Provider::StreamBim),
            Caps {
                mkdir: true,
                rename: true,
                move_items: true,
                delete: true,
                lock: false,
                web_link: true,
                ranged_download: false,
            }
        );
        assert_eq!(
            caps(Provider::Interaxo),
            Caps {
                mkdir: false,
                rename: false,
                move_items: false,
                delete: true,
                lock: false,
                web_link: true,
                ranged_download: false,
            }
        );
    }

    #[tokio::test]
    async fn streambim_calls_reach_the_streambim_client() {
        let f = fixture("streambim");
        let mut p = project(Provider::StreamBim);
        p.meta =
            Some(r#"{"region":"app","base":"https://app.streambim.com","project":"1552"}"#.into());
        // Not signed in: the client fails with its auth error before any request is sent.
        let err = folder_contents(&f.ctx, &p, "0").await.unwrap_err();
        assert!(matches!(err, ApiError::Auth(_)), "{err}");
        assert!(list_hubs(&f.ctx, Provider::StreamBim)
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            set_reserved(&f.ctx, &p, "1", true).await,
            Err(ApiError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn interaxo_refuses_what_its_api_cannot_do() {
        let f = fixture("interaxo");
        let p = project(Provider::Interaxo);
        let unsupported = |r: Result<(), ApiError>| {
            assert!(matches!(r, Err(ApiError::Unsupported(_))), "{r:?}");
        };
        assert!(matches!(
            create_folder(&f.ctx, &p, "root", "new").await,
            Err(ApiError::Unsupported(_))
        ));
        unsupported(rename_file(&f.ctx, &p, "1", None, "new.pdf").await);
        unsupported(move_file(&f.ctx, &p, "1", "2").await);
        unsupported(patch_folder(&f.ctx, &p, "2", Some("x"), None).await);
        assert!(matches!(
            set_reserved(&f.ctx, &p, "1", true).await,
            Err(ApiError::Unsupported(_))
        ));
        // Nothing to change is not an error.
        assert!(patch_folder(&f.ctx, &p, "2", None, None).await.is_ok());
    }

    #[tokio::test]
    async fn acc_calls_reach_the_acc_client() {
        let f = fixture("acc");
        // No session: the ACC client fails before any request is sent, with its auth error.
        let p = project(Provider::Acc);
        let err = folder_contents(&f.ctx, &p, "co.x").await.unwrap_err();
        assert!(matches!(err, ApiError::Auth(_)), "{err}");
        assert!(matches!(
            list_hubs(&f.ctx, Provider::Acc).await,
            Err(ApiError::Auth(_))
        ));
        // Without a version there is nothing to rename on ACC. The engine's message is shown
        // bare, as it was before the provider split.
        let err = rename_file(&f.ctx, &p, "L1", None, "new.pdf")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Message(_)), "{err:?}");
        assert_eq!(err.to_string(), "file has no remote version yet");
        assert_eq!(err.status(), None);
    }
}

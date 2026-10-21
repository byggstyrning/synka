//! Provider dispatch: the functions the engine calls to reach a provider's API.
//!
//! Each function picks the provider from `p.provider` (or `hub.provider`) and calls that
//! provider's client: `api::dm` and `api::oss` for ACC, `api::streambim` for StreamBIM and
//! `api::interaxo` for Interaxo. The ACC arms pass the arguments the engine passed before, so ACC
//! behaviour does not change. StreamBIM has one client per account: each StreamBIM arm takes the
//! account of its project or hub (`streambim_account`), so a request carries the token of the
//! server the project lives on. See docs/PROVIDERS.md "The dispatch module".
//!
//! The engine (`sync`) reaches every provider only through these functions.

use crate::api::{dm, interaxo, oss, streambim};
use crate::app::Ctx;
use crate::http::{Api, ApiError};
use crate::model::{
    urn_namespace, urn_suffix, HubInfo, Item, Kind, Project, ProjectInfo, Provider,
};
use crate::streambim_auth::{canonical_host, sign_in_label, StreamBimAccount};
use anyhow::anyhow;
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
#[derive(Clone, PartialEq, Eq)]
pub struct DownloadUrl {
    pub url: String,
    pub size: u64,
    /// Send `Authorization: Bearer <token>` with the request. Never for presigned URLs.
    pub bearer: Option<String>,
    /// The server answers Range requests, so the download may run in parallel segments.
    pub ranges: bool,
}

/// Logs show the handle without its secrets: the bearer is redacted, and the URL loses its query
/// (a presigned URL carries its signature there).
impl std::fmt::Debug for DownloadUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = self.url.split('?').next().unwrap_or_default();
        f.debug_struct("DownloadUrl")
            .field("url", &url)
            .field("size", &self.size)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .field("ranges", &self.ranges)
            .finish()
    }
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

/// The signed-in StreamBIM account of a project. Without one (never signed in there, or signed out)
/// the error names the server to sign in to. The engine pauses such a project before it asks, so
/// this is a guard, not the usual path.
fn streambim_account(ctx: &Ctx, p: &Project) -> Result<StreamBimAccount, ApiError> {
    ctx.streambim
        .for_project(p)
        .filter(|account| account.auth.is_signed_in())
        .ok_or_else(|| ApiError::Auth(anyhow!("Logga in på {} för att synka.", sign_in_label(p))))
}

/// The signed-in StreamBIM account of a hub (see `streambim_account`).
fn streambim_hub_account(ctx: &Ctx, hub: &HubInfo) -> Result<StreamBimAccount, ApiError> {
    ctx.streambim
        .for_hub(hub)
        .filter(|account| account.auth.is_signed_in())
        .ok_or_else(|| {
            let host = canonical_host(hub.region.as_deref().unwrap_or_default());
            ApiError::Auth(anyhow!("Logga in på StreamBIM ({host}) först."))
        })
}

/// Hubs of one provider: ACC's account hubs, one StreamBIM hub per signed-in account, Interaxo's
/// communities.
pub async fn list_hubs(ctx: &Ctx, provider: Provider) -> Result<Vec<HubInfo>, ApiError> {
    match provider {
        Provider::Acc => dm::list_hubs(&ctx.api).await,
        // A hub is made from the account alone (no request), so no account fails here.
        Provider::StreamBim => Ok(ctx
            .streambim
            .signed_in()
            .iter()
            .map(streambim::hub_for_account)
            .collect()),
        Provider::Interaxo => interaxo::list_hubs(&ctx.interaxo_api).await,
    }
}

/// Projects of one hub.
pub async fn list_projects(ctx: &Ctx, hub: &HubInfo) -> Result<Vec<ProjectInfo>, ApiError> {
    match hub.provider {
        Provider::Acc => dm::list_projects(&ctx.api, &hub.id).await,
        Provider::StreamBim => {
            let account = streambim_hub_account(ctx, hub)?;
            streambim::list_projects(&account.api, account.auth.base_url(), hub).await
        }
        Provider::Interaxo => interaxo::list_projects(&ctx.interaxo_api, hub).await,
    }
}

/// What subscribing to a project needs (the ACC arm is the logic of `sync::subscribe`).
pub async fn project_setup(
    ctx: &Ctx,
    hub: &HubInfo,
    project: &ProjectInfo,
) -> Result<ProjectSetup, ApiError> {
    match hub.provider {
        Provider::Acc => acc_project_setup(&ctx.api, hub, project).await,
        Provider::StreamBim => {
            let account = streambim_hub_account(ctx, hub)?;
            streambim::project_setup(&account.api, hub, project).await
        }
        Provider::Interaxo => interaxo::project_setup(&ctx.interaxo_api, hub, project).await,
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
    match p.provider {
        Provider::Acc => {
            let found = dm::top_folders(&ctx.api, &p.hub_id, &p.project_id).await?;
            Ok(found
                .into_iter()
                .map(|(urn, name)| (urn_suffix(&urn).to_string(), name))
                .collect())
        }
        Provider::StreamBim => streambim::top_folders(p).await,
        Provider::Interaxo => interaxo::top_folders(&ctx.interaxo_api, p).await,
    }
}

/// Children (folders and files) of one folder.
pub async fn folder_contents(
    ctx: &Ctx,
    p: &Project,
    folder_id: &str,
) -> Result<Vec<Item>, ApiError> {
    match p.provider {
        Provider::Acc => dm::folder_contents(&ctx.api, p, folder_id).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::folder_contents(&account.api, p, folder_id).await
        }
        Provider::Interaxo => interaxo::folder_contents(&ctx.interaxo_api, p, folder_id).await,
    }
}

/// A download handle for the version stored at `storage_urn` of item `item_id`.
pub async fn download_url(
    ctx: &Ctx,
    p: &Project,
    item_id: &str,
    storage_urn: &str,
) -> Result<DownloadUrl, ApiError> {
    match p.provider {
        // A presigned S3 URL: no bearer token, and ranges are answered.
        Provider::Acc => {
            let (url, size) = oss::download_url(&ctx.api, p, storage_urn).await?;
            Ok(DownloadUrl {
                url,
                size,
                bearer: None,
                ranges: true,
            })
        }
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::download_url(&account.api, p, item_id, storage_urn).await
        }
        Provider::Interaxo => {
            interaxo::download_url(&ctx.interaxo_api, p, item_id, storage_urn).await
        }
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
    match p.provider {
        Provider::Acc => {
            oss::open_download_with(&ctx.api, &d.url, offset, d.bearer.as_deref()).await
        }
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::open_download(&account.api, p, d, offset).await
        }
        Provider::Interaxo => interaxo::open_download(&ctx.interaxo_api, p, d, offset).await,
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
    match p.provider {
        Provider::Acc => {
            oss::open_range_with(&ctx.api, &d.url, start, end, d.bearer.as_deref()).await
        }
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::open_range(&account.api, p, d, start, end).await
        }
        Provider::Interaxo => interaxo::open_range(&ctx.interaxo_api, p, d, start, end).await,
    }
}

/// The current tip of a file and what its upload needs to know.
pub(crate) async fn get_item_upload_info(
    ctx: &Ctx,
    p: &Project,
    id: &str,
) -> Result<(Item, dm::UploadInfo), ApiError> {
    match p.provider {
        Provider::Acc => dm::get_item_upload_info(&ctx.api, p, id).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::get_item_upload_info(&account.api, p, id).await
        }
        Provider::Interaxo => interaxo::get_item_upload_info(&ctx.interaxo_api, p, id).await,
    }
}

/// True when a storage handle saved in an upload intent proves that the provider committed the
/// bytes. Only ACC can tell: its OSS object is listed with the version. The other providers
/// commit inside `create_item` / `create_version`, and their intent marker never appears in a
/// listing, so a saved marker is never evidence of a commit.
pub fn intent_proves_commit(provider: Provider) -> bool {
    provider == Provider::Acc
}

/// Reserve storage for the bytes of a new file. ACC answers a storage URN. The other providers
/// answer a marker that is unique per call, so two attempts never share one.
pub async fn create_storage(
    ctx: &Ctx,
    p: &Project,
    parent_id: &str,
    name: &str,
) -> Result<String, ApiError> {
    match p.provider {
        Provider::Acc => dm::create_storage(&ctx.api, p, parent_id, name).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            let marker = streambim::create_storage(&account.api, p, parent_id, name).await?;
            unique_marker(&marker)
        }
        Provider::Interaxo => {
            let marker = interaxo::create_storage(&ctx.interaxo_api, p, parent_id, name).await?;
            unique_marker(&marker)
        }
    }
}

/// `marker` with 128 random bits appended (`<provider>:pending:<hex>`).
fn unique_marker(marker: &str) -> Result<String, ApiError> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw)
        .map_err(|e| ApiError::Message(format!("slumpgenerator misslyckades: {e}")))?;
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{marker}:{hex}"))
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
    match p.provider {
        Provider::Acc => oss::upload_file(&ctx.api, p, storage, path, content_type, progress).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::upload_file(&account.api, p, storage, path, content_type, progress).await
        }
        Provider::Interaxo => {
            interaxo::upload_file(&ctx.interaxo_api, p, storage, path, content_type, progress).await
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
    match p.provider {
        Provider::Acc => dm::create_item(&ctx.api, p, parent_id, name, storage).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::create_item(&account.api, p, parent_id, name, storage, path, progress).await
        }
        Provider::Interaxo => {
            interaxo::create_item(
                &ctx.interaxo_api,
                p,
                parent_id,
                name,
                storage,
                path,
                progress,
            )
            .await
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
    match p.provider {
        Provider::Acc => dm::create_version(&ctx.api, p, id, name, storage).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::create_version(&account.api, p, id, name, storage, path, progress).await
        }
        Provider::Interaxo => {
            interaxo::create_version(&ctx.interaxo_api, p, id, name, storage, path, progress).await
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
    match p.provider {
        Provider::Acc => dm::create_folder(&ctx.api, p, parent_id, name).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::create_folder(&account.api, p, parent_id, name).await
        }
        Provider::Interaxo => interaxo::create_folder(&ctx.interaxo_api, p, parent_id, name).await,
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
    match p.provider {
        Provider::Acc => {
            let vurn = version_urn
                .ok_or_else(|| ApiError::Message("file has no remote version yet".to_string()))?;
            dm::rename_file(&ctx.api, p, vurn, new_name).await
        }
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::rename_file(&account.api, p, id, version_urn, new_name).await
        }
        Provider::Interaxo => {
            interaxo::rename_file(&ctx.interaxo_api, p, id, version_urn, new_name).await
        }
    }
}

/// Move a file into another folder.
pub async fn move_file(
    ctx: &Ctx,
    p: &Project,
    id: &str,
    new_parent_id: &str,
) -> Result<(), ApiError> {
    match p.provider {
        Provider::Acc => dm::move_file(&ctx.api, p, id, new_parent_id).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::move_file(&account.api, p, id, new_parent_id).await
        }
        Provider::Interaxo => interaxo::move_file(&ctx.interaxo_api, p, id, new_parent_id).await,
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
    match p.provider {
        Provider::Acc => dm::patch_folder(&ctx.api, p, folder_id, new_name, new_parent_id).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::patch_folder(&account.api, p, folder_id, new_name, new_parent_id).await
        }
        Provider::Interaxo => {
            interaxo::patch_folder(&ctx.interaxo_api, p, folder_id, new_name, new_parent_id).await
        }
    }
}

/// Delete a file (soft delete on ACC and StreamBIM).
pub async fn delete_file(ctx: &Ctx, p: &Project, id: &str) -> Result<(), ApiError> {
    match p.provider {
        Provider::Acc => dm::delete_file(&ctx.api, p, id).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::delete_file(&account.api, p, id).await
        }
        Provider::Interaxo => interaxo::delete_file(&ctx.interaxo_api, p, id).await,
    }
}

/// Delete a folder.
pub async fn delete_folder(ctx: &Ctx, p: &Project, folder_id: &str) -> Result<(), ApiError> {
    match p.provider {
        Provider::Acc => dm::delete_folder(&ctx.api, p, folder_id).await,
        Provider::StreamBim => {
            let account = streambim_account(ctx, p)?;
            streambim::delete_folder(&account.api, p, folder_id).await
        }
        Provider::Interaxo => interaxo::delete_folder(&ctx.interaxo_api, p, folder_id).await,
    }
}

/// The provider's web page for an item (the API part only: the engine refuses local-only
/// items and the hub itself before it asks).
pub async fn web_link(ctx: &Ctx, p: &Project, item: &Item) -> Result<String, ApiError> {
    match p.provider {
        Provider::Acc => acc_web_link(&ctx.api, p, item).await,
        // The page is named by the project's meta: no request, so no account is needed.
        Provider::StreamBim => streambim::web_link(p, item).await,
        Provider::Interaxo => interaxo::web_link(&ctx.interaxo_api, p, item).await,
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
    match p.provider {
        Provider::Acc => dm::set_reserved(&ctx.api, p, id, reserve).await,
        // StreamBIM has no locks: refused before any account is needed.
        Provider::StreamBim => streambim::set_reserved(p, id, reserve).await,
        Provider::Interaxo => interaxo::set_reserved(&ctx.interaxo_api, p, id, reserve).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{dpapi_protect, Auth};
    use crate::config::Config;
    use crate::ipc::IpcHub;
    use crate::model::{ChangeSourceKind, TopFolder};
    use crate::paths::Paths;
    use crate::store::Store;
    use crate::streambim_auth::StreamBimAccounts;
    use crate::test_support::{Fault, Server};
    use serde_json::json;
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
        let streambim = Arc::new(StreamBimAccounts::load(http.clone(), &state));
        let (tx, _rx) = std::sync::mpsc::channel();
        let ui = crate::app::UiSender {
            tx,
            main_thread_id: 0,
            hub: Arc::new(IpcHub::new(paths.sync_root.clone(), false)),
        };
        let cfg = Config::default();
        let ctx = Ctx {
            settings: RwLock::new(cfg.clone()),
            cfg,
            paths,
            store,
            auth: auth.clone(),
            api: Api::new(http.clone(), auth.clone()),
            streambim,
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

    /// A StreamBIM project of account `key`, served at `base` (its meta names both).
    fn streambim_project(key: &str, base: &str, id: &str) -> Project {
        let mut p = project(Provider::StreamBim);
        p.hub_id = format!("streambim:{key}");
        p.project_id = format!("streambim:{key}:{id}");
        p.meta = Some(json!({"region": key, "base": base, "project": id}).to_string());
        p
    }

    /// Writes a signed-in StreamBIM account: its token file (DPAPI) and its profile.
    fn write_session(state: &Path, key: &str, base: &str, id_token: &str) {
        let tokens = json!({
            "refresh_token": "test-only-refresh",
            "id_token": id_token,
            "expires_at": 4_102_444_800i64,
        });
        std::fs::write(
            state.join(format!("token-streambim-{key}.bin")),
            dpapi_protect(tokens.to_string().as_bytes()).unwrap(),
        )
        .unwrap();
        let profile = json!({
            "user_id": "test-only-streambim-user",
            "name": "Testanvändare",
            "email": "test@example.invalid",
            "region": key,
            "base": base,
        });
        std::fs::write(
            state.join(format!("user-streambim-{key}.json")),
            profile.to_string(),
        )
        .unwrap();
    }

    /// Loads the accounts of the fixture's state directory again, after `write_session`.
    fn reload_accounts(f: &mut Fixture) {
        let http = crate::http::build_client();
        f.ctx.streambim = Arc::new(StreamBimAccounts::load(http, &f.ctx.paths.state_dir));
    }

    /// An empty listing for the StreamBIM JSON:API calls of `folder_contents`.
    fn empty_listing() -> Fault {
        Fault::reply(200, json!({"data": []}).to_string())
            .header("Content-Type", "application/vnd.api+json")
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
        // Not signed in: the call fails with an auth error before any request is sent, and the
        // error names the server to sign in to.
        let err = folder_contents(&f.ctx, &p, "0").await.unwrap_err();
        assert!(matches!(err, ApiError::Auth(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "auth: Logga in på StreamBIM (app.streambim.com) för att synka."
        );
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
    async fn two_accounts_make_two_hubs_and_each_project_uses_its_own_token() {
        let app = Server::new(|req, _| {
            if req.header("authorization") == Some("Bearer id-app") {
                empty_listing()
            } else {
                Fault::reply(401, "wrong token")
            }
        })
        .await;
        let japan = Server::new(|req, _| {
            if req.header("authorization") == Some("Bearer id-japan") {
                empty_listing()
            } else {
                Fault::reply(401, "wrong token")
            }
        })
        .await;
        let mut f = fixture("two-accounts");
        let state = f.ctx.paths.state_dir.clone();
        write_session(&state, "app", &app.origin, "id-app");
        write_session(&state, "japan", &japan.origin, "id-japan");
        reload_accounts(&mut f);

        let hubs = list_hubs(&f.ctx, Provider::StreamBim).await.unwrap();
        let ids: Vec<(&str, &str)> = hubs
            .iter()
            .map(|h| (h.id.as_str(), h.name.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                ("streambim:app", "StreamBIM"),
                ("streambim:japan", "StreamBIM Japan")
            ]
        );
        assert!(hubs.iter().all(|h| h.provider == Provider::StreamBim));

        let app_project = streambim_project("app", &app.origin, "1552");
        let japan_project = streambim_project("japan", &japan.origin, "9001");
        folder_contents(&f.ctx, &app_project, "0").await.unwrap();
        folder_contents(&f.ctx, &japan_project, "0").await.unwrap();
        assert!(app
            .requests()
            .iter()
            .all(|r| r.header("authorization") == Some("Bearer id-app")));
        assert!(japan
            .requests()
            .iter()
            .all(|r| r.header("authorization") == Some("Bearer id-japan")));
        assert!(!app.requests().is_empty() && !japan.requests().is_empty());
    }

    #[tokio::test]
    async fn a_project_of_a_signed_out_account_is_refused_without_a_request() {
        let server = Server::new(|_, _| empty_listing()).await;
        let mut f = fixture("signed-out-account");
        let state = f.ctx.paths.state_dir.clone();
        write_session(&state, "app", &server.origin, "id-app");
        reload_accounts(&mut f);
        // The project's account (japan) has no session on this install.
        let japan = streambim_project("japan", &server.origin, "9001");
        let err = folder_contents(&f.ctx, &japan, "0").await.unwrap_err();
        assert!(matches!(err, ApiError::Auth(_)), "{err}");
        assert!(
            err.to_string().contains("StreamBIM (japan.streambim.com)"),
            "{err}"
        );
        assert_eq!(server.count(), 0);
    }

    #[tokio::test]
    async fn the_existing_user_project_keeps_its_account_and_hub() {
        let mut f = fixture("legacy-meta");
        let state = f.ctx.paths.state_dir.clone();
        write_session(&state, "app", "https://app.streambim.com", "id-app");
        reload_accounts(&mut f);
        // The meta and ids of a project made by the single-account version.
        let mut p = project(Provider::StreamBim);
        p.hub_id = "streambim:app".into();
        p.project_id = "streambim:app:13588".into();
        p.meta =
            Some(r#"{"base":"https://app.streambim.com","project":"13588","region":"app"}"#.into());
        let account = f.ctx.streambim.for_project(&p).expect("the app account");
        assert_eq!(account.key, "app");
        assert!(f.ctx.project_connected(&p));
        let hubs = list_hubs(&f.ctx, Provider::StreamBim).await.unwrap();
        assert_eq!(hubs.len(), 1);
        assert_eq!(hubs[0].id, "streambim:app");
        assert_eq!(hubs[0].name, "StreamBIM");
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
    async fn non_acc_storage_markers_are_unique_and_never_prove_a_commit() {
        let mut f = fixture("markers");
        let state = f.ctx.paths.state_dir.clone();
        let server = Server::new(|_, _| empty_listing()).await;
        write_session(&state, "app", &server.origin, "id-app");
        reload_accounts(&mut f);
        let sb = streambim_project("app", &server.origin, "1552");
        let mut ix = project(Provider::Interaxo);
        ix.meta = None;
        for p in [&sb, &ix] {
            let first = create_storage(&f.ctx, p, "0", "a.pdf").await.unwrap();
            let second = create_storage(&f.ctx, p, "0", "a.pdf").await.unwrap();
            let prefix = format!("{}:pending:", p.provider.as_str());
            assert!(first.starts_with(&prefix), "{first}");
            assert_ne!(first, second, "every attempt gets its own marker");
            assert!(!intent_proves_commit(p.provider));
        }
        assert!(intent_proves_commit(Provider::Acc));
    }

    #[test]
    fn a_download_handle_never_logs_its_bearer_or_its_signature() {
        let d = DownloadUrl {
            url: "https://files.example.invalid/object?X-Amz-Signature=abc123".into(),
            size: 14,
            bearer: Some("secret-bearer-token".into()),
            ranges: true,
        };
        let text = format!("{d:?}");
        assert!(!text.contains("secret-bearer-token"), "{text}");
        assert!(!text.contains("abc123"), "{text}");
        assert!(text.contains("bearer: Some(\"<redacted>\")"), "{text}");
        assert!(
            text.contains("https://files.example.invalid/object"),
            "{text}"
        );
        let open = DownloadUrl { bearer: None, ..d };
        assert!(format!("{open:?}").contains("bearer: None"));
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

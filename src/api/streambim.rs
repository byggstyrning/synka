//! StreamBIM REST client. STUB: every function returns `ApiError::Unsupported` until the client
//! is written. The signatures are final; `crate::remote` calls them with the same arguments
//! (`api` = `ctx.api_for(provider)`, the rest as in `remote`). Endpoints and the facts they rely
//! on: docs/specs/streambim-endpoints.md and docs/PROVIDERS.md "StreamBIM".
//!
//! `list_hubs` takes the sign-in too: the region host comes from it.

#![allow(dead_code)]

use crate::api::dm::UploadInfo;
use crate::http::{Api, ApiError};
use crate::model::{HubInfo, Item, Project, ProjectInfo};
use crate::remote::{DownloadUrl, ProjectSetup};
use crate::streambim_auth::StreamBimAuth;
use std::path::Path;

const NOT_YET: &str = "StreamBIM stöds inte ännu";

fn not_yet<T>() -> Result<T, ApiError> {
    Err(ApiError::Unsupported(NOT_YET.into()))
}

pub async fn list_hubs(_api: &Api, _auth: &StreamBimAuth) -> Result<Vec<HubInfo>, ApiError> {
    not_yet()
}

pub async fn list_projects(_api: &Api, _hub: &HubInfo) -> Result<Vec<ProjectInfo>, ApiError> {
    not_yet()
}

pub async fn project_setup(
    _api: &Api,
    _hub: &HubInfo,
    _project: &ProjectInfo,
) -> Result<ProjectSetup, ApiError> {
    not_yet()
}

pub async fn top_folders(_api: &Api, _p: &Project) -> Result<Vec<(String, String)>, ApiError> {
    not_yet()
}

pub async fn folder_contents(
    _api: &Api,
    _p: &Project,
    _folder_id: &str,
) -> Result<Vec<Item>, ApiError> {
    not_yet()
}

pub async fn download_url(
    _api: &Api,
    _p: &Project,
    _item_id: &str,
    _storage_urn: &str,
) -> Result<DownloadUrl, ApiError> {
    not_yet()
}

pub async fn open_download(
    _api: &Api,
    _p: &Project,
    _d: &DownloadUrl,
    _offset: u64,
) -> Result<reqwest::Response, ApiError> {
    not_yet()
}

pub async fn open_range(
    _api: &Api,
    _p: &Project,
    _d: &DownloadUrl,
    _start: u64,
    _end: u64,
) -> Result<reqwest::Response, ApiError> {
    not_yet()
}

pub(crate) async fn get_item_upload_info(
    _api: &Api,
    _p: &Project,
    _id: &str,
) -> Result<(Item, UploadInfo), ApiError> {
    not_yet()
}

pub async fn create_storage(
    _api: &Api,
    _p: &Project,
    _parent_id: &str,
    _name: &str,
) -> Result<String, ApiError> {
    not_yet()
}

pub async fn upload_file(
    _api: &Api,
    _p: &Project,
    _storage: &str,
    _path: &Path,
    _content_type: Option<&str>,
    _progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), ApiError> {
    not_yet()
}

pub async fn create_item(
    _api: &Api,
    _p: &Project,
    _parent_id: &str,
    _name: &str,
    _storage: &str,
    _path: &Path,
    _progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<Item, ApiError> {
    not_yet()
}

pub async fn create_version(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _name: &str,
    _storage: &str,
    _path: &Path,
    _progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(i64, String), ApiError> {
    not_yet()
}

pub async fn create_folder(
    _api: &Api,
    _p: &Project,
    _parent_id: &str,
    _name: &str,
) -> Result<Item, ApiError> {
    not_yet()
}

pub async fn rename_file(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _version_urn: Option<&str>,
    _new_name: &str,
) -> Result<(), ApiError> {
    not_yet()
}

pub async fn move_file(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _new_parent_id: &str,
) -> Result<(), ApiError> {
    not_yet()
}

pub async fn patch_folder(
    _api: &Api,
    _p: &Project,
    _folder_id: &str,
    _new_name: Option<&str>,
    _new_parent_id: Option<&str>,
) -> Result<(), ApiError> {
    not_yet()
}

pub async fn delete_file(_api: &Api, _p: &Project, _id: &str) -> Result<(), ApiError> {
    not_yet()
}

pub async fn delete_folder(_api: &Api, _p: &Project, _folder_id: &str) -> Result<(), ApiError> {
    not_yet()
}

pub async fn web_link(_api: &Api, _p: &Project, _item: &Item) -> Result<String, ApiError> {
    not_yet()
}

pub async fn set_reserved(
    _api: &Api,
    _p: &Project,
    _id: &str,
    _reserve: bool,
) -> Result<Option<String>, ApiError> {
    not_yet()
}

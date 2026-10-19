//! `desktop3:CustomStateHandler`: the Låst av / Version / Ändrad av values for one path,
//! answered from SQLite (no network, no hydration). Ids and names are
//! `super::PROPERTY_DEFINITIONS`.
//!
//! Only the lock state carries an icon: `Assets\LockedByMe.ico` and `Assets\LockedByOther.ico`
//! in the package (ui/Assets, made by packaging/make_icons.py), referenced by absolute file
//! path, which `StorageProviderItemProperty.IconResource` accepts (docs/specs/cloud-files.md).
//! System icon resource ids are undocumented, so none are used.

use super::{PROP_LOCKED_BY, PROP_MODIFIED_BY, PROP_VERSION};
use crate::model::{Item, Kind};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::core::{implement, HSTRING};
use windows::Storage::Provider::{
    IStorageProviderItemPropertySource, IStorageProviderItemPropertySource_Impl,
    StorageProviderItemProperty,
};
use windows_collections::IIterable;

pub(super) const ICON_LOCKED_BY_ME: &str = "LockedByMe.ico";
pub(super) const ICON_LOCKED_BY_OTHER: &str = "LockedByOther.ico";

#[implement(IStorageProviderItemPropertySource)]
pub(super) struct CustomStates;

impl IStorageProviderItemPropertySource_Impl for CustomStates_Impl {
    fn GetItemProperties(
        &self,
        item_path: &HSTRING,
    ) -> windows::core::Result<IIterable<StorageProviderItemProperty>> {
        let path = PathBuf::from(item_path.to_os_string());
        let props = catch_unwind(AssertUnwindSafe(|| lookup(&path))).unwrap_or_default();
        let mut out = Vec::with_capacity(props.len());
        for p in &props {
            out.push(Some(to_winrt(p)?));
        }
        Ok(IIterable::from(out))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LockIcon {
    Mine,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Prop {
    pub id: i32,
    pub value: String,
    pub icon: Option<LockIcon>,
}

fn lookup(path: &Path) -> Vec<Prop> {
    let Some(ctx) = super::context() else {
        return Vec::new();
    };
    let Some((_, item)) = super::resolve_path(ctx, path) else {
        return Vec::new();
    };
    let me = ctx.auth.user().map(|u| u.name);
    item_props(&item, me.as_deref())
}

/// The custom states of one item. Files only; folders and project roots have none.
pub(super) fn item_props(it: &Item, me: Option<&str>) -> Vec<Prop> {
    let mut out = Vec::new();
    if it.kind != Kind::File {
        return out;
    }
    if let Some(who) = it.locked_by.as_deref().map(str::trim) {
        let mine = me
            .map(str::trim)
            .is_some_and(|m| !m.is_empty() && m.to_lowercase() == who.to_lowercase());
        out.push(Prop {
            id: PROP_LOCKED_BY,
            value: if who.is_empty() {
                "okänd".to_string()
            } else {
                who.to_string()
            },
            icon: Some(if mine {
                LockIcon::Mine
            } else {
                LockIcon::Other
            }),
        });
    }
    if let Some(v) = it.remote_version {
        out.push(Prop {
            id: PROP_VERSION,
            value: format!("V{v}"),
            icon: None,
        });
    }
    if let Some(by) = it
        .modified_by
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(Prop {
            id: PROP_MODIFIED_BY,
            value: by.to_string(),
            icon: None,
        });
    }
    out
}

fn to_winrt(p: &Prop) -> windows::core::Result<StorageProviderItemProperty> {
    let prop = StorageProviderItemProperty::new()?;
    prop.SetId(p.id)?;
    prop.SetValue(&HSTRING::from(p.value.as_str()))?;
    if let Some(icon) = p.icon.and_then(icon_resource) {
        prop.SetIconResource(icon)?;
    }
    Ok(prop)
}

fn icon_resource(icon: LockIcon) -> Option<&'static HSTRING> {
    static ICONS: OnceLock<Option<(HSTRING, HSTRING)>> = OnceLock::new();
    let (mine, other) = ICONS
        .get_or_init(|| {
            let dir = icon_dir()?;
            Some((
                HSTRING::from(dir.join(ICON_LOCKED_BY_ME).as_path()),
                HSTRING::from(dir.join(ICON_LOCKED_BY_OTHER).as_path()),
            ))
        })
        .as_ref()?;
    Some(match icon {
        LockIcon::Mine => mine,
        LockIcon::Other => other,
    })
}

/// `<package>\Assets` next to `engine\` when installed; `ui\Assets` of the repo for a dev
/// build in `target\<profile>\`.
fn icon_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let candidates = [
        exe_dir.parent().map(|d| d.join("Assets")),
        exe_dir
            .parent()
            .and_then(Path::parent)
            .map(|d| d.join("ui").join("Assets")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|d| d.join(ICON_LOCKED_BY_ME).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LocalState;

    fn file() -> Item {
        Item {
            pid: 1,
            id: "lin1".into(),
            parent: Some("co.f".into()),
            name: "Plan.rvt".into(),
            kind: Kind::File,
            remote_version: Some(7),
            base_version: Some(7),
            size: 1,
            ctime_ms: 0,
            mtime_ms: 0,
            storage_urn: None,
            version_urn: None,
            modified_by: Some("Anna Andersson".into()),
            locked_by: None,
            local_state: LocalState::Clean,
            populated: false,
        }
    }

    fn prop(id: i32, value: &str, icon: Option<LockIcon>) -> Prop {
        Prop {
            id,
            value: value.into(),
            icon,
        }
    }

    #[test]
    fn values_for_a_file() {
        let it = file();
        assert_eq!(
            item_props(&it, Some("Bo")),
            vec![
                prop(PROP_VERSION, "V7", None),
                prop(PROP_MODIFIED_BY, "Anna Andersson", None)
            ]
        );

        let mut locked = file();
        locked.locked_by = Some("Bo Ek".into());
        assert_eq!(
            item_props(&locked, Some("bo ek"))[0],
            prop(PROP_LOCKED_BY, "Bo Ek", Some(LockIcon::Mine))
        );
        assert_eq!(
            item_props(&locked, Some("Anna"))[0],
            prop(PROP_LOCKED_BY, "Bo Ek", Some(LockIcon::Other))
        );
        assert_eq!(item_props(&locked, None)[0].icon, Some(LockIcon::Other));

        locked.locked_by = Some(" ".into());
        assert_eq!(
            item_props(&locked, Some(""))[0],
            prop(PROP_LOCKED_BY, "okänd", Some(LockIcon::Other))
        );

        let mut bare = file();
        bare.remote_version = None;
        bare.modified_by = Some("  ".into());
        assert!(
            item_props(&bare, None).is_empty(),
            "nothing known, nothing shown"
        );
    }

    #[test]
    fn folders_have_no_states() {
        let mut dir = file();
        dir.kind = Kind::Folder;
        dir.locked_by = Some("Bo".into());
        assert!(item_props(&dir, None).is_empty());
        dir.kind = Kind::ProjectRoot;
        assert!(item_props(&dir, None).is_empty());
    }

    #[test]
    fn ids_match_the_definitions() {
        let ids: Vec<i32> = super::super::PROPERTY_DEFINITIONS
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ids, vec![PROP_LOCKED_BY, PROP_VERSION, PROP_MODIFIED_BY]);
    }

    #[test]
    fn lock_icons_are_shipped() {
        // The file names must match ICON_LOCKED_BY_ME / ICON_LOCKED_BY_OTHER.
        let shipped: [(&str, &[u8]); 2] = [
            (
                "LockedByMe.ico",
                include_bytes!("../../ui/Assets/LockedByMe.ico"),
            ),
            (
                "LockedByOther.ico",
                include_bytes!("../../ui/Assets/LockedByOther.ico"),
            ),
        ];
        assert_eq!(
            [shipped[0].0, shipped[1].0],
            [ICON_LOCKED_BY_ME, ICON_LOCKED_BY_OTHER]
        );
        for (name, bytes) in shipped {
            assert!(bytes.starts_with(&[0, 0, 1, 0]), "{name} is an .ico");
        }
    }
}

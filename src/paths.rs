//! Locations. Packaged (MSIX): state in ApplicationData::Current().LocalFolder()
//! (%LOCALAPPDATA%\Packages\<PFN>\LocalState). Unpackaged dev run: %LOCALAPPDATA%\Synka.
//! Sync root: %USERPROFILE%\Synka unless config.sync_root is set. NOT under AppData (MSIX
//! would virtualize it and Explorer users could not find it).

use std::path::PathBuf;
use windows::ApplicationModel::Package;
use windows::Storage::ApplicationData;

/// Name of the default sync root folder under %USERPROFILE%.
pub const DEFAULT_SYNC_ROOT_NAME: &str = "Synka";

#[derive(Debug, Clone)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
    pub db_file: PathBuf,
    pub sync_root: PathBuf,
    /// True when running with package identity.
    pub packaged: bool,
    /// Package family name when packaged (for the desktop shortcut target).
    pub package_family_name: Option<String>,
}

/// Resolve and create directories. `sync_root_override` from config.
pub fn resolve(sync_root_override: Option<&str>) -> anyhow::Result<Paths> {
    // Package::Current() fails with APPMODEL_ERROR_NO_PACKAGE when running unpackaged.
    let (packaged, package_family_name) = match Package::Current() {
        Ok(pkg) => {
            let pfn = pkg.Id()?.FamilyName()?.to_string();
            (true, Some(pfn))
        }
        Err(_) => (false, None),
    };

    let state_dir = if packaged {
        let folder = ApplicationData::Current()?.LocalFolder()?;
        PathBuf::from(folder.Path()?.to_string())
    } else {
        let local = std::env::var_os("LOCALAPPDATA")
            .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA is not set"))?;
        PathBuf::from(local).join("Synka")
    };
    let log_dir = state_dir.join("logs");
    let db_file = state_dir.join("synka.db");

    let sync_root = match sync_root_override.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => PathBuf::from(expand_env(s)),
        None => {
            let profile = std::env::var_os("USERPROFILE")
                .ok_or_else(|| anyhow::anyhow!("USERPROFILE is not set"))?;
            PathBuf::from(profile).join(DEFAULT_SYNC_ROOT_NAME)
        }
    };

    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(&log_dir)?;
    std::fs::create_dir_all(&sync_root)?;

    Ok(Paths {
        state_dir,
        log_dir,
        db_file,
        sync_root,
        packaged,
        package_family_name,
    })
}

/// Expands `%NAME%` sequences from the environment. Unknown names are kept verbatim.
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::expand_env;

    #[test]
    fn expands_known_and_keeps_unknown() {
        std::env::set_var("SYNKA_TEST_VAR", "abc");
        assert_eq!(expand_env("%SYNKA_TEST_VAR%\\x"), "abc\\x");
        assert_eq!(
            expand_env("%NO_SUCH_SYNKA_VAR%\\x"),
            "%NO_SUCH_SYNKA_VAR%\\x"
        );
        assert_eq!(expand_env("plain"), "plain");
        assert_eq!(expand_env("50% off"), "50% off");
    }
}

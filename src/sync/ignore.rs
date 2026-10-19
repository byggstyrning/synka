//! Compiled ignore rules for local files that must never be uploaded: editor lock, temp and backup
//! files from Office, AutoCAD and Revit, browser partials and OS metadata. Sources per rule:
//! docs/specs/save-patterns.md.
//!   names starting "~$" or ".~lock." ; "desktop.ini", "thumbs.db", ".ds_store";
//!   extensions: .tmp .temp .bak .dwl .dwl2 .lck .ac$ .sv$ .swp .crdownload .part .partial
//!               .slog (Revit) .rvt.slog ; shortcuts .url .lnk .wsh (ACC listings hide them, so
//!               an uploaded one would look deleted remotely and be removed locally) ;
//!               Office 8-hex-char temp names without extension
//!               (e.g. "A1B2C3D4"), "atmp*" ; Revit backups "name.0001.rvt"/".rfa" (4 digits);
//!   any path component "*_backup" (Revit central backup dirs) or ending ".rvt_backup";
//!   FILE_ATTRIBUTE_HIDDEN or TEMPORARY files.
//! No regex crate: hand-written matching, case-insensitive, allocation-free where possible.

use std::path::{Component, Path};

const ATTR_HIDDEN: u32 = 0x2;
const ATTR_TEMPORARY: u32 = 0x100;

/// Extensions (lowercase, with the dot). Matched case-insensitively as a suffix.
const IGNORED_EXTS: &[&str] = &[
    ".tmp",
    ".temp",
    ".bak",
    ".dwl",
    ".dwl2",
    ".lck",
    ".ac$",
    ".sv$",
    ".swp",
    ".crdownload",
    ".part",
    ".partial",
    ".slog",
    ".url",
    ".lnk",
    ".wsh",
];
/// Whole names, matched case-insensitively.
const IGNORED_NAMES: &[&str] = &["desktop.ini", "thumbs.db", ".ds_store"];
/// Prefixes, matched case-insensitively ("atmp*" = Office/Autodesk temp files).
const IGNORED_PREFIXES: &[&str] = &["~$", ".~lock.", "atmp", ".synka-reset-"];
/// Suffixes, matched case-insensitively (Revit central backup folders "x.rvt_backup").
const IGNORED_SUFFIXES: &[&str] = &["_backup"];

/// ASCII case-insensitive prefix test on bytes (never slices a UTF-8 char boundary).
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    let (sb, pb) = (s.as_bytes(), prefix.as_bytes());
    sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb)
}

/// ASCII case-insensitive suffix test on bytes.
fn ends_with_ci(s: &str, suffix: &str) -> bool {
    let (sb, xb) = (s.as_bytes(), suffix.as_bytes());
    sb.len() >= xb.len() && sb[sb.len() - xb.len()..].eq_ignore_ascii_case(xb)
}

/// Office temp names: exactly 8 hex characters and no extension.
fn is_office_temp(name: &str) -> bool {
    name.len() == 8 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Revit backups: "<stem>.NNNN.rvt" or "<stem>.NNNN.rfa" with exactly four digits.
fn is_revit_backup(name: &str) -> bool {
    let b = name.as_bytes();
    if b.len() < 9 || !(ends_with_ci(name, ".rvt") || ends_with_ci(name, ".rfa")) {
        return false;
    }
    // ".NNNN" sits right before the 4-byte extension.
    b[b.len() - 9] == b'.' && b[b.len() - 8..b.len() - 4].iter().all(u8::is_ascii_digit)
}

/// True if the file/dir name alone must be ignored.
pub fn ignored_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    IGNORED_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
        || IGNORED_PREFIXES.iter().any(|p| starts_with_ci(name, p))
        || IGNORED_SUFFIXES.iter().any(|s| ends_with_ci(name, s))
        || IGNORED_EXTS.iter().any(|e| ends_with_ci(name, e))
        || is_office_temp(name)
        || is_revit_backup(name)
}

/// True if any component of `rel_path` (relative to the sync root) is ignored, or
/// `attributes` has HIDDEN (0x2) or TEMPORARY (0x100).
pub fn ignored(rel_path: &Path, attributes: u32) -> bool {
    if attributes & (ATTR_HIDDEN | ATTR_TEMPORARY) != 0 {
        return true;
    }
    rel_path.components().any(|c| match c {
        Component::Normal(s) => match s.to_str() {
            Some(n) => ignored_name(n),
            None => ignored_name(&s.to_string_lossy()),
        },
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ignores_office_and_temp_names() {
        for n in [
            "~$Ritning.docx",
            "~$",
            ".~lock.Budget.xlsx#",
            "A1B2C3D4",
            "a1b2c3d4",
            "atmp123.tmp",
            "atmpfile",
            "desktop.ini",
            "Desktop.INI",
            "Thumbs.db",
            ".DS_Store",
            "x.tmp",
            "x.TEMP",
            "x.bak",
            "x.dwl",
            "x.dwl2",
            "x.lck",
            "x.ac$",
            "x.sv$",
            "x.swp",
            "x.crdownload",
            "x.part",
            "x.partial",
            "Länk till ACC.url",
            "Genväg.LNK",
            "script.wsh",
        ] {
            assert!(ignored_name(n), "should ignore {n:?}");
        }
    }

    #[test]
    fn ignores_revit_files() {
        for n in [
            "model.0001.rvt",
            "MODEL.0042.RVT",
            "model.0001.rfa",
            "x.rvt.slog",
            "x.slog",
            "Kopmannen.rvt.dwl2",
            "Kopmannen_backup",
            "Kopmannen.rvt_backup",
        ] {
            assert!(ignored_name(n), "should ignore {n:?}");
        }
    }

    #[test]
    fn keeps_normal_files() {
        for n in [
            "Kopmannen.rvt",
            "ritning.pdf",
            "Köpmannen - fasad.ifc",
            "abc.0001.txt",
            "A1B2C3D",
            "A1B2C3D4E",
            "A1B2C3DG",
            "model.001.rvt",
            "model.00001.rvt",
            "Kopmannen.rfa",
            "Plan.dwg",
            "temperature.txt",
            "backup-plan.pdf",
            "",
        ] {
            assert!(!ignored_name(n), "should keep {n:?}");
        }
    }

    #[test]
    fn components_and_attributes() {
        assert!(ignored(Path::new("Hus/Kopmannen.rvt_backup/x.rvt"), 0));
        assert!(ignored(Path::new("Hus/Kopmannen_backup/x.rvt"), 0));
        assert!(ignored(Path::new("Hus/ritning.pdf"), ATTR_HIDDEN));
        assert!(ignored(Path::new("Hus/ritning.pdf"), ATTR_TEMPORARY));
        assert!(!ignored(Path::new("Hus/ritning.pdf"), 0x20));
        assert!(!ignored(Path::new("Hus/Kopmannen.rvt"), 0));
        assert!(!ignored(&PathBuf::from("Köpmannen - fasad.ifc"), 0));
        assert!(!ignored(Path::new(""), 0));
    }
}

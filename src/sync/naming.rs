//! Local names: the one place that decides how an ACC name looks on disk and when two names
//! refer to the same entry. Everything that maps names between ACC, the DB and the disk goes
//! through here, so the engine, the uploader and the IPC layer can never disagree.
//!
//! Windows compares names case-insensitively with a simple one-to-one upcase table (NTFS),
//! for all of Unicode, not only ASCII: "Över" and "över" are the same file. `same_name`
//! mirrors that by upcasing per character and keeping characters whose uppercase form is
//! more than one character (ß stays ß, as on NTFS).

use std::path::{Path, PathBuf};

/// Windows-safe name for a disk entry: replace <>:"/\|?* and control characters with '_',
/// trim trailing dots and spaces, and escape reserved device names (CON, PRN, AUX, NUL,
/// CONIN$, CONOUT$, COM1-9, LPT1-9 and COM¹²³/LPT¹²³ with superscript digits, which Windows
/// reserves too) by inserting '_' before the first dot.
pub fn sanitize_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() {
        return "_".into();
    }
    let stem_end = s.find('.').unwrap_or(s.len());
    if is_reserved(&s[..stem_end].to_ascii_uppercase()) {
        s.insert(stem_end, '_');
    }
    s
}

fn is_reserved(stem: &str) -> bool {
    if matches!(stem, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let (Some(port), Some(digit)) = (stem.get(..3), stem.get(3..)) else {
        return false;
    };
    matches!(port, "COM" | "LPT")
        && digit.chars().count() == 1
        && matches!(digit.chars().next(), Some('1'..='9' | '¹' | '²' | '³'))
}

fn fold(c: char) -> char {
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// Two names as Windows compares them (case-insensitive over all of Unicode).
pub fn same_name(a: &str, b: &str) -> bool {
    a.chars().map(fold).eq(b.chars().map(fold))
}

/// `local` (a name as found on disk) folded the way Windows compares names.
pub fn fold_key(local: &str) -> String {
    local.chars().map(fold).collect()
}

/// Index key of an ACC/DB name: equal keys <=> `matches_local(name, local)` for the on-disk
/// name `local` with `fold_key(local) == name_key(name)`.
pub fn name_key(name: &str) -> String {
    fold_key(&sanitize_name(name))
}

/// A name as stored in ACC/the DB refers to the local entry `local` (as found on disk).
pub fn matches_local(item_name: &str, local: &str) -> bool {
    same_name(&sanitize_name(item_name), local)
}

/// Same path on Windows (component-wise, case-insensitive).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let mut ac = a.components();
    let mut bc = b.components();
    loop {
        match (ac.next(), bc.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                if !same_name(
                    &x.as_os_str().to_string_lossy(),
                    &y.as_os_str().to_string_lossy(),
                ) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Timestamp used in conflict names, local time "YYYY-MM-DD HHMM".
pub fn conflict_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H%M").to_string()
}

/// "plan.dwg" -> "plan (konflikt <ts>).dwg"; names without an extension (or dot files) get
/// the suffix at the end.
pub fn conflict_name(name: &str, ts: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} (konflikt {ts}){}", &name[..i], &name[i..]),
        _ => format!("{name} (konflikt {ts})"),
    }
}

/// A free path for a conflict copy of `path`. Two conflicts in the same minute get " 2", " 3"…
/// appended inside the parentheses, so an earlier copy is never overwritten.
pub fn conflict_path(path: &Path, ts: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut candidate = path.with_file_name(conflict_name(&name, ts));
    let mut n = 2;
    while candidate.symlink_metadata().is_ok() && n < 1000 {
        candidate = path.with_file_name(conflict_name(&name, &format!("{ts} {n}")));
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(sanitize_name("name. "), "name");
        assert_eq!(sanitize_name("CON.txt"), "CON_.txt");
        assert_eq!(sanitize_name("con"), "con_");
        assert_eq!(sanitize_name("lpt3"), "lpt3_");
        assert_eq!(sanitize_name("COM0"), "COM0");
        assert_eq!(sanitize_name("COM¹.log"), "COM¹_.log");
        assert_eq!(sanitize_name("lpt³"), "lpt³_");
        assert_eq!(sanitize_name("CONOUT$.txt"), "CONOUT$_.txt");
        assert_eq!(sanitize_name("COM10"), "COM10");
        assert_eq!(sanitize_name("Comfort.pdf"), "Comfort.pdf");
        assert_eq!(sanitize_name("..."), "_");
        assert_eq!(sanitize_name("Projekt 1 "), "Projekt 1");
        assert_eq!(
            sanitize_name("Köpmannen - fasad.ifc"),
            "Köpmannen - fasad.ifc"
        );
    }

    #[test]
    fn names_compare_like_windows() {
        assert!(same_name("Ritningar Över", "ritningar över"));
        assert!(same_name("ÅÄÖ.pdf", "åäö.PDF"));
        assert!(!same_name("a.pdf", "a.pdf "));
        assert!(
            !same_name("straße", "STRASSE"),
            "no multi-char folding, like NTFS"
        );
        assert!(matches_local("a:b.txt", "A_B.TXT"));
        assert!(matches_local("CON.dwg", "con_.dwg"));
        assert!(!matches_local("plan.dwg", "plan.dwg.bak"));
        for (name, local) in [
            ("a:b.txt", "A_B.TXT"),
            ("Över", "över"),
            ("CON.dwg", "con_.dwg"),
        ] {
            assert_eq!(name_key(name), fold_key(local), "{name} / {local}");
        }
        assert_ne!(name_key("plan.dwg"), fold_key("plan.dwg.bak"));
    }

    #[test]
    fn paths_compare_like_windows() {
        assert!(same_path(
            Path::new(r"C:\Synka\Hub\Över\a.txt"),
            Path::new(r"c:\Synka\hub\över\A.TXT")
        ));
        assert!(!same_path(
            Path::new(r"C:\Synka\Hub\a.txt"),
            Path::new(r"C:\Synka\Hub\b\a.txt")
        ));
    }

    #[test]
    fn conflict_names() {
        assert_eq!(
            conflict_name("plan.dwg", "2026-10-16 1200"),
            "plan (konflikt 2026-10-16 1200).dwg"
        );
        assert_eq!(
            conflict_name("Kopmannen.rvt", "2026-10-16 1405"),
            "Kopmannen (konflikt 2026-10-16 1405).rvt"
        );
        assert_eq!(
            conflict_name("Köpmannen - fasad.ifc", "t"),
            "Köpmannen - fasad (konflikt t).ifc"
        );
        assert_eq!(conflict_name("README", "t"), "README (konflikt t)");
        assert_eq!(conflict_name(".gitignore", "t"), ".gitignore (konflikt t)");
    }

    #[test]
    fn conflict_path_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("synka-naming-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let orig = dir.join("a.txt");
        let first = conflict_path(&orig, "T");
        assert_eq!(first.file_name().unwrap(), "a (konflikt T).txt");
        std::fs::write(&first, b"1").unwrap();
        let second = conflict_path(&orig, "T");
        assert_eq!(second.file_name().unwrap(), "a (konflikt T 2).txt");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

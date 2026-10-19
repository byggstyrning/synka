//! Diagnostics export for support: one zip with the recent log files and a sanitized summary of
//! the engine state (counts, job kinds and their last errors, settings). Never tokens, never the
//! user's name or e-mail, never file contents. Written to the Desktop and shown in Explorer.

use crate::app::Ctx;
use crate::model::LocalState;
use anyhow::{Context, Result};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Log files are kept for 7 days; all of them go in.
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

pub fn export(ctx: &Ctx) -> Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H%M%S");
    let dir = desktop_dir().unwrap_or_else(|| ctx.paths.state_dir.clone());
    let path = dir.join(format!("Synka-diagnostik {stamp}.zip"));
    let mut zip = ZipWriter::new(
        std::fs::File::create(&path).with_context(|| format!("skapa {}", path.display()))?,
    );
    zip.add(
        "summary.json",
        serde_json::to_string_pretty(&summary(ctx))?.as_bytes(),
    )?;

    let mut logs: Vec<PathBuf> = std::fs::read_dir(&ctx.paths.log_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    logs.sort();
    let mut budget = MAX_LOG_BYTES;
    for log in logs.iter().rev() {
        let Ok(data) = std::fs::read(log) else {
            continue;
        };
        if data.len() as u64 > budget {
            break;
        }
        budget -= data.len() as u64;
        let name = log
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        zip.add(&format!("logs/{name}"), &data)?;
    }
    zip.finish()?;
    Ok(path)
}

fn summary(ctx: &Ctx) -> serde_json::Value {
    let s = ctx.cfg_now();
    let projects: Vec<serde_json::Value> = ctx
        .store
        .projects()
        .unwrap_or_default()
        .iter()
        .map(|p| {
            let counts = ctx.store.state_counts(p.pid).unwrap_or_default();
            json!({
                "pid": p.pid,
                "enabled": p.enabled,
                "change_source": format!("{:?}", p.change_source),
                "last_sync_ms": p.last_sync_ms,
                "items": counts.iter().map(|(st, n)| (format!("{:?}", LocalState::from_u8(*st)), json!(n))).collect::<serde_json::Map<_, _>>(),
            })
        })
        .collect();
    let jobs: Vec<serde_json::Value> = ctx
        .store
        .job_summary()
        .unwrap_or_default()
        .into_iter()
        .map(|(kind, dead, attempts, err)| json!({ "kind": kind, "dead": dead, "attempts": attempts, "last_error": err }))
        .collect();
    json!({
        "engine_version": env!("CARGO_PKG_VERSION"),
        "packaged": ctx.paths.packaged,
        "os": os_version(),
        "signed_in": ctx.auth.is_signed_in(),
        "settings": settings_summary(&s),
        "projects": projects,
        "jobs": jobs,
    })
}

fn settings_summary(config: &crate::config::Config) -> serde_json::Value {
    json!({
        "propagate_deletes": config.propagate_deletes,
        "max_downloads": config.max_downloads,
        "max_uploads": config.max_uploads,
        "poll_active_secs": config.poll_active_secs,
        "log_level": config.log_level,
        "auto_update": config.auto_update,
        "update_channel_configured": !config.update_channel.trim().is_empty(),
        "desktop_sync_feed": config.desktop_sync_feed,
        "custom_sync_root": config.sync_root.is_some(),
        "client_id_configured": config.client_id_configured(),
        "client_id_source": config.client_id_source(),
    })
}

fn os_version() -> String {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buf = [0u16; 64];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            windows::core::w!("CurrentBuild"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    format!("build {}", String::from_utf16_lossy(&buf[..n]))
}

fn desktop_dir() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
        s.map(PathBuf::from).filter(|d| d.is_dir())
    }
}

/// Show the file selected in Explorer.
pub fn reveal(path: &Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

// ---- minimal zip writer (stored entries, no compression) ----------------------------------

struct ZipWriter<W: Write> {
    out: W,
    offset: u32,
    central: Vec<u8>,
    entries: u16,
    /// MS-DOS time and date of every entry (now).
    stamp: [u8; 4],
}

impl<W: Write> ZipWriter<W> {
    fn new(out: W) -> Self {
        use chrono::{Datelike, Timelike};
        let t = chrono::Local::now();
        let time = (t.hour() << 11 | t.minute() << 5 | (t.second() / 2)) as u16;
        let date =
            (((t.year() - 1980).clamp(0, 127) as u32) << 9 | t.month() << 5 | t.day()) as u16;
        let mut stamp = [0u8; 4];
        stamp[..2].copy_from_slice(&time.to_le_bytes());
        stamp[2..].copy_from_slice(&date.to_le_bytes());
        ZipWriter {
            out,
            offset: 0,
            central: Vec::new(),
            entries: 0,
            stamp,
        }
    }

    fn add(&mut self, name: &str, data: &[u8]) -> Result<()> {
        let crc = crc32(data);
        let size = u32::try_from(data.len()).context("entry too large")?;
        let name = name.as_bytes();
        // General purpose flag bit 11: names are UTF-8.
        let mut local = Vec::with_capacity(30 + name.len());
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&[20, 0, 0, 0x08, 0, 0]); // version needed, flags (UTF-8), stored
        local.extend_from_slice(&self.stamp);
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name);
        self.out.write_all(&local)?;
        self.out.write_all(data)?;

        self.central
            .extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        self.central
            .extend_from_slice(&[20, 0, 20, 0, 0, 0x08, 0, 0]); // made by, needed, flags, stored
        self.central.extend_from_slice(&self.stamp);
        self.central.extend_from_slice(&crc.to_le_bytes());
        self.central.extend_from_slice(&size.to_le_bytes());
        self.central.extend_from_slice(&size.to_le_bytes());
        self.central
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.central.extend_from_slice(&[0; 8]); // extra, comment, disk, internal attrs
        self.central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        self.central.extend_from_slice(&self.offset.to_le_bytes());
        self.central.extend_from_slice(name);

        self.offset = self
            .offset
            .checked_add((local.len() + data.len()) as u32)
            .context("zip larger than 4 GB")?;
        self.entries += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<W> {
        self.out.write_all(&self.central)?;
        let mut end = Vec::with_capacity(22);
        end.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        end.extend_from_slice(&[0; 4]); // disk numbers
        end.extend_from_slice(&self.entries.to_le_bytes());
        end.extend_from_slice(&self.entries.to_le_bytes());
        end.extend_from_slice(&(self.central.len() as u32).to_le_bytes());
        end.extend_from_slice(&self.offset.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes());
        self.out.write_all(&end)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_diagnostics_report_availability_without_exporting_ids() {
        let default = crate::config::Config::default();
        let summary = settings_summary(&default);
        assert_eq!(summary["client_id_configured"], true);
        assert_eq!(summary["client_id_source"], default.client_id_source());
        assert!(summary.get("client_id").is_none());
        assert!(summary.get("aps_client_id").is_none());

        let override_config = crate::config::Config {
            aps_client_id: Some(" diagnostic-test-override ".into()),
            ..crate::config::Config::default()
        };
        let summary = settings_summary(&override_config);
        assert_eq!(summary["client_id_configured"], true);
        assert_eq!(summary["client_id_source"], "settings");
        assert!(!summary.to_string().contains("diagnostic-test-override"));
    }

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn zip_layout_is_well_formed() {
        let mut z = ZipWriter::new(Vec::new());
        z.add("summary.json", b"{}").unwrap();
        z.add("logs/å.log", b"rad 1\n").unwrap();
        let bytes = z.finish().unwrap();
        // End of central directory: 2 entries, central dir right after the local entries.
        let end = &bytes[bytes.len() - 22..];
        assert_eq!(&end[..4], &0x0605_4b50u32.to_le_bytes());
        assert_eq!(u16::from_le_bytes([end[8], end[9]]), 2);
        let cd_size = u32::from_le_bytes(end[12..16].try_into().unwrap()) as usize;
        let cd_off = u32::from_le_bytes(end[16..20].try_into().unwrap()) as usize;
        assert_eq!(cd_off + cd_size + 22, bytes.len());
        assert_eq!(&bytes[cd_off..cd_off + 4], &0x0201_4b50u32.to_le_bytes());
        assert_eq!(&bytes[..4], &0x0403_4b50u32.to_le_bytes());
    }
}

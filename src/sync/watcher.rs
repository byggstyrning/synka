//! ONE ReadDirectoryChangesW (recursive) on the sync root, on a dedicated std thread with a
//! 64 KiB buffer (FILE_NOTIFY_CHANGE_FILE_NAME | DIR_NAME | SIZE | LAST_WRITE | ATTRIBUTES).
//! Overflow (0 bytes returned) -> emit LocalChange::Rescan.
//! Events are coalesced per path and emitted after 3 s of quiet for that path (debounce),
//! as a batch on a tokio mpsc. Paths currently suppressed (our own writes) are delayed until
//! the suppression expires, so a user save during that window is still inspected.
//! Ignore rules (sync::ignore) are applied here so ignored files never leave the thread.
//! Pin/unpin is noticed as an attribute change: the engine checks placeholder_info for
//! PINNED/UNPINNED and hydrates/dehydrates.
//!
//! The directory handle is opened FILE_FLAG_OVERLAPPED so the wait can time out every second
//! (to flush debounced paths) and so that WatchHandle::drop can CancelIoEx the pending read.

use crate::sync::ignore;
use std::collections::HashMap;
use std::ffi::{c_void, OsString};
use std::iter::once;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileAttributesW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

#[derive(Debug, Clone)]
pub enum LocalChange {
    /// Created or modified (content or attributes) — engine inspects the path.
    Changed(PathBuf),
    /// Buffer overflow or watcher restart: engine does a full local scan of populated dirs.
    Rescan,
}

const BUF_BYTES: usize = 64 * 1024;
const DEBOUNCE: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(1000);
const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME.0
    | FILE_NOTIFY_CHANGE_DIR_NAME.0
    | FILE_NOTIFY_CHANGE_SIZE.0
    | FILE_NOTIFY_CHANGE_LAST_WRITE.0
    | FILE_NOTIFY_CHANGE_ATTRIBUTES.0;
const INVALID_ATTRS: u32 = u32::MAX;

/// Paths our own writes are about to touch, with the instant the suppression ends.
static SUPPRESS: Mutex<Vec<(PathBuf, Instant)>> = Mutex::new(Vec::new());

/// Start watching. Returns a handle that stops the thread on drop.
pub fn start(
    root: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<Vec<LocalChange>>,
) -> anyhow::Result<WatchHandle> {
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(once(0)).collect();
    let hdir = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("open {} for watching: {e}", root.display()))?;

    let stop = Arc::new(AtomicBool::new(false));
    let raw = hdir.0 as isize;
    let slot = Arc::new(AtomicIsize::new(raw));
    let root_buf = root.to_path_buf();
    let (stop_t, slot_t) = (stop.clone(), slot.clone());
    let thread = std::thread::Builder::new()
        .name("synka-watcher".into())
        .spawn(move || run(HANDLE(raw as *mut c_void), &root_buf, &tx, &stop_t, &slot_t))?;

    Ok(WatchHandle {
        stop,
        slot,
        thread: Some(thread),
    })
}

pub struct WatchHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Directory handle as isize (0 once the thread has closed it).
    slot: Arc<AtomicIsize>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let h = self.slot.load(Ordering::SeqCst);
        if h != 0 {
            // Completes the pending ReadDirectoryChangesW with ERROR_OPERATION_ABORTED.
            unsafe {
                let _ = CancelIoEx(HANDLE(h as *mut c_void), None);
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Ignore events for `path` (and children) for `dur` — used around our own disk writes.
pub fn suppress(path: &Path, dur: Duration) {
    let now = Instant::now();
    let mut v = SUPPRESS.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|(_, until)| *until > now);
    v.push((path.to_path_buf(), now + dur));
}

pub(crate) fn is_suppressed(path: &Path) -> bool {
    let now = Instant::now();
    let v = SUPPRESS.lock().unwrap_or_else(|e| e.into_inner());
    v.iter()
        .any(|(p, until)| *until > now && starts_with_ci(path, p))
}

/// Component-wise, case-insensitive prefix test (Windows paths).
fn starts_with_ci(path: &Path, prefix: &Path) -> bool {
    let mut comps = path.components();
    prefix.components().all(|pc| {
        comps.next().is_some_and(|ac| {
            ac.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&pc.as_os_str().to_string_lossy())
        })
    })
}

fn file_attributes(path: &Path) -> u32 {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    let a = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    if a == INVALID_ATTRS {
        0 // vanished (or inaccessible): treat as a plain file
    } else {
        a
    }
}

fn run(
    hdir: HANDLE,
    root: &Path,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<LocalChange>>,
    stop: &AtomicBool,
    slot: &AtomicIsize,
) {
    let event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("watcher: CreateEventW failed: {e}");
            close(hdir, slot);
            return;
        }
    };
    // u32 storage keeps the buffer DWORD-aligned as ReadDirectoryChangesW requires.
    let mut buf: Vec<u32> = vec![0; BUF_BYTES / 4];
    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let pending_io = HRESULT::from_win32(ERROR_IO_PENDING.0);
    let aborted = HRESULT::from_win32(ERROR_OPERATION_ABORTED.0);

    'outer: while !stop.load(Ordering::SeqCst) {
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let issued = unsafe {
            ReadDirectoryChangesW(
                hdir,
                buf.as_mut_ptr() as *mut c_void,
                BUF_BYTES as u32,
                true,
                FILE_NOTIFY_CHANGE(FILTER),
                None,
                Some(&mut ov),
                None,
            )
        };
        if let Err(e) = issued {
            if e.code() != pending_io {
                tracing::warn!("watcher: ReadDirectoryChangesW failed: {e}");
                let _ = tx.send(vec![LocalChange::Rescan]);
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }

        // Wait for completion, flushing debounced paths on every tick.
        loop {
            let w = unsafe { WaitForSingleObject(event, TICK.as_millis() as u32) };
            if w == WAIT_OBJECT_0 {
                break;
            }
            if w != WAIT_TIMEOUT {
                tracing::warn!("watcher: wait failed ({})", w.0);
                break 'outer;
            }
            if stop.load(Ordering::SeqCst) {
                unsafe {
                    let _ = CancelIoEx(hdir, Some(&ov));
                }
            }
            if !flush_due(&mut pending, tx, Instant::now()) {
                break 'outer;
            }
        }

        let mut bytes = 0u32;
        match unsafe { GetOverlappedResult(hdir, &ov, &mut bytes, false) } {
            Ok(()) => {
                if bytes == 0 {
                    // Buffer overflow: the engine must rescan.
                    let _ = tx.send(vec![LocalChange::Rescan]);
                } else {
                    parse(&buf, bytes, root, &mut pending);
                }
            }
            Err(e) if e.code() == aborted => break 'outer,
            Err(e) => {
                tracing::warn!("watcher: read failed: {e}");
                let _ = tx.send(vec![LocalChange::Rescan]);
            }
        }
        if !flush_due(&mut pending, tx, Instant::now()) {
            break 'outer;
        }
    }

    unsafe {
        let _ = CloseHandle(event);
    }
    close(hdir, slot);
}

fn close(hdir: HANDLE, slot: &AtomicIsize) {
    slot.store(0, Ordering::SeqCst);
    unsafe {
        let _ = CloseHandle(hdir);
    }
}

/// Walk the FILE_NOTIFY_INFORMATION records in the first `bytes` of the buffer.
fn parse(buf: &[u32], bytes: u32, root: &Path, pending: &mut HashMap<PathBuf, Instant>) {
    let data: &[u8] =
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, bytes as usize) };
    let now = Instant::now();
    // NextEntryOffset (4) + Action (4) + FileNameLength (4); FileName follows at offset 12.
    let header = 12usize;
    let rd = |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let mut off = 0usize;
    loop {
        if off + header > data.len() {
            break;
        }
        let next = rd(off) as usize;
        let name_len = rd(off + 8) as usize;
        let name_start = off + header;
        let name_end = (name_start + name_len).min(data.len());
        let (pairs, _) = data[name_start..name_end].as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
        if !units.is_empty() {
            let rel = PathBuf::from(OsString::from_wide(&units));
            record(root, rel, now, pending);
        }
        if next == 0 {
            break;
        }
        off += next;
    }
}

fn record(root: &Path, rel: PathBuf, now: Instant, pending: &mut HashMap<PathBuf, Instant>) {
    let full = root.join(&rel);
    if ignore::ignored(&rel, file_attributes(&full)) {
        return;
    }
    pending.insert(full, now); // coalesce: a later event restarts the quiet period
}

/// Emit every pending path that has been quiet for `DEBOUNCE`. Returns false when the
/// receiver is gone (the watcher should stop).
fn flush_due(
    pending: &mut HashMap<PathBuf, Instant>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<LocalChange>>,
    now: Instant,
) -> bool {
    if pending.is_empty() {
        return true;
    }
    let due: Vec<PathBuf> = pending
        .iter()
        .filter(|(p, t)| now.duration_since(**t) >= DEBOUNCE && !is_suppressed(p))
        .map(|(p, _)| p.clone())
        .collect();
    if due.is_empty() {
        return true;
    }
    for p in &due {
        pending.remove(p);
    }
    let batch: Vec<LocalChange> = due.into_iter().map(LocalChange::Changed).collect();
    batch.is_empty() || tx.send(batch).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppress_matches_by_component() {
        suppress(Path::new(r"C:\tmp\wd_sup_a\dir"), Duration::from_secs(5));
        assert!(is_suppressed(Path::new(r"C:\tmp\wd_sup_a\dir\f.txt")));
        assert!(is_suppressed(Path::new(r"c:\TMP\wd_sup_a\DIR")));
        assert!(!is_suppressed(Path::new(r"C:\tmp\wd_sup_a\dir2\f.txt")));
        assert!(!is_suppressed(Path::new(r"C:\tmp\wd_sup_b\f.txt")));
    }

    #[test]
    fn suppressed_change_is_reported_after_the_window() {
        let root = Path::new(r"C:\tmp\synka-watcher-suppression-regression");
        let rel = PathBuf::from("edited.bin");
        let path = root.join(&rel);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut pending = HashMap::new();
        suppress(&path, Duration::from_millis(80));
        record(root, rel, Instant::now() - DEBOUNCE, &mut pending);
        assert!(
            pending.contains_key(&path),
            "a suppressed write must stay pending"
        );
        assert!(flush_due(&mut pending, &tx, Instant::now()));
        assert!(
            pending.contains_key(&path),
            "the write must wait for suppression to expire"
        );
        assert!(rx.try_recv().is_err());

        std::thread::sleep(Duration::from_millis(100));
        assert!(flush_due(&mut pending, &tx, Instant::now()));
        assert!(pending.is_empty());
        assert!(
            matches!(rx.try_recv(), Ok(batch) if matches!(&batch[..], [LocalChange::Changed(p)] if p == &path))
        );
    }

    #[tokio::test]
    #[ignore = "touches the real file system and waits for the 3 s debounce"]
    async fn reports_new_file_within_6s() {
        let root = std::env::temp_dir().join(format!("synka-watch-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = start(&root, tx).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await; // let the read be armed
        std::fs::write(root.join("hello.txt"), b"hi").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(6), async {
            while let Some(batch) = rx.recv().await {
                if batch
                    .iter()
                    .any(|c| matches!(c, LocalChange::Changed(p) if p.ends_with("hello.txt")))
                {
                    return true;
                }
            }
            false
        })
        .await;
        drop(handle);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            matches!(got, Ok(true)),
            "no Changed event for hello.txt: {got:?}"
        );
    }
}

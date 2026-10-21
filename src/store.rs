//! SQLite state: one file, WAL, small page cache. All writes for one delta happen in one
//! transaction together with the cursor. An empty delta must not write anything.
//!
//! Schema (create on open, `PRAGMA user_version` = 4):
//!   PRAGMA journal_mode=WAL; synchronous=NORMAL; cache_size=-2048; foreign_keys=OFF;
//!   kv(k TEXT PRIMARY KEY, v TEXT NOT NULL)
//!   project(pid INTEGER PRIMARY KEY, hub_id TEXT NOT NULL, hub_name TEXT NOT NULL,
//!           project_id TEXT NOT NULL UNIQUE, name TEXT NOT NULL, root_folder_urn TEXT NOT NULL,
//!           urn_ns TEXT NOT NULL, oss_scopes TEXT, change_source INTEGER NOT NULL DEFAULT 0,
//!           cursor TEXT, enabled INTEGER NOT NULL DEFAULT 1, last_sync_ms INTEGER)
//!   item(pid INTEGER NOT NULL, id TEXT NOT NULL, parent TEXT, name TEXT NOT NULL,
//!        kind INTEGER NOT NULL, rver INTEGER, bver INTEGER, size INTEGER NOT NULL DEFAULT 0,
//!        ctime_ms INTEGER NOT NULL DEFAULT 0, mtime_ms INTEGER NOT NULL DEFAULT 0,
//!        storage_urn TEXT, version_urn TEXT, modby TEXT, locked_by TEXT,
//!        lstate INTEGER NOT NULL DEFAULT 0, populated INTEGER NOT NULL DEFAULT 0,
//!        PRIMARY KEY(pid, id)) WITHOUT ROWID
//!   INDEX item_by_parent ON item(pid, parent)
//!   v2: item.lkey TEXT = `naming::name_key(name)` (the name as Windows compares it on disk),
//!       INDEX item_by_key ON item(pid, parent, lkey) — child lookups by local name are one
//!       indexed query instead of a scan of all siblings.
//!   v3: project.tops TEXT = JSON `[{"id","name"}]`, the folders shown in the project directory
//!       (`model::TopFolder`). NULL for projects from v2, whose rows still use the old layout
//!       (the project directory was "Project Files"); the engine rebuilds those.
//!   v4: project.provider TEXT NOT NULL DEFAULT 'acc' (`model::Provider::as_str`) and
//!       project.meta TEXT (provider JSON, NULL for ACC). Existing rows are ACC.
//!   outbox(id INTEGER PRIMARY KEY, pid INTEGER NOT NULL, kind TEXT NOT NULL, item TEXT,
//!          payload TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_ms INTEGER NOT NULL,
//!          created_ms INTEGER NOT NULL, dead INTEGER NOT NULL DEFAULT 0, last_error TEXT)
//!          dead: 0 = live, 1 = failed/retryable by user, 2 = held mass delete,
//!                3 = unsupported file type (local only, never retried)

use crate::model::*;
use crate::sync::naming::name_key;
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, Params, Row};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub struct Store {
    conn: Mutex<rusqlite::Connection>,
}

/// Result of applying a delta: (before, after) per touched item. `before` None = created,
/// `after` None = deleted. Used by the engine to update placeholders on disk.
#[derive(Debug, Default)]
pub struct Applied {
    pub changes: Vec<(Option<Item>, Option<Item>)>,
}

/// A queued side effect (upload, remote rename, remote delete...).
#[derive(Debug, Clone)]
pub struct OutboxJob {
    pub id: i64,
    pub pid: Pid,
    /// "upload", "mkdir", "rename", "move", "delete" ...
    pub kind: String,
    pub item: Option<String>,
    /// JSON payload, shape defined by sync::upload.
    pub payload: String,
    pub attempts: i64,
}

/// (kind, dead, attempts, last error) of one outbox job, for diagnostics.
pub type JobSummary = (String, bool, i64, Option<String>);

/// Upper bound on parent hops in `path_of` (cycle guard).
const MAX_PATH_STEPS: usize = 512;

const PRAGMAS: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA cache_size = -2048;
    PRAGMA foreign_keys = OFF;";

const SCHEMA_V1: &str = "
    CREATE TABLE IF NOT EXISTS kv (
        k TEXT PRIMARY KEY,
        v TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS project (
        pid INTEGER PRIMARY KEY,
        hub_id TEXT NOT NULL,
        hub_name TEXT NOT NULL,
        project_id TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        root_folder_urn TEXT NOT NULL,
        urn_ns TEXT NOT NULL,
        oss_scopes TEXT,
        change_source INTEGER NOT NULL DEFAULT 0,
        cursor TEXT,
        enabled INTEGER NOT NULL DEFAULT 1,
        last_sync_ms INTEGER
    );
    CREATE TABLE IF NOT EXISTS item (
        pid INTEGER NOT NULL,
        id TEXT NOT NULL,
        parent TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        rver INTEGER,
        bver INTEGER,
        size INTEGER NOT NULL DEFAULT 0,
        ctime_ms INTEGER NOT NULL DEFAULT 0,
        mtime_ms INTEGER NOT NULL DEFAULT 0,
        storage_urn TEXT,
        version_urn TEXT,
        modby TEXT,
        locked_by TEXT,
        lstate INTEGER NOT NULL DEFAULT 0,
        populated INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (pid, id)
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS item_by_parent ON item (pid, parent);
    CREATE TABLE IF NOT EXISTS outbox (
        id INTEGER PRIMARY KEY,
        pid INTEGER NOT NULL,
        kind TEXT NOT NULL,
        item TEXT,
        payload TEXT NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0,
        next_ms INTEGER NOT NULL,
        created_ms INTEGER NOT NULL,
        dead INTEGER NOT NULL DEFAULT 0,
        last_error TEXT
    );";

/// Column list shared by every item SELECT; order matches `item_from_row`.
const ITEM_COLS: &str = "pid, id, parent, name, kind, rver, bver, size, ctime_ms, mtime_ms, \
                         storage_urn, version_urn, modby, locked_by, lstate, populated";

/// Column list shared by every project SELECT; order matches `project_from_row`.
const PROJECT_COLS: &str = "pid, hub_id, hub_name, project_id, name, root_folder_urn, urn_ns, \
                            oss_scopes, change_source, cursor, enabled, last_sync_ms, tops, \
                            provider, meta";

fn u8_of(v: i64) -> u8 {
    v.clamp(0, 255) as u8
}

fn item_from_row(r: &Row<'_>) -> rusqlite::Result<Item> {
    let kind_raw: i64 = r.get(4)?;
    let kind = u8::try_from(kind_raw)
        .ok()
        .and_then(Kind::from_u8)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(4, kind_raw))?;
    Ok(Item {
        pid: r.get(0)?,
        id: r.get(1)?,
        parent: r.get(2)?,
        name: r.get(3)?,
        kind,
        remote_version: r.get(5)?,
        base_version: r.get(6)?,
        size: r.get(7)?,
        ctime_ms: r.get(8)?,
        mtime_ms: r.get(9)?,
        storage_urn: r.get(10)?,
        version_urn: r.get(11)?,
        modified_by: r.get(12)?,
        locked_by: r.get(13)?,
        local_state: LocalState::from_u8(u8_of(r.get::<_, i64>(14)?)),
        populated: r.get::<_, i64>(15)? != 0,
    })
}

fn project_from_row(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        pid: r.get(0)?,
        hub_id: r.get(1)?,
        hub_name: r.get(2)?,
        project_id: r.get(3)?,
        name: r.get(4)?,
        root_folder_urn: r.get(5)?,
        urn_ns: r.get(6)?,
        oss_scopes: r.get(7)?,
        change_source: ChangeSourceKind::from_u8(u8_of(r.get::<_, i64>(8)?)),
        cursor: r.get(9)?,
        enabled: r.get::<_, i64>(10)? != 0,
        last_sync_ms: r.get(11)?,
        tops: r
            .get::<_, Option<String>>(12)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        // An unknown name (a newer build wrote it) reads as ACC rather than failing the row.
        provider: r
            .get::<_, Option<String>>(13)?
            .as_deref()
            .and_then(Provider::parse)
            .unwrap_or_default(),
        meta: r.get(14)?,
    })
}

fn tops_json(tops: &[TopFolder]) -> Option<String> {
    (!tops.is_empty()).then(|| serde_json::to_string(tops).unwrap_or_default())
}

/// Run a query returning many rows (statement cached).
fn collect<T, P: Params>(
    c: &Connection,
    sql: &str,
    p: P,
    f: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut st = c.prepare_cached(sql)?;
    let rows = st.query_map(p, f)?;
    let out = rows.collect::<rusqlite::Result<Vec<T>>>()?;
    Ok(out)
}

/// Run a query returning at most one row (statement cached).
fn one<T, P: Params>(
    c: &Connection,
    sql: &str,
    p: P,
    f: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Option<T>> {
    let mut st = c.prepare_cached(sql)?;
    let out = st.query_row(p, f).optional()?;
    Ok(out)
}

fn get_item(c: &Connection, pid: Pid, id: &str) -> Result<Option<Item>> {
    one(
        c,
        &format!("SELECT {} FROM item WHERE pid = ?1 AND id = ?2", ITEM_COLS),
        params![pid, id],
        item_from_row,
    )
}

fn child_rows(c: &Connection, pid: Pid, parent: &str) -> Result<Vec<Item>> {
    collect(
        c,
        &format!(
            "SELECT {} FROM item WHERE pid = ?1 AND parent = ?2 ORDER BY name COLLATE NOCASE, id",
            ITEM_COLS
        ),
        params![pid, parent],
        item_from_row,
    )
}

/// Insert or replace a full item row.
fn write_item(c: &Connection, it: &Item) -> Result<()> {
    c.prepare_cached(
        "INSERT OR REPLACE INTO item (pid, id, parent, name, kind, rver, bver, size, ctime_ms, \
         mtime_ms, storage_urn, version_urn, modby, locked_by, lstate, populated, lkey) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
    )?
    .execute(params![
        it.pid,
        it.id,
        it.parent,
        it.name,
        it.kind as u8 as i64,
        it.remote_version,
        it.base_version,
        it.size,
        it.ctime_ms,
        it.mtime_ms,
        it.storage_urn,
        it.version_urn,
        it.modified_by,
        it.locked_by,
        it.local_state as u8 as i64,
        it.populated as i64,
        name_key(&it.name),
    ])?;
    Ok(())
}

fn delete_row(c: &Connection, pid: Pid, id: &str) -> Result<()> {
    c.prepare_cached("DELETE FROM item WHERE pid = ?1 AND id = ?2")?
        .execute(params![pid, id])?;
    Ok(())
}

/// The item `root_id` plus all its descendants, root first. Empty if the root is unknown.
/// Iterative, with a visited set so corrupt parent cycles cannot loop forever.
fn subtree(c: &Connection, pid: Pid, root_id: &str) -> Result<Vec<Item>> {
    let Some(root) = get_item(c, pid, root_id)? else {
        return Ok(Vec::new());
    };
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(root.id.clone());
    let mut stack: Vec<String> = vec![root.id.clone()];
    let mut out = vec![root];
    while let Some(parent) = stack.pop() {
        for ch in child_rows(c, pid, &parent)? {
            if seen.insert(ch.id.clone()) {
                stack.push(ch.id.clone());
                out.push(ch);
            }
        }
    }
    Ok(out)
}

/// Equality of the fields that come from the remote side (ignores local-only fields).
fn same_remote(a: &Item, b: &Item) -> bool {
    a.parent == b.parent
        && a.name == b.name
        && a.kind == b.kind
        && a.remote_version == b.remote_version
        && a.size == b.size
        && a.ctime_ms == b.ctime_ms
        && a.mtime_ms == b.mtime_ms
        && a.storage_urn == b.storage_urn
        && a.version_urn == b.version_urn
        && a.modified_by == b.modified_by
        && a.locked_by == b.locked_by
}

fn migrate(conn: &Connection) -> Result<()> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v == 0 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        tx.execute_batch("PRAGMA user_version = 1;")?;
        tx.commit()?;
    }
    match v {
        0 | 1 => {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("ALTER TABLE item ADD COLUMN lkey TEXT;")?;
            let rows: Vec<(Pid, String, String)> = {
                let mut st = tx.prepare("SELECT pid, id, name FROM item")?;
                let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                it.collect::<rusqlite::Result<_>>()?
            };
            {
                let mut up = tx.prepare("UPDATE item SET lkey = ?3 WHERE pid = ?1 AND id = ?2")?;
                for (pid, id, name) in rows {
                    up.execute(params![pid, id, name_key(&name)])?;
                }
            }
            tx.execute_batch(
                "CREATE INDEX IF NOT EXISTS item_by_key ON item (pid, parent, lkey);
                 PRAGMA user_version = 2;",
            )?;
            tx.commit()?;
        }
        2..=4 => {}
        other => bail!("synka.db schema version {other} is newer than this build supports"),
    }
    if v < 3 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch("ALTER TABLE project ADD COLUMN tops TEXT; PRAGMA user_version = 3;")?;
        tx.commit()?;
    }
    if v < 4 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "ALTER TABLE project ADD COLUMN provider TEXT NOT NULL DEFAULT 'acc';
             ALTER TABLE project ADD COLUMN meta TEXT;
             PRAGMA user_version = 4;",
        )?;
        tx.commit()?;
    }
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(PRAGMAS)?;
        migrate(&conn)?;
        // Earlier builds counted ACC's permanent file-type rejection as a retryable
        // failure. Preserve the local file and diagnostic while removing that retry.
        conn.execute(
            "UPDATE outbox SET dead = 3 WHERE dead = 1 AND kind = 'upload' \
             AND last_error LIKE '%File with extension % is not allowed to upload.%'",
            [],
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))
    }

    pub fn kv_get(&self, k: &str) -> Result<Option<String>> {
        let c = self.conn()?;
        one(&c, "SELECT v FROM kv WHERE k = ?1", params![k], |r| {
            r.get(0)
        })
    }

    /// Every (key, value) whose key starts with `prefix`.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let c = self.conn()?;
        let all = collect(&c, "SELECT k, v FROM kv ORDER BY k", [], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(all
            .into_iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .collect())
    }

    pub fn kv_set(&self, k: &str, v: Option<&str>) -> Result<()> {
        let c = self.conn()?;
        match v {
            Some(v) => {
                c.prepare_cached(
                    "INSERT INTO kv (k, v) VALUES (?1, ?2) \
                     ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                )?
                .execute(params![k, v])?;
            }
            None => {
                c.prepare_cached("DELETE FROM kv WHERE k = ?1")?
                    .execute(params![k])?;
            }
        }
        Ok(())
    }

    // ---- projects
    pub fn projects(&self) -> Result<Vec<Project>> {
        let c = self.conn()?;
        collect(
            &c,
            &format!("SELECT {} FROM project ORDER BY pid", PROJECT_COLS),
            [],
            project_from_row,
        )
    }

    pub fn project(&self, pid: Pid) -> Result<Option<Project>> {
        let c = self.conn()?;
        one(
            &c,
            &format!("SELECT {} FROM project WHERE pid = ?1", PROJECT_COLS),
            params![pid],
            project_from_row,
        )
    }

    /// Insert or update by `project_id`; returns pid. Ignores `p.pid` on insert.
    /// On update only the descriptive fields (hub, name, root, namespace, scopes, top folders,
    /// provider, meta) are overwritten; `change_source`, `cursor`, `enabled` and `last_sync_ms`
    /// are owned by their dedicated setters / `apply_delta` and are kept.
    pub fn upsert_project(&self, p: &Project) -> Result<Pid> {
        let c = self.conn()?;
        c.prepare_cached(
            "INSERT INTO project (hub_id, hub_name, project_id, name, root_folder_urn, urn_ns, \
             oss_scopes, change_source, cursor, enabled, last_sync_ms, tops, provider, meta) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
             ON CONFLICT(project_id) DO UPDATE SET \
               hub_id = excluded.hub_id, \
               hub_name = excluded.hub_name, \
               name = excluded.name, \
               root_folder_urn = excluded.root_folder_urn, \
               urn_ns = excluded.urn_ns, \
               oss_scopes = excluded.oss_scopes, \
               tops = excluded.tops, \
               provider = excluded.provider, \
               meta = excluded.meta",
        )?
        .execute(params![
            p.hub_id,
            p.hub_name,
            p.project_id,
            p.name,
            p.root_folder_urn,
            p.urn_ns,
            p.oss_scopes,
            p.change_source as u8 as i64,
            p.cursor,
            p.enabled as i64,
            p.last_sync_ms,
            tops_json(&p.tops),
            p.provider.as_str(),
            p.meta,
        ])?;
        let pid: Pid = c.query_row(
            "SELECT pid FROM project WHERE project_id = ?1",
            params![p.project_id],
            |r| r.get(0),
        )?;
        Ok(pid)
    }

    pub fn set_project_enabled(&self, pid: Pid, enabled: bool) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE project SET enabled = ?2 WHERE pid = ?1")?
            .execute(params![pid, enabled as i64])?;
        Ok(())
    }

    pub fn set_tops(&self, pid: Pid, tops: &[TopFolder]) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE project SET tops = ?2 WHERE pid = ?1")?
            .execute(params![pid, tops_json(tops)])?;
        Ok(())
    }

    /// True when the project's rows use the layout from before top folders: its root row is
    /// an ACC folder (the project directory was "Project Files").
    pub fn old_layout(&self, pid: Pid) -> Result<bool> {
        let c = self.conn()?;
        let ids = collect(
            &c,
            "SELECT id FROM item WHERE pid = ?1 AND kind = 2",
            params![pid],
            |r| r.get::<_, String>(0),
        )?;
        Ok(ids.iter().any(|id| id != PROJECT_ROOT_ID))
    }

    /// Drop the feed cursor: the next poll enumerates the project from scratch (rows stay).
    pub fn forget_cursor(&self, pid: Pid) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE project SET cursor = NULL WHERE pid = ?1")?
            .execute(params![pid])?;
        Ok(())
    }

    pub fn set_change_source(&self, pid: Pid, k: ChangeSourceKind) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE project SET change_source = ?2 WHERE pid = ?1")?
            .execute(params![pid, k as u8 as i64])?;
        Ok(())
    }

    /// Deletes the project, its items and outbox rows.
    pub fn delete_project(&self, pid: Pid) -> Result<()> {
        let mut guard = self.conn()?;
        let tx = guard.transaction()?;
        tx.prepare_cached("DELETE FROM outbox WHERE pid = ?1")?
            .execute(params![pid])?;
        tx.prepare_cached("DELETE FROM item WHERE pid = ?1")?
            .execute(params![pid])?;
        tx.prepare_cached("DELETE FROM project WHERE pid = ?1")?
            .execute(params![pid])?;
        tx.commit()?;
        Ok(())
    }

    // ---- items
    pub fn item(&self, pid: Pid, id: &str) -> Result<Option<Item>> {
        let c = self.conn()?;
        get_item(&c, pid, id)
    }

    /// Clean, visible placeholders whose recorded backing version predates the ACC tip.
    /// Recover these on startup even if a crash occurred before the refresh marker was saved.
    pub fn stale_remote_files(&self) -> Result<Vec<(Pid, String)>> {
        let c = self.conn()?;
        collect(
            &c,
            "SELECT i.pid, i.id FROM item i
             JOIN project p ON p.pid = i.pid
             JOIN item parent ON parent.pid = i.pid AND parent.id = i.parent
             WHERE p.enabled = 1 AND parent.populated = 1
               AND i.kind = ?1 AND i.lstate = ?2
               AND i.bver IS NOT NULL AND i.bver IS NOT i.rver",
            params![Kind::File as u8 as i64, LocalState::Clean as u8 as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    pub fn children(&self, pid: Pid, parent: &str) -> Result<Vec<Item>> {
        let c = self.conn()?;
        child_rows(&c, pid, parent)
    }

    /// Children of `parent` whose name is `local` on disk (see `naming::name_key`). Usually one
    /// row; a deleted row and its replacement can share a name.
    pub fn children_named(&self, pid: Pid, parent: &str, local: &str) -> Result<Vec<Item>> {
        let c = self.conn()?;
        collect(
            &c,
            &format!(
                "SELECT {} FROM item WHERE pid = ?1 AND parent = ?2 AND lkey = ?3 ORDER BY id",
                ITEM_COLS
            ),
            params![pid, parent, crate::sync::naming::fold_key(local)],
            item_from_row,
        )
    }

    /// Apply a remote delta in ONE transaction, also writing `delta.new_cursor` (if Some)
    /// and `last_sync_ms`. For upserts: preserve existing `base_version`, `local_state`,
    /// `populated` from the DB row. Deletes are recursive (children of deleted folders go too,
    /// and are reported in `Applied`). `delta.reset` = delete all items of the project first
    /// (but report only real differences: items that vanish, appear or change).
    /// Unchanged upserts (identical remote fields) must not appear in `Applied`.
    ///
    /// Order inside the transaction: reset cleanup, then deletes, then upserts (so an id that
    /// is both deleted and upserted in one delta ends up present). A new row (no DB row yet)
    /// starts as `Clean` with `base_version` None and `populated` false.
    /// An empty delta (no upserts, deletes, reset or cursor) returns immediately with no writes.
    pub fn apply_delta(&self, pid: Pid, delta: &Delta) -> Result<Applied> {
        let mut applied = Applied::default();
        if delta.is_empty() && delta.new_cursor.is_none() {
            return Ok(applied);
        }

        let mut guard = self.conn()?;
        let tx = guard.transaction()?;

        if delta.reset {
            let keep: HashSet<&str> = delta.upserts.iter().map(|u| u.id.as_str()).collect();
            let all = collect(
                &tx,
                &format!("SELECT {} FROM item WHERE pid = ?1", ITEM_COLS),
                params![pid],
                item_from_row,
            )?;
            for row in all {
                // Local-only rows (pending creation) are not in any remote listing by
                // definition; dropping them would orphan their queued upload or mkdir. The
                // project directory is ours, not ACC's.
                if keep.contains(row.id.as_str())
                    || is_local_id(&row.id)
                    || row.kind == Kind::ProjectRoot
                {
                    continue;
                }
                delete_row(&tx, pid, &row.id)?;
                applied.changes.push((Some(row), None));
            }
        }

        for d in &delta.deletes {
            for row in subtree(&tx, pid, d)? {
                delete_row(&tx, pid, &row.id)?;
                applied.changes.push((Some(row), None));
            }
        }

        for up in &delta.upserts {
            let mut new = up.clone();
            new.pid = pid;
            match get_item(&tx, pid, &up.id)? {
                None => {
                    new.local_state = LocalState::Clean;
                    new.base_version = None;
                    new.populated = false;
                    write_item(&tx, &new)?;
                    applied.changes.push((None, Some(new)));
                }
                Some(old) => {
                    if same_remote(&old, up) {
                        continue;
                    }
                    new.local_state = old.local_state;
                    // A never-hydrated placeholder may have no base yet. If a
                    // remote tip arrives before the watcher notices a local
                    // write, preserve the old on-disk source version rather
                    // than treating the newer tip as that file's backing data.
                    new.base_version = old.base_version.or_else(|| {
                        if old.remote_version != new.remote_version {
                            old.remote_version
                        } else {
                            None
                        }
                    });
                    new.populated = old.populated;
                    write_item(&tx, &new)?;
                    applied.changes.push((Some(old), Some(new)));
                }
            }
        }

        // Past the early return, the delta had content or a cursor: always record the sync.
        tx.prepare_cached("UPDATE project SET last_sync_ms = ?2 WHERE pid = ?1")?
            .execute(params![pid, now_ms()])?;
        if let Some(cur) = &delta.new_cursor {
            tx.prepare_cached("UPDATE project SET cursor = ?2 WHERE pid = ?1")?
                .execute(params![pid, cur])?;
        }
        tx.commit()?;
        Ok(applied)
    }

    /// Insert or replace a full row.
    pub fn upsert_item(&self, it: &Item) -> Result<()> {
        let c = self.conn()?;
        write_item(&c, it)
    }

    /// Recursive delete; returns the deleted rows.
    pub fn delete_item(&self, pid: Pid, id: &str) -> Result<Vec<Item>> {
        let mut guard = self.conn()?;
        let tx = guard.transaction()?;
        let rows = subtree(&tx, pid, id)?;
        for r in &rows {
            tx.execute(
                "DELETE FROM outbox WHERE pid = ?1 AND item = ?2",
                params![pid, r.id],
            )?;
            delete_row(&tx, pid, &r.id)?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// Replace a temporary local id with the remote id once the item exists in ACC:
    /// the row itself, its children's `parent`, and queued jobs that refer to it.
    pub fn rekey_item(&self, pid: Pid, old_id: &str, new_id: &str) -> Result<()> {
        if old_id == new_id {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;
        let target_exists = get_item(&tx, pid, new_id)?.is_some();
        if target_exists {
            // A poll may have inserted the item returned by our create request before
            // this job could bind its temporary row. Merge the local row's references
            // into that item instead of failing the primary-key update.
            delete_row(&tx, pid, old_id)?;
        } else {
            tx.execute(
                "UPDATE item SET id = ?3 WHERE pid = ?1 AND id = ?2",
                rusqlite::params![pid, old_id, new_id],
            )?;
        }
        tx.execute(
            "UPDATE item SET parent = ?3 WHERE pid = ?1 AND parent = ?2",
            rusqlite::params![pid, old_id, new_id],
        )?;
        tx.execute(
            "UPDATE outbox SET item = ?3 WHERE pid = ?1 AND item = ?2",
            rusqlite::params![pid, old_id, new_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Persist the exact OSS object and file fingerprint about to be sent to create_item.
    /// The item marker lets a concurrent poll recognize our own new ACC item; the job
    /// payload lets a retry identify a create whose response was lost.
    pub fn save_create_intent(
        &self,
        pid: Pid,
        local_id: &str,
        job_id: i64,
        storage: &str,
        payload: &str,
    ) -> Result<()> {
        let mut guard = self.conn()?;
        let tx = guard.transaction()?;
        let item_changed = tx.execute(
            "UPDATE item SET storage_urn = ?3 WHERE pid = ?1 AND id = ?2",
            params![pid, local_id, storage],
        )?;
        let job_changed = tx.execute(
            "UPDATE outbox SET payload = ?2 WHERE id = ?1 AND pid = ?3 AND item = ?4 AND kind = 'upload' AND dead = 0",
            params![job_id, payload, pid, local_id],
        )?;
        if item_changed != 1 || job_changed != 1 {
            anyhow::bail!("upload intent lost its local item or outbox job");
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist the OSS object about to become a new version. Keep the current item
    /// storage pointer untouched until ACC accepts the version.
    pub fn save_version_intent(
        &self,
        pid: Pid,
        item_id: &str,
        job_id: i64,
        payload: &str,
    ) -> Result<()> {
        let c = self.conn()?;
        let changed = c.execute(
            "UPDATE outbox SET payload = ?4 WHERE id = ?1 AND pid = ?2 AND item = ?3 AND kind = 'upload' AND dead = 0",
            params![job_id, pid, item_id, payload],
        )?;
        if changed != 1 {
            bail!("version intent lost its outbox job");
        }
        Ok(())
    }

    pub fn set_local_state(&self, pid: Pid, id: &str, s: LocalState) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE item SET lstate = ?3 WHERE pid = ?1 AND id = ?2")?
            .execute(params![pid, id, s as u8 as i64])?;
        Ok(())
    }

    /// The first local edit of a remote placeholder records the version its
    /// missing ranges still depend on. One SQL statement prevents a poll from
    /// changing the tip between stamping the base and marking the row dirty.
    pub fn mark_dirty_with_base(&self, pid: Pid, id: &str) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached(
            "UPDATE item SET lstate = ?3, bver = COALESCE(bver, rver) WHERE pid = ?1 AND id = ?2",
        )?
        .execute(params![pid, id, LocalState::Dirty as u8 as i64])?;
        Ok(())
    }

    pub fn set_base_version(&self, pid: Pid, id: &str, v: Option<i64>) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE item SET bver = ?3 WHERE pid = ?1 AND id = ?2")?
            .execute(params![pid, id, v])?;
        Ok(())
    }

    /// A completed fetch establishes the base only when the DB still points at
    /// the version that supplied its bytes. Never overwrite a newer clean
    /// replacement or a base stamped by a concurrent first local write.
    pub fn set_base_if_unset_at_tip(&self, pid: Pid, id: &str, v: Option<i64>) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached(
            "UPDATE item SET bver = ?3 WHERE pid = ?1 AND id = ?2 AND bver IS NULL AND rver IS ?3",
        )?
        .execute(params![pid, id, v])?;
        Ok(())
    }

    pub fn set_populated(&self, pid: Pid, id: &str, populated: bool) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE item SET populated = ?3 WHERE pid = ?1 AND id = ?2")?
            .execute(params![pid, id, populated as i64])?;
        Ok(())
    }

    /// Lock owner after a successful reserve (Some(name)) or unreserve (None).
    pub fn set_locked_by(&self, pid: Pid, id: &str, locked_by: Option<&str>) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE item SET locked_by = ?3 WHERE pid = ?1 AND id = ?2")?
            .execute(params![pid, id, locked_by])?;
        Ok(())
    }

    /// ACC folders with populated = 1 (not the project directory, which is ours).
    pub fn populated_folders(&self, pid: Pid) -> Result<Vec<Item>> {
        let c = self.conn()?;
        // kind 3 = Folder
        collect(
            &c,
            &format!(
                "SELECT {} FROM item WHERE pid = ?1 AND populated = 1 AND kind = 3 ORDER BY id",
                ITEM_COLS
            ),
            params![pid],
            item_from_row,
        )
    }

    /// Names from the project root (exclusive) down to the item (inclusive).
    /// Root itself -> Some(vec![]). Unknown id or broken chain -> None.
    pub fn path_of(&self, pid: Pid, id: &str) -> Result<Option<Vec<String>>> {
        let c = self.conn()?;
        let Some(mut cur) = get_item(&c, pid, id)? else {
            return Ok(None);
        };
        let mut names: Vec<String> = Vec::new();
        for _ in 0..MAX_PATH_STEPS {
            let Some(parent_id) = cur.parent.clone() else {
                // `cur` is the project root: stop without including its name.
                names.reverse();
                return Ok(Some(names));
            };
            names.push(cur.name.clone());
            let Some(parent) = get_item(&c, pid, &parent_id)? else {
                return Ok(None);
            };
            cur = parent;
        }
        Ok(None)
    }

    // ---- outbox
    pub fn enqueue(&self, pid: Pid, kind: &str, item: Option<&str>, payload: &str) -> Result<i64> {
        self.enqueue_at(pid, kind, item, payload, now_ms())
    }

    /// Enqueue a job that becomes due at `due_ms` (e.g. a delete with a grace period).
    pub fn enqueue_at(
        &self,
        pid: Pid,
        kind: &str,
        item: Option<&str>,
        payload: &str,
        due_ms: i64,
    ) -> Result<i64> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // SQLite reuses deleted INTEGER PRIMARY KEY values, including when the
        // queue becomes empty. A late worker must never update/delete a new job.
        // Persist the high-water mark even after all corresponding jobs are gone.
        let live_max: i64 = tx
            .prepare_cached("SELECT COALESCE(MAX(id), 0) FROM outbox")?
            .query_row([], |row| row.get(0))?;
        let persisted: Option<String> = tx
            .prepare_cached("SELECT v FROM kv WHERE k = 'outbox_id_sequence'")?
            .query_row([], |row| row.get(0))
            .optional()?;
        let previous = persisted
            .map(|value| value.parse::<i64>())
            .transpose()?
            .unwrap_or(0);
        let id = previous
            .max(live_max)
            .max(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("outbox job ID sequence exhausted"))?;
        tx.prepare_cached(
            "INSERT INTO outbox (id, pid, kind, item, payload, attempts, next_ms, created_ms, dead) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, 0)",
        )?
        .execute(params![id, pid, kind, item, payload, due_ms, now_ms()])?;
        tx.prepare_cached(
            "INSERT INTO kv (k, v) VALUES ('outbox_id_sequence', ?1) \
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        )?
        .execute(params![id.to_string()])?;
        tx.commit()?;
        Ok(id)
    }

    /// Turn an upload whose ACC commit already succeeded into a local-only CFAPI
    /// finalization retry. Keeping the same outbox row prevents another cloud version.
    pub fn retarget_job(&self, id: i64, kind: &str, payload: &str) -> Result<()> {
        let c = self.conn()?;
        let changed = c.execute(
            "UPDATE outbox SET kind = ?2, payload = ?3, attempts = 0 WHERE id = ?1 AND dead = 0",
            params![id, kind, payload],
        )?;
        if changed != 1 {
            bail!("cannot retarget outbox job {id}");
        }
        Ok(())
    }

    /// Clear only the OSS object proven rejected by the version POST. An in-flight
    /// worker's snapshot may predate a watcher retarget or a replacement intent.
    /// Preserve the current kind, path, scheduling and other non-intent fields.
    pub fn clear_upload_intent(&self, id: i64, expected_storage: &str) -> Result<()> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        let current: Option<String> = tx
            .query_row(
                "SELECT payload FROM outbox WHERE id = ?1 AND kind = 'upload' AND dead = 0",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            let mut payload: serde_json::Value = serde_json::from_str(&current)?;
            let Some(fields) = payload.as_object_mut() else {
                bail!("upload job {id} has no object payload");
            };
            if fields
                .get("intent_storage")
                .and_then(serde_json::Value::as_str)
                == Some(expected_storage)
            {
                fields.retain(|key, _| !key.starts_with("intent_"));
                tx.execute(
                    "UPDATE outbox SET payload = ?2 WHERE id = ?1 AND kind = 'upload' AND dead = 0",
                    params![id, serde_json::to_string(fields)?],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Carry forward the journal guard after CFAPI itself changes metadata during
    /// a failed local finalization. Keep attempts and due time so a persistent
    /// platform error still reaches the normal bounded retry limit.
    pub fn update_finalize_usn(&self, id: i64, usn: i64) -> Result<()> {
        let c = self.conn()?;
        let changed = c.execute(
            "UPDATE outbox SET payload = json_set(payload, '$.usn', ?2) \
             WHERE id = ?1 AND kind = 'finalize' AND dead = 0",
            params![id, usn],
        )?;
        if changed != 1 {
            bail!("cannot update USN of finalize job {id}");
        }
        Ok(())
    }

    /// Jobs with dead = 0 and next_ms <= now, oldest first, at most `limit`.
    pub fn due_jobs(&self, now: i64, limit: usize) -> Result<Vec<OutboxJob>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT id, pid, kind, item, payload, attempts FROM outbox \
             WHERE dead = 0 AND next_ms <= ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = st.query_map(params![now, limit as i64], |r| {
            Ok(OutboxJob {
                id: r.get(0)?,
                pid: r.get(1)?,
                kind: r.get(2)?,
                item: r.get(3)?,
                payload: r.get(4)?,
                attempts: r.get(5)?,
            })
        })?;
        let out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Earliest `next_ms` of a live job, so the engine sleeps exactly until it is due.
    pub fn next_job_due(&self) -> Result<Option<i64>> {
        let c = self.conn()?;
        let v: Option<i64> =
            c.query_row("SELECT MIN(next_ms) FROM outbox WHERE dead = 0", [], |r| {
                r.get(0)
            })?;
        Ok(v)
    }

    /// Push a job back without counting an attempt (file still open elsewhere).
    pub fn defer_job(&self, id: i64, next_ms: i64) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE outbox SET next_ms = ?2 WHERE id = ?1")?
            .execute(params![id, next_ms])?;
        Ok(())
    }

    /// Drop the live jobs of `kind` for one item. Returns how many were dropped.
    pub fn cancel_jobs(&self, pid: Pid, item: &str, kind: &str) -> Result<usize> {
        let c = self.conn()?;
        let n = c
            .prepare_cached(
                "DELETE FROM outbox WHERE pid = ?1 AND item = ?2 AND kind = ?3 AND dead = 0",
            )?
            .execute(params![pid, item, kind])?;
        Ok(n)
    }

    /// True when `item` has a live or held job of one of `kinds` (a failed one does not count).
    pub fn has_job(&self, pid: Pid, item: &str, kinds: &[&str]) -> Result<bool> {
        let c = self.conn()?;
        let mut st =
            c.prepare_cached("SELECT kind FROM outbox WHERE pid = ?1 AND item = ?2 AND dead != 1")?;
        let rows = st.query_map(params![pid, item], |r| r.get::<_, String>(0))?;
        for kind in rows {
            if kinds.contains(&kind?.as_str()) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Resume a failed create whose original OSS object was persisted in its job.
    /// Retaining that payload lets a restart adopt an ACC create with a lost reply.
    pub fn revive_create_intent(&self, pid: Pid, item: &str) -> Result<bool> {
        let c = self.conn()?;
        let candidate: Option<(i64, String)> = c
            .query_row(
                "SELECT id, payload FROM outbox WHERE pid = ?1 AND item = ?2 AND kind = 'upload' AND dead = 1 ORDER BY id DESC LIMIT 1",
                params![pid, item],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, payload)) = candidate else {
            return Ok(false);
        };
        if serde_json::from_str::<serde_json::Value>(&payload)
            .ok()
            .and_then(|v| {
                v.get("intent_storage")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
            })
            .is_none()
        {
            return Ok(false);
        }
        Ok(c.execute(
            "UPDATE outbox SET dead = 0, attempts = 0, next_ms = ?2 WHERE id = ?1 AND dead = 1",
            params![id, now_ms()],
        )? == 1)
    }

    /// A permanent ACC file-type rejection. Editing the file cannot make its extension valid.
    pub fn has_unsupported_job(&self, pid: Pid, item: &str) -> Result<bool> {
        let c = self.conn()?;
        let found: i64 = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM outbox WHERE pid = ?1 AND item = ?2 AND kind = 'upload' AND dead = 3)",
            params![pid, item],
            |r| r.get(0),
        )?;
        Ok(found != 0)
    }

    /// Forget a permanent extension rejection after the user changes the file type.
    /// The caller then queues a fresh upload at the new path. The local item remains.
    pub fn clear_unsupported_job(&self, pid: Pid, item: &str) -> Result<usize> {
        let c = self.conn()?;
        let removed = c.execute(
            "DELETE FROM outbox WHERE pid = ?1 AND item = ?2 AND kind = 'upload' AND dead = 3",
            params![pid, item],
        )?;
        Ok(removed)
    }

    /// Items whose rename or move the provider refused (`dead = 3`, see `job_unsupported`). Their
    /// local name and place stay until the user changes them again.
    pub fn refused_renames(&self, pid: Pid) -> Result<HashSet<String>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT item FROM outbox WHERE pid = ?1 AND dead = 3 AND item IS NOT NULL \
             AND kind IN ('rename', 'move')",
        )?;
        let rows = st.query_map(params![pid], |r| r.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Items with a live job of one of `kinds` (their local state wins over the next poll).
    pub fn pending_items(&self, pid: Pid, kinds: &[&str]) -> Result<HashSet<String>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT item, kind FROM outbox WHERE pid = ?1 AND dead = 0 AND item IS NOT NULL",
        )?;
        let rows = st.query_map(params![pid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = HashSet::new();
        for row in rows {
            let (item, kind) = row?;
            if kinds.contains(&kind.as_str()) {
                out.insert(item);
            }
        }
        Ok(out)
    }

    /// Item -> exact OSS object still being committed/finalized by our own job.
    /// A poll carrying that same object should wait for this job to bind its row.
    pub fn pending_upload_storages(&self, pid: Pid) -> Result<HashMap<String, HashSet<String>>> {
        let c = self.conn()?;
        let mut st = c.prepare_cached(
            "SELECT o.item, o.kind, o.payload, i.storage_urn, i.rver \
             FROM outbox o LEFT JOIN item i ON i.pid = o.pid AND i.id = o.item \
             WHERE o.pid = ?1 AND o.dead = 0 AND o.item IS NOT NULL AND o.kind IN ('upload', 'finalize')",
        )?;
        let rows = st.query_map(params![pid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (item, kind, payload, bound_storage, bound_version) = row?;
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            let key = if kind == "finalize" {
                "storage"
            } else {
                "intent_storage"
            };
            let storage = v
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    // Finalize jobs saved before the exact-storage marker was
                    // added have no `storage` field. Their bound row identifies
                    // our commit only while it still names that same version.
                    if kind == "finalize" {
                        v.get("version")
                            .and_then(serde_json::Value::as_i64)
                            .filter(|version| Some(*version) == bound_version)
                            .and(bound_storage)
                    } else {
                        None
                    }
                });
            if let Some(storage) = storage {
                out.entry(item).or_insert_with(HashSet::new).insert(storage);
            }
        }
        Ok(out)
    }

    /// Rows per local state of one project, as (state, count) — diagnostics.
    pub fn state_counts(&self, pid: Pid) -> Result<Vec<(u8, i64)>> {
        let c = self.conn()?;
        collect(
            &c,
            "SELECT lstate, COUNT(*) FROM item WHERE pid = ?1 GROUP BY lstate",
            params![pid],
            |r| Ok((u8_of(r.get(0)?), r.get(1)?)),
        )
    }

    /// Live and dead jobs as (kind, dead, attempts, last error), oldest first, at most 200 —
    /// diagnostics. Errors are cut to 300 characters.
    pub fn job_summary(&self) -> Result<Vec<JobSummary>> {
        let c = self.conn()?;
        collect(
            &c,
            "SELECT kind, dead, attempts, substr(last_error, 1, 300) FROM outbox ORDER BY id LIMIT 200",
            [],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?, r.get(3)?)),
        )
    }

    /// Weight of the project's deletes not sent yet (live or held): files count 1, folders the
    /// size of their subtree (stored in the payload as "weight").
    pub fn pending_delete_weight(&self, pid: Pid) -> Result<i64> {
        let c = self.conn()?;
        let v: i64 = c.query_row(
            "SELECT COALESCE(SUM(COALESCE(json_extract(payload, '$.weight'), 1)), 0) FROM outbox \
             WHERE pid = ?1 AND kind = 'delete' AND dead IN (0, 2)",
            params![pid],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    /// Hold the project's live deletes until the user confirms. Returns how many were held.
    pub fn hold_deletes(&self, pid: Pid) -> Result<usize> {
        let c = self.conn()?;
        let n = c
            .prepare_cached(
                "UPDATE outbox SET dead = 2 WHERE pid = ?1 AND kind = 'delete' AND dead = 0",
            )?
            .execute(params![pid])?;
        Ok(n)
    }

    /// Projects with held deletes, as (pid, weight).
    pub fn held_deletes(&self) -> Result<Vec<(Pid, i64)>> {
        let c = self.conn()?;
        collect(
            &c,
            "SELECT pid, SUM(COALESCE(json_extract(payload, '$.weight'), 1)) FROM outbox \
             WHERE kind = 'delete' AND dead = 2 GROUP BY pid ORDER BY pid",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// The user confirmed: held deletes run now.
    pub fn release_deletes(&self, pid: Pid) -> Result<usize> {
        let c = self.conn()?;
        let n = c
            .prepare_cached("UPDATE outbox SET dead = 0, next_ms = ?2 WHERE pid = ?1 AND kind = 'delete' AND dead = 2")?
            .execute(params![pid, now_ms()])?;
        Ok(n)
    }

    /// The user declined: drop the held deletes and return the items they were for.
    pub fn take_held_deletes(&self, pid: Pid) -> Result<Vec<String>> {
        let mut guard = self.conn()?;
        let tx = guard.transaction()?;
        let ids = collect(
            &tx,
            "SELECT item FROM outbox WHERE pid = ?1 AND kind = 'delete' AND dead = 2 AND item IS NOT NULL ORDER BY id",
            params![pid],
            |r| r.get::<_, String>(0),
        )?;
        tx.execute(
            "DELETE FROM outbox WHERE pid = ?1 AND kind = 'delete' AND dead = 2",
            params![pid],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    /// Mark every folder in the subtree of `id` as not populated (its contents are not on disk).
    pub fn unpopulate_subtree(&self, pid: Pid, id: &str) -> Result<()> {
        let mut guard = self.conn()?;
        let tx = guard.transaction()?;
        for row in subtree(&tx, pid, id)?
            .into_iter()
            .filter(|r| r.kind.is_dir())
        {
            tx.prepare_cached("UPDATE item SET populated = 0 WHERE pid = ?1 AND id = ?2")?
                .execute(params![pid, row.id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Number of rows in the subtree of `id` (itself included).
    pub fn subtree_len(&self, pid: Pid, id: &str) -> Result<usize> {
        let c = self.conn()?;
        Ok(subtree(&c, pid, id)?.len())
    }

    /// Forget a folder's contents (rows and their jobs), keeping the folder row itself, which
    /// becomes unpopulated so the next listing rebuilds it from ACC.
    pub fn reset_folder(&self, pid: Pid, id: &str) -> Result<()> {
        let mut guard = self.conn()?;
        let tx = guard.transaction()?;
        for row in subtree(&tx, pid, id)? {
            tx.prepare_cached("DELETE FROM outbox WHERE pid = ?1 AND item = ?2")?
                .execute(params![pid, row.id])?;
            if row.id != id {
                delete_row(&tx, pid, &row.id)?;
            }
        }
        tx.execute(
            "UPDATE item SET populated = 0, lstate = ?3 WHERE pid = ?1 AND id = ?2",
            params![pid, id, LocalState::Clean as u8 as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Forget everything about a project except the subscription itself: rows, jobs, cursor,
    /// change source. The next poll enumerates it from scratch.
    pub fn reset_project(&self, pid: Pid) -> Result<()> {
        let mut guard = self.conn()?;
        let tx = guard.transaction()?;
        tx.execute("DELETE FROM outbox WHERE pid = ?1", params![pid])?;
        tx.execute("DELETE FROM item WHERE pid = ?1", params![pid])?;
        tx.execute(
            "UPDATE project SET cursor = NULL, change_source = 0, last_sync_ms = NULL WHERE pid = ?1",
            params![pid],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Revive every dead job (the user asked to try again). Returns how many.
    pub fn retry_dead_jobs(&self) -> Result<usize> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        // Older builds kept an OSS intent after a conclusive MIME rejection.
        // Repair only that exact HTTP/JSON:API failure on explicit user retry;
        // uncertain writes must retain their object for success reconciliation.
        let rejected: Vec<(i64, String, String)> = {
            let mut query = tx.prepare(
                "SELECT id, payload, last_error FROM outbox WHERE dead = 1 AND kind = 'upload' AND last_error LIKE 'HTTP 400: %'",
            )?;
            let rows = query.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, payload, error) in rejected {
            let Some(body) = error.strip_prefix("HTTP 400: ") else {
                continue;
            };
            let Ok(error) = serde_json::from_str::<serde_json::Value>(body) else {
                continue;
            };
            let mismatch = error["errors"].as_array().is_some_and(|errors| {
                errors.iter().any(|error| {
                    error["code"] == "ERR_WIP_ERROR" && error["title"] == "VER_MISMATCH_MIMETYPE"
                })
            });
            if !mismatch {
                continue;
            }
            let Ok(mut fields) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            let Some(fields) = fields.as_object_mut() else {
                continue;
            };
            fields.retain(|key, _| !key.starts_with("intent_"));
            tx.execute(
                "UPDATE outbox SET payload = ?2 WHERE id = ?1",
                params![id, serde_json::to_string(fields)?],
            )?;
        }
        let n = tx
            .prepare_cached("UPDATE outbox SET dead = 0, attempts = 0, next_ms = ?1, last_error = NULL WHERE dead = 1")?
            .execute(params![now_ms()])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn job_done(&self, id: i64) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("DELETE FROM outbox WHERE id = ?1")?
            .execute(params![id])?;
        Ok(())
    }

    /// next_ms = retry_at_ms, last_error = err; the attempt is not counted (offline, signed out).
    pub fn job_postponed(&self, id: i64, retry_at_ms: i64, err: &str) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached("UPDATE outbox SET next_ms = ?2, last_error = ?3 WHERE id = ?1")?
            .execute(params![id, retry_at_ms, err])?;
        Ok(())
    }

    /// attempts += 1, next_ms = retry_at_ms, last_error = err; dead = 1 when `dead`.
    pub fn job_failed(&self, id: i64, retry_at_ms: i64, err: &str, dead: bool) -> Result<()> {
        let c = self.conn()?;
        c.prepare_cached(
            "UPDATE outbox SET attempts = attempts + 1, next_ms = ?2, last_error = ?3, dead = ?4 \
             WHERE id = ?1",
        )?
        .execute(params![id, retry_at_ms, err, dead as i64])?;
        Ok(())
    }

    pub fn job_unsupported(&self, id: i64, err: &str) -> Result<()> {
        let c = self.conn()?;
        c.execute(
            "UPDATE outbox SET attempts = attempts + 1, last_error = ?2, dead = 3 WHERE id = ?1",
            params![id, err],
        )?;
        Ok(())
    }

    /// Count of non-dead jobs, and dead jobs (for the tray status).
    pub fn job_counts(&self) -> Result<(i64, i64)> {
        let c = self.conn()?;
        let counts = c.query_row(
            "SELECT COALESCE(SUM(dead = 0), 0), COALESCE(SUM(dead = 1), 0) FROM outbox",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Deletes the database file (and WAL side files) on drop.
    struct TempDb(std::path::PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// Store first, so it is dropped (connection closed) before the files are removed.
    fn open_temp(tag: &str) -> (Store, TempDb) {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("synka-store-{tag}-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        (store, TempDb(path))
    }

    fn project() -> Project {
        Project {
            pid: 0,
            hub_id: "b.hub".into(),
            hub_name: "Hub".into(),
            project_id: "b.proj1".into(),
            name: "Proj".into(),
            root_folder_urn: "urn:adsk.wipemea:fs.folder:co.root".into(),
            urn_ns: "wipemea".into(),
            tops: vec![],
            oss_scopes: None,
            change_source: ChangeSourceKind::Unknown,
            cursor: None,
            enabled: true,
            last_sync_ms: None,
            provider: Provider::Acc,
            meta: None,
        }
    }

    fn item(pid: Pid, id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Item {
        Item {
            pid,
            id: id.into(),
            parent: parent.map(Into::into),
            name: name.into(),
            kind,
            remote_version: None,
            base_version: None,
            size: 0,
            ctime_ms: 1,
            mtime_ms: 1,
            storage_urn: None,
            version_urn: None,
            modified_by: None,
            locked_by: None,
            local_state: LocalState::Clean,
            populated: false,
        }
    }

    fn file(pid: Pid, id: &str, parent: &str, name: &str, size: i64) -> Item {
        let mut it = item(pid, id, Some(parent), name, Kind::File);
        it.size = size;
        it.remote_version = Some(1);
        it
    }

    fn root(pid: Pid) -> Item {
        item(pid, "co.root", None, "Project Files", Kind::ProjectRoot)
    }

    fn total(store: &Store) -> u64 {
        store.conn.lock().unwrap().total_changes()
    }

    #[test]
    fn upsert_get_and_lookup() {
        let (store, _g) = open_temp("basic");
        let pid = store.upsert_project(&project()).unwrap();
        assert_eq!(
            store.upsert_project(&project()).unwrap(),
            pid,
            "same project_id, same pid"
        );
        let p = store.project(pid).unwrap().unwrap();
        assert_eq!(p.name, "Proj");
        assert_eq!(store.projects().unwrap().len(), 1);

        store.kv_set("k", Some("v")).unwrap();
        assert_eq!(store.kv_get("k").unwrap().as_deref(), Some("v"));
        store.kv_set("k", None).unwrap();
        assert_eq!(store.kv_get("k").unwrap(), None);

        let r = root(pid);
        let f = item(pid, "co.f", Some("co.root"), "Drawings", Kind::Folder);
        let a = file(pid, "lin1", "co.f", "Plan.rvt", 42);
        store.upsert_item(&r).unwrap();
        store.upsert_item(&f).unwrap();
        store.upsert_item(&a).unwrap();

        assert_eq!(store.item(pid, "lin1").unwrap().unwrap(), a);
        assert!(store.item(pid, "nope").unwrap().is_none());
        assert_eq!(store.children(pid, "co.root").unwrap(), vec![f.clone()]);
        assert_eq!(store.children(pid, "co.f").unwrap(), vec![a.clone()]);

        store.set_locked_by(pid, "lin1", Some("Bo")).unwrap();
        assert_eq!(
            store
                .item(pid, "lin1")
                .unwrap()
                .unwrap()
                .locked_by
                .as_deref(),
            Some("Bo")
        );
        store.set_locked_by(pid, "lin1", None).unwrap();
        assert_eq!(store.item(pid, "lin1").unwrap().unwrap(), a);

        store.delete_project(pid).unwrap();
        assert!(store.projects().unwrap().is_empty());
        assert!(store.item(pid, "lin1").unwrap().is_none());
    }

    #[test]
    fn first_local_edit_keeps_the_version_before_a_remote_tip_change() {
        let (store, _g) = open_temp("first-edit-base");
        let pid = store.upsert_project(&project()).unwrap();
        let mut old = file(pid, "lin1", "co.root", "Plan.rvt", 100);
        old.remote_version = Some(3);
        store.upsert_item(&old).unwrap();

        // The watcher sees the edit before the poll.
        store.mark_dirty_with_base(pid, &old.id).unwrap();
        let dirty = store.item(pid, &old.id).unwrap().unwrap();
        assert_eq!(
            (dirty.local_state, dirty.base_version),
            (LocalState::Dirty, Some(3))
        );

        // The poll also has to keep the old base if it wins before the watcher.
        store
            .set_local_state(pid, &old.id, LocalState::Clean)
            .unwrap();
        store.set_base_version(pid, &old.id, None).unwrap();
        let mut tip = old.clone();
        tip.remote_version = Some(4);
        tip.storage_urn = Some("new-object".into());
        store
            .apply_delta(
                pid,
                &Delta {
                    upserts: vec![tip],
                    ..Default::default()
                },
            )
            .unwrap();
        let polled = store.item(pid, &old.id).unwrap().unwrap();
        assert_eq!(
            (polled.remote_version, polled.base_version),
            (Some(4), Some(3))
        );
        store.mark_dirty_with_base(pid, &old.id).unwrap();
        let dirty_after_poll = store.item(pid, &old.id).unwrap().unwrap();
        assert_eq!(
            (dirty_after_poll.local_state, dirty_after_poll.base_version),
            (LocalState::Dirty, Some(3))
        );
        store
            .set_base_if_unset_at_tip(pid, &old.id, Some(4))
            .unwrap();
        assert_eq!(
            store.item(pid, &old.id).unwrap().unwrap().base_version,
            Some(3),
            "an in-flight fetch cannot replace the first-edit base"
        );
        store.set_base_version(pid, &old.id, None).unwrap();
        store
            .set_base_if_unset_at_tip(pid, &old.id, Some(3))
            .unwrap();
        assert_eq!(
            store.item(pid, &old.id).unwrap().unwrap().base_version,
            None,
            "a completed old fetch cannot establish a base after the tip advanced"
        );
        store
            .set_base_if_unset_at_tip(pid, &old.id, Some(4))
            .unwrap();
        assert_eq!(
            store.item(pid, &old.id).unwrap().unwrap().base_version,
            Some(4)
        );
    }

    #[test]
    fn apply_delta_create_update_unchanged_delete_reset() {
        let (store, _g) = open_temp("apply");
        let pid = store.upsert_project(&project()).unwrap();
        let f = item(pid, "co.f", Some("co.root"), "Drawings", Kind::Folder);
        let a = file(pid, "lin1", "co.f", "Plan.rvt", 42);
        let d1 = Delta {
            upserts: vec![root(pid), f.clone(), a.clone()],
            ..Default::default()
        };

        // create
        let applied = store.apply_delta(pid, &d1).unwrap();
        assert_eq!(applied.changes.len(), 3);
        assert!(applied
            .changes
            .iter()
            .all(|(b, af)| b.is_none() && af.is_some()));

        // unchanged: nothing reported
        let applied = store.apply_delta(pid, &d1).unwrap();
        assert!(applied.changes.is_empty());

        // update preserves local fields
        store
            .set_local_state(pid, "lin1", LocalState::Dirty)
            .unwrap();
        store.set_base_version(pid, "lin1", Some(1)).unwrap();
        let mut a2 = file(pid, "lin1", "co.f", "Plan.rvt", 99);
        a2.remote_version = Some(2);
        let applied = store
            .apply_delta(
                pid,
                &Delta {
                    upserts: vec![a2],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(applied.changes.len(), 1);
        let (before, after) = &applied.changes[0];
        assert_eq!(before.as_ref().unwrap().size, 42);
        let after = after.as_ref().unwrap();
        assert_eq!(after.size, 99);
        assert_eq!(after.remote_version, Some(2));
        assert_eq!(after.local_state, LocalState::Dirty);
        assert_eq!(after.base_version, Some(1));
        assert_eq!(store.item(pid, "lin1").unwrap().unwrap(), *after);

        // recursive delete of the folder also removes its file
        let applied = store
            .apply_delta(
                pid,
                &Delta {
                    deletes: vec!["co.f".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        let mut gone: Vec<String> = applied
            .changes
            .iter()
            .map(|(b, af)| {
                assert!(af.is_none());
                b.as_ref().unwrap().id.clone()
            })
            .collect();
        gone.sort();
        assert_eq!(gone, vec!["co.f".to_string(), "lin1".to_string()]);
        assert!(store.item(pid, "lin1").unwrap().is_none());

        // reset: re-enumerate with root + a new file; the folder and file are not in the
        // listing so they vanish; root is identical so it is not reported
        store.apply_delta(pid, &d1).unwrap();
        let lin2 = file(pid, "lin2", "co.root", "Other.dwg", 7);
        let applied = store
            .apply_delta(
                pid,
                &Delta {
                    upserts: vec![root(pid), lin2],
                    reset: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut deleted: Vec<String> = applied
            .changes
            .iter()
            .filter(|(_, af)| af.is_none())
            .map(|(b, _)| b.as_ref().unwrap().id.clone())
            .collect();
        deleted.sort();
        assert_eq!(deleted, vec!["co.f".to_string(), "lin1".to_string()]);
        let created: Vec<String> = applied
            .changes
            .iter()
            .filter(|(b, _)| b.is_none())
            .map(|(_, af)| af.as_ref().unwrap().id.clone())
            .collect();
        assert_eq!(created, vec!["lin2".to_string()]);
        assert_eq!(applied.changes.len(), 3);
        assert!(store.item(pid, "co.f").unwrap().is_none());

        // cursor + last_sync are written with the delta
        store
            .apply_delta(
                pid,
                &Delta {
                    new_cursor: Some("{\"f\":1}".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let p = store.project(pid).unwrap().unwrap();
        assert_eq!(p.cursor.as_deref(), Some("{\"f\":1}"));
        assert!(p.last_sync_ms.is_some());
    }

    #[test]
    fn empty_delta_writes_nothing() {
        let (store, _g) = open_temp("noop");
        let pid = store.upsert_project(&project()).unwrap();
        let before = total(&store);
        let applied = store.apply_delta(pid, &Delta::default()).unwrap();
        assert!(applied.changes.is_empty());
        assert_eq!(total(&store), before, "empty delta must not write");

        store
            .apply_delta(
                pid,
                &Delta {
                    new_cursor: Some("c1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(total(&store) > before, "cursor-only delta writes");
    }

    #[test]
    fn path_of_walks_to_root() {
        let (store, _g) = open_temp("path");
        let pid = store.upsert_project(&project()).unwrap();
        store.upsert_item(&root(pid)).unwrap();
        store
            .upsert_item(&item(
                pid,
                "co.f",
                Some("co.root"),
                "Drawings",
                Kind::Folder,
            ))
            .unwrap();
        store
            .upsert_item(&file(pid, "lin1", "co.f", "Plan.rvt", 1))
            .unwrap();

        assert_eq!(store.path_of(pid, "co.root").unwrap(), Some(vec![]));
        assert_eq!(
            store.path_of(pid, "co.f").unwrap(),
            Some(vec!["Drawings".to_string()])
        );
        assert_eq!(
            store.path_of(pid, "lin1").unwrap(),
            Some(vec!["Drawings".to_string(), "Plan.rvt".to_string()])
        );
        assert_eq!(store.path_of(pid, "missing").unwrap(), None);

        // broken chain
        store
            .upsert_item(&file(pid, "lin9", "co.gone", "Lost.dwg", 1))
            .unwrap();
        assert_eq!(store.path_of(pid, "lin9").unwrap(), None);

        // cycle without a root
        store
            .upsert_item(&item(pid, "x", Some("y"), "X", Kind::Folder))
            .unwrap();
        store
            .upsert_item(&item(pid, "y", Some("x"), "Y", Kind::Folder))
            .unwrap();
        assert_eq!(store.path_of(pid, "x").unwrap(), None);
    }

    #[test]
    fn resilience_manual_retry_repairs_only_confirmed_historical_mime_rejection() {
        let (store, _guard) = open_temp("mime-retry");
        let pid = store.upsert_project(&project()).unwrap();
        let body = serde_json::json!({"errors":[{
            "code":"ERR_WIP_ERROR","title":"VER_MISMATCH_MIMETYPE"}]})
        .to_string();
        let payload = serde_json::json!({"path":"C:\\Synka\\model.rvt",
            "intent_storage":"possibly-committed", "intent_size":17,
            "intent_kind":"version", "future_field":"preserve"})
        .to_string();
        let mut cases = Vec::new();
        for (kind, error, clear) in [
            ("upload", format!("HTTP 400: {body}"), true),
            ("upload", format!("HTTP 503: {body}"), false),
            (
                "upload",
                format!("transport: lost reply HTTP 400: {body}"),
                false,
            ),
            (
                "upload",
                "HTTP 400: not JSON VER_MISMATCH_MIMETYPE".into(),
                false,
            ),
            (
                "upload",
                "HTTP 400: {\"errors\":[{\"code\":\"OTHER\",\"title\":\"VER_MISMATCH_MIMETYPE\"}]}"
                    .into(),
                false,
            ),
            ("delete", format!("HTTP 400: {body}"), false),
        ] {
            let id = store.enqueue(pid, kind, None, &payload).unwrap();
            store.job_failed(id, 0, &error, true).unwrap();
            cases.push((id, clear));
        }
        let still_pending = store.enqueue(pid, "upload", None, &payload).unwrap();
        assert_eq!(store.retry_dead_jobs().unwrap(), cases.len());
        let jobs = store.due_jobs(i64::MAX, 100).unwrap();
        for (id, clear) in cases.into_iter().chain([(still_pending, false)]) {
            let job = jobs.iter().find(|job| job.id == id).unwrap();
            let fields: serde_json::Value = serde_json::from_str(&job.payload).unwrap();
            assert_eq!(fields.get("intent_storage").is_none(), clear, "job {id}");
            assert_eq!(fields["path"], "C:\\Synka\\model.rvt");
            assert_eq!(fields["future_field"], "preserve");
        }
    }

    #[test]
    fn resilience_outbox_ids_survive_highest_deletion_empty_queue_and_restart() {
        let (store, guard) = open_temp("outbox-id-reuse");
        let pid = store.upsert_project(&project()).unwrap();
        let first = store.enqueue(pid, "upload", None, "{}").unwrap();
        let highest = store.enqueue(pid, "upload", None, "{}").unwrap();
        store.job_done(highest).unwrap();
        let replacement = store.enqueue(pid, "upload", None, "{}").unwrap();
        assert!(replacement > highest, "deleted highest job ID was reused");
        store.job_done(first).unwrap();
        store.job_done(replacement).unwrap();
        assert!(store.due_jobs(i64::MAX, 10).unwrap().is_empty());
        drop(store);

        let store = Store::open(&guard.0).unwrap();
        let payload = serde_json::json!({"path":"newer.rvt",
            "intent_storage":"possibly-committed-new-storage"})
        .to_string();
        let new_id = store.enqueue(pid, "upload", None, &payload).unwrap();
        assert!(
            new_id > replacement,
            "empty queue lost its ID high-water mark"
        );
        // Delayed responses from workers holding old snapshots cannot mark the
        // replacement dead, erase its ambiguous-success intent or delete it.
        let error = "HTTP 400: {\"errors\":[{\"code\":\"ERR_WIP_ERROR\",\"title\":\"VER_MISMATCH_MIMETYPE\"}]}";
        for stale_id in [first, highest, replacement] {
            store.job_failed(stale_id, 0, error, true).unwrap();
            store.job_done(stale_id).unwrap();
        }
        assert_eq!(store.retry_dead_jobs().unwrap(), 0);
        let jobs = store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, new_id);
        assert_eq!(jobs[0].attempts, 0);
        assert_eq!(jobs[0].payload, payload);
    }

    #[test]
    fn resilience_outbox_id_allocator_seeds_legacy_rows_and_rejects_overflow() {
        let (store, _guard) = open_temp("outbox-id-seed");
        let pid = store.upsert_project(&project()).unwrap();
        store
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO outbox (id, pid, kind, payload, next_ms, created_ms) \
             VALUES (41, ?1, 'upload', '{}', 0, 0)",
                params![pid],
            )
            .unwrap();
        assert_eq!(store.enqueue(pid, "upload", None, "{}").unwrap(), 42);
        store.kv_set("outbox_id_sequence", Some("7")).unwrap();
        assert_eq!(store.enqueue(pid, "upload", None, "{}").unwrap(), 43);
        store
            .kv_set("outbox_id_sequence", Some(&i64::MAX.to_string()))
            .unwrap();
        let error = store.enqueue(pid, "upload", None, "{}").unwrap_err();
        assert!(error.to_string().contains("ID sequence exhausted"));
        assert_eq!(store.job_counts().unwrap(), (3, 0));
        assert_eq!(
            store.kv_get("outbox_id_sequence").unwrap(),
            Some(i64::MAX.to_string())
        );
    }

    #[test]
    fn outbox_enqueue_due_failed_done() {
        let (store, _g) = open_temp("outbox");
        let pid = store.upsert_project(&project()).unwrap();
        let a = store.enqueue(pid, "upload", Some("lin1"), "{}").unwrap();
        let b = store.enqueue(pid, "delete", None, "{\"x\":1}").unwrap();
        let now = now_ms();

        let due = store.due_jobs(now + 1000, 10).unwrap();
        assert_eq!(due.iter().map(|j| j.id).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(due[0].kind, "upload");
        assert_eq!(due[0].item.as_deref(), Some("lin1"));
        assert_eq!(due[1].item, None);
        assert_eq!(store.due_jobs(now + 1000, 1).unwrap().len(), 1);

        // retry later: a drops out of the due list
        store.job_failed(a, now + 60_000, "boom", false).unwrap();
        let due = store.due_jobs(now + 1000, 10).unwrap();
        assert_eq!(due.iter().map(|j| j.id).collect::<Vec<_>>(), vec![b]);
        assert_eq!(store.job_counts().unwrap(), (2, 0));

        // dead jobs are never due and are counted separately
        store.job_failed(b, 0, "permanent", true).unwrap();
        let due = store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(due.iter().map(|j| j.id).collect::<Vec<_>>(), vec![a]);
        assert_eq!(due[0].attempts, 1);
        assert_eq!(store.job_counts().unwrap(), (1, 1));

        store.job_done(a).unwrap();
        assert_eq!(store.job_counts().unwrap(), (0, 1));

        // dead jobs can be revived
        assert_eq!(store.retry_dead_jobs().unwrap(), 1);
        assert_eq!(store.job_counts().unwrap(), (1, 0));

        let unsupported = store
            .enqueue(pid, "upload", Some("local-unsupported"), "{}")
            .unwrap();
        store
            .job_unsupported(
                unsupported,
                "File with extension svg is not allowed to upload.",
            )
            .unwrap();
        assert!(store.has_unsupported_job(pid, "local-unsupported").unwrap());
        assert_eq!(store.retry_dead_jobs().unwrap(), 0);
        assert_eq!(store.job_counts().unwrap(), (1, 0));
    }

    #[test]
    fn outbox_scheduling_helpers() {
        let (store, _g) = open_temp("sched");
        let pid = store.upsert_project(&project()).unwrap();
        assert_eq!(store.next_job_due().unwrap(), None);
        let now = now_ms();
        let del = store
            .enqueue_at(pid, "delete", Some("lin1"), "{}", now + 15_000)
            .unwrap();
        let up = store.enqueue(pid, "upload", Some("lin2"), "{}").unwrap();
        assert!(store.next_job_due().unwrap().unwrap() <= now_ms());
        assert!(
            store
                .due_jobs(now_ms(), 10)
                .unwrap()
                .iter()
                .all(|j| j.id != del),
            "grace period"
        );

        // deferral does not count an attempt
        store.defer_job(up, now + 10_000).unwrap();
        assert_eq!(store.next_job_due().unwrap(), Some(now + 10_000));
        let job = store
            .due_jobs(i64::MAX, 10)
            .unwrap()
            .into_iter()
            .find(|j| j.id == up)
            .unwrap();
        assert_eq!(job.attempts, 0);

        let pending = store.pending_items(pid, &["delete"]).unwrap();
        assert!(pending.contains("lin1") && !pending.contains("lin2"));
        assert_eq!(store.cancel_jobs(pid, "lin1", "delete").unwrap(), 1);
        assert_eq!(store.cancel_jobs(pid, "lin1", "delete").unwrap(), 0);
        assert_eq!(store.next_job_due().unwrap(), Some(now + 10_000));
    }

    #[test]
    fn updated_finalize_usn_keeps_bounded_retry_count() {
        let (store, _g) = open_temp("finalize-usn");
        let pid = store.upsert_project(&project()).unwrap();
        let id = store
            .enqueue(pid, "finalize", Some("LIN-one"), r#"{"path":"x","usn":10}"#)
            .unwrap();
        store
            .job_failed(id, now_ms(), "CFAPI retry", false)
            .unwrap();
        store.update_finalize_usn(id, 20).unwrap();
        let job = store.due_jobs(i64::MAX, 1).unwrap().pop().unwrap();
        assert_eq!(job.attempts, 1, "metadata retries remain bounded");
        let payload: serde_json::Value = serde_json::from_str(&job.payload).unwrap();
        assert_eq!(payload["usn"].as_i64(), Some(20));
    }

    #[test]
    fn v1_databases_upgrade_and_keep_the_old_layout_visible() {
        let path = std::env::temp_dir().join(format!("synka-store-v1-{}.db", std::process::id()));
        let _g = TempDb(path.clone());
        let _ = std::fs::remove_file(&path);
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute_batch(
                "PRAGMA user_version = 1;
                 INSERT INTO project (pid, hub_id, hub_name, project_id, name, root_folder_urn, urn_ns)
                   VALUES (1, 'b.h', 'Hub', 'b.p', 'Proj', 'urn:adsk.wipemea:fs.folder:co.R', 'wipemea');
                 INSERT INTO item (pid, id, parent, name, kind) VALUES (1, 'co.R', NULL, 'Proj', 2);
                 INSERT INTO item (pid, id, parent, name, kind) VALUES (1, 'L1', 'co.R', 'Ritning Över:1.pdf', 4);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let v: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 4);
        // v3: the project's top folders are unknown and its rows use the old layout (its
        // root row is an ACC folder), so the engine rebuilds it.
        assert!(store.project(1).unwrap().unwrap().tops.is_empty());
        assert!(store.old_layout(1).unwrap());
        let tops = vec![TopFolder {
            id: "co.R".into(),
            name: "Project Files".into(),
        }];
        store.set_tops(1, &tops).unwrap();
        assert_eq!(store.project(1).unwrap().unwrap().tops, tops);
        store.reset_project(1).unwrap();
        store
            .upsert_item(&item(1, PROJECT_ROOT_ID, None, "Proj", Kind::ProjectRoot))
            .unwrap();
        assert!(!store.old_layout(1).unwrap());
        store
            .upsert_item(&item(
                1,
                "L1",
                Some("co.R"),
                "Ritning Över:1.pdf",
                Kind::File,
            ))
            .unwrap();
        let hits = store
            .children_named(1, "co.R", "ritning över_1.PDF")
            .unwrap();
        assert_eq!(
            hits.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["L1"]
        );
        assert!(store
            .children_named(1, "co.R", "ritning.pdf")
            .unwrap()
            .is_empty());
        // New and renamed rows get their key on write.
        let mut it = hits[0].clone();
        it.name = "Ny.pdf".into();
        store.upsert_item(&it).unwrap();
        assert_eq!(store.children_named(1, "co.R", "NY.PDF").unwrap().len(), 1);
        drop(store);
    }

    #[test]
    fn provider_and_meta_round_trip() {
        let (store, _g) = open_temp("provider");
        let mut p = project();
        p.project_id = "streambim:app:1552".into();
        p.provider = Provider::StreamBim;
        p.meta = Some(r#"{"region":"app"}"#.into());
        let pid = store.upsert_project(&p).unwrap();
        let back = store.project(pid).unwrap().unwrap();
        assert_eq!(back.provider, Provider::StreamBim);
        assert_eq!(back.meta.as_deref(), Some(r#"{"region":"app"}"#));

        // The update path writes both columns too.
        p.provider = Provider::Interaxo;
        p.meta = None;
        assert_eq!(store.upsert_project(&p).unwrap(), pid);
        let back = store.project(pid).unwrap().unwrap();
        assert_eq!(back.provider, Provider::Interaxo);
        assert_eq!(back.meta, None);

        // A provider name this build does not know reads as ACC instead of failing the row.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE project SET provider = 'future' WHERE pid = ?1",
                [pid],
            )
            .unwrap();
        assert_eq!(store.project(pid).unwrap().unwrap().provider, Provider::Acc);
    }

    #[test]
    fn v3_databases_upgrade_and_their_projects_are_acc() {
        let path = std::env::temp_dir().join(format!("synka-store-v3-{}.db", std::process::id()));
        let _g = TempDb(path.clone());
        let _ = std::fs::remove_file(&path);
        {
            // The v3 layout by hand: v1 tables, then the v2 and v3 column additions.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute_batch(
                "ALTER TABLE item ADD COLUMN lkey TEXT;
                 CREATE INDEX IF NOT EXISTS item_by_key ON item (pid, parent, lkey);
                 ALTER TABLE project ADD COLUMN tops TEXT;
                 PRAGMA user_version = 3;
                 INSERT INTO project (pid, hub_id, hub_name, project_id, name, root_folder_urn, urn_ns, tops)
                   VALUES (1, 'b.h', 'Hub', 'b.p', 'Proj', 'urn:adsk.wipemea:fs.folder:co.R', 'wipemea',
                           '[{\"id\":\"co.R\",\"name\":\"Project Files\"}]');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let v: i64 = store
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 4);
        let p = store.project(1).unwrap().unwrap();
        assert_eq!(p.provider, Provider::Acc);
        assert_eq!(p.meta, None);
        assert_eq!(p.tops.len(), 1, "the v3 columns survive the upgrade");
        // A v4 database opens again unchanged.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.project(1).unwrap().unwrap().provider, Provider::Acc);
    }

    #[test]
    fn held_deletes_and_resets() {
        let (store, _g) = open_temp("held");
        let pid = store.upsert_project(&project()).unwrap();
        store.upsert_item(&root(pid)).unwrap();
        store
            .upsert_item(&item(
                pid,
                "co.f",
                Some("co.root"),
                "Drawings",
                Kind::Folder,
            ))
            .unwrap();
        store
            .upsert_item(&file(pid, "lin1", "co.f", "Plan.rvt", 1))
            .unwrap();
        store
            .upsert_item(&file(pid, "lin2", "co.f", "Fasad.rvt", 1))
            .unwrap();
        assert_eq!(store.subtree_len(pid, "co.f").unwrap(), 3);

        store.enqueue(pid, "delete", Some("lin1"), "{}").unwrap();
        store
            .enqueue(pid, "delete", Some("co.f"), r#"{"weight":3}"#)
            .unwrap();
        assert_eq!(store.pending_delete_weight(pid).unwrap(), 4);
        assert_eq!(store.hold_deletes(pid).unwrap(), 2);
        assert!(
            store.due_jobs(i64::MAX, 10).unwrap().is_empty(),
            "held jobs never run"
        );
        assert_eq!(
            store.job_counts().unwrap(),
            (0, 0),
            "held is neither pending nor failed"
        );
        assert_eq!(store.held_deletes().unwrap(), vec![(pid, 4)]);
        assert_eq!(store.pending_delete_weight(pid).unwrap(), 4);
        assert_eq!(
            store.take_held_deletes(pid).unwrap(),
            vec!["lin1".to_string(), "co.f".to_string()]
        );
        assert!(store.held_deletes().unwrap().is_empty());

        store.enqueue(pid, "delete", Some("lin2"), "{}").unwrap();
        store.hold_deletes(pid).unwrap();
        assert_eq!(store.release_deletes(pid).unwrap(), 1);
        assert_eq!(store.due_jobs(i64::MAX, 10).unwrap().len(), 1);

        // Folder reset keeps the folder, drops its contents and their jobs.
        store.set_populated(pid, "co.f", true).unwrap();
        store.reset_folder(pid, "co.f").unwrap();
        let f = store.item(pid, "co.f").unwrap().unwrap();
        assert!(!f.populated);
        assert!(store.children(pid, "co.f").unwrap().is_empty());
        assert_eq!(store.job_counts().unwrap(), (0, 0));

        store
            .apply_delta(
                pid,
                &Delta {
                    new_cursor: Some("c".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store.reset_project(pid).unwrap();
        let p = store.project(pid).unwrap().unwrap();
        assert!(p.cursor.is_none() && p.enabled);
        assert!(store.item(pid, "co.root").unwrap().is_none());
    }

    #[test]
    fn reset_keeps_local_only_rows() {
        let (store, _g) = open_temp("reset-local");
        let pid = store.upsert_project(&project()).unwrap();
        store.upsert_item(&root(pid)).unwrap();
        let mut pending = file(pid, "local-0011", "co.root", "Ny.pdf", 3);
        pending.remote_version = None;
        pending.local_state = LocalState::New;
        store.upsert_item(&pending).unwrap();
        store
            .upsert_item(&file(pid, "lin1", "co.root", "Gammal.pdf", 3))
            .unwrap();
        let applied = store
            .apply_delta(
                pid,
                &Delta {
                    upserts: vec![root(pid)],
                    reset: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let gone: Vec<String> = applied
            .changes
            .iter()
            .filter(|&(_, a)| a.is_none())
            .map(|(b, _)| b.as_ref().unwrap().id.clone())
            .collect();
        assert_eq!(gone, vec!["lin1".to_string()]);
        assert!(store.item(pid, "local-0011").unwrap().is_some());
    }
}

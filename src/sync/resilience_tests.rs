//! Repeatable fault and event-storm tests against disposable folders and real SQLite.
//! `SYNKA_TEST_SEED` and `SYNKA_STRESS_OPS` reproduce a run without touching a live sync root.

use super::tests::{client_override_signed_in, fixture, row, Fixture};
use super::*;
use crate::store::OutboxJob;
use std::collections::BTreeMap;
use std::sync::Barrier;

pub(super) fn directory_junction(link: &Path, target: &Path) {
    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("start Windows junction creation");
    assert!(
        output.status.success(),
        "create disposable junction: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_link(link).is_ok(),
        "the test must create a real junction"
    );
}

fn jobs(f: &Fixture) -> Vec<OutboxJob> {
    f.ctx.store.due_jobs(i64::MAX, 100_000).unwrap()
}

fn delete(f: &Fixture, item: &Item, path: &Path) {
    handle_delete(
        &f.ctx,
        &DeleteReq {
            keys: OpKeys {
                connection_key: 0,
                transfer_key: 0,
                request_key: 0,
                file_id: 0,
            },
            path: path.to_path_buf(),
            identity: ph::encode_identity(item.kind, f.p.pid, &item.id),
            undelete: false,
        },
    );
}

#[test]
fn resilience_fixture_closes_database_before_cleaning_temporary_root() {
    let f = fixture("resilience-cleanup");
    let root = f.ctx.paths.state_dir.parent().unwrap().to_path_buf();
    assert!(f.ctx.paths.db_file.is_file());
    drop(f);
    assert!(
        !root.exists(),
        "fixture cleanup left an open SQLite database at {}",
        root.display()
    );
}

#[test]
fn resilience_watcher_never_uploads_external_files_through_directory_junction() {
    let f = fixture("resilience-junction-escape");
    let outside = f.ctx.paths.state_dir.join("outside-sync");
    std::fs::create_dir(&outside).unwrap();
    let secret = outside.join("external-private.bin");
    std::fs::write(&secret, b"synthetic private bytes outside the sync tree").unwrap();
    let link = f.dir.join("ReplacedRemoteFolder");
    f.ctx
        .store
        .upsert_item(&row(
            &f.p,
            "co.JUNCTION",
            Some("co.ROOT"),
            "ReplacedRemoteFolder",
            Kind::Folder,
        ))
        .unwrap();
    directory_junction(&link, &outside);
    let redirected = link.join("external-private.bin");
    assert_eq!(
        std::fs::canonicalize(&redirected).unwrap(),
        std::fs::canonicalize(&secret).unwrap(),
        "prove the path actually leaves the sync tree"
    );
    process_local(&f.ctx, vec![redirected]);
    let pending = jobs(&f);
    std::fs::remove_dir(&link).unwrap();
    assert_eq!(
        std::fs::read(&secret).unwrap(),
        b"synthetic private bytes outside the sync tree"
    );
    assert!(
        pending.is_empty(),
        "watcher queued bytes outside the sync root through a real junction: {pending:?}"
    );
    assert!(f
        .ctx
        .store
        .children(f.p.pid, "co.JUNCTION")
        .unwrap()
        .is_empty());
}

#[test]
fn resilience_registration_rejects_junction_sources_and_accepts_normal_paths() {
    let f = fixture("resilience-register-junction");
    let outside = f.ctx.paths.state_dir.join("outside-sync");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("private.bin"), b"external source").unwrap();
    let link = f.dir.join("Linked");
    directory_junction(&link, &outside);
    let result = register_local(
        &f.ctx,
        f.p.pid,
        "co.ROOT",
        "private.bin",
        false,
        &link.join("private.bin"),
    );
    std::fs::remove_dir(&link).unwrap();
    assert!(
        result.is_err(),
        "registration itself must reject redirected paths"
    );
    assert!(jobs(&f).is_empty());
    let ordinary = f.dir.join("ordinary.bin");
    std::fs::write(&ordinary, b"normal sync-root bytes").unwrap();
    assert!(local_path_is_safe(&f.ctx.paths.sync_root, &ordinary));
    assert!(local_path_is_safe(&f.ctx.paths.sync_root, &f.dir));
    assert!(register_local(&f.ctx, f.p.pid, "co.ROOT", "ordinary.bin", false, &ordinary).is_ok());
    assert_eq!(jobs(&f).len(), 1);
    assert_eq!(
        std::fs::read(outside.join("private.bin")).unwrap(),
        b"external source"
    );
}

#[test]
fn resilience_reset_rescue_keeps_locked_source_until_a_complete_retry() {
    use std::os::windows::fs::OpenOptionsExt;

    let f = fixture("resilience-reset-locked-source");
    let aside = f.pdir.with_file_name(".synka-reset-20261017-130001-locked");
    std::fs::create_dir_all(&aside).unwrap();
    let source = aside.join("precious.bin");
    std::fs::write(
        &source,
        b"only unsynced copy survives a real sharing violation",
    )
    .unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&source)
        .unwrap();
    assert!(
        std::fs::File::open(&source).is_err(),
        "the fault must actually deny readers"
    );
    let (saved, destination) = reset::finish(&f.ctx, &aside, "locked");
    assert_eq!(saved, 0);
    assert!(destination.is_some());
    assert!(aside.is_dir(), "failed copy must retain the aside tree");
    drop(lock);
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"only unsynced copy survives a real sharing violation"
    );
    let (saved, destination) = reset::finish(&f.ctx, &aside, "locked");
    assert_eq!(saved, 1);
    assert_eq!(
        std::fs::read(destination.unwrap().join("precious.bin")).unwrap(),
        b"only unsynced copy survives a real sharing violation"
    );
    assert!(
        !aside.exists(),
        "source is removed only after successful recovery"
    );
}

#[test]
fn resilience_reset_rescue_keeps_source_when_destination_cannot_be_created() {
    let f = fixture("resilience-reset-blocked-destination");
    let aside = f
        .pdir
        .with_file_name(".synka-reset-20261017-130002-blocked");
    std::fs::create_dir_all(&aside).unwrap();
    let source = aside.join("precious.bin");
    std::fs::write(&source, b"source bytes while destination is unavailable").unwrap();
    let blocked_root = reset::rescue_root(&f.ctx);
    std::fs::write(
        &blocked_root,
        b"an existing file blocks the rescue directory",
    )
    .unwrap();
    let (saved, destination) = reset::finish(&f.ctx, &aside, "blocked");
    assert_eq!(saved, 0);
    assert_eq!(destination.as_deref(), Some(aside.as_path()));
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"source bytes while destination is unavailable"
    );
    assert_eq!(
        std::fs::read(&blocked_root).unwrap(),
        b"an existing file blocks the rescue directory"
    );
    std::fs::remove_file(&blocked_root).unwrap();
    let (saved, destination) = reset::finish(&f.ctx, &aside, "blocked");
    assert_eq!(saved, 1);
    assert_eq!(
        std::fs::read(destination.unwrap().join("precious.bin")).unwrap(),
        b"source bytes while destination is unavailable"
    );
    assert!(!aside.exists());
}

#[test]
fn resilience_corrupt_database_open_preserves_the_unreadable_file() {
    let f = fixture("resilience-corrupt-sqlite");
    let database = f.ctx.paths.state_dir.join("corrupt-test.db");
    let original = b"SQLite format 3\0deliberately truncated corrupt synthetic database";
    std::fs::write(&database, original).unwrap();
    assert!(
        Store::open(&database).is_err(),
        "an unreadable database must fail explicitly"
    );
    assert_eq!(
        std::fs::read(&database).unwrap(),
        original,
        "failed open must preserve recovery evidence"
    );
}

#[test]
fn resilience_duplicate_watcher_storm_has_one_row_and_upload() {
    let f = fixture("resilience-watcher-storm");
    let path = f.dir.join("rapid-save.bin");
    std::fs::write(&path, b"latest complete edit").unwrap();
    let gate = Arc::new(Barrier::new(9));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let ctx = f.ctx.clone();
            let path = path.clone();
            let gate = gate.clone();
            scope.spawn(move || {
                gate.wait();
                for _ in 0..32 {
                    process_local(&ctx, vec![path.clone(); 32]);
                }
            });
        }
        gate.wait();
    });
    let rows = f.ctx.store.children(f.p.pid, "co.ROOT").unwrap();
    assert_eq!(rows.len(), 1, "8,192 notifications must represent one file");
    assert_eq!(rows[0].local_state, LocalState::New);
    let pending = jobs(&f);
    assert_eq!(
        pending.len(),
        1,
        "concurrent events must not multiply uploads"
    );
    assert_eq!(pending[0].kind, "upload");
    assert_eq!(pending[0].item.as_deref(), Some(rows[0].id.as_str()));
    assert_eq!(std::fs::read(&path).unwrap(), b"latest complete edit");
}

#[test]
fn resilience_repeated_safe_saves_coalesce_without_losing_latest_bytes() {
    let f = fixture("resilience-safe-save");
    let item = row(&f.p, "LIN-safe", Some("co.ROOT"), "model.bin", Kind::File);
    f.ctx.store.upsert_item(&item).unwrap();
    let path = f.dir.join(&item.name);
    std::fs::write(&path, b"initial").unwrap();
    let mut first_job = None;
    for generation in 0..40 {
        delete(&f, &item, &path);
        std::fs::remove_file(&path).unwrap();
        let latest = format!("complete safe save generation {generation}");
        std::fs::write(&path, latest.as_bytes()).unwrap();
        process_local(&f.ctx, vec![path.clone(); 16]);
        let rows = f.ctx.store.children(f.p.pid, "co.ROOT").unwrap();
        assert_eq!(rows.len(), 1, "safe save generation {generation}");
        assert_eq!(rows[0].id, item.id, "keep the ACC version history");
        assert_eq!(rows[0].local_state, LocalState::Dirty);
        let pending = jobs(&f);
        assert_eq!(
            pending.len(),
            1,
            "safe save generation {generation}: uploads must coalesce"
        );
        assert_eq!(
            pending[0].kind, "upload",
            "a safe save must cancel the delete"
        );
        if let Some(id) = first_job {
            assert_eq!(
                pending[0].id, id,
                "reuse the job that owns the cloud commit intent"
            );
            assert_eq!(
                serde_json::from_str::<Value>(&pending[0].payload).unwrap()["intent_storage"],
                "keep-exact-cloud-object"
            );
        } else {
            first_job = Some(pending[0].id);
            // A lost create/version response may leave an exact object marker in
            // the job. More editor saves must not discard that recovery evidence.
            f.ctx
                .store
                .retarget_job(
                    pending[0].id,
                    "upload",
                    &json!({"path": path, "intent_storage": "keep-exact-cloud-object"}).to_string(),
                )
                .unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), latest.as_bytes());
    }
}

#[tokio::test]
async fn resilience_saves_during_finalization_reuse_the_committed_job() {
    use std::os::windows::fs::MetadataExt;

    let mut f = fixture("resilience-finalize-save");
    // The outbox only runs for signed-in projects: this is an ACC job.
    client_override_signed_in(&mut f);
    let path = f.dir.join("committed.bin");
    std::fs::write(&path, b"first committed bytes").unwrap();
    let before = std::fs::metadata(&path).unwrap();
    let mut item = row(
        &f.p,
        "LIN-finalize",
        Some("co.ROOT"),
        "committed.bin",
        Kind::File,
    );
    item.local_state = LocalState::Dirty;
    f.ctx.store.upsert_item(&item).unwrap();
    let id = f
        .ctx
        .store
        .enqueue(
            f.p.pid,
            "finalize",
            Some(&item.id),
            &json!({
                "path": path, "size": before.len(), "mtime_ticks": before.last_write_time(),
                "version": 3, "storage": "already-committed-object",
            })
            .to_string(),
        )
        .unwrap();
    std::fs::write(&path, b"new edit made during local finalization").unwrap();
    for _ in 0..16 {
        process_local(&f.ctx, vec![path.clone(); 8]);
        let pending = jobs(&f);
        assert_eq!(
            pending.len(),
            1,
            "the finalizer already owns recovery for this item"
        );
        assert_eq!((pending[0].id, pending[0].kind.as_str()), (id, "finalize"));
    }
    assert_eq!(upload::process_due(&f.ctx).await.unwrap(), 0);
    let pending = jobs(&f);
    assert_eq!(pending.len(), 1);
    assert_eq!((pending[0].id, pending[0].kind.as_str()), (id, "upload"));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"new edit made during local finalization"
    );
    assert_eq!(
        f.ctx
            .store
            .item(f.p.pid, &item.id)
            .unwrap()
            .unwrap()
            .local_state,
        LocalState::Dirty
    );
}

#[test]
fn resilience_mass_delete_hold_survives_restart_and_never_releases_itself() {
    let f = fixture("resilience-held-delete");
    for i in 0..60 {
        let item = row(
            &f.p,
            &format!("LIN-{i}"),
            Some("co.ROOT"),
            &format!("drawing-{i}.bin"),
            Kind::File,
        );
        f.ctx.store.upsert_item(&item).unwrap();
        delete(&f, &item, &f.dir.join(&item.name));
        // Duplicate callbacks must not increase the weight or create another job.
        delete(&f, &item, &f.dir.join(&item.name));
    }
    assert_eq!(f.ctx.store.pending_delete_weight(f.p.pid).unwrap(), 60);
    for _ in 0..8 {
        let reopened = Store::open(&f.ctx.paths.db_file).unwrap();
        assert_eq!(reopened.held_deletes().unwrap(), vec![(f.p.pid, 60)]);
        assert_eq!(reopened.next_job_due().unwrap(), None);
        assert!(reopened.due_jobs(i64::MAX, 100).unwrap().is_empty());
    }
    assert_eq!(restore_held(&f.ctx, &f.p).unwrap(), 60);
    assert!(f.ctx.store.held_deletes().unwrap().is_empty());
    assert!(jobs(&f).is_empty());
    assert!(f
        .ctx
        .store
        .children(f.p.pid, "co.ROOT")
        .unwrap()
        .iter()
        .all(|item| item.local_state == LocalState::Clean));
}

#[test]
fn resilience_remote_delete_burst_preserves_every_unsynced_byte() {
    let f = fixture("resilience-remote-delete");
    let mut expected = Vec::new();
    for i in 0..12 {
        let name = format!("unsynced-{i}.bin");
        let path = f.dir.join(&name);
        let bytes = format!("precious local contents {i}").into_bytes();
        std::fs::write(&path, &bytes).unwrap();
        let mut item = row(
            &f.p,
            &format!("LIN-{i}"),
            Some("co.ROOT"),
            &name,
            Kind::File,
        );
        item.local_state = LocalState::Dirty;
        f.ctx.store.upsert_item(&item).unwrap();
        f.ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                Some(&item.id),
                &json!({"path": path}).to_string(),
            )
            .unwrap();
        expected.push((item, path, bytes));
    }
    let delta = Delta {
        deletes: expected
            .iter()
            .map(|(item, _, _)| item.id.clone())
            .collect(),
        ..Default::default()
    };
    let applied = f.ctx.store.apply_delta(f.p.pid, &delta).unwrap();
    assert!(disk_apply(&f.ctx, &f.p, &applied.changes).enqueued);
    let pending = jobs(&f);
    assert_eq!(pending.len(), expected.len());
    for (old, path, bytes) in expected {
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(f.ctx.store.item(f.p.pid, &old.id).unwrap().is_none());
        let replacement = find_child_local(&f.ctx, f.p.pid, "co.ROOT", &old.name).unwrap();
        assert!(is_local_id(&replacement.id));
        assert_eq!(replacement.local_state, LocalState::New);
        assert_eq!(
            pending
                .iter()
                .filter(|job| job.item.as_deref() == Some(replacement.id.as_str())
                    && job.kind == "upload")
                .count(),
            1
        );
    }
}

#[test]
fn resilience_closed_database_retains_pending_commit_and_retry_state() {
    let f = fixture("resilience-db-restart");
    // All connections to this separate DB really close between restarts.
    let db = f.ctx.paths.state_dir.join("restart.db");
    let mut project = f.p.clone();
    let mut item = row(
        &project,
        "LIN-durable",
        Some("co.ROOT"),
        "durable.bin",
        Kind::File,
    );
    item.local_state = LocalState::Dirty;
    let payload = json!({"path": f.dir.join(&item.name), "intent_kind": "version", "intent_storage": "exact-persisted-object", "intent_size": 27, "intent_usn": 1234}).to_string();
    let id = {
        let store = Store::open(&db).unwrap();
        project.pid = store.upsert_project(&project).unwrap();
        item.pid = project.pid;
        store.upsert_item(&item).unwrap();
        let id = store
            .enqueue(project.pid, "upload", Some(&item.id), &payload)
            .unwrap();
        store
            .job_failed(id, 9_000_000_000_000, "injected HTTP 503", false)
            .unwrap();
        id
    };
    for _ in 0..12 {
        let store = Store::open(&db).unwrap();
        assert!(store.due_jobs(0, 10).unwrap().is_empty());
        let pending = store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!((pending[0].id, pending[0].attempts), (id, 1));
        assert_eq!(
            pending[0].payload, payload,
            "never lose commit intent on restart"
        );
        assert_eq!(
            store
                .item(project.pid, &item.id)
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Dirty
        );
        assert_eq!(store.next_job_due().unwrap(), Some(9_000_000_000_000));
    }
    let check = rusqlite::Connection::open(&db).unwrap();
    let result: String = check
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(result, "ok");
}

fn seed() -> u32 {
    std::env::var("SYNKA_TEST_SEED")
        .ok()
        .map(|value| {
            value
                .parse()
                .expect("SYNKA_TEST_SEED must be an unsigned 32-bit integer")
        })
        .unwrap_or(0xBDC0_2026)
}

fn operations(default: usize, ceiling: usize) -> usize {
    std::env::var("SYNKA_STRESS_OPS")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("SYNKA_STRESS_OPS must be a positive integer")
        })
        .unwrap_or(default)
        .clamp(1, ceiling)
}

fn next(state: &mut u32) -> u32 {
    // A small fixed PRNG keeps a seed portable across Rust/toolchain versions.
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *state
}

fn assert_local_model(f: &Fixture, expected: &BTreeMap<usize, Vec<u8>>, seed: u32, step: usize) {
    let rows = f.ctx.store.children(f.p.pid, "co.ROOT").unwrap();
    let pending = jobs(f);
    assert_eq!(
        rows.len(),
        expected.len(),
        "seed={seed} step={step}: row count"
    );
    assert_eq!(
        pending.len(),
        expected.len(),
        "seed={seed} step={step}: queue count"
    );
    for (index, bytes) in expected {
        let name = format!("seeded-{index}.bin");
        let matching: Vec<_> = rows.iter().filter(|row| row.name == name).collect();
        assert_eq!(matching.len(), 1, "seed={seed} step={step}: {name}");
        let item = matching[0];
        assert!(is_local_id(&item.id));
        assert_eq!(item.local_state, LocalState::New);
        assert_eq!(
            pending
                .iter()
                .filter(|job| job.kind == "upload" && job.item.as_deref() == Some(item.id.as_str()))
                .count(),
            1,
            "seed={seed} step={step}: one upload for {name}"
        );
        assert_eq!(
            &std::fs::read(f.dir.join(&name)).unwrap(),
            bytes,
            "seed={seed} step={step}: bytes for {name}"
        );
    }
}

fn seeded_local_sequence(count: usize) {
    let f = fixture("resilience-seeded");
    let initial = seed();
    let mut state = initial;
    let mut expected = BTreeMap::new();
    eprintln!("Synka resilience replay: SYNKA_TEST_SEED={initial} SYNKA_STRESS_OPS={count}");
    for step in 0..count {
        let index = (next(&mut state) % 16) as usize;
        let action = next(&mut state) % 7;
        let path = f.dir.join(format!("seeded-{index}.bin"));
        match action {
            0..=2 => {
                let bytes =
                    format!("seed={initial}; step={step}; value={}", next(&mut state)).into_bytes();
                std::fs::write(&path, &bytes).unwrap();
                process_local(
                    &f.ctx,
                    vec![path.clone(); 1 + (next(&mut state) % 16) as usize],
                );
                expected.insert(index, bytes);
            }
            3 if expected.contains_key(&index) => {
                let item =
                    find_child_local(&f.ctx, f.p.pid, "co.ROOT", &file_name_of(&path)).unwrap();
                delete(&f, &item, &path);
                std::fs::remove_file(&path).unwrap();
                process_local(&f.ctx, vec![path.clone(); 8]);
                expected.remove(&index);
            }
            4 if expected.contains_key(&index) => {
                // Simulate a lost queue entry from an interrupted older engine.
                let item =
                    find_child_local(&f.ctx, f.p.pid, "co.ROOT", &file_name_of(&path)).unwrap();
                f.ctx
                    .store
                    .cancel_jobs(f.p.pid, &item.id, "upload")
                    .unwrap();
                process_local(&f.ctx, vec![path.clone(); 8]);
            }
            5 => {
                let reopened = Store::open(&f.ctx.paths.db_file).unwrap();
                assert_eq!(
                    reopened.due_jobs(i64::MAX, 100).unwrap().len(),
                    expected.len(),
                    "seed={initial} step={step}: reopen queue"
                );
            }
            _ => {
                process_local(&f.ctx, vec![path.clone(); 16]);
            }
        }
        assert_local_model(&f, &expected, initial, step);
    }
}

#[test]
fn resilience_seeded_local_event_sequence_preserves_model() {
    seeded_local_sequence(operations(150, 2_000));
}

#[test]
#[ignore = "long seeded event-spam run; use the resilience stress runner"]
fn resilience_seeded_local_event_soak_preserves_model() {
    seeded_local_sequence(operations(5_000, 100_000));
}

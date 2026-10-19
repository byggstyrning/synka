//! Real Windows reset recovery regressions. The ignored integration test uses a
//! fresh synthetic registration and private NTFS tree, never the installed app's root.

use super::*;
use crate::sync::tests::{fixture, row};
use std::os::windows::fs::OpenOptionsExt;

struct TestRegistration {
    connection: Option<cf::Connection>,
    worker: Option<std::thread::JoinHandle<()>>,
    user: String,
}

impl Drop for TestRegistration {
    fn drop(&mut self) {
        self.connection.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Err(error) = cf::unregister(&self.user) {
            eprintln!("private reset registration cleanup failed: {error:#}");
        }
    }
}

fn connect_fixture(ctx: CtxRef, tag: &str) -> TestRegistration {
    let user = format!(
        "synka-private-reset-{tag}-{}-{}",
        std::process::id(),
        now_ms()
    );
    cf::expose_placeholders();
    cf::register(
        &ctx.paths.sync_root,
        &user,
        "Synka isolated reset test",
        "0.0.0-test",
    )
    .expect("register private test sync root");
    let mut registration = TestRegistration {
        connection: None,
        worker: None,
        user,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    registration.connection =
        Some(cf::connect(&ctx.paths.sync_root, tx).expect("connect private test root"));
    registration.worker = Some(std::thread::spawn(move || {
        while let Some(event) = rx.blocking_recv() {
            match event {
                CfEvent::Rename(req) => {
                    eprintln!(
                        "private rename callback path={} target={} under_reset={}",
                        req.path.display(),
                        req.target_path.display(),
                        under_reset(&req.path)
                    );
                    handle_rename(&ctx, &req);
                }
                CfEvent::Delete(req) => {
                    handle_delete(&ctx, &req);
                }
                CfEvent::FetchPlaceholders(req) => {
                    cb::transfer_placeholders(&req.keys, &[])
                        .expect("test directory listing response");
                }
                CfEvent::FetchData(req) => {
                    cb::fail_data(
                        &req.keys,
                        req.required_offset,
                        req.required_length,
                        STATUS_CLOUD_FILE_UNSUCCESSFUL,
                    );
                }
                CfEvent::CancelFetchData { .. } | CfEvent::CancelFetchPlaceholders => {}
            }
        }
    }));
    registration
}

#[test]
#[ignore = "registers and connects a unique private Cloud Files root; run alone with --ignored --test-threads=1"]
fn resilience_connected_reset_preserves_dirty_bytes_with_plain_and_placeholder_trees() {
    for placeholders in [false, true] {
        let f = fixture(if placeholders {
            "private-reset-cloud"
        } else {
            "private-reset-plain"
        });
        let registration =
            connect_fixture(f.ctx.clone(), if placeholders { "cloud" } else { "plain" });
        let file = f.dir.join("synthetic-model.rvt");
        std::fs::write(&file, b"synthetic unsynced Revit bytes").unwrap();
        let mut item = row(
            &f.p,
            "private-dirty-model",
            Some("co.ROOT"),
            "synthetic-model.rvt",
            Kind::File,
        );
        item.local_state = LocalState::Dirty;
        f.ctx.store.upsert_item(&item).unwrap();
        f.ctx
            .store
            .set_populated(f.p.pid, PROJECT_ROOT_ID, true)
            .unwrap();
        f.ctx.store.set_populated(f.p.pid, "co.ROOT", true).unwrap();
        if placeholders {
            for (path, kind, id, in_sync) in [
                (f.pdir.as_path(), Kind::ProjectRoot, PROJECT_ROOT_ID, true),
                (f.dir.as_path(), Kind::Folder, "co.ROOT", true),
                (file.as_path(), Kind::File, "private-dirty-model", false),
            ] {
                ph::convert_to_placeholder(path, &ph::encode_identity(kind, f.p.pid, id), in_sync)
                    .expect("convert private tree to actual placeholders");
            }
        }
        let held_directory = held_directory(&f.dir, false);
        let locked = begin(&f.ctx, &f.p, None)
            .err()
            .expect("open directory blocks connected reset");
        assert!(format!("{locked:#}").starts_with("Windows nekade åtkomst"));
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"synthetic unsynced Revit bytes"
        );
        assert!(f
            .ctx
            .store
            .item(f.p.pid, "private-dirty-model")
            .unwrap()
            .is_some());
        drop(held_directory);
        let set = begin(&f.ctx, &f.p, None).unwrap_or_else(|error| {
            panic!("connected reset placeholders={placeholders}: {error:#}")
        });
        let aside = set.aside.as_ref().expect("original tree set aside");
        assert_eq!(
            std::fs::read(aside.join("Project Files").join("synthetic-model.rvt")).unwrap(),
            b"synthetic unsynced Revit bytes"
        );
        assert!(f.pdir.is_dir(), "fresh project exists");
        assert!(f
            .ctx
            .store
            .item(f.p.pid, "private-dirty-model")
            .unwrap()
            .is_none());
        let (saved, destination) = finish(&f.ctx, aside, &set.name);
        assert_eq!(saved, 1);
        assert_eq!(
            std::fs::read(
                destination
                    .unwrap()
                    .join("Project Files")
                    .join("synthetic-model.rvt")
            )
            .unwrap(),
            b"synthetic unsynced Revit bytes"
        );
        drop(set);
        drop(registration);
    }
}

fn held_directory(path: &Path, share_delete: bool) -> std::fs::File {
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let mut sharing = FILE_SHARE_READ | FILE_SHARE_WRITE;
    if share_delete {
        sharing |= FILE_SHARE_DELETE;
    }
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(sharing.0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
        .unwrap()
}

#[test]
fn resilience_reset_directory_handle_refusal_preserves_source_and_database() {
    let f = fixture("reset-directory-held");
    let file = f.dir.join("synthetic-model.rvt");
    std::fs::write(&file, b"local recovery bytes").unwrap();
    f.ctx
        .store
        .upsert_item(&row(
            &f.p,
            "private-model",
            Some("co.ROOT"),
            "synthetic-model.rvt",
            Kind::File,
        ))
        .unwrap();
    f.ctx
        .store
        .set_populated(f.p.pid, PROJECT_ROOT_ID, false)
        .unwrap();
    f.ctx.store.set_populated(f.p.pid, "co.ROOT", true).unwrap();
    // The root stays unpopulated, demonstrating that known materialized child folders
    // are still probed without listing the root or downloading any cloud data.
    let job = f
        .ctx
        .store
        .enqueue(f.p.pid, "upload", Some("private-model"), "synthetic-intent")
        .unwrap();
    let held = held_directory(&f.dir, false);
    // Windows metadata-only directory handles can block an ancestor rename even
    // when the direct DELETE-sharing probe succeeds. Do not blame an open app.
    assert!(rename_probe(&f.dir).is_ok());
    let error = begin(&f.ctx, &f.p, None)
        .err()
        .expect("directory handle must prevent rename");
    let detail = format!("{error:#}");
    eprintln!("directory-handle reset refusal: {detail}");
    assert!(detail.starts_with("Windows nekade åtkomst"), "{detail}");
    assert!(!detail.contains("Windows håller"));
    assert!(detail.find("Dina lokala filer finns kvar").unwrap() < detail.find("Sökväg:").unwrap());
    assert_eq!(std::fs::read(&file).unwrap(), b"local recovery bytes");
    assert!(f
        .ctx
        .store
        .item(f.p.pid, "private-model")
        .unwrap()
        .is_some());
    assert_eq!(f.ctx.store.due_jobs(i64::MAX, 10).unwrap()[0].id, job);
    assert!(!project_resetting(f.p.pid));
    drop(held);
}

#[test]
fn resilience_reset_directory_listing_lock_is_named_without_hydration() {
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let f = fixture("reset-directory-listing-held");
    f.ctx
        .store
        .set_populated(f.p.pid, PROJECT_ROOT_ID, false)
        .unwrap();
    f.ctx.store.set_populated(f.p.pid, "co.ROOT", true).unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(&f.dir)
        .unwrap();
    assert!(is_open_elsewhere(&f.dir));
    let open = open_files(&f.ctx, &f.p, &f.pdir, None, 5);
    assert!(
        open.iter().any(|name| name == "mappen Project Files"),
        "{open:?}"
    );
    let error = begin(&f.ctx, &f.p, None)
        .err()
        .expect("listing handle must prevent reset");
    assert!(format!("{error:#}").starts_with("Windows håller"));
    assert!(f.pdir.exists());
    assert!(f.ctx.store.item(f.p.pid, "co.ROOT").unwrap().is_some());
    drop(held);
}

#[test]
fn resilience_reset_delete_probe_accepts_handles_that_share_delete() {
    let f = fixture("reset-benign-reader");
    let held = held_directory(&f.dir, true);
    assert!(
        !is_open_elsewhere(&f.dir),
        "share-delete readers must not be blamed for rename denial"
    );
    assert!(rename_probe(&f.dir).is_ok());
    drop(held);
    let detail = rename_failure(&f.pdir, &std::io::Error::from_raw_os_error(5), &[]).to_string();
    assert!(detail.starts_with("Windows nekade åtkomst"));
    assert!(detail.find("Dina lokala filer finns kvar").unwrap() < detail.find("Sökväg:").unwrap());
    assert!(
        !detail.contains("Stäng program"),
        "do not blame open applications without a blocked handle"
    );
}

#[test]
fn resilience_reset_aside_collision_preserves_both_complete_copies() {
    let f = fixture("reset-aside-collision");
    let source_file = f.dir.join("synthetic-model.rvt");
    std::fs::write(&source_file, b"new unsynced bytes").unwrap();
    let base = ".synka-reset-20261019-101010-Köpmannen";
    let previous = f.pdir.with_file_name(base);
    std::fs::create_dir(&previous).unwrap();
    std::fs::write(previous.join("older-model.rvt"), b"previous unsynced bytes").unwrap();
    let mut guard = Guard::new(vec![(f.pdir.clone(), false)], vec![f.p.pid]);
    let aside = move_aside_named(&f.pdir, base, &mut guard).unwrap();
    assert_ne!(aside, previous);
    assert_eq!(aside.file_name().unwrap(), format!("{base} (1)").as_str());
    assert_eq!(
        std::fs::read(previous.join("older-model.rvt")).unwrap(),
        b"previous unsynced bytes"
    );
    assert_eq!(
        std::fs::read(aside.join("Project Files").join("synthetic-model.rvt")).unwrap(),
        b"new unsynced bytes"
    );
    assert!(!f.pdir.exists());
    assert!(
        f.ctx.store.item(f.p.pid, "co.ROOT").unwrap().is_some(),
        "move helper does not mutate metadata"
    );
}

#[test]
fn resilience_reset_rename_never_replaces_an_existing_empty_directory() {
    let f = fixture("reset-no-replace");
    let target = f.pdir.with_file_name("existing-empty-recovery-folder");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(f.dir.join("synthetic-model.rvt"), b"untouched source bytes").unwrap();
    assert!(rename_no_replace(&f.pdir, &target).is_err());
    assert!(target.is_dir());
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    assert_eq!(
        std::fs::read(f.dir.join("synthetic-model.rvt")).unwrap(),
        b"untouched source bytes"
    );
}

#[test]
fn resilience_reset_database_failure_rolls_back_directory_and_keeps_jobs() {
    let f = fixture("reset-database-rollback");
    let file = f.dir.join("synthetic-model.rvt");
    std::fs::write(&file, b"unsynced bytes survive failed reset").unwrap();
    let mut model = row(
        &f.p,
        "private-model",
        Some("co.ROOT"),
        "synthetic-model.rvt",
        Kind::File,
    );
    model.local_state = LocalState::Dirty;
    f.ctx.store.upsert_item(&model).unwrap();
    let job = f
        .ctx
        .store
        .enqueue(f.p.pid, "upload", Some(&model.id), "exact-synthetic-intent")
        .unwrap();
    let database = rusqlite::Connection::open(&f.ctx.paths.db_file).unwrap();
    database.execute_batch("CREATE TRIGGER fail_private_reset BEFORE DELETE ON item BEGIN SELECT RAISE(ABORT, 'injected reset failure'); END;").unwrap();
    let original_attributes = attributes_of(&f.pdir);
    let error = begin(&f.ctx, &f.p, None)
        .err()
        .expect("trigger must fail reset transaction");
    assert!(
        format!("{error:#}").starts_with("Återställningen avbröts och projektmappen återställdes")
    );
    assert_eq!(
        std::fs::read(file).unwrap(),
        b"unsynced bytes survive failed reset"
    );
    assert_eq!(
        attributes_of(&f.pdir) & 2,
        original_attributes & 2,
        "rollback must restore hidden state"
    );
    assert_eq!(
        f.ctx
            .store
            .item(f.p.pid, &model.id)
            .unwrap()
            .unwrap()
            .local_state,
        LocalState::Dirty
    );
    let pending = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, job);
    assert_eq!(pending[0].payload, "exact-synthetic-intent");
    assert!(!project_resetting(f.p.pid));
    assert!(
        leftovers(&f.ctx).is_empty(),
        "successful rollback must leave no hidden recovery copy"
    );
}

#[test]
fn resilience_reset_readme_explains_cloud_revit_recovery() {
    let introduction = rebuilt_intro("synthetic project");
    assert!(introduction.contains("Revit-molnmodeller ska synkas via Revit"));
}

#[test]
fn resilience_reset_atomic_rename_preserves_deep_path_support() {
    let f = fixture("reset-long-path");
    let parent = f
        .ctx
        .paths
        .state_dir
        .join("nested".repeat(15))
        .join("deeper".repeat(15))
        .join("more".repeat(20));
    let from = parent.join("original");
    let to = parent.join("retained-copy");
    std::fs::create_dir_all(&from).unwrap();
    std::fs::write(
        from.join("synthetic-model.rvt"),
        b"deep local recovery bytes",
    )
    .unwrap();
    assert!(from.as_os_str().encode_wide().count() > 260);
    rename_no_replace(&from, &to).unwrap();
    assert!(!from.exists());
    assert_eq!(
        std::fs::read(to.join("synthetic-model.rvt")).unwrap(),
        b"deep local recovery bytes"
    );
}

#[test]
fn resilience_reset_transient_metadata_handle_recovers_without_losing_bytes() {
    let f = fixture("reset-transient-metadata-handle");
    let source = f.dir.join("synthetic-model.rvt");
    std::fs::write(&source, b"local bytes survive transient Windows handle").unwrap();
    let base = ".synka-reset-20261019-101010-transient";
    let aside = f.pdir.with_file_name(base);
    let held = held_directory(&f.dir, false);
    let error = rename_no_replace(&f.pdir, &aside)
        .expect_err("metadata directory handle must reproduce Windows access denial");
    assert_eq!(error.raw_os_error(), Some(5));
    assert!(f.pdir.is_dir());
    let (release, wait) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        drop(held);
    });
    let mut guard = Guard::new(vec![(f.pdir.clone(), false)], vec![f.p.pid]);
    release.send(()).unwrap();
    let moved = move_aside_named(&f.pdir, base, &mut guard)
        .expect("bounded retries must recover after the metadata handle closes");
    worker.join().unwrap();
    assert_eq!(moved, aside);
    assert!(!f.pdir.exists());
    assert_eq!(
        std::fs::read(moved.join("Project Files").join("synthetic-model.rvt")).unwrap(),
        b"local bytes survive transient Windows handle"
    );
    assert!(
        f.ctx.store.item(f.p.pid, "co.ROOT").unwrap().is_some(),
        "atomic move does not change the database"
    );
}

#[tokio::test]
async fn resilience_partial_rebuild_keeps_tree_and_project_paused_until_restart() {
    let mut f = fixture("reset-partial-rebuild");
    crate::sync::tests::client_override_signed_in(&mut f);
    let file = f.dir.join("synthetic-model.rvt");
    std::fs::write(&file, b"local work survives incomplete rebuild").unwrap();
    let database = rusqlite::Connection::open(&f.ctx.paths.db_file).unwrap();
    database.execute_batch("CREATE TRIGGER fail_private_rebuild BEFORE INSERT ON item BEGIN SELECT RAISE(ABORT, 'injected rebuild failure'); END;").unwrap();
    let engine = Engine::new(f.ctx.clone());
    let _own = engine.maintenance_guard();
    engine
        .clone()
        .reset(Some(f.p.project_id.clone()), None)
        .await;
    assert_eq!(engine.st().incomplete_recoveries.len(), 1);
    engine.publish_now();
    let status: serde_json::Value = serde_json::from_str(&f.ctx.ui.hub.snapshot()[0]).unwrap();
    assert_eq!(status["state"], "error");
    assert!(status["text"].as_str().unwrap().contains("Starta om Synka"));
    assert!(
        project_resetting(f.p.pid),
        "do not resume jobs against a partially rebuilt tree"
    );
    let copies = leftovers(&f.ctx);
    assert_eq!(copies.len(), 1);
    assert_eq!(
        std::fs::read(
            copies[0]
                .0
                .join("Project Files")
                .join("synthetic-model.rvt")
        )
        .unwrap(),
        b"local work survives incomplete rebuild"
    );

    // Even a replacement path created after the failed rebuild must not revive
    // stale work while the user is being asked to restart Synka.
    std::fs::create_dir_all(&f.dir).unwrap();
    let replacement = f.dir.join("new.txt");
    std::fs::write(&replacement, b"new separate local bytes").unwrap();
    assert!(!process_local(&f.ctx, vec![replacement]).enqueued);
    assert!(f.ctx.store.due_jobs(i64::MAX, 10).unwrap().is_empty());
    database
        .execute_batch("DROP TRIGGER fail_private_rebuild")
        .unwrap();
    // Engine shutdown drops these retained errors; model it without ending the
    // test runtime, whose status notifications may still own an Arc<Engine>.
    engine.st().incomplete_recoveries.clear();
    assert!(!project_resetting(f.p.pid));
}

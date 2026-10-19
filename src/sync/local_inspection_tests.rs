//! Local inspection failures must preserve unknown bytes and be retried without login traffic.
use super::tests::{fixture, row, Fixture};
use super::*;

fn changed_file(tag: &str, state: LocalState) -> (Fixture, Item, PathBuf) {
    let f = fixture(tag);
    let mut item = row(
        &f.p,
        "LIN-inspection",
        Some("co.ROOT"),
        "model.rvt",
        Kind::File,
    );
    item.local_state = state;
    f.ctx.store.upsert_item(&item).unwrap();
    let path = f.dir.join("model.rvt");
    std::fs::write(&path, b"possibly edited local model").unwrap();
    (f, item, path)
}

#[test]
fn resilience_failed_placeholder_query_preserves_state_and_retries_real_inspection() {
    for state in [LocalState::Clean, LocalState::Error] {
        let (f, item, path) = changed_file("query-failure", state);
        let mut inspected = 0;
        let work = process_local_with_info(&f.ctx, vec![path.clone(), path.clone()], |_| {
            inspected += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
        });
        assert_eq!(inspected, 1);
        assert_eq!(work.local_retry, vec![path.clone()]);
        assert!(!work.enqueued);
        assert!(work.touched.is_empty());
        assert_eq!(
            f.ctx.store.item(f.p.pid, &item.id).unwrap(),
            Some(item.clone())
        );
        assert!(f.ctx.store.due_jobs(i64::MAX, 10).unwrap().is_empty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"possibly edited local model"
        );

        // Only the failed Cloud Files query was injected. The deferred pass now
        // uses the real Windows query: an ordinary changed file still uploads.
        let work = process_local(&f.ctx, work.local_retry);
        assert!(work.local_retry.is_empty());
        assert!(work.enqueued);
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, &item.id)
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Dirty
        );
        process_local(&f.ctx, vec![path.clone()]);
        let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(jobs.len(), 1, "retry duplicated a pending upload");
        assert_eq!(jobs[0].kind, "upload");
        assert_eq!(jobs[0].item.as_deref(), Some(item.id.as_str()));
        assert_eq!(std::fs::read(path).unwrap(), b"possibly edited local model");
    }
}

fn local_test_session(f: &mut Fixture) {
    // Auth's signed-in admission check reads only this isolated fixture profile;
    // no engine run loop starts and no token exchange or remote API is used.
    let state = f.ctx.paths.state_dir.clone();
    std::fs::write(state.join("token.bin"), b"test-only-session-marker").unwrap();
    std::fs::write(
        state.join("user.json"),
        serde_json::to_vec(&UserInfo {
            user_id: "test-only-local-inspection".into(),
            name: "Test User".into(),
            email: "test@example.invalid".into(),
        })
        .unwrap(),
    )
    .unwrap();
    let http = crate::http::build_client();
    let auth = Arc::new(crate::auth::Auth::new("test".into(), http.clone(), &state));
    let ctx = Arc::get_mut(&mut f.ctx).unwrap();
    ctx.api = crate::http::Api::new(http, auth.clone());
    ctx.auth = auth;
    assert!(ctx.auth.is_signed_in());
}

async fn await_retries(engine: &Engine) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !engine.st().pending_local_retries.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("local retry timer did not drain");
}

#[tokio::test]
async fn resilience_local_inspection_retry_is_deduplicated_and_detects_later_edit() {
    let (mut f, item, path) = changed_file("query-retry", LocalState::Clean);
    local_test_session(&mut f);
    let engine = Engine::new(f.ctx.clone());
    for _ in 0..100 {
        engine.schedule_local_retry(path.clone(), Duration::from_millis(20));
    }
    assert_eq!(engine.st().pending_local_retries.len(), 1);
    assert_eq!(
        engine.st().maintenance_running,
        0,
        "sleep blocked maintenance"
    );
    await_retries(&engine).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.ctx.store.due_jobs(i64::MAX, 10).unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("deferred local edit was not inspected");
    let jobs = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].item.as_deref(), Some(item.id.as_str()));
    assert_eq!(std::fs::read(path).unwrap(), b"possibly edited local model");
}

#[tokio::test]
async fn resilience_local_inspection_retry_stops_while_signed_out() {
    let (f, item, path) = changed_file("query-retry-signed-out", LocalState::Clean);
    let engine = Engine::new(f.ctx.clone());
    for _ in 0..100 {
        engine.schedule_local_retry(path.clone(), Duration::from_millis(20));
    }
    assert_eq!(engine.st().pending_local_retries.len(), 1);
    assert_eq!(engine.st().maintenance_running, 0);
    await_retries(&engine).await;
    assert!(f.ctx.store.due_jobs(i64::MAX, 10).unwrap().is_empty());
    assert_eq!(f.ctx.store.item(f.p.pid, &item.id).unwrap(), Some(item));
    assert_eq!(std::fs::read(path).unwrap(), b"possibly edited local model");
}

#[test]
fn resilience_unsubscribe_refuses_unknown_populated_subtree() {
    for fail_entry in [false, true] {
        let (f, item, path) = changed_file("unsubscribe-query-failure", LocalState::Dirty);
        for id in [PROJECT_ROOT_ID, "co.ROOT"] {
            f.ctx.store.set_populated(f.p.pid, id, true).unwrap();
        }
        let mut inspected = Vec::new();
        let mut read_dir =
            |dir: &Path| -> std::io::Result<Vec<std::io::Result<std::fs::DirEntry>>> {
                inspected.push(dir.to_path_buf());
                if dir == f.dir {
                    let failure = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
                    if fail_entry {
                        Ok(vec![Err(failure)])
                    } else {
                        Err(failure)
                    }
                } else {
                    std::fs::read_dir(dir).map(Iterator::collect)
                }
            };
        assert!(
            !tree_is_clean_with_entries(&f.ctx, f.p.pid, PROJECT_ROOT_ID, &f.pdir, &mut read_dir),
            "unknown subtree allowed unsubscribe"
        );
        assert_eq!(inspected, vec![f.pdir.clone(), f.dir.clone()]);
        assert_eq!(f.ctx.store.item(f.p.pid, &item.id).unwrap(), Some(item));
        assert_eq!(std::fs::read(path).unwrap(), b"possibly edited local model");
    }
}

#[test]
fn resilience_unsubscribe_does_not_enumerate_unpopulated_cloud_folder() {
    let f = fixture("unsubscribe-unpopulated");
    f.ctx
        .store
        .set_populated(f.p.pid, PROJECT_ROOT_ID, true)
        .unwrap();
    f.ctx
        .store
        .set_populated(f.p.pid, "co.ROOT", false)
        .unwrap();
    assert!(
        !f.ctx
            .store
            .item(f.p.pid, "co.ROOT")
            .unwrap()
            .unwrap()
            .populated
    );
    let mut inspected = Vec::new();
    let mut read_dir = |dir: &Path| -> std::io::Result<Vec<std::io::Result<std::fs::DirEntry>>> {
        inspected.push(dir.to_path_buf());
        assert_ne!(dir, f.dir, "cleanliness check hydrated an unopened folder");
        std::fs::read_dir(dir).map(Iterator::collect)
    };
    assert!(tree_is_clean_with_entries(
        &f.ctx,
        f.p.pid,
        PROJECT_ROOT_ID,
        &f.pdir,
        &mut read_dir
    ));
    assert_eq!(inspected, vec![f.pdir.clone()]);
}

#[test]
fn resilience_unsubscribe_refuses_missing_folder_metadata_without_enumeration() {
    let f = fixture("unsubscribe-missing-folder");
    let mut read_dir = |_dir: &Path| -> std::io::Result<std::fs::ReadDir> {
        panic!("missing folder metadata must not trigger cloud enumeration");
    };
    assert!(!tree_is_clean_with_entries(
        &f.ctx,
        f.p.pid,
        "missing-folder",
        &f.dir,
        &mut read_dir
    ));
}

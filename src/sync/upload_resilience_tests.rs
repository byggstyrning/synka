//! Fault injection at the real outbox boundary; no APS account or cloud writes.

use super::*;
use crate::sync::tests::{client_override_signed_in, fixture, row, Fixture};
use std::sync::{Arc, Barrier};

fn queued_file(tag: &str) -> (Fixture, PathBuf, OutboxJob) {
    let f = fixture(tag);
    let path = f.dir.join("pending.bin");
    std::fs::write(&path, b"the only local copy must survive").unwrap();
    let mut item = row(
        &f.p,
        "local-resilience",
        Some("co.ROOT"),
        "pending.bin",
        Kind::File,
    );
    item.remote_version = None;
    item.base_version = None;
    item.local_state = LocalState::New;
    f.ctx.store.upsert_item(&item).unwrap();
    f.ctx
        .store
        .enqueue(
            f.p.pid,
            "upload",
            Some(&item.id),
            &serde_json::json!({"path": path}).to_string(),
        )
        .unwrap();
    let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    (f, path, job)
}

fn assert_bytes(path: &Path) {
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"the only local copy must survive"
    );
}

#[tokio::test]
async fn resilience_queued_upload_rejects_a_parent_replaced_by_real_junction() {
    let (f, initial_path, mut job) = queued_file("resilience-queued-junction");
    let outside = f.ctx.paths.state_dir.join("outside-sync");
    std::fs::create_dir(&outside).unwrap();
    let private = outside.join("pending.bin");
    std::fs::rename(&initial_path, &private).unwrap();
    f.ctx
        .store
        .upsert_item(&row(
            &f.p,
            "co.JUNCTION",
            Some("co.ROOT"),
            "Replaced",
            Kind::Folder,
        ))
        .unwrap();
    let link = f.dir.join("Replaced");
    crate::sync::resilience_tests::directory_junction(&link, &outside);
    let path = link.join("pending.bin");
    let mut item = f
        .ctx
        .store
        .item(f.p.pid, job.item.as_deref().unwrap())
        .unwrap()
        .unwrap();
    item.parent = Some("co.JUNCTION".into());
    f.ctx.store.upsert_item(&item).unwrap();
    job.payload = serde_json::json!({"path": path}).to_string();
    f.ctx
        .store
        .retarget_job(job.id, "upload", &job.payload)
        .unwrap();
    assert!(run_job(f.ctx.clone(), job).await);
    std::fs::remove_dir(&link).unwrap();
    assert_eq!(
        f.ctx.store.job_counts().unwrap(),
        (0, 1),
        "a redirected queued upload must stop before asking for authentication or cloud storage"
    );
    assert_eq!(
        f.ctx
            .store
            .item(f.p.pid, &item.id)
            .unwrap()
            .unwrap()
            .local_state,
        LocalState::Error
    );
    assert!(f.ctx.store.job_summary().unwrap()[0]
        .3
        .as_ref()
        .unwrap()
        .contains("länk"));
    assert_bytes(&private);
}

#[test]
fn resilience_upload_lease_excludes_concurrent_case_aliases_and_releases() {
    let f = fixture("resilience-lease");
    let path = f.dir.join("lease.bin");
    let gate = Arc::new(Barrier::new(17));
    let holding = Arc::new(Barrier::new(16));
    let winners = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..16 {
            let path = if index % 2 == 0 {
                path.clone()
            } else {
                PathBuf::from(path.to_string_lossy().to_uppercase())
            };
            let gate = gate.clone();
            let holding = holding.clone();
            handles.push(scope.spawn(move || {
                gate.wait();
                let lease = UploadLease::take(&path);
                let won = lease.is_some();
                // Every contender tries while the winner still owns its lease.
                holding.wait();
                drop(lease);
                usize::from(won)
            }));
        }
        gate.wait();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum::<usize>()
    });
    assert_eq!(winners, 1, "only one version creation may own a local path");
    assert!(
        UploadLease::take(&path).is_some(),
        "a finished/aborted worker releases its path"
    );
}

#[test]
fn resilience_rejected_upload_lease_never_releases_the_current_owner() {
    let mut f = fixture("resilience-rejected-lease");
    // The outbox only runs for signed-in projects: these jobs are ACC jobs.
    client_override_signed_in(&mut f);
    let path = f.dir.join("owned.bin");
    let owner = UploadLease::take(&path).unwrap();
    for _ in 0..64 {
        assert!(
            UploadLease::take(&path).is_none(),
            "a rejected contender must leave the original lease in place"
        );
    }
    drop(owner);
    assert!(UploadLease::take(&path).is_some());
}

#[tokio::test]
async fn resilience_locked_file_retries_without_spending_attempts_or_bytes() {
    let (f, path, job) = queued_file("resilience-file-lock");
    let writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ_BITS)
        .open(&path)
        .unwrap();
    assert!(
        open_shared(&path).is_err(),
        "the test must create a real Windows sharing violation"
    );
    for attempt in 0..20 {
        f.ctx.store.defer_job(job.id, 0).unwrap();
        let before = now_ms();
        assert_eq!(
            process_due(&f.ctx).await.unwrap(),
            0,
            "locked pass {attempt}"
        );
        let pending = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!((pending[0].id, pending[0].attempts), (job.id, 0));
        assert!(f.ctx.store.next_job_due().unwrap().unwrap() >= before + LOCKED_RETRY_MS);
        assert_eq!(f.ctx.store.job_counts().unwrap(), (1, 0));
        assert_bytes(&path);
    }
    drop(writer);
    assert!(
        open_shared(&path).is_ok(),
        "releasing the editor makes the file readable"
    );
    assert_bytes(&path);
}

#[tokio::test]
async fn resilience_duplicate_jobs_wait_for_current_upload_without_consumption() {
    let (f, path, first) = queued_file("resilience-duplicate-jobs");
    for _ in 0..23 {
        f.ctx
            .store
            .enqueue(f.p.pid, "upload", first.item.as_deref(), &first.payload)
            .unwrap();
    }
    let _lease = UploadLease::take(&path).unwrap();
    assert_eq!(process_due(&f.ctx).await.unwrap(), 0);
    let pending = f.ctx.store.due_jobs(i64::MAX, 100).unwrap();
    assert_eq!(pending.len(), 24, "deferred rows must remain retryable");
    assert!(pending.iter().all(|job| job.attempts == 0));
    assert!(f.ctx.store.due_jobs(now_ms(), 100).unwrap().is_empty());
    assert_bytes(&path);
}

#[test]
fn resilience_extended_offline_and_auth_failures_keep_job_retryable() {
    let (f, path, original) = queued_file("resilience-offline");
    for iteration in 0..256 {
        let error: anyhow::Error = if iteration % 2 == 0 {
            ApiError::Auth(anyhow!("injected signed-out session")).into()
        } else {
            // A real reqwest error, injected before the outbox classification boundary.
            ApiError::Transport(
                reqwest::Client::new()
                    .get("http://[invalid")
                    .build()
                    .unwrap_err(),
            )
            .into()
        };
        let before = now_ms();
        handle_failure(&f.ctx, &original, &error);
        let pending = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!(pending.len(), 1, "offline iteration {iteration}");
        assert_eq!((pending[0].id, pending[0].attempts), (original.id, 0));
        assert!(f.ctx.store.next_job_due().unwrap().unwrap() >= before + OFFLINE_RETRY_MS);
        assert_eq!(f.ctx.store.job_counts().unwrap(), (1, 0));
    }
    let reopened = crate::store::Store::open(&f.ctx.paths.db_file).unwrap();
    assert_eq!(reopened.due_jobs(i64::MAX, 10).unwrap()[0].attempts, 0);
    assert_eq!(
        reopened
            .item(f.p.pid, original.item.as_deref().unwrap())
            .unwrap()
            .unwrap()
            .local_state,
        LocalState::New
    );
    assert_bytes(&path);
}

#[test]
fn resilience_transient_http_errors_back_off_then_allow_manual_recovery() {
    for status in [408, 429, 500, 502, 503, 504] {
        let (f, path, original) = queued_file(&format!("resilience-http-{status}"));
        for attempt in 0..MAX_ATTEMPTS {
            let current = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
            assert_eq!(current.attempts, attempt);
            let before = now_ms();
            handle_failure(
                &f.ctx,
                &current,
                &ApiError::Status {
                    status,
                    body: "injected service failure".into(),
                }
                .into(),
            );
            if attempt + 1 < MAX_ATTEMPTS {
                let pending = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].attempts, attempt + 1);
                let delay = (BASE_BACKOFF_MS << attempt as u32).min(MAX_BACKOFF_MS);
                assert!(f.ctx.store.next_job_due().unwrap().unwrap() >= before + delay);
                assert_eq!(f.ctx.store.job_counts().unwrap(), (1, 0));
            }
        }
        assert_eq!(f.ctx.store.job_counts().unwrap(), (0, 1));
        assert!(f.ctx.store.due_jobs(i64::MAX, 10).unwrap().is_empty());
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, original.item.as_deref().unwrap())
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Error
        );
        assert_bytes(&path);
        assert_eq!(f.ctx.store.retry_dead_jobs().unwrap(), 1);
        let recovered = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
        assert_eq!((recovered[0].id, recovered[0].attempts), (original.id, 0));
        assert_bytes(&path);
    }
}

#[test]
fn resilience_permanent_http_errors_fail_visibly_and_preserve_local_copy() {
    for status in [400, 403, 409, 422] {
        let (f, path, job) = queued_file(&format!("resilience-permanent-{status}"));
        handle_failure(
            &f.ctx,
            &job,
            &ApiError::Status {
                status,
                body: "injected permanent rejection".into(),
            }
            .into(),
        );
        assert_eq!(f.ctx.store.job_counts().unwrap(), (0, 1));
        let summary = f.ctx.store.job_summary().unwrap();
        assert_eq!(summary.len(), 1);
        assert!(summary[0].1);
        assert_eq!(
            summary[0].2, 1,
            "permanent errors fail on the first attempt"
        );
        assert!(summary[0].3.as_ref().unwrap().contains(&status.to_string()));
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, job.item.as_deref().unwrap())
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Error
        );
        assert_bytes(&path);
    }
}

#[tokio::test]
async fn resilience_corrupt_outbox_payload_fails_without_touching_local_bytes() {
    let (mut f, path, job) = queued_file("resilience-corrupt-payload");
    client_override_signed_in(&mut f);
    f.ctx
        .store
        .retarget_job(job.id, "upload", "{interrupted JSON")
        .unwrap();
    assert_eq!(process_due(&f.ctx).await.unwrap(), 1);
    assert_eq!(f.ctx.store.job_counts().unwrap(), (0, 1));
    assert!(f.ctx.store.job_summary().unwrap()[0]
        .3
        .as_ref()
        .unwrap()
        .contains("bad payload"));
    assert_bytes(&path);
}

#[test]
fn resilience_save_between_final_check_and_completion_keeps_one_upload() {
    for kind in ["upload", "finalize"] {
        for adopted_after_snapshot in [false, true] {
            let (f, path, mut completed) = queued_file("resilience-completion-save");
            let local_id = completed.item.clone().unwrap();
            f.ctx
                .store
                .rekey_item(f.p.pid, &local_id, "LIN-completed")
                .unwrap();
            let mut committed = row(
                &f.p,
                "LIN-completed",
                Some("co.ROOT"),
                "pending.bin",
                Kind::File,
            );
            committed.storage_urn = Some("completed-cloud-object".into());
            f.ctx.store.upsert_item(&committed).unwrap();
            f.ctx
                .store
                .retarget_job(completed.id, kind, &completed.payload)
                .unwrap();
            completed.kind = kind.into();
            if !adopted_after_snapshot {
                completed.item = Some(committed.id.clone());
            }

            // The final byte check has passed and the row is clean. Inject the
            // watcher save before the same success path run_job uses deletes it.
            std::fs::write(&path, b"new bytes after final content check").unwrap();
            sync::process_local(&f.ctx, vec![path.clone()]);
            assert_eq!(
                f.ctx
                    .store
                    .item(f.p.pid, &committed.id)
                    .unwrap()
                    .unwrap()
                    .local_state,
                LocalState::Dirty
            );
            let before = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
            assert_eq!(before.len(), 1, "the watcher coalesces with {kind}");
            assert_eq!(before[0].id, completed.id);

            finish_job(&f.ctx, &completed).unwrap();
            let pending = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
            assert_eq!(
                pending.len(),
                1,
                "{kind}; adopted after snapshot={adopted_after_snapshot}: retain the latest save"
            );
            assert_eq!(pending[0].kind, "upload");
            assert_eq!(pending[0].item.as_deref(), Some(committed.id.as_str()));
            assert_eq!(pending[0].attempts, 0);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"new bytes after final content check"
            );
        }
    }
}

#[test]
fn resilience_completion_leaves_clean_plain_file_without_another_upload() {
    for kind in ["upload", "finalize"] {
        let (f, path, mut completed) = queued_file("resilience-completion-clean");
        completed.kind = kind.into();
        f.ctx
            .store
            .retarget_job(completed.id, kind, &completed.payload)
            .unwrap();
        f.ctx
            .store
            .set_local_state(
                f.p.pid,
                completed.item.as_deref().unwrap(),
                LocalState::Clean,
            )
            .unwrap();
        finish_job(&f.ctx, &completed).unwrap();
        assert!(
            f.ctx.store.due_jobs(i64::MAX, 10).unwrap().is_empty(),
            "a clean plain file must stay clean after {kind}"
        );
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, completed.item.as_deref().unwrap())
                .unwrap()
                .unwrap()
                .local_state,
            LocalState::Clean
        );
        assert_bytes(&path);
    }
}

#[test]
fn resilience_completion_restores_new_file_job_without_duplicating_other_work() {
    let (f, path, completed) = queued_file("resilience-completion-new");
    finish_job(&f.ctx, &completed).unwrap();
    let pending = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
    assert_eq!(pending.len(), 1, "a New row still needs an upload");
    assert_eq!(pending[0].item, completed.item);
    assert_bytes(&path);

    // Completing an independent legacy job must respect an already-bound job.
    let legacy_id = f
        .ctx
        .store
        .enqueue(f.p.pid, "upload", None, &completed.payload)
        .unwrap();
    let legacy = f
        .ctx
        .store
        .due_jobs(i64::MAX, 10)
        .unwrap()
        .into_iter()
        .find(|job| job.id == legacy_id)
        .unwrap();
    finish_job(&f.ctx, &legacy).unwrap();
    let after = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, pending[0].id);
}

#[tokio::test]
async fn resilience_legacy_unbound_upload_preserves_existing_finalizer() {
    let (f, path, finalizer) = queued_file("resilience-legacy-finalize");
    f.ctx
        .store
        .retarget_job(finalizer.id, "finalize", &finalizer.payload)
        .unwrap();
    let legacy_id = f
        .ctx
        .store
        .enqueue(f.p.pid, "upload", None, &finalizer.payload)
        .unwrap();
    let legacy = f
        .ctx
        .store
        .due_jobs(i64::MAX, 10)
        .unwrap()
        .into_iter()
        .find(|job| job.id == legacy_id)
        .unwrap();
    assert!(run_job(f.ctx.clone(), legacy).await);
    let pending = f.ctx.store.due_jobs(i64::MAX, 10).unwrap();
    assert_eq!(
        pending.len(),
        1,
        "legacy adoption must not queue work beside its finalizer"
    );
    assert_eq!(
        (pending[0].id, pending[0].kind.as_str()),
        (finalizer.id, "finalize")
    );
    assert_bytes(&path);
}

//! Real local HTTP requests through the upload worker; no Autodesk account or writes.
use super::*;
use crate::sync::tests::{fixture, row, Fixture};
use crate::test_support::{Fault, Server};
use std::sync::Arc;

fn metadata(mime: Option<&str>, extension: &str) -> Value {
    serde_json::json!({
        "data": {"type":"items", "id":"urn:adsk.wipemea:dm.lineage:LIN-target",
            "attributes":{"displayName":"model.rvt"},
            "relationships":{"parent":{"data":{"id":"urn:adsk.wipemea:fs.folder:co.ROOT"}},
                "tip":{"data":{"id":"urn:adsk.wipemea:fs.file:vf.MODEL?version=3"}}}},
        "included":[{"type":"versions", "id":"urn:adsk.wipemea:fs.file:vf.MODEL?version=3",
            "attributes":{"name":"model.rvt", "versionNumber":3, "mimeType":mime,
                "extension":{"type":extension,"version":"1.0"}},
            "relationships":{"storage":{"data":{"id":"old-storage"}}}}]
    })
}

fn pending(server: &Server, tag: &str) -> (Fixture, OutboxJob, PathBuf) {
    let mut f = fixture(tag);
    Arc::get_mut(&mut f.ctx).unwrap().api =
        crate::http::Api::for_test(&server.origin, Duration::from_secs(2));
    let path = f.dir.join("model.rvt");
    std::fs::write(&path, b"local model bytes").unwrap();
    let mut item = row(&f.p, "LIN-target", Some("co.ROOT"), "model.rvt", Kind::File);
    item.local_state = LocalState::Dirty;
    item.storage_urn = Some("old-storage".into());
    f.ctx.store.upsert_item(&item).unwrap();
    f.ctx
        .store
        .enqueue(
            f.p.pid,
            "upload",
            Some(&item.id),
            &serde_json::json!({"path":path}).to_string(),
        )
        .unwrap();
    let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    (f, job, path)
}

#[tokio::test]
async fn resilience_cloud_model_upload_is_rejected_before_any_mutation_even_with_saved_intent() {
    for (mime, extension) in [
        (None, "versions:autodesk.bim360:C4RModel"),
        (
            Some("application/vnd.autodesk.r360"),
            "versions:autodesk.bim360:File",
        ),
    ] {
        let document = metadata(mime, extension).to_string();
        let server = Server::new(move |_, _| Fault::reply(200, document.clone())).await;
        let (f, mut job, path) = pending(&server, "resilience-c4r-upload");
        let snapshot = std::fs::metadata(&path).unwrap();
        job.payload = serde_json::json!({"path":path, "intent_storage":"uncommitted-storage",
            "intent_size":snapshot.len(), "intent_mtime_ms":mtime_ms(&snapshot),
            "intent_mtime_ticks":snapshot.last_write_time(), "intent_kind":"version"})
        .to_string();
        f.ctx
            .store
            .retarget_job(job.id, "upload", &job.payload)
            .unwrap();
        let error = upload(&f.ctx, &job)
            .await
            .err()
            .expect("cloud model was uploaded");
        assert!(error.to_string().contains("Revit cloud model"));
        assert!(!is_transient(&error));
        assert_eq!(server.count(), 1);
        assert!(server
            .requests()
            .iter()
            .all(|request| request.method == "GET"));
        assert_eq!(std::fs::read(path).unwrap(), b"local model bytes");
    }
}

#[tokio::test]
async fn resilience_ordinary_rvt_preserves_known_mime_at_oss_completion() {
    for (status, title, discard) in [
        (400, "VER_MISMATCH_MIMETYPE", true),
        (503, "VER_MISMATCH_MIMETYPE", false),
        (400, "OTHER", false),
    ] {
        let document = metadata(
            Some("application/vnd.autodesk.revit"),
            "versions:autodesk.bim360:File",
        )
        .to_string();
        let version_error = serde_json::json!({
            "errors":[{"code":"ERR_WIP_ERROR","title":title}]})
        .to_string();
        let server = Server::new(move |request, _| {
        if request.method == "GET" && request.target.contains("/items/") {
            Fault::reply(200, document.clone())
        } else if request.method == "POST" && request.target.ends_with("/storage") {
            Fault::reply(
                201,
                r#"{"data":{"type":"objects","id":"urn:adsk.objects:os.object:bucket/fresh.rvt"}}"#,
            )
        } else if request.method == "GET" && request.target.contains("signeds3upload") {
            let host = request.header("host").unwrap();
            Fault::reply(
                200,
                serde_json::json!({"uploadKey":"synthetic",
                "urls":[format!("http://{host}/part")]})
                .to_string(),
            )
        } else if request.method == "PUT" && request.target == "/part" {
            Fault::reply(200, "")
        } else if request.method == "POST" && request.target.contains("signeds3upload") {
            Fault::reply(200, r#"{"size":17}"#)
        } else if request.method == "POST" && request.target.ends_with("/versions") {
            // Stop before local CFAPI finalization; inspect the real version POST too.
            Fault::reply(status, version_error.clone())
        } else {
            Fault::reply(599, "unexpected request")
        }
    })
    .await;
        let (f, job, path) = pending(&server, "resilience-ordinary-rvt-mime");
        let Err(error) = upload(&f.ctx, &job).await else {
            panic!("injected version failure was ignored");
        };
        assert_eq!(
            error.downcast_ref::<ApiError>().and_then(ApiError::status),
            Some(status)
        );
        let current = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
        let fields: Value = serde_json::from_str(&current.payload).unwrap();
        assert_eq!(fields.get("intent_storage").is_none(), discard);
        if !discard {
            assert_eq!(
                fields["intent_storage"],
                "urn:adsk.objects:os.object:bucket/fresh.rvt"
            );
        }
        let requests = server.requests();
        let complete = requests
            .iter()
            .find(|request| request.method == "POST" && request.target.contains("signeds3upload"))
            .expect("ordinary RVT was blocked");
        assert_eq!(
            complete.header("x-ads-meta-content-type"),
            Some("application/vnd.autodesk.revit")
        );
        let part = requests
            .iter()
            .find(|request| request.method == "PUT")
            .unwrap();
        assert_eq!(part.header("authorization"), None);
        assert_eq!(part.header("x-ads-meta-content-type"), None);
        let version = requests
            .iter()
            .find(|request| request.target.ends_with("/versions"))
            .unwrap();
        let body: Value = serde_json::from_slice(&version.body).unwrap();
        assert!(body["data"]["attributes"].get("mimeType").is_none());
        assert_eq!(std::fs::read(path).unwrap(), b"local model bytes");
    }
}

#[test]
fn resilience_rejected_mime_cleanup_preserves_newer_job_payload_and_kind() {
    let f = fixture("resilience-mime-stale-job");
    let original = serde_json::json!({"path":"old-path.rvt", "intent_storage":"rejected-storage"});
    let id = f
        .ctx
        .store
        .enqueue(f.p.pid, "upload", None, &original.to_string())
        .unwrap();
    let stale = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    let current = serde_json::json!({"path":"new-path.rvt", "newer_field":"keep",
        "intent_storage":"rejected-storage", "intent_kind":"version"});
    // Simulate a watcher changing the current job after the HTTP worker took its snapshot.
    f.ctx
        .store
        .retarget_job(id, "upload", &current.to_string())
        .unwrap();
    f.ctx
        .store
        .job_failed(id, 1234, "previous transient error", false)
        .unwrap();
    f.ctx
        .store
        .clear_upload_intent(stale.id, "rejected-storage")
        .unwrap();
    let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    let fields: Value = serde_json::from_str(&job.payload).unwrap();
    assert_eq!(job.kind, "upload");
    assert_eq!(job.attempts, 1, "cleanup reset a newer attempt count");
    assert_eq!(
        fields,
        serde_json::json!({"path":"new-path.rvt", "newer_field":"keep"})
    );

    let replacement = serde_json::json!({"path":"newer-path.rvt", "newer_field":"keep",
        "intent_storage":"possibly-committed-new-storage", "intent_kind":"version"});
    f.ctx
        .store
        .retarget_job(id, "upload", &replacement.to_string())
        .unwrap();
    f.ctx
        .store
        .clear_upload_intent(stale.id, "rejected-storage")
        .unwrap();
    let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    assert_eq!(
        serde_json::from_str::<Value>(&job.payload).unwrap(),
        replacement,
        "old rejection discarded a different potentially committed object"
    );

    let without_intent = serde_json::json!({"path":"newer-path.rvt", "newer_field":"keep"});
    f.ctx
        .store
        .retarget_job(id, "upload", &without_intent.to_string())
        .unwrap();
    f.ctx
        .store
        .clear_upload_intent(stale.id, "rejected-storage")
        .unwrap();
    let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    assert_eq!(
        serde_json::from_str::<Value>(&job.payload).unwrap(),
        without_intent
    );

    let finalize = serde_json::json!({"path":"new-path.rvt", "version":4, "size":17});
    f.ctx
        .store
        .retarget_job(id, "finalize", &finalize.to_string())
        .unwrap();
    f.ctx
        .store
        .clear_upload_intent(stale.id, "rejected-storage")
        .unwrap();
    let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    assert_eq!(
        job.kind, "finalize",
        "stale upload resurrected after finalization"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&job.payload).unwrap(),
        finalize
    );
    f.ctx.store.job_done(id).unwrap();
    f.ctx
        .store
        .clear_upload_intent(stale.id, "rejected-storage")
        .unwrap();
    assert!(f.ctx.store.due_jobs(i64::MAX, 10).unwrap().is_empty());
}

#[test]
fn resilience_only_confirmed_mime_rejection_discards_storage_intent_for_manual_retry() {
    for (status, title, discard) in [
        (400, "VER_MISMATCH_MIMETYPE", true),
        (503, "VER_MISMATCH_MIMETYPE", false),
        (400, "OTHER", false),
    ] {
        let f = fixture("resilience-mime-retry");
        let path = f.dir.join("model.rvt");
        std::fs::write(&path, b"local model bytes").unwrap();
        f.ctx
            .store
            .enqueue(
                f.p.pid,
                "upload",
                None,
                &serde_json::json!({"path":path,"intent_storage":"possibly-committed",
                "intent_size":17,"intent_kind":"version"})
                .to_string(),
            )
            .unwrap();
        let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
        let error: anyhow::Error = ApiError::Status {
            status,
            body: serde_json::json!({
            "errors":[{"code":"ERR_WIP_ERROR","title":title}]})
            .to_string(),
        }
        .into();
        handle_failure(&f.ctx, &job, &error);
        f.ctx.store.retry_dead_jobs().unwrap();
        let retried = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
        let fields: Value = serde_json::from_str(&retried.payload).unwrap();
        assert_eq!(fields.get("intent_storage").is_none(), discard);
        assert_eq!(fields["path"], path.to_string_lossy().as_ref());
        assert_eq!(std::fs::read(path).unwrap(), b"local model bytes");
    }
}

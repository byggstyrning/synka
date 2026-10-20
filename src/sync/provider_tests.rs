//! Several accounts at once: the sync root id, sign-in and sign-out rules, per-provider gating of
//! polls and jobs, and the folder and rename rules of a provider without folder creation
//! (Interaxo, `remote::caps`).

use super::tests::{client_override_signed_in, fixture, row, Fixture};
use super::*;
use crate::app::Secret;
use crate::config::protect_secret;
use crate::http::ApiError;
use crate::model::{ChangeSourceKind, Delta, TopFolder};
use serde_json::json;
use std::os::windows::fs::MetadataExt;

fn user(id: &str) -> UserInfo {
    UserInfo {
        user_id: id.into(),
        name: "Testanvändare".into(),
        email: "test@example.invalid".into(),
    }
}

fn test_live() -> Live {
    let (cf_tx, _) = unbounded_channel();
    let (w_tx, _) = unbounded_channel();
    Live {
        conn: None,
        watch: None,
        cf_tx,
        w_tx,
        sign_ins: HashMap::new(),
        sign_in_generation: 0,
        client_validation: None,
        restart_pending: false,
    }
}

/// A subscribed project of another provider in the same fixture (own hub and folders).
fn other_provider_project(f: &Fixture, provider: Provider) -> Project {
    let mut p = f.p.clone();
    p.pid = 0;
    p.hub_id = format!("{}:hub-1", provider.as_str());
    p.hub_name = format!("{} hub", provider.label());
    p.project_id = format!("{}:proj-1", provider.as_str());
    p.name = "Bygge".into();
    p.provider = provider;
    p.change_source = ChangeSourceKind::PublicWalk;
    p.root_folder_urn = "0".into();
    p.urn_ns = String::new();
    p.oss_scopes = None;
    p.tops = vec![TopFolder {
        id: "0".into(),
        name: "Dokument".into(),
    }];
    p.meta = None;
    p.pid = f.ctx.store.upsert_project(&p).unwrap();
    p
}

/// The fixture's project becomes an Interaxo project in the store.
fn make_interaxo(f: &Fixture) {
    let mut p = f.p.clone();
    p.provider = Provider::Interaxo;
    p.meta = None;
    f.ctx.store.upsert_project(&p).unwrap();
}

/// Makes Interaxo look signed in, the way `client_override_signed_in` does for ACC.
fn interaxo_signed_in(f: &mut Fixture) {
    let state = f.ctx.paths.state_dir.clone();
    std::fs::write(
        state.join("token-interaxo.bin"),
        b"test-only-interaxo-credentials",
    )
    .unwrap();
    std::fs::write(
        state.join("user-interaxo.json"),
        serde_json::to_vec(&user("test-only-interaxo-user")).unwrap(),
    )
    .unwrap();
    let http = crate::http::build_client();
    let interaxo = Arc::new(crate::auth::Auth::with_files(
        String::new(),
        http.clone(),
        &state,
        "token-interaxo.bin",
        "user-interaxo.json",
    ));
    let ctx = Arc::get_mut(&mut f.ctx).unwrap();
    ctx.interaxo_api = crate::http::Api::new(http, interaxo.clone());
    ctx.interaxo = interaxo;
    assert!(f.ctx.interaxo.is_signed_in());
}

fn due(f: &Fixture) -> Vec<(String, Option<String>)> {
    f.ctx
        .store
        .due_jobs(i64::MAX, 100)
        .unwrap()
        .into_iter()
        .map(|j| (j.kind, j.item))
        .collect()
}

/// Notifications whose text contains `needle`.
fn notices(f: &Fixture, needle: &str) -> usize {
    f._rx
        .try_iter()
        .filter(|m| matches!(m, UiMsg::Notify { body, .. } if body.contains(needle)))
        .count()
}

#[test]
fn the_sync_root_keeps_the_acc_registration_and_names_the_other_providers() {
    let acc = user("acc-user-1");
    let streambim = user("sb-user-2");
    assert_eq!(
        root_uid(None, Some(&acc), Some((Provider::StreamBim, &streambim))).as_deref(),
        Some("acc-user-1"),
        "an existing ACC install keeps its id"
    );
    assert_eq!(
        root_uid(None, None, Some((Provider::StreamBim, &streambim))).as_deref(),
        Some("streambim:sb-user-2")
    );
    assert_eq!(
        root_uid(None, None, Some((Provider::Interaxo, &streambim))).as_deref(),
        Some("interaxo:sb-user-2")
    );
    assert_eq!(
        root_uid(Some("stored-1".into()), Some(&acc), None).as_deref(),
        Some("stored-1"),
        "a stored id always wins"
    );
    assert_eq!(
        root_uid(Some("  ".into()), Some(&acc), None).as_deref(),
        Some("acc-user-1"),
        "a blank stored id is no id"
    );
    assert_eq!(root_uid(None, None, None), None);
}

#[test]
fn the_status_user_is_the_first_signed_in_account_in_account_order() {
    let account = |p: Provider, id: &str, signed: bool| AccountStatus {
        provider: p,
        label: p.label(),
        configured: true,
        signed_in: signed,
        signing_in: false,
        user: signed.then(|| user(id)),
        host: None,
    };
    let only_streambim = [
        account(Provider::Acc, "a", false),
        account(Provider::StreamBim, "s", true),
        account(Provider::Interaxo, "i", true),
    ];
    assert_eq!(status_user(&only_streambim).unwrap().user_id, "s");
    let acc_too = [
        account(Provider::Acc, "a", true),
        account(Provider::StreamBim, "s", true),
        account(Provider::Interaxo, "i", true),
    ];
    assert_eq!(status_user(&acc_too).unwrap().user_id, "a");
    assert!(status_user(&[account(Provider::Acc, "a", false)]).is_none());
}

#[test]
fn the_settings_event_reports_the_interaxo_secret_only_as_a_flag() {
    let f = fixture("interaxo-settings-view");
    let secret = "interaxo-test-secret-value";
    let blob = protect_secret(secret).unwrap();
    let mut cfg = f.ctx.cfg_now();
    cfg.interaxo_client_id = Some(" ix-client ".into());
    cfg.interaxo_client_secret_protected = Some(blob.clone());
    *f.ctx.settings.write().unwrap() = cfg;
    let engine = Engine::new(f.ctx.clone());
    engine.publish_settings();
    let view = f
        ._rx
        .try_iter()
        .find_map(|m| match m {
            UiMsg::Settings(view) => Some(view),
            _ => None,
        })
        .expect("a settings event");
    let text = serde_json::to_string(&view).unwrap();
    assert!(view.interaxo_client_secret_set);
    assert_eq!(view.interaxo_client_id.as_deref(), Some("ix-client"));
    assert_eq!(view.interaxo_redirect_uri, INTERAXO_DEFAULT_REDIRECT);
    assert_eq!(
        view.interaxo_redirect_uri_default,
        INTERAXO_DEFAULT_REDIRECT
    );
    assert_eq!(view.providers, vec!["acc", "streambim", "interaxo"]);
    assert!(!text.contains(secret), "the secret is never sent");
    assert!(!text.contains(&blob), "not even its protected form");
}

#[tokio::test]
async fn interaxo_client_settings_are_refused_while_interaxo_is_signed_in() {
    let mut f = fixture("interaxo-refused");
    interaxo_signed_in(&mut f);
    let engine = Engine::new(f.ctx.clone());
    engine
        .set_setting("interaxo_client_id", &json!("new-client"))
        .await;
    assert_eq!(f.ctx.cfg_now().interaxo_client_id, None);
    assert!(!f.ctx.paths.state_dir.join("settings.json").exists());
    assert_eq!(notices(&f, "Logga ut från Interaxo"), 1);
}

#[tokio::test]
async fn a_signed_out_providers_project_is_not_polled_and_its_jobs_wait_without_an_attempt() {
    let mut f = fixture("provider-gating");
    client_override_signed_in(&mut f);
    assert!(f.ctx.signed_in(Provider::Acc));
    assert!(!f.ctx.signed_in(Provider::StreamBim));
    let sb = other_provider_project(&f, Provider::StreamBim);
    let engine = Engine::new(f.ctx.clone());
    engine.st().next_due.insert(sb.pid, Instant::now());
    engine.on_timers();
    assert!(
        !engine.st().next_due.contains_key(&sb.pid),
        "a paused project leaves the schedule"
    );
    assert!(engine.st().polling.is_empty(), "and nothing polls it");

    let job = f.ctx.store.enqueue(sb.pid, "upload", None, "{}").unwrap();
    let processed = upload::process_due(&f.ctx).await.unwrap();
    assert_eq!(processed, 0, "the job waits");
    let waiting = f
        .ctx
        .store
        .due_jobs(i64::MAX, 100)
        .unwrap()
        .into_iter()
        .find(|j| j.id == job)
        .expect("still queued");
    assert_eq!(waiting.attempts, 0, "waiting spends no attempt");
    assert_eq!(waiting.pid, sb.pid);
}

#[test]
fn a_folder_made_here_in_an_interaxo_project_stays_local() {
    let f = fixture("interaxo-folder");
    make_interaxo(&f);
    let folder = f.dir.join("Nytt");
    std::fs::create_dir_all(&folder).unwrap();
    let _ = process_local(&f.ctx, vec![folder.clone()]);
    let _ = process_local(&f.ctx, vec![folder.clone()]);
    std::fs::write(folder.join("ritning.pdf"), b"x").unwrap();
    let _ = process_local(&f.ctx, vec![folder.join("ritning.pdf")]);
    assert!(due(&f).is_empty(), "no mkdir job, and no upload inside it");
    assert_eq!(
        notices(&f, "Interaxo API:t kan inte skapa mappar"),
        1,
        "told once"
    );
    let children = f.ctx.store.children(f.p.pid, "co.ROOT").unwrap();
    assert!(
        children.iter().all(|i| i.name != "Nytt"),
        "the folder has no row"
    );
}

#[test]
fn a_rename_the_provider_refuses_keeps_the_local_name_and_queues_nothing_more() {
    let f = fixture("interaxo-rename");
    make_interaxo(&f);
    f.ctx
        .store
        .upsert_item(&row(&f.p, "I1", Some("co.ROOT"), "a.txt", Kind::File))
        .unwrap();
    let mut w = Work::default();
    record_local_rename(
        &f.ctx,
        &f.p,
        Kind::File,
        "I1",
        &["Project Files".to_string(), "b.txt".to_string()],
        &mut w,
    );
    assert_eq!(
        due(&f),
        vec![("rename".to_string(), Some("I1".to_string()))]
    );

    // Interaxo has no rename: the worker parks the job exactly as run_job does.
    let job = f.ctx.store.due_jobs(i64::MAX, 10).unwrap().remove(0);
    upload::handle_failure(
        &f.ctx,
        &job,
        &anyhow::Error::from(ApiError::Unsupported(
            "Interaxo API:t kan inte byta namn på filer.".into(),
        )),
    );
    assert!(due(&f).is_empty(), "parked once, never retried");
    assert!(f.ctx.store.refused_renames(f.p.pid).unwrap().contains("I1"));

    // The next poll still reports the old name.
    let mut delta = Delta::default();
    delta
        .upserts
        .push(row(&f.p, "I1", Some("co.ROOT"), "a.txt", Kind::File));
    keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
    assert_eq!(delta.upserts[0].name, "b.txt", "the local name wins");
    let applied = f.ctx.store.apply_delta(f.p.pid, &delta).unwrap();
    assert!(
        applied
            .changes
            .iter()
            .all(|(before, after)| match (before, after) {
                (Some(b), Some(a)) => b.name == a.name,
                _ => true,
            }),
        "no rename back on disk"
    );
    assert_eq!(
        f.ctx.store.item(f.p.pid, "I1").unwrap().unwrap().name,
        "b.txt"
    );
    assert!(due(&f).is_empty(), "and no second job");

    // Refused moves of a file and of a folder (Interaxo has no move). Locally they sit in the
    // new folder "fb". The listing of the old parent "fa" still shows them there, the listing of
    // "fb" does not: neither poll may delete them from "fb" and re-create them in "fa" again.
    for dir in [
        row(&f.p, "fa", Some("co.ROOT"), "Ritningar", Kind::Folder),
        row(&f.p, "fb", Some("co.ROOT"), "Arkiv", Kind::Folder),
        row(&f.p, "FX", Some("fb"), "Skiss", Kind::Folder),
    ] {
        f.ctx.store.upsert_item(&dir).unwrap();
    }
    for file in [
        row(&f.p, "F1", Some("fb"), "plan.pdf", Kind::File),
        row(&f.p, "G1", Some("FX"), "bild.jpg", Kind::File),
    ] {
        f.ctx.store.upsert_item(&file).unwrap();
    }
    for id in ["F1", "FX"] {
        let job = f
            .ctx
            .store
            .enqueue(f.p.pid, "move", Some(id), "{}")
            .unwrap();
        f.ctx
            .store
            .job_unsupported(job, "Interaxo API:t kan inte flytta filer.")
            .unwrap();
    }
    let remote = |f: &Fixture| {
        vec![
            (
                "fa",
                vec![
                    row(&f.p, "F1", Some("fa"), "plan.pdf", Kind::File),
                    row(&f.p, "FX", Some("fa"), "Skiss", Kind::Folder),
                ],
            ),
            ("fb", Vec::new()),
            (
                "FX",
                vec![row(&f.p, "G1", Some("FX"), "bild.jpg", Kind::File)],
            ),
        ]
    };
    assert_eq!(refused_move_poll(&f, remote(&f)), 0, "first poll");
    assert_eq!(refused_move_poll(&f, remote(&f)), 0, "second poll");
    for (id, parent) in [("F1", "fb"), ("FX", "fb"), ("G1", "FX")] {
        assert_eq!(
            f.ctx
                .store
                .item(f.p.pid, id)
                .unwrap()
                .unwrap()
                .parent
                .as_deref(),
            Some(parent),
            "{id} keeps its local place"
        );
    }
}

/// One poll over the given folder listings (the walk of `poll_project`, minus the disk); returns
/// how many store changes it applied, which is what the engine reports as activity.
fn refused_move_poll(f: &Fixture, listings: Vec<(&str, Vec<crate::model::Item>)>) -> usize {
    let mut delta = Delta::default();
    for (folder, remote) in listings {
        let local = f.ctx.store.children(f.p.pid, folder).unwrap();
        diff_listing(&local, remote, &mut delta);
    }
    keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
    f.ctx
        .store
        .apply_delta(f.p.pid, &delta)
        .unwrap()
        .changes
        .len()
}

#[tokio::test]
async fn a_streambim_mfa_wait_that_runs_out_ends_as_a_cancelled_sign_in() {
    let f = fixture("mfa-expiry");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    live.sign_ins.insert(
        Provider::StreamBim,
        SignInAttempt {
            task: tokio::spawn(std::future::pending::<()>()),
            generation: 7,
            started_at: Instant::now(),
            mfa_pending: true,
        },
    );
    engine.st().signing_in.insert(Provider::StreamBim);
    engine.on_sign_in_expired(Provider::StreamBim, 7, &mut live);
    assert!(live.sign_ins.is_empty());
    assert!(engine.st().signing_in.is_empty());
    assert!(f._rx.try_iter().any(|m| matches!(
        m,
        UiMsg::SignInProgress {
            provider: Provider::StreamBim,
            state: "cancelled",
            error: Some(ref e),
        } if e == "Tiden för koden gick ut – logga in igen."
    )));
}

#[tokio::test]
async fn a_code_without_a_pending_mfa_wait_is_refused() {
    let f = fixture("mfa-none");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignInMfa {
                provider: Provider::StreamBim,
                code: Secret("123456".into()),
            },
            &mut live,
            &itx,
        )
        .await;
    assert!(live.sign_ins.is_empty());
    assert_eq!(notices(&f, "Ingen kod väntas"), 1);
}

/// An Interaxo project whose API is the loopback `origin`, signed in with the test bearer.
fn interaxo_loopback(f: &mut Fixture, origin: &str) {
    let mut p = f.p.clone();
    p.provider = Provider::Interaxo;
    p.meta = Some(
        json!({
            "base": format!("{origin}/v1"),
            "community": "example-corp",
            "room": "room-1",
            "root": "co.ROOT",
        })
        .to_string(),
    );
    f.ctx.store.upsert_project(&p).unwrap();
    f.p = p;
    interaxo_signed_in(f);
    let ctx = Arc::get_mut(&mut f.ctx).unwrap();
    ctx.interaxo_api = crate::http::Api::for_test(origin, std::time::Duration::from_secs(5));
}

fn interaxo_reply(version: &str) -> crate::test_support::Fault {
    let body = json!({
        "type": "file", "id": "I-1", "parent_id": "co.ROOT", "name": "a.pdf",
        "version": version, "file_size": 14
    });
    crate::test_support::Fault::reply(200, serde_json::to_vec(&body).unwrap())
        .header("Content-Type", "application/json")
}

/// Stores `a.pdf` (remote I-1 at version 1.0, the bytes already on disk) and queues its upload
/// with `payload_extra` merged into the job payload.
fn queue_interaxo_version(f: &Fixture, storage: &str, payload_extra: serde_json::Value) -> PathBuf {
    let path = f.dir.join("a.pdf");
    let mut item = row(&f.p, "I-1", Some("co.ROOT"), "a.pdf", Kind::File);
    item.remote_version = Some(1000);
    item.base_version = Some(1000);
    item.storage_urn = Some(storage.to_string());
    item.size = 14;
    f.ctx.store.upsert_item(&item).unwrap();
    let mut payload = json!({ "path": path });
    payload
        .as_object_mut()
        .unwrap()
        .extend(payload_extra.as_object().unwrap().clone());
    f.ctx
        .store
        .enqueue(f.p.pid, "upload", Some("I-1"), &payload.to_string())
        .unwrap();
    path
}

#[tokio::test]
async fn a_committed_non_acc_version_keeps_the_handle_it_returned() {
    use crate::test_support::Server;
    let server = Server::scripted(vec![
        interaxo_reply("1.0"),
        interaxo_reply("1.0"),
        interaxo_reply("2.0"),
    ])
    .await;
    let mut f = fixture("interaxo-commit");
    interaxo_loopback(&mut f, &server.origin);
    std::fs::write(f.dir.join("a.pdf"), b"new bytes here").unwrap();
    queue_interaxo_version(&f, "interaxo:I-1:1.0", json!({}));

    // The commit is accepted; turning the file into a placeholder fails outside a sync root,
    // so the job then waits for its finalization (`finalize`), as it does after any commit.
    upload::process_due(&f.ctx).await.unwrap();
    assert_eq!(server.count(), 3, "two checks and one commit");
    assert_eq!(server.requests()[2].method, "POST");
    let row = f.ctx.store.item(f.p.pid, "I-1").unwrap().unwrap();
    assert_eq!(row.remote_version, Some(2000));
    assert_eq!(
        row.storage_urn.as_deref(),
        Some("interaxo:I-1:2.0"),
        "the handle the provider returned, not the marker"
    );
    assert_eq!(
        row.version_urn, None,
        "non-ACC providers have no version URN"
    );
}

#[tokio::test]
async fn a_resumed_non_acc_version_intent_never_counts_as_committed() {
    use crate::test_support::Server;
    let server = Server::scripted(vec![
        interaxo_reply("1.0"),
        interaxo_reply("1.0"),
        interaxo_reply("2.0"),
    ])
    .await;
    let mut f = fixture("interaxo-resumed");
    interaxo_loopback(&mut f, &server.origin);
    std::fs::write(f.dir.join("a.pdf"), b"new bytes here").unwrap();
    let meta = std::fs::metadata(f.dir.join("a.pdf")).unwrap();
    // The row carries the pending marker of an earlier attempt and the version intent names
    // that same marker. On ACC this would prove a commit; here the bytes must go up again.
    let marker = "interaxo:pending:0123456789abcdef0123456789abcdef";
    queue_interaxo_version(
        &f,
        marker,
        json!({
            "intent_storage": marker,
            "intent_size": meta.len(),
            "intent_mtime_ms": 0,
            "intent_mtime_ticks": meta.last_write_time(),
            "intent_usn": null,
            "intent_kind": "version",
        }),
    );

    upload::process_due(&f.ctx).await.unwrap();
    assert_eq!(
        server.count(),
        3,
        "the version is uploaded again instead of taken as committed"
    );
    let row = f.ctx.store.item(f.p.pid, "I-1").unwrap().unwrap();
    assert_eq!(row.storage_urn.as_deref(), Some("interaxo:I-1:2.0"));
}

#[test]
fn a_same_name_file_listed_while_our_upload_runs_is_not_a_conflict() {
    let f = fixture("interaxo-own-create");
    make_interaxo(&f);
    let mut local = row(&f.p, "local-1", Some("co.ROOT"), "ritning.pdf", Kind::File);
    local.remote_version = None;
    local.base_version = None;
    local.local_state = LocalState::New;
    f.ctx.store.upsert_item(&local).unwrap();
    let job = f
        .ctx
        .store
        .enqueue(f.p.pid, "upload", Some("local-1"), "{}")
        .unwrap();
    // The poll sees our committed file before the job binds the local row to its ID.
    let mut delta = Delta {
        upserts: vec![row(
            &f.p,
            "I-900",
            Some("co.ROOT"),
            "ritning.pdf",
            Kind::File,
        )],
        ..Default::default()
    };
    keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
    assert!(
        delta.upserts.is_empty(),
        "no conflict copy and no second row for our own upload"
    );

    // Once that upload has finished, a same-name file is a remote file again.
    f.ctx.store.job_done(job).unwrap();
    let mut other = Delta {
        upserts: vec![row(
            &f.p,
            "I-901",
            Some("co.ROOT"),
            "ritning.pdf",
            Kind::File,
        )],
        ..Default::default()
    };
    keep_pending_local(&f.ctx.store, f.p.pid, &mut other).unwrap();
    assert_eq!(other.upserts.len(), 1);
}

#[test]
fn acc_keeps_its_own_matching_rules_for_a_same_name_listing() {
    let f = fixture("acc-same-name");
    let mut local = row(&f.p, "local-1", Some("co.ROOT"), "ritning.pdf", Kind::File);
    local.remote_version = None;
    local.base_version = None;
    local.local_state = LocalState::New;
    f.ctx.store.upsert_item(&local).unwrap();
    f.ctx
        .store
        .enqueue(f.p.pid, "upload", Some("local-1"), "{}")
        .unwrap();
    let mut remote = row(&f.p, "urn-900", Some("co.ROOT"), "ritning.pdf", Kind::File);
    remote.storage_urn = Some("urn:adsk.objects:os.object:bucket/colleague".into());
    let mut delta = Delta {
        upserts: vec![remote],
        ..Default::default()
    };
    keep_pending_local(&f.ctx.store, f.p.pid, &mut delta).unwrap();
    assert_eq!(
        delta.upserts.len(),
        1,
        "a foreign ACC object stays a remote file"
    );
}

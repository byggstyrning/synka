//! Several accounts at once: the sync root id, sign-in and sign-out rules, per-provider gating of
//! polls and jobs, and the folder and rename rules of a provider without folder creation
//! (Interaxo, `remote::caps`).

use super::tests::{
    client_override_signed_in, fixture, fixture_acc_only, fixture_interaxo_off, row, Fixture,
};
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
        account: None,
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

/// The Interaxo refusal the engine sends while Interaxo is off (`config::INTERAXO_OFF_TEXT`).
const INTERAXO_OFF: &str = "Interaxo-inloggning är avstängd i den här versionen.";

#[test]
fn providers_and_accounts_leave_interaxo_out_while_it_is_off() {
    // Off: `settings.providers` names ACC and StreamBIM, and the status has no Interaxo account.
    let off = fixture_interaxo_off("interaxo-off-view");
    let engine = Engine::new(off.ctx.clone());
    engine.publish_settings();
    let view = off
        ._rx
        .try_iter()
        .find_map(|m| match m {
            UiMsg::Settings(view) => Some(view),
            _ => None,
        })
        .expect("a settings event");
    assert_eq!(view.providers, vec!["acc", "streambim"]);
    engine.publish_now();
    let status = last_status(&off);
    assert!(
        status
            .accounts
            .iter()
            .all(|account| account.provider != Provider::Interaxo),
        "no Interaxo account while it is off: {:?}",
        status.accounts
    );
    assert_eq!(
        status.accounts.len(),
        1,
        "ACC only: no StreamBIM account is signed in"
    );

    // On: the same engine lists and reports Interaxo as it always did.
    let on = fixture("interaxo-on-view");
    let engine = Engine::new(on.ctx.clone());
    engine.publish_settings();
    let view = on
        ._rx
        .try_iter()
        .find_map(|m| match m {
            UiMsg::Settings(view) => Some(view),
            _ => None,
        })
        .expect("a settings event");
    assert_eq!(view.providers, vec!["acc", "streambim", "interaxo"]);
    engine.publish_now();
    let status = last_status(&on);
    assert_eq!(
        status.accounts.last().map(|account| account.provider),
        Some(Provider::Interaxo)
    );
}

#[tokio::test]
async fn interaxo_sign_in_and_settings_are_refused_while_it_is_off() {
    let f = fixture_interaxo_off("interaxo-off-refused");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (tx, _) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignIn {
                provider: Provider::Interaxo,
                streambim: None,
            },
            &mut live,
            &tx,
        )
        .await;
    assert!(live.sign_ins.is_empty(), "no browser attempt started");
    assert!(!f.ctx.signed_in(Provider::Interaxo));
    // `notices` drains the window's channel, so each count is what arrived since the last one.
    assert_eq!(notices(&f, INTERAXO_OFF), 1);

    engine
        .set_setting("interaxo_client_id", &json!("new-client"))
        .await;
    assert_eq!(f.ctx.cfg_now().interaxo_client_id, None);
    assert!(!f.ctx.paths.state_dir.join("settings.json").exists());
    assert_eq!(notices(&f, INTERAXO_OFF), 1);
}

#[tokio::test]
async fn a_disabled_interaxo_is_signed_out_for_the_list_polls_and_jobs() {
    let mut f = fixture_interaxo_off("interaxo-off-gating");
    // An Interaxo session from an earlier run stays on disk and is ignored, not deleted.
    interaxo_signed_in(&mut f);
    assert!(f.ctx.interaxo.is_signed_in());
    assert!(!f.ctx.signed_in(Provider::Interaxo));
    assert!(f.ctx.user_for(Provider::Interaxo).is_none());
    assert!(!f.ctx.any_signed_in(), "no ACC or StreamBIM session");
    let ix = other_provider_project(&f, Provider::Interaxo);
    assert!(!f.ctx.project_connected(&ix));

    // Not polled: the scheduler drops the project as it drops a signed-out provider's.
    let engine = Engine::new(f.ctx.clone());
    engine.st().next_due.insert(ix.pid, Instant::now());
    engine.on_timers();
    assert!(!engine.st().next_due.contains_key(&ix.pid));
    assert!(engine.st().polling.is_empty());

    // Its job waits without an attempt.
    let job = f.ctx.store.enqueue(ix.pid, "upload", None, "{}").unwrap();
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

    // Not listed: the hub list names no Interaxo hub, although the project is subscribed in the store.
    engine.refresh_projects().await;
    let listed = f
        ._rx
        .try_iter()
        .filter_map(|m| match m {
            UiMsg::Projects {
                hubs,
                loading: false,
            } => Some(hubs),
            _ => None,
        })
        .last()
        .expect("a projects event");
    assert!(
        listed
            .iter()
            .all(|(hub, _)| hub.provider != Provider::Interaxo),
        "no Interaxo hub while it is off"
    );
    assert!(
        f.ctx.store.project(ix.pid).unwrap().is_some(),
        "the project is kept"
    );
    assert!(
        f.ctx.paths.state_dir.join("token-interaxo.bin").exists(),
        "the session file is kept"
    );
}

#[test]
fn the_acc_only_build_offers_and_reports_acc_alone() {
    let f = fixture_acc_only("acc-only-view");
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
    assert_eq!(view.providers, vec!["acc"]);
    assert_eq!(view.build_providers, vec!["acc"]);
    engine.publish_now();
    let status = last_status(&f);
    assert_eq!(
        status
            .accounts
            .iter()
            .map(|account| account.provider)
            .collect::<Vec<_>>(),
        vec![Provider::Acc]
    );

    // The default build reports all three as built, whatever is offered.
    let full = fixture_interaxo_off("acc-only-full-view");
    let engine = Engine::new(full.ctx.clone());
    engine.publish_settings();
    let view = full
        ._rx
        .try_iter()
        .find_map(|m| match m {
            UiMsg::Settings(view) => Some(view),
            _ => None,
        })
        .expect("a settings event");
    assert_eq!(view.build_providers, vec!["acc", "streambim", "interaxo"]);
}

#[tokio::test]
async fn the_acc_only_build_ignores_streambim_and_interaxo_sessions_and_refuses_sign_in() {
    let mut f = fixture_acc_only("acc-only-gating");
    // Sessions from a default build stay on disk and are ignored, not deleted.
    streambim_signed_in(&mut f, "app");
    interaxo_signed_in(&mut f);
    for provider in [Provider::StreamBim, Provider::Interaxo] {
        assert!(!f.ctx.offers(provider));
        assert!(!f.ctx.signed_in(provider), "{provider:?}");
        assert!(f.ctx.user_for(provider).is_none(), "{provider:?}");
        let p = other_provider_project(&f, provider);
        assert!(!f.ctx.project_connected(&p), "{provider:?}");
    }
    assert!(!f.ctx.any_signed_in());

    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (tx, _) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignIn {
                provider: Provider::Interaxo,
                streambim: None,
            },
            &mut live,
            &tx,
        )
        .await;
    assert!(live.sign_ins.is_empty(), "no browser attempt started");
    assert_eq!(notices(&f, INTERAXO_OFF), 1);

    engine
        .on_cmd(TrayCmd::StreamBimRegions, &mut live, &tx)
        .await;
    assert!(
        f._rx
            .try_iter()
            .all(|m| !matches!(m, UiMsg::StreamBimRegions(_))),
        "no region list is fetched"
    );
    assert!(f.ctx.paths.state_dir.join("token-interaxo.bin").exists());
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

#[test]
fn a_refused_rename_or_move_gives_way_to_a_remote_delete() {
    let f = fixture("interaxo-refused-then-deleted");
    make_interaxo(&f);
    for it in [
        row(&f.p, "fa", Some("co.ROOT"), "Ritningar", Kind::Folder),
        row(&f.p, "fb", Some("co.ROOT"), "Arkiv", Kind::Folder),
        // Renamed here from "a.txt"; the server still has the old name.
        row(&f.p, "I1", Some("co.ROOT"), "b.txt", Kind::File),
        // Moved here from "fa"; the server still has it there.
        row(&f.p, "F1", Some("fb"), "plan.pdf", Kind::File),
    ] {
        f.ctx.store.upsert_item(&it).unwrap();
    }
    for (kind, id) in [("rename", "I1"), ("move", "F1")] {
        let job = f.ctx.store.enqueue(f.p.pid, kind, Some(id), "{}").unwrap();
        f.ctx
            .store
            .job_unsupported(job, "Interaxo API:t stöder inte det.")
            .unwrap();
    }
    let refused = f.ctx.store.refused_renames(f.p.pid).unwrap();
    assert!(refused.contains("I1") && refused.contains("F1"));

    // A colleague deletes both in Interaxo: no listing shows them any more.
    let listings = vec![
        (
            "co.ROOT",
            vec![
                row(&f.p, "fa", Some("co.ROOT"), "Ritningar", Kind::Folder),
                row(&f.p, "fb", Some("co.ROOT"), "Arkiv", Kind::Folder),
            ],
        ),
        ("fa", Vec::new()),
        ("fb", Vec::new()),
    ];
    assert_eq!(refused_move_poll(&f, listings), 2, "both deletes apply");
    for id in ["I1", "F1"] {
        assert!(
            f.ctx.store.item(f.p.pid, id).unwrap().is_none(),
            "{id} is gone after its remote delete"
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
        (Provider::StreamBim, Some("app".into())),
        SignInAttempt {
            task: tokio::spawn(std::future::pending::<()>()),
            generation: 7,
            started_at: Instant::now(),
            mfa_pending: true,
        },
    );
    engine
        .st()
        .signing_in
        .insert((Provider::StreamBim, Some("app".into())));
    engine.on_sign_in_expired((Provider::StreamBim, Some("app".into())), 7, &mut live);
    assert!(live.sign_ins.is_empty());
    assert!(engine.st().signing_in.is_empty());
    assert!(f._rx.try_iter().any(|m| matches!(
        m,
        UiMsg::SignInProgress {
            provider: Provider::StreamBim,
            account: Some(ref account),
            state: "cancelled",
            error: Some(ref e),
        } if e == "Tiden för koden gick ut – logga in igen." && account == "app"
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
                account: "app".into(),
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

#[tokio::test]
async fn an_offline_provider_pauses_only_its_own_jobs() {
    let mut f = fixture("provider-offline-jobs");
    client_override_signed_in(&mut f);
    interaxo_signed_in(&mut f);
    let interaxo = other_provider_project(&f, Provider::Interaxo);
    f.ctx
        .store
        .enqueue(interaxo.pid, "upload", None, "{}")
        .unwrap();
    f.ctx.store.enqueue(f.p.pid, "upload", None, "{}").unwrap();

    let offline = HashSet::from([Provider::Interaxo]);
    let processed = upload::process_due_except(&f.ctx, &offline).await.unwrap();
    assert_eq!(processed, 1, "the ACC job ran although Interaxo is offline");
    let summary = f.ctx.store.job_summary().unwrap();
    let waiting = summary
        .iter()
        .find(|(_, _, _, error)| error.as_deref() == Some("Ingen anslutning till Interaxo."))
        .expect("the Interaxo job waits with the offline reason");
    assert_eq!(waiting.2, 0, "waiting spends no attempt");
    assert!(!waiting.1, "and is not dead");
}

/// The last status event the engine sent.
fn last_status(f: &Fixture) -> Status {
    f._rx
        .try_iter()
        .filter_map(|m| match m {
            UiMsg::Status(status) => Some(status),
            _ => None,
        })
        .last()
        .expect("a status event")
}

#[test]
fn the_engine_is_offline_only_when_every_signed_in_provider_is() {
    let mut f = fixture("provider-offline-status");
    client_override_signed_in(&mut f);
    interaxo_signed_in(&mut f);
    other_provider_project(&f, Provider::Interaxo);
    let engine = Engine::new(f.ctx.clone());

    engine.st().offline.insert(Provider::Interaxo);
    engine.publish_now();
    let status = last_status(&f);
    assert_ne!(
        status.state, "offline",
        "ACC still works, so the engine is online"
    );

    engine.st().offline.insert(Provider::Acc);
    engine.publish_now();
    let status = last_status(&f);
    assert_eq!(status.state, "offline");
    assert_eq!(status.text, "Ingen anslutning till ACC och Interaxo");

    // With ACC alone, the status keeps the text it always had.
    let mut acc_only = fixture("provider-offline-acc-only");
    client_override_signed_in(&mut acc_only);
    let engine = Engine::new(acc_only.ctx.clone());
    engine.st().offline.insert(Provider::Acc);
    engine.publish_now();
    let status = last_status(&acc_only);
    assert_eq!(status.state, "offline");
    assert_eq!(status.text, "Ingen anslutning till ACC");
}

#[test]
fn a_lock_held_by_the_users_own_interaxo_account_is_not_read_only() {
    let mut f = fixture("interaxo-lock-owner");
    interaxo_signed_in(&mut f);
    make_interaxo(&f);
    let p = f.ctx.store.project(f.p.pid).unwrap().unwrap();
    let me = me_for(&f.ctx, Provider::Interaxo);
    assert_eq!(me.as_deref(), Some("Testanvändare"));

    let mut own = row(&p, "I1", Some("co.ROOT"), "a.pdf", Kind::File);
    own.locked_by = Some("Testanvändare".into());
    assert!(
        !spec_for(&own, me.as_deref()).read_only,
        "the user's own lock"
    );

    let mut other = row(&p, "I2", Some("co.ROOT"), "b.pdf", Kind::File);
    other.locked_by = Some("Bo Ek".into());
    assert!(spec_for(&other, me.as_deref()).read_only);
}

/// Writes the token and profile of a signed-in StreamBIM account `key` served at `base`, the way a
/// sign-in leaves them, and adds the account to the fixture. The token is DPAPI-protected; an
/// unreadable file would mean signed out.
pub(super) fn streambim_session(f: &Fixture, key: &str, base: &str) {
    let state = f.ctx.paths.state_dir.clone();
    let tokens = json!({
        "refresh_token": "test-only-refresh",
        "id_token": format!("test-only-id-{key}"),
        "expires_at": 4_102_444_800i64,
    });
    std::fs::write(
        state.join(format!("token-streambim-{key}.bin")),
        crate::auth::dpapi_protect(tokens.to_string().as_bytes()).unwrap(),
    )
    .unwrap();
    let profile = json!({
        "user_id": "test-only-streambim-user",
        "name": "Testanvändare",
        "email": "test@example.invalid",
        "region": key,
        "base": base,
    });
    std::fs::write(
        state.join(format!("user-streambim-{key}.json")),
        profile.to_string(),
    )
    .unwrap();
    let account = f.ctx.streambim.get_or_add(key, base);
    assert!(account.auth.is_signed_in());
}

/// Makes the StreamBIM account of `region` (on its own server) look signed in.
fn streambim_signed_in(f: &mut Fixture, region: &str) {
    streambim_session(f, region, &format!("https://{region}.streambim.com"));
}

#[tokio::test]
async fn a_streambim_project_of_another_server_stays_paused() {
    let mut f = fixture("streambim-region");
    streambim_signed_in(&mut f, "sweden");
    let mut sb = other_provider_project(&f, Provider::StreamBim);
    sb.meta =
        Some(r#"{"region":"app","base":"https://app.streambim.com","project":"1552"}"#.into());
    f.ctx.store.upsert_project(&sb).unwrap();
    assert!(f.ctx.signed_in(Provider::StreamBim));
    assert!(
        !f.ctx.project_connected(&sb),
        "its account (app) is not signed in"
    );
    assert_eq!(f.ctx.sign_in_target(&sb), "StreamBIM (app.streambim.com)");
    let mut same_region = sb.clone();
    same_region.meta = Some(
        r#"{"region":"sweden","base":"https://sweden.streambim.com","project":"1552"}"#.into(),
    );
    assert!(f.ctx.project_connected(&same_region));

    let engine = Engine::new(f.ctx.clone());
    engine.st().next_due.insert(sb.pid, Instant::now());
    engine.on_timers();
    assert!(
        !engine.st().next_due.contains_key(&sb.pid),
        "it leaves the schedule"
    );
    assert!(engine.st().polling.is_empty(), "and nothing polls it");

    f.ctx.store.enqueue(sb.pid, "upload", None, "{}").unwrap();
    assert_eq!(
        upload::process_due(&f.ctx).await.unwrap(),
        0,
        "the job waits"
    );
    let summary = f.ctx.store.job_summary().unwrap();
    assert!(
        summary.iter().any(|(_, _, attempts, error)| *attempts == 0
            && error.as_deref()
                == Some("Logga in på StreamBIM (app.streambim.com) för att synka.")),
        "{summary:?}"
    );
}

#[test]
fn a_signed_out_providers_unsubscribed_projects_leave_the_projects_list() {
    let project = |id: &str, hub: &str, provider: Provider| ProjectInfo {
        id: id.into(),
        hub_id: hub.into(),
        name: id.into(),
        root_folder_urn: None,
        scopes: Vec::new(),
        provider,
        meta: None,
    };
    let hub = |id: &str, provider: Provider| HubInfo {
        id: id.into(),
        name: id.into(),
        region: None,
        provider,
    };
    let previous: HubList = vec![
        (
            hub("streambim:app", Provider::StreamBim),
            vec![
                (
                    project("streambim:app:1", "streambim:app", Provider::StreamBim),
                    false,
                ),
                (
                    project("streambim:app:2", "streambim:app", Provider::StreamBim),
                    true,
                ),
            ],
        ),
        (
            hub("streambim:other", Provider::StreamBim),
            vec![(
                project("streambim:other:3", "streambim:other", Provider::StreamBim),
                false,
            )],
        ),
        (
            hub("b.acc-hub", Provider::Acc),
            vec![(project("b.acc-1", "b.acc-hub", Provider::Acc), false)],
        ),
    ];
    let subscribed: HashSet<String> = HashSet::from(["streambim:app:2".to_string()]);
    let hubs = offline_hubs(Provider::StreamBim, None, &previous, &[], &subscribed);
    assert_eq!(
        hubs.len(),
        1,
        "the hub without a subscribed project is gone"
    );
    assert_eq!(hubs[0].0.id, "streambim:app");
    let ids: Vec<&str> = hubs[0].1.iter().map(|(p, _)| p.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["streambim:app:2"],
        "only the subscribed project stays"
    );
    assert!(hubs[0].1[0].1);
}

#[tokio::test]
async fn a_newer_non_acc_revision_is_not_streamed_into_an_older_placeholder() {
    use crate::cf::{FetchDataReq, OpKeys};
    use crate::test_support::{Fault, Server};
    // The object is now 20 bytes; the placeholder was made for 14. Interaxo's download handle
    // carries no size, so only the response's Content-Length tells.
    let server = Server::scripted(vec![Fault::reply(200, "0123456789abcdefghij")]).await;
    let mut f = fixture("interaxo-download-size");
    interaxo_loopback(&mut f, &server.origin);
    let mut item = row(&f.p, "I-1", Some("co.ROOT"), "a.pdf", Kind::File);
    item.remote_version = Some(2000);
    item.base_version = Some(2000);
    item.storage_urn = Some("interaxo:I-1:2.0".into());
    item.size = 14;
    f.ctx.store.upsert_item(&item).unwrap();
    let engine = Engine::new(f.ctx.clone());
    let req = FetchDataReq {
        keys: OpKeys {
            connection_key: 0,
            transfer_key: 7,
            request_key: 0,
            file_id: 0,
        },
        path: f.dir.join("a.pdf"),
        identity: crate::cf::placeholder::encode_identity(Kind::File, f.p.pid, "I-1"),
        file_size: 14,
        required_offset: 0,
        required_length: 14,
    };
    let err = engine.stream_file(&req, f.p.pid, "I-1").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "filen har ändrats i Interaxo (14 → 20 byte), öppna den igen"
    );
    assert_eq!(
        server.count(),
        1,
        "the object was asked for and not streamed"
    );
}

/// A StreamBIM project whose API calls go to a loopback server.
fn streambim_loopback(f: &mut Fixture, origin: &str) {
    let mut p = f.p.clone();
    p.provider = Provider::StreamBim;
    p.meta = Some(json!({"region": "app", "base": origin, "project": "1552"}).to_string());
    f.ctx.store.upsert_project(&p).unwrap();
    f.p = p;
    streambim_session(f, "app", origin);
}

#[tokio::test]
async fn a_streambim_revision_change_refreshes_the_placeholder_and_fails_the_open() {
    use crate::cf::{FetchDataReq, OpKeys};
    use crate::test_support::{Fault, Server};
    // StreamBIM answers 409 when the file's current revision is not the placeholder's.
    let server = Server::scripted(vec![Fault::reply(409, r#"{"error":"revision"}"#)]).await;
    let mut f = fixture("streambim-revision-409");
    streambim_loopback(&mut f, &server.origin);
    let mut item = row(&f.p, "d1002", Some("0"), "a.pdf", Kind::File);
    item.remote_version = Some(6);
    item.base_version = Some(6);
    item.storage_urn = Some("streambim:d1002:6".into());
    item.size = 14;
    f.ctx.store.upsert_item(&item).unwrap();
    let engine = Engine::new(f.ctx.clone());
    let req = FetchDataReq {
        keys: OpKeys {
            connection_key: 0,
            transfer_key: 7,
            request_key: 0,
            file_id: 0,
        },
        path: f.dir.join("a.pdf"),
        identity: crate::cf::placeholder::encode_identity(Kind::File, f.p.pid, "d1002"),
        file_size: 14,
        required_offset: 0,
        required_length: 14,
    };
    let err = engine
        .stream_file(&req, f.p.pid, "d1002")
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "filen har ändrats i StreamBIM, öppna den igen"
    );
    assert_eq!(server.count(), 1, "one download link was asked for");
    assert!(
        f.ctx
            .store
            .kv_get(&refresh_key(f.p.pid, "d1002"))
            .unwrap()
            .is_some(),
        "the placeholder is scheduled for a refresh"
    );
}

#[test]
fn the_aps_client_id_is_held_by_acc_alone() {
    let mut f = fixture("aps-client-id-holder");
    streambim_signed_in(&mut f, "app");
    let engine = Engine::new(f.ctx.clone());
    assert!(
        engine.client_validation_guard(&test_live()).is_ok(),
        "StreamBIM's session does not block a change of the APS client id"
    );

    let mut acc = fixture("aps-client-id-acc");
    client_override_signed_in(&mut acc);
    let engine = Engine::new(acc.ctx.clone());
    assert!(
        engine.client_validation_guard(&test_live()).is_err(),
        "a signed-in ACC account does"
    );
}

#[tokio::test]
async fn an_interaxo_setting_that_cannot_be_saved_is_not_applied() {
    let f = fixture("interaxo-save-fails");
    // A directory where settings.json belongs: the save fails.
    std::fs::create_dir_all(f.ctx.paths.state_dir.join("settings.json")).unwrap();
    let engine = Engine::new(f.ctx.clone());
    engine
        .set_setting("interaxo_client_id", &json!("new-client"))
        .await;
    assert_eq!(
        f.ctx.cfg_now().interaxo_client_id,
        None,
        "the saved value stays in memory too"
    );
    let sent: Vec<UiMsg> = f._rx.try_iter().collect();
    assert!(sent.iter().any(|m| matches!(
        m,
        UiMsg::Notify { body, .. } if body.contains("Kunde inte spara Interaxo-inställningen")
    )));
    assert!(
        sent.iter().any(|m| matches!(m, UiMsg::Settings(_))),
        "the UI is sent the unchanged settings again"
    );
}

#[tokio::test]
async fn sign_in_steps_are_admitted_during_a_move() {
    let f = fixture("sign-in-during-move");
    let engine = Engine::new(f.ctx.clone());
    engine.st().moving = true;
    let mfa = TrayCmd::SignInMfa {
        provider: Provider::StreamBim,
        account: "app".into(),
        code: Secret("123456".into()),
    };
    assert!(
        engine.begin_command(&mfa).is_some(),
        "a code may be sent during a move"
    );
    let cancel = TrayCmd::CancelSignIn {
        provider: Provider::StreamBim,
        account: None,
    };
    assert!(engine.begin_command(&cancel).is_some());
    let reset = TrayCmd::SignOut {
        provider: None,
        account: None,
    };
    assert!(
        engine.begin_command(&reset).is_none(),
        "other commands still wait for the move"
    );
}

#[tokio::test]
async fn a_sign_in_during_a_rebuild_makes_its_folders_after_the_rebuild() {
    let mut f = fixture("connect-after-rebuild");
    streambim_signed_in(&mut f, "app");
    let sb = other_provider_project(&f, Provider::StreamBim);
    let engine = Engine::new(f.ctx.clone());
    let gate = engine.begin_recovery().unwrap();
    engine.connect_or_defer(Provider::StreamBim).await;
    {
        let st = engine.st();
        assert!(st.deferred_connects.contains(&Provider::StreamBim));
        assert!(st.next_due.is_empty(), "no folder work during the rebuild");
        assert!(st.outbox_next.is_none());
    }
    engine.resume_deferred_connects();
    assert!(
        engine.st().deferred_connects.contains(&Provider::StreamBim),
        "the rebuild still runs"
    );
    drop(gate);
    engine.resume_deferred_connects();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !engine.st().next_due.contains_key(&sb.pid) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the deferred connect did not run");
    let st = engine.st();
    assert!(st.deferred_connects.is_empty());
    assert!(st.outbox_next.is_some());
}

#[tokio::test]
async fn a_second_code_while_one_is_being_checked_is_a_quiet_repeat() {
    let f = fixture("mfa-repeat");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    live.sign_ins.insert(
        (Provider::StreamBim, Some("app".into())),
        SignInAttempt {
            task: tokio::spawn(std::future::pending::<()>()),
            generation: 3,
            started_at: Instant::now(),
            mfa_pending: false,
        },
    );
    let (itx, _irx) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignInMfa {
                provider: Provider::StreamBim,
                account: "app".into(),
                code: Secret("123456".into()),
            },
            &mut live,
            &itx,
        )
        .await;
    assert_eq!(notices(&f, "Ingen kod väntas"), 0, "no warning");
    assert_eq!(
        live.sign_ins
            .get(&(Provider::StreamBim, Some("app".into())))
            .map(|a| a.generation),
        Some(3),
        "the check that is running stays"
    );
}

/// A subscribed StreamBIM project of the account `key` (its own hub, project and meta).
fn streambim_project_of(f: &Fixture, key: &str) -> Project {
    let mut p = f.p.clone();
    p.pid = 0;
    p.provider = Provider::StreamBim;
    p.hub_id = format!("streambim:{key}");
    p.hub_name = format!("StreamBIM {key}");
    p.project_id = format!("streambim:{key}:1552");
    p.name = format!("Bygge {key}");
    p.change_source = ChangeSourceKind::PublicWalk;
    p.root_folder_urn = "0".into();
    p.urn_ns = String::new();
    p.oss_scopes = None;
    p.tops = vec![TopFolder {
        id: "0".into(),
        name: "Dokument".into(),
    }];
    p.meta = Some(
        json!({
            "region": key,
            "base": format!("https://{key}.streambim.com"),
            "project": "1552",
            "account": key,
        })
        .to_string(),
    );
    p.pid = f.ctx.store.upsert_project(&p).unwrap();
    p
}

/// The `sign_in_progress` messages sent so far, as (account, state, error); the other messages are
/// dropped. Used where a refusal must end the window's dialog and nothing else may be sent.
fn sign_in_progress_sent(f: &Fixture) -> Vec<(Option<String>, &'static str, Option<String>)> {
    f._rx
        .try_iter()
        .filter_map(|m| match m {
            UiMsg::SignInProgress {
                provider: Provider::StreamBim,
                account,
                state,
                error,
            } => Some((account, state, error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_second_sign_in_to_a_signed_in_server_ends_as_a_failed_progress() {
    let mut f = fixture("sign-in-refused");
    streambim_signed_in(&mut f, "app");
    let session = f.ctx.paths.state_dir.join("token-streambim-app.bin");
    let session_before = std::fs::read(&session).unwrap();
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    let login = StreamBimLogin {
        server: StreamBimServer::Region("app".into()),
        username: "ada@example.com".into(),
        password: Secret("pw-test".into()),
    };
    engine
        .on_cmd(
            TrayCmd::SignIn {
                provider: Provider::StreamBim,
                streambim: Some(login),
            },
            &mut live,
            &itx,
        )
        .await;
    assert!(live.sign_ins.is_empty(), "no attempt is started");
    assert!(
        engine.st().signing_in.is_empty(),
        "nothing is marked as signing in"
    );
    assert_eq!(
        sign_in_progress_sent(&f),
        vec![(
            Some("app".to_string()),
            "failed",
            Some("Du är redan inloggad på app.streambim.com – logga ut först.".to_string()),
        )]
    );
    assert_eq!(
        notices(&f, "redan inloggad"),
        0,
        "the refusal is no longer a notify"
    );
    let account = f.ctx.streambim.get("app").expect("the account stays");
    assert!(
        account.auth.is_signed_in(),
        "the signed-in account keeps its session"
    );
    assert_eq!(std::fs::read(&session).unwrap(), session_before);
}

#[tokio::test]
async fn a_custom_address_that_is_not_https_ends_as_a_failed_progress_without_an_account() {
    let f = fixture("sign-in-url");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    let login = StreamBimLogin {
        server: StreamBimServer::Url("http://streambim.example.com".into()),
        username: "ada@example.com".into(),
        password: Secret("pw-test".into()),
    };
    engine
        .on_cmd(
            TrayCmd::SignIn {
                provider: Provider::StreamBim,
                streambim: Some(login),
            },
            &mut live,
            &itx,
        )
        .await;
    assert!(live.sign_ins.is_empty());
    let sent = sign_in_progress_sent(&f);
    assert_eq!(sent.len(), 1, "one terminal progress: {sent:?}");
    assert_eq!(sent[0].0, None);
    assert_eq!(sent[0].1, "failed");
    assert!(sent[0]
        .2
        .as_deref()
        .is_some_and(|e| e.starts_with("Adressen måste börja med https://")));
    assert_eq!(notices(&f, "Adressen måste börja med https://"), 0);
}

#[tokio::test]
async fn a_streambim_sign_in_without_a_login_ends_as_a_failed_progress() {
    let f = fixture("sign-in-no-login");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignIn {
                provider: Provider::StreamBim,
                streambim: None,
            },
            &mut live,
            &itx,
        )
        .await;
    assert!(live.sign_ins.is_empty());
    assert_eq!(
        sign_in_progress_sent(&f),
        vec![(
            None,
            "failed",
            Some("Ange region, e-postadress och lösenord för StreamBIM.".to_string()),
        )]
    );
}

#[tokio::test]
async fn a_streambim_sign_in_refused_by_a_move_or_a_restart_ends_as_a_failed_progress() {
    let f = fixture("sign-in-refused-move");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    let login = || StreamBimLogin {
        server: StreamBimServer::Region("app".into()),
        username: "ada@example.com".into(),
        password: Secret("pw-test".into()),
    };
    let sign_in = || TrayCmd::SignIn {
        provider: Provider::StreamBim,
        streambim: Some(login()),
    };

    engine.st().moving = true;
    engine.on_cmd(sign_in(), &mut live, &itx).await;
    engine.st().moving = false;
    assert!(live.sign_ins.is_empty());
    assert!(engine.st().signing_in.is_empty());
    assert_eq!(
        sign_in_progress_sent(&f),
        vec![(
            Some("app".to_string()),
            "failed",
            Some("Vänta tills den pågående återställningen eller flytten är klar.".to_string()),
        )]
    );

    live.restart_pending = true;
    engine.on_cmd(sign_in(), &mut live, &itx).await;
    engine
        .on_cmd(
            TrayCmd::SignIn {
                provider: Provider::Acc,
                streambim: None,
            },
            &mut live,
            &itx,
        )
        .await;
    assert!(live.sign_ins.is_empty());
    assert_eq!(
        sign_in_progress_sent(&f),
        vec![(
            Some("app".to_string()),
            "failed",
            Some("Synka startar redan om. Försök igen efter omstarten.".to_string()),
        )],
        "a restart ends a StreamBIM sign-in only; an ACC command stays silent"
    );
}

#[tokio::test]
async fn an_acc_sign_in_refused_during_a_move_keeps_its_warning_notify() {
    let f = fixture("acc-sign-in-refused-move");
    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    engine.st().moving = true;
    engine
        .on_cmd(
            TrayCmd::SignIn {
                provider: Provider::Acc,
                streambim: None,
            },
            &mut live,
            &itx,
        )
        .await;
    assert_eq!(notices(&f, "Vänta tills den pågående återställningen"), 1);
    assert!(sign_in_progress_sent(&f).is_empty());
}

#[tokio::test]
async fn signing_out_one_account_pauses_only_its_projects_and_keeps_the_root() {
    let mut f = fixture("sign-out-one-account");
    streambim_signed_in(&mut f, "app");
    streambim_signed_in(&mut f, "japan");
    let app = streambim_project_of(&f, "app");
    let japan = streambim_project_of(&f, "japan");
    let engine = Engine::new(f.ctx.clone());
    {
        let mut st = engine.st();
        st.next_due.insert(app.pid, Instant::now());
        st.next_due.insert(japan.pid, Instant::now());
    }
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignOut {
                provider: Some(Provider::StreamBim),
                account: Some("japan".into()),
            },
            &mut live,
            &itx,
        )
        .await;
    // Japan's files are gone; app's stay, and the account stays signed in.
    let state = f.ctx.paths.state_dir.clone();
    assert!(!state.join("token-streambim-japan.bin").exists());
    assert!(!state.join("user-streambim-japan.json").exists());
    assert!(state.join("token-streambim-app.bin").exists());
    assert!(f.ctx.streambim.get("japan").is_none());
    assert!(f.ctx.streambim.get("app").unwrap().auth.is_signed_in());
    // The root stays registered: something is still signed in.
    assert!(f.ctx.any_signed_in());
    let st = engine.st();
    assert!(st.next_due.contains_key(&app.pid), "app keeps polling");
    assert!(!st.next_due.contains_key(&japan.pid), "japan is paused");
}

#[tokio::test]
async fn an_account_sign_out_leaves_the_other_providers_signed_in() {
    let mut f = fixture("sign-out-keeps-acc");
    client_override_signed_in(&mut f);
    streambim_signed_in(&mut f, "japan");
    let japan = streambim_project_of(&f, "japan");
    let engine = Engine::new(f.ctx.clone());
    engine.st().next_due.insert(japan.pid, Instant::now());
    let mut live = test_live();
    let (itx, _irx) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignOut {
                provider: Some(Provider::StreamBim),
                account: None,
            },
            &mut live,
            &itx,
        )
        .await;
    assert!(f.ctx.signed_in(Provider::Acc), "ACC is untouched");
    assert!(!f.ctx.signed_in(Provider::StreamBim));
    assert!(!engine.st().next_due.contains_key(&japan.pid));
}

#[tokio::test]
async fn status_accounts_are_one_entry_per_streambim_account_in_key_order() {
    let mut f = fixture("status-accounts");
    streambim_signed_in(&mut f, "japan");
    streambim_signed_in(&mut f, "app");
    let engine = Engine::new(f.ctx.clone());
    // A third account is signing in (no session yet): it is listed too.
    engine
        .st()
        .signing_in
        .insert((Provider::StreamBim, Some("sweden".into())));
    let accounts = engine.account_statuses();
    let shape: Vec<(Provider, Option<String>, bool, bool)> = accounts
        .iter()
        .map(|a| (a.provider, a.account.clone(), a.signed_in, a.signing_in))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Provider::Acc, None, false, false),
            (Provider::StreamBim, Some("app".into()), true, false),
            (Provider::StreamBim, Some("japan".into()), true, false),
            (Provider::StreamBim, Some("sweden".into()), false, true),
            (Provider::Interaxo, None, false, false),
        ]
    );
    let japan = accounts
        .iter()
        .find(|a| a.account.as_deref() == Some("japan"))
        .unwrap();
    assert_eq!(japan.host.as_deref(), Some("japan.streambim.com"));
    assert_eq!(japan.label, "StreamBIM");
    assert!(japan.user.is_some());
    // The status user is the first signed-in entry: app.
    assert_eq!(
        status_user(&accounts).unwrap().email,
        "test@example.invalid"
    );
    assert_eq!(accounts[0].host, None, "ACC has no host");
}

/// A wrong StreamBIM code keeps the sign-in on its code step: the window is asked for the code
/// again, with the reason, and the password is not needed again (StreamBIM's own shape: the
/// challenge comes with HTTP 401 and a wrong code is `{"error": "invalidCode"}`).
#[tokio::test]
async fn a_wrong_streambim_code_asks_for_the_code_again() {
    use crate::test_support::{Fault, Server};
    let f = fixture("mfa-wrong-code");
    let reply = |status: u16, body: serde_json::Value| {
        Fault::reply(status, body.to_string()).header("Content-Type", "application/json")
    };
    let server = Server::scripted(vec![
        reply(
            401,
            json!({"result": "CHALLENGE_REQUESTED", "session": "sess-1"}),
        ),
        reply(400, json!({"error": "invalidCode"})),
    ])
    .await;
    let account = f.ctx.streambim.get_or_add("app", &server.origin);
    assert!(matches!(
        account
            .auth
            .login("ada@example.com", "pw-test")
            .await
            .unwrap(),
        LoginOutcome::MfaRequired
    ));

    let engine = Engine::new(f.ctx.clone());
    let mut live = test_live();
    let key: SignInKey = (Provider::StreamBim, Some("app".into()));
    live.sign_ins.insert(
        key.clone(),
        SignInAttempt {
            task: tokio::spawn(std::future::pending::<()>()),
            generation: 4,
            started_at: Instant::now(),
            mfa_pending: true,
        },
    );
    engine.st().signing_in.insert(key.clone());
    let (itx, mut irx) = unbounded_channel();
    engine
        .on_cmd(
            TrayCmd::SignInMfa {
                provider: Provider::StreamBim,
                account: "app".into(),
                code: Secret("111111".into()),
            },
            &mut live,
            &itx,
        )
        .await;
    let checked = tokio::time::timeout(Duration::from_secs(5), irx.recv())
        .await
        .expect("the code was not checked");
    let Some(Internal::SignedIn {
        key: done,
        generation,
        result,
    }) = checked
    else {
        panic!("no sign-in result");
    };
    engine
        .on_signed_in(done, generation, result, &mut live, &itx)
        .await;

    assert!(
        live.sign_ins
            .get(&key)
            .is_some_and(|attempt| attempt.mfa_pending && attempt.generation == 4),
        "the attempt waits for a code again"
    );
    assert!(engine.st().signing_in.contains(&key));
    assert!(account.auth.has_pending_mfa(), "the challenge is kept");
    assert_eq!(
        sign_in_progress_sent(&f),
        vec![(
            Some("app".to_string()),
            "mfa_required",
            Some("Fel kod. Ange koden som autentiseringsappen visar nu.".to_string()),
        )]
    );
}

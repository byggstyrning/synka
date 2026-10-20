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

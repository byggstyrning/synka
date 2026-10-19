//! The undocumented dm/v3 feed is never called with the default configuration, and updates
//! stay off until an update channel is configured.
use super::tests::fixture;
use super::*;
use crate::test_support::{Fault, Server};

/// A fake ACC: one top folder, empty contents, and 404 for everything else, dm/v3 included.
async fn acc() -> Server {
    Server::new(|r, _| {
        if r.target.contains("/dm/v3/") {
            Fault::reply(404, "{}")
        } else if r.target.contains("/topFolders") {
            Fault::reply(
                200,
                r#"{"data":[{"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.ROOT","attributes":{"name":"Project Files"}}]}"#,
            )
        } else if r.target.contains("/contents") {
            Fault::reply(200, r#"{"data":[]}"#)
        } else {
            Fault::reply(404, "{}")
        }
    })
    .await
}

async fn poll_once(feed: bool, start: ChangeSourceKind) -> (Server, Project) {
    let server = acc().await;
    let mut f = fixture(&format!("change-source-{feed}-{start:?}"));
    {
        let ctx = Arc::get_mut(&mut f.ctx).unwrap();
        ctx.api = crate::http::Api::for_test(&server.origin, Duration::from_secs(2));
        ctx.cfg.desktop_sync_feed = feed;
    }
    let pid = f.p.pid;
    f.ctx.store.set_change_source(pid, start).unwrap();
    f.ctx
        .store
        .apply_delta(
            pid,
            &Delta {
                new_cursor: Some(r#"{"f":"old","d":"old"}"#.into()),
                ..Default::default()
            },
        )
        .unwrap();
    f.ctx.store.set_populated(pid, "co.ROOT", true).unwrap();
    let engine = Engine::new(f.ctx.clone());
    engine.poll_project(pid).await.unwrap();
    let p = f.ctx.store.project(pid).unwrap().unwrap();
    (server, p)
}

#[tokio::test]
async fn default_config_never_calls_dm_v3_and_lists_instead() {
    for start in [
        ChangeSourceKind::Unknown,
        ChangeSourceKind::DesktopSync,
        ChangeSourceKind::PublicWalk,
    ] {
        let (server, p) = poll_once(false, start).await;
        let targets: Vec<String> = server.requests().into_iter().map(|r| r.target).collect();
        assert!(
            targets.iter().all(|t| !t.contains("/dm/v3/")),
            "{start:?}: {targets:?}"
        );
        assert!(targets
            .iter()
            .any(|t| t.contains("/data/v1/") && t.contains("/contents")));
        assert_eq!(p.change_source, ChangeSourceKind::PublicWalk);
        if start != ChangeSourceKind::PublicWalk {
            assert!(p.cursor.is_none(), "{start:?} kept its feed cursor");
        }
    }
}

#[tokio::test]
async fn feed_is_only_tried_when_explicitly_enabled() {
    let (server, p) = poll_once(true, ChangeSourceKind::Unknown).await;
    assert!(server
        .requests()
        .iter()
        .any(|r| r.target.contains("/dm/v3/")));
    // 404 from dm/v3 -> NotSupported -> listing
    assert_eq!(p.change_source, ChangeSourceKind::PublicWalk);
}

#[test]
fn default_config_has_updates_and_feed_off() {
    let cfg = crate::config::Config::default();
    assert!(cfg.update_channel.trim().is_empty());
    assert!(!cfg.auto_update);
    assert!(!cfg.desktop_sync_feed);
}

#[tokio::test]
async fn updates_off_reports_only_on_manual_check_and_never_installs() {
    let f = fixture("updates-off");
    let engine = Engine::new(f.ctx.clone());

    engine.check_updates(false);
    assert!(f._rx.try_iter().next().is_none());

    engine.check_updates(true);
    engine.clone().install_update(true).await;
    let off: Vec<bool> = f
        ._rx
        .try_iter()
        .map(|m| {
            matches!(
                m,
                UiMsg::Update { available: false, error: Some(ref e), .. } if e == UPDATES_OFF
            )
        })
        .collect();
    assert_eq!(off, vec![true, true]);
}

#[test]
fn settings_report_update_channel_not_configured() {
    let f = fixture("updates-settings");
    let engine = Engine::new(f.ctx.clone());
    engine.publish_settings();
    let flags: Vec<bool> = f
        ._rx
        .try_iter()
        .filter_map(|m| match m {
            UiMsg::Settings(v) => Some(v.update_channel_configured),
            _ => None,
        })
        .collect();
    assert_eq!(flags, vec![false]);
}

//! A 429 pauses the whole provider: the folder walk stops at the first failure, and the
//! provider's projects wait for the pause instead of failing one by one.
use super::tests::{client_override_signed_in, fixture, row, Fixture};
use super::*;
use crate::test_support::{Fault, Server};

/// Populated folders under the top folder, many more than the walk lists at a time (4).
const FOLDERS: usize = 20;

/// A fake ACC: one top folder, and every folder listing answers `contents` (200: empty).
async fn acc(contents: u16) -> Server {
    Server::new(move |r, _| {
        if r.target.contains("/topFolders") {
            Fault::reply(
                200,
                r#"{"data":[{"type":"folders","id":"urn:adsk.wipemea:fs.folder:co.ROOT","attributes":{"name":"Project Files"}}]}"#,
            )
        } else if r.target.contains("/contents") && contents == 200 {
            Fault::reply(200, r#"{"data":[]}"#)
        } else if r.target.contains("/contents") {
            Fault::reply(contents, r#"{"developerMessage":"Quota limit exceeded."}"#)
        } else {
            Fault::reply(404, "{}")
        }
    })
    .await
}

fn use_server(f: &mut Fixture, server: &Server) {
    Arc::get_mut(&mut f.ctx).unwrap().api =
        crate::http::Api::for_test(&server.origin, Duration::from_secs(2));
}

/// The fixture against `server`, with FOLDERS populated folders under its top folder.
fn walked(tag: &str, server: &Server) -> Fixture {
    let mut f = fixture(tag);
    use_server(&mut f, server);
    let pid = f.p.pid;
    f.ctx.store.set_populated(pid, "co.ROOT", true).unwrap();
    for i in 0..FOLDERS {
        let id = format!("co.F{i}");
        f.ctx
            .store
            .upsert_item(&row(
                &f.p,
                &id,
                Some("co.ROOT"),
                &format!("Mapp {i}"),
                Kind::Folder,
            ))
            .unwrap();
        f.ctx.store.set_populated(pid, &id, true).unwrap();
    }
    f
}

fn listings(server: &Server) -> usize {
    server
        .requests()
        .iter()
        .filter(|r| r.target.contains("/contents"))
        .count()
}

#[tokio::test]
async fn a_429_ends_the_walk_and_pauses_the_provider_not_the_project() {
    let server = acc(429).await;
    let f = walked("throttle-walk", &server);
    let pid = f.p.pid;
    let engine = Engine::new(f.ctx.clone());

    let err = engine.poll_project(pid).await.unwrap_err();
    assert!(throttle::is_429(&err), "{err:#}");
    // At most the 4 folders in flight, each with its first try and 3 retries. Without the early
    // stop every folder would be asked 4 times (80 requests).
    let asked = listings(&server);
    assert!((4..=16).contains(&asked), "{asked} folder listings");

    engine.st().next_due.insert(pid, Instant::now());
    let before = Instant::now();
    engine.finish_poll(pid, Err(err));
    let left = f.ctx.throttle.remaining(Provider::Acc).expect("ACC is paused");
    assert!(left > Duration::from_secs(55), "{left:?}");
    assert!(f.ctx.throttle.remaining(Provider::StreamBim).is_none());
    let st = engine.st();
    assert!(
        st.next_due[&pid] >= before + Duration::from_secs(55),
        "the project waits for the pause"
    );
    assert!(st.last_error.is_none(), "{:?}", st.last_error);
    assert!(!st.fails.contains_key(&pid), "not the project's failure");
}

#[tokio::test]
async fn a_paused_provider_is_not_asked_and_its_pause_is_not_lengthened() {
    let server = acc(200).await;
    let f = walked("throttle-paused", &server);
    let pid = f.p.pid;
    f.ctx.throttle.hit(Provider::Acc);
    let engine = Engine::new(f.ctx.clone());

    let err = engine.poll_project(pid).await.unwrap_err();
    assert!(throttle::is_paused(&err), "{err:#}");
    assert_eq!(server.count(), 0, "no request while paused");

    let left = f.ctx.throttle.remaining(Provider::Acc).unwrap();
    engine.st().next_due.insert(pid, Instant::now());
    engine.finish_poll(pid, Err(err));
    assert!(f.ctx.throttle.remaining(Provider::Acc).unwrap() <= left);
    assert!(engine.st().last_error.is_none());
}

#[tokio::test]
async fn a_good_walk_lists_every_folder() {
    let server = acc(200).await;
    let f = walked("throttle-good", &server);
    let engine = Engine::new(f.ctx.clone());
    engine.poll_project(f.p.pid).await.unwrap();
    assert_eq!(listings(&server), FOLDERS + 1, "every folder and the top folder");
    assert!(f.ctx.throttle.remaining(Provider::Acc).is_none());
}

#[tokio::test]
async fn due_projects_of_a_paused_provider_wait_even_after_sync_now() {
    let server = acc(200).await;
    let mut f = walked("throttle-timers", &server);
    client_override_signed_in(&mut f);
    use_server(&mut f, &server);
    let pid = f.p.pid;
    f.ctx.throttle.hit(Provider::Acc);
    let left = f.ctx.throttle.remaining(Provider::Acc).unwrap();
    let engine = Engine::new(f.ctx.clone());

    let now = Instant::now();
    engine.st().next_due.insert(pid, now);
    engine.on_timers();
    let st = engine.st();
    assert!(!st.polling.contains(&pid), "no poll while paused");
    assert!(st.next_due[&pid] >= now + left, "it waits for the pause");
    drop(st);
    assert_eq!(server.count(), 0);
}

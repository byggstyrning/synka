//! Per-folder poll tiers (StreamBIM): between full walks a poll lists only the active folders, and
//! an item that leaves an active folder is looked for in the quiet ones before anything is applied.
use super::provider_tests::streambim_session;
use super::tests::{fixture, login_test_live, row, Fixture};
use super::*;
use crate::streambim_auth::StreamBimAccounts;
use crate::test_support::{Fault, Server};

/// Populated folders "1".."8" under the top folder "0".
const FOLDERS: usize = 8;

/// The documents the fake StreamBIM shows in each folder, as JSON:API resources.
type Docs = Arc<Mutex<HashMap<String, Vec<Value>>>>;

fn doc(id: &str, name: &str) -> Value {
    json!({"id": id, "type": "documents", "attributes": {"filename": name, "revision": 1}})
}

/// The value of the query parameter `key` (percent-encoded, with its '=') in `target`.
fn param(target: &str, key: &str) -> Option<String> {
    let rest = target.split_once(key)?.1;
    Some(rest.split('&').next().unwrap_or_default().to_string())
}

/// A fake StreamBIM project: the top folder "0" holds "Mapp 1".."Mapp 8" (ids "1".."8"), and
/// each folder's documents come from `docs`.
async fn streambim(docs: Docs) -> Server {
    Server::new(move |r, _| {
        let data: Vec<Value> = if let Some(parent) = param(&r.target, "filter%5Bparent%5D=") {
            if parent == "0" {
                (1..=FOLDERS)
                    .map(|i| {
                        json!({
                            "id": i.to_string(),
                            "type": "folders",
                            "attributes": {"name": format!("Mapp {i}")}
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            }
        } else if let Some(folder) = param(&r.target, "filter%5BfolderId%5D=") {
            docs.lock()
                .unwrap()
                .get(&folder)
                .cloned()
                .unwrap_or_default()
        } else {
            return Fault::reply(404, "{}");
        };
        let total = data.len();
        Fault::reply(
            200,
            json!({"data": data, "meta": {"total": total}}).to_string(),
        )
        .header("Content-Type", "application/vnd.api+json")
    })
    .await
}

struct Sb {
    f: Fixture,
    p: Project,
    server: Server,
    docs: Docs,
    engine: Arc<Engine>,
}

/// A signed-in StreamBIM project against the fake server, with every folder populated.
async fn setup(tag: &str) -> Sb {
    let docs = Docs::default();
    let server = streambim(docs.clone()).await;
    let mut f = fixture(tag);
    let state = f.ctx.paths.state_dir.clone();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    Arc::get_mut(&mut f.ctx).unwrap().streambim = Arc::new(StreamBimAccounts::load(http, &state));
    streambim_session(&f, "app", &server.origin);

    let mut p = f.p.clone();
    p.pid = 0;
    p.provider = Provider::StreamBim;
    p.hub_id = "streambim:app".into();
    p.hub_name = "StreamBIM".into();
    p.project_id = "streambim:app:1552".into();
    p.name = "Bygge".into();
    p.root_folder_urn = "0".into();
    p.urn_ns = String::new();
    p.oss_scopes = None;
    p.tops = vec![TopFolder {
        id: "0".into(),
        name: "Dokument".into(),
    }];
    p.meta = Some(
        json!({"region": "app", "base": server.origin, "project": "1552", "account": "app"})
            .to_string(),
    );
    p.pid = f.ctx.store.upsert_project(&p).unwrap();
    let mut folders = vec![row(
        &p,
        "0",
        Some(PROJECT_ROOT_ID),
        "Dokument",
        Kind::Folder,
    )];
    for i in 1..=FOLDERS {
        folders.push(row(
            &p,
            &i.to_string(),
            Some("0"),
            &format!("Mapp {i}"),
            Kind::Folder,
        ));
    }
    for folder in folders {
        f.ctx.store.upsert_item(&folder).unwrap();
        f.ctx.store.set_populated(p.pid, &folder.id, true).unwrap();
    }

    let engine = Engine::new(f.ctx.clone());
    // The top folders are known: a poll asks only for folder listings.
    engine.st().tops_at.insert(p.pid, Instant::now());
    Sb {
        f,
        p,
        server,
        docs,
        engine,
    }
}

impl Sb {
    /// Walk now: the folders it listed (sorted), its delta, and whether it asked anything.
    async fn walk(&self) -> (Vec<String>, Delta, bool) {
        let before = self.server.count();
        let (delta, asked) = self.engine.walk_delta(&self.p).await.unwrap();
        let mut listed: Vec<String> = self.server.requests()[before..]
            .iter()
            .filter_map(|r| param(&r.target, "filter%5Bparent%5D="))
            .collect();
        listed.sort();
        (listed, delta, asked)
    }

    /// Store the documents the server shows in `folder`, as an earlier poll would have.
    async fn seed(&self, folder: &str) {
        let items = remote::folder_contents(&self.f.ctx, &self.p, folder)
            .await
            .unwrap();
        for it in items.into_iter().filter(|it| it.kind == Kind::File) {
            self.f.ctx.store.upsert_item(&it).unwrap();
        }
    }

    fn sync_now_pending(&self) {
        self.engine.st().next_due.insert(self.p.pid, Instant::now());
    }
}

fn all() -> Vec<String> {
    let mut all: Vec<String> = (0..=FOLDERS).map(|i| i.to_string()).collect();
    all.sort();
    all
}

fn ids(items: &[Item]) -> Vec<&str> {
    items.iter().map(|it| it.id.as_str()).collect()
}

#[tokio::test]
async fn between_full_walks_only_active_folders_are_listed() {
    let sb = setup("hot-active").await;
    let pid = sb.p.pid;

    let (listed, delta, asked) = sb.walk().await;
    assert_eq!(listed, all(), "the first walk lists every folder");
    assert!(delta.is_empty() && asked);

    let before = sb.server.count();
    let (listed, _, asked) = sb.walk().await;
    assert!(listed.is_empty(), "nothing is active: {listed:?}");
    assert!(!asked);
    assert_eq!(sb.server.count(), before, "no request at all");

    // The user opens folder 3 in Explorer.
    sb.engine.touch_at(pid, Spot::Folder("3".into()));
    let before = sb.server.count();
    let (listed, _, asked) = sb.walk().await;
    assert_eq!(listed, ["3"]);
    assert!(asked);
    assert_eq!(
        sb.server.count() - before,
        2,
        "its subfolders and its documents"
    );
}

#[tokio::test]
async fn an_opened_file_or_a_local_change_makes_its_folder_active() {
    let sb = setup("hot-local").await;
    let pid = sb.p.pid;
    sb.docs
        .lock()
        .unwrap()
        .insert("7".into(), vec![doc("77", "plan.pdf")]);
    sb.seed("7").await;
    sb.walk().await;

    // A file in folder 7 is opened, and files are saved in folder 5.
    sb.engine.touch_at(pid, Spot::In("d77".into()));
    sb.engine.after_work(Work {
        active: HashSet::from([(pid, "5".to_string())]),
        ..Work::default()
    });
    let (listed, delta, _) = sb.walk().await;
    assert_eq!(listed, ["5", "7"]);
    assert!(delta.is_empty(), "{:?}", ids(&delta.upserts));
}

#[tokio::test]
async fn sync_now_lists_every_folder_and_a_changed_folder_stays_active() {
    let sb = setup("hot-remote").await;
    sb.walk().await;

    // Someone adds a document to folder 4, which nobody here is working in.
    sb.docs
        .lock()
        .unwrap()
        .insert("4".into(), vec![doc("40", "nytt.pdf")]);
    let (listed, _, _) = sb.walk().await;
    assert!(listed.is_empty(), "a quiet folder waits for the full walk");

    sb.sync_now_pending();
    let mut live = login_test_live();
    let (tx, _rx) = unbounded_channel();
    sb.engine.on_cmd(TrayCmd::SyncNow, &mut live, &tx).await;
    let (listed, delta, _) = sb.walk().await;
    assert_eq!(listed, all(), "Sync now lists every folder");
    assert_eq!(ids(&delta.upserts), ["d40"]);

    let (listed, _, _) = sb.walk().await;
    assert_eq!(
        listed,
        ["4"],
        "the folder where something changed is active now"
    );
}

#[tokio::test]
async fn an_item_that_left_an_active_folder_is_looked_for_in_the_quiet_ones() {
    let sb = setup("hot-move").await;
    let pid = sb.p.pid;
    sb.docs
        .lock()
        .unwrap()
        .insert("2".into(), vec![doc("41", "ritning.pdf")]);
    sb.seed("2").await;
    sb.walk().await;

    // Someone moves the drawing from folder 2, active here, to folder 6, quiet.
    {
        let mut docs = sb.docs.lock().unwrap();
        let moved = docs.remove("2").unwrap();
        docs.insert("6".into(), moved);
    }
    sb.engine.touch_at(pid, Spot::Folder("2".into()));
    let (listed, delta, _) = sb.walk().await;
    assert_eq!(
        listed,
        all(),
        "folder 2 lost an item, so every folder is listed"
    );
    assert!(
        !sb.engine.st().hot.full_due(pid, Instant::now()),
        "that was a full walk"
    );
    sb.f.ctx.store.apply_delta(pid, &delta).unwrap();
    let drawing =
        sb.f.ctx
            .store
            .item(pid, "d41")
            .unwrap()
            .expect("the drawing is still here");
    assert_eq!(drawing.parent.as_deref(), Some("6"), "it moved");
}

#[tokio::test]
async fn a_local_move_that_is_still_queued_lists_no_quiet_folder() {
    let sb = setup("hot-local-move").await;
    let pid = sb.p.pid;
    sb.docs
        .lock()
        .unwrap()
        .insert("2".into(), vec![doc("41", "ritning.pdf")]);
    sb.seed("2").await;
    sb.walk().await;

    // The user moves the drawing to folder 6; the move has not reached StreamBIM yet.
    let mut drawing = sb.f.ctx.store.item(pid, "d41").unwrap().unwrap();
    drawing.parent = Some("6".into());
    sb.f.ctx.store.upsert_item(&drawing).unwrap();
    enqueue(&sb.f.ctx, pid, "move", Some("d41"), json!({})).unwrap();
    sb.engine.touch_at(pid, Spot::Folder("6".into()));
    let (listed, delta, _) = sb.walk().await;
    assert_eq!(listed, ["6"], "the queued move keeps the drawing anyway");
    assert_eq!(delta.deletes, ["d41"], "keep_pending_local drops this one");
}

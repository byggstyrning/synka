//! Fault injection for the production IPC session, without opening the installed engine pipe.
//! Every wait is bounded and every spawned server is aborted if an assertion fails.

use super::*;
use std::time::Duration;
use tokio::io::DuplexStream;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(10);

struct ServerTask(Option<JoinHandle<()>>);

impl ServerTask {
    async fn finish(&mut self) {
        timeout(DEADLINE, self.0.as_mut().unwrap())
            .await
            .expect("IPC session did not notice disconnect")
            .expect("IPC session panicked");
        self.0.take();
    }
}

impl Drop for ServerTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

fn hub() -> Arc<IpcHub> {
    // This is a label in the snapshots only. These tests never open this path.
    Arc::new(IpcHub::new(PathBuf::from(r"C:\Synka-resilience-test-only")))
}

fn operations() -> usize {
    std::env::var("SYNKA_STRESS_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000usize)
        .clamp(1, 10_000)
}

async fn session(
    hub: &Arc<IpcHub>,
    tx: UnboundedSender<TrayCmd>,
    capacity: usize,
) -> (DuplexStream, ServerTask) {
    let before = hub.client_count();
    let (peer, stream) = tokio::io::duplex(capacity);
    let task = ServerTask(Some(tokio::spawn(client(stream, hub.clone(), tx))));
    timeout(DEADLINE, async {
        while hub.client_count() != before + 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("IPC session did not start");
    (peer, task)
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) {
    timeout(DEADLINE, writer.write_all(bytes))
        .await
        .expect("IPC write stalled")
        .expect("IPC write failed");
}

async fn receive<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Value {
    let mut line = String::new();
    let len = timeout(DEADLINE, reader.read_line(&mut line))
        .await
        .expect("IPC response timed out")
        .expect("IPC response was not UTF-8");
    assert_ne!(len, 0, "IPC closed before the expected response");
    serde_json::from_str(&line).expect("IPC response was not JSON")
}

fn activity(index: usize) -> UiMsg {
    UiMsg::Activity(ActivityItem {
        ts_ms: index as i64,
        kind: "download",
        name: format!("file-{index}.rvt"),
        project: "Isolated resilience project".into(),
        detail: String::new(),
    })
}

fn assert_sync_commands(rx: &mut UnboundedReceiver<TrayCmd>, count: usize) {
    for index in 0..count {
        assert!(
            matches!(rx.try_recv(), Ok(TrayCmd::SyncNow)),
            "command {index} was lost, reordered, or changed"
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "unexpected command reached the engine"
    );
}

#[tokio::test]
async fn resilience_fragmented_and_coalesced_command_spam_preserves_ack_order() {
    let hub = hub();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (peer, mut server) = session(&hub, tx, 128).await;
    let (read, mut write) = tokio::io::split(peer);
    let count = operations();
    let flood = (0..count)
        .map(|id| {
            let command = if id % 2 == 0 {
                "sync_now"
            } else {
                "refresh_projects"
            };
            format!("{{\"cmd\":\"{command}\",\"id\":{id}}}\n")
        })
        .collect::<String>();
    // A one-byte write splits a command; a 97-byte write coalesces several commands.
    let writer = tokio::spawn(async move {
        let mut remaining = flood.as_bytes();
        let sizes = [1, 7, 97, 2, 31];
        let mut chunk = 0;
        while !remaining.is_empty() {
            let len = sizes[chunk % sizes.len()].min(remaining.len());
            send(&mut write, &remaining[..len]).await;
            remaining = &remaining[len..];
            chunk += 1;
        }
        write.shutdown().await.unwrap();
    });
    let mut read = BufReader::new(read);
    for id in 0..count {
        assert_eq!(
            receive(&mut read).await,
            json!({"type":"ack", "ok":true, "id":id})
        );
    }
    timeout(DEADLINE, writer).await.unwrap().unwrap();
    server.finish().await;
    for id in 0..count {
        match (id % 2, rx.try_recv()) {
            (0, Ok(TrayCmd::SyncNow)) | (1, Ok(TrayCmd::RefreshProjects)) => {}
            (_, command) => panic!("command {id} was lost or reordered: {command:?}"),
        }
    }
    assert!(rx.try_recv().is_err());
    assert_eq!(hub.client_count(), 0);
}

#[tokio::test]
async fn resilience_bad_commands_and_invalid_utf8_do_not_poison_the_session() {
    let hub = hub();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (peer, mut server) = session(&hub, tx, 512).await;
    let mut peer = BufReader::new(peer);
    let malformed: &[&[u8]] = &[
        b"\xff\xfe\n",
        b"{\n",
        b"null\n",
        b"[]\n",
        b"42\n",
        b"{\"cmd\":\"unknown\",\"id\":9}\n",
        b"{\"cmd\":false,\"id\":9}\n",
        b"{\"cmd\":\"set_project\",\"id\":9}\n",
        b"{\"cmd\":\"set_setting\",\"key\":\"max_downloads\",\"value\":-1,\"id\":9}\n",
        b"{\"cmd\":\"set_setting\",\"key\":\"unknown\",\"value\":true,\"id\":9}\n",
        b"{\"cmd\":\"open_folder\",\"path\":\"relative\",\"id\":9}\n",
        b"{\"cmd\":\"reset\",\"id\":9}\n",
        b"{\"cmd\":\"confirm_deletes\",\"project_id\":\"p\",\"confirm\":\"yes\",\"id\":9}\n",
    ];
    for bad in malformed {
        send(peer.get_mut(), bad).await;
        let reply = receive(&mut peer).await;
        assert_eq!(reply["type"], "ack", "{bad:?}");
        assert_eq!(reply["ok"], false, "{bad:?}");
        assert!(reply["error"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(rx.try_recv().is_err(), "malformed command was dispatched");
        send(peer.get_mut(), b"{\"cmd\":\"sync_now\",\"id\":10}\n").await;
        assert_eq!(
            receive(&mut peer).await,
            json!({"type":"ack","ok":true,"id":10})
        );
        assert_sync_commands(&mut rx, 1);
    }
    // Blank lines are harmless and do not create extra ACKs.
    send(
        peer.get_mut(),
        b"\r\n \t\n{\"cmd\":\"sync_now\",\"id\":11}\r\n",
    )
    .await;
    assert_eq!(receive(&mut peer).await["id"], 11);
    assert_sync_commands(&mut rx, 1);
    drop(peer);
    server.finish().await;
    assert_eq!(hub.client_count(), 0);
}

#[test]
fn resilience_parser_rejects_deterministic_binary_garbage_without_dispatch() {
    let hub = hub();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut state = std::env::var("SYNKA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0x0BDC_5EED);
    for index in 0..operations() {
        let mut bytes = vec![0xff];
        for _ in 0..(index % 257) {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            bytes.push((state >> 32) as u8);
        }
        let replies = handle_line(&bytes, &hub, &tx);
        assert_eq!(replies.len(), 1);
        let reply: Value = serde_json::from_str(&replies[0]).unwrap();
        assert_eq!(reply["ok"], false, "garbage iteration {index}");
    }
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn resilience_line_limit_has_exact_boundary_and_bounded_memory() {
    for payload_len in [MAX_LINE - 1, MAX_LINE, MAX_LINE + 1, MAX_LINE * 4] {
        for terminated in [false, true] {
            let mut payload = vec![b'a'; payload_len];
            if terminated {
                payload.push(b'\n');
            }
            let mut reader = BufReader::new(payload.as_slice());
            let mut line = Vec::new();
            let result = timeout(DEADLINE, read_line(&mut reader, &mut line))
                .await
                .unwrap();
            if payload_len > MAX_LINE {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
            } else if terminated {
                assert_eq!(result.unwrap(), payload_len + 1);
            } else {
                assert_eq!(result.unwrap(), 0, "unterminated EOF is not a command");
            }
            assert!(
                line.len() <= MAX_LINE + 1,
                "oversized input grew the command buffer"
            );
        }
    }
}

#[tokio::test]
async fn resilience_cancelled_partial_read_keeps_bytes_and_next_command() {
    let (mut peer, stream) = tokio::io::duplex(128);
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let first = b"{\"cmd\":\"sync_";
    send(&mut peer, first).await;
    for _ in 0..2 {
        assert!(
            timeout(Duration::from_millis(10), read_line(&mut reader, &mut line))
                .await
                .is_err()
        );
        assert_eq!(
            line, first,
            "select cancellation lost or duplicated partial bytes"
        );
    }
    send(
        &mut peer,
        b"now\",\"id\":1}\n{\"cmd\":\"sync_now\",\"id\":2}\n",
    )
    .await;
    timeout(DEADLINE, read_line(&mut reader, &mut line))
        .await
        .unwrap()
        .unwrap();
    let first: Value = serde_json::from_slice(&line).unwrap();
    assert_eq!(first["id"], 1);
    line.clear();
    timeout(DEADLINE, read_line(&mut reader, &mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&line).unwrap()["id"], 2);
}

#[tokio::test]
async fn resilience_partial_command_disconnect_and_oversize_isolate_other_clients() {
    let hub = hub();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (healthy, mut healthy_server) = session(&hub, tx.clone(), 512).await;
    let mut healthy = BufReader::new(healthy);
    let (mut broken, mut broken_server) = session(&hub, tx.clone(), 128).await;
    send(&mut broken, b"{\"cmd\":\"sync_now\",\"id\":1}").await;
    broken.shutdown().await.unwrap();
    broken_server.finish().await;
    assert!(rx.try_recv().is_err(), "partial EOF command was dispatched");
    assert_eq!(hub.client_count(), 1);

    let (mut oversized, mut oversized_server) = session(&hub, tx, MAX_LINE * 2).await;
    send(&mut oversized, &vec![b'a'; MAX_LINE + 1]).await;
    oversized_server.finish().await;
    assert_eq!(hub.client_count(), 1);
    send(healthy.get_mut(), b"{\"cmd\":\"sync_now\",\"id\":2}\n").await;
    assert_eq!(
        receive(&mut healthy).await,
        json!({"type":"ack","ok":true,"id":2})
    );
    assert_sync_commands(&mut rx, 1);
    drop(healthy);
    healthy_server.finish().await;
    assert_eq!(hub.client_count(), 0);
}

#[tokio::test]
async fn resilience_disconnect_mid_reply_releases_client_and_keeps_hub_usable() {
    let hub = hub();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (peer, mut server) = session(&hub, tx.clone(), 16).await;
    let (read, mut write) = tokio::io::split(peer);
    send(&mut write, b"{\"cmd\":\"hello\",\"id\":1}\n").await;
    timeout(DEADLINE, async {
        assert!(matches!(rx.recv().await, Some(TrayCmd::Hello)));
    })
    .await
    .expect("hello was not dispatched");
    // A hello ACK is larger than this transport's capacity; the writer is blocked.
    drop(read);
    drop(write);
    server.finish().await;
    assert_eq!(hub.client_count(), 0);

    let (peer, mut server) = session(&hub, tx, 512).await;
    let mut peer = BufReader::new(peer);
    send(peer.get_mut(), b"{\"cmd\":\"sync_now\",\"id\":2}\n").await;
    assert_eq!(
        receive(&mut peer).await,
        json!({"type":"ack","ok":true,"id":2})
    );
    assert_sync_commands(&mut rx, 1);
    drop(peer);
    server.finish().await;
    assert_eq!(hub.client_count(), 0);
}

#[tokio::test]
async fn resilience_reconnect_hello_restores_latest_status_and_bounded_activity() {
    let hub = hub();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for index in 0..ACTIVITY_CAP * 3 {
        hub.on_msg(&activity(index));
    }
    for reconnect in 0..12 {
        hub.on_msg(&UiMsg::Status(Status {
            state: "syncing",
            text: format!("reconnect-{reconnect}"),
            pending_jobs: reconnect,
            ..Default::default()
        }));
        let (peer, mut server) = session(&hub, tx.clone(), 512).await;
        let mut peer = BufReader::new(peer);
        send(peer.get_mut(), b"{\"cmd\":\"hello\",\"id\":1}\n").await;
        assert_eq!(
            receive(&mut peer).await,
            json!({"type":"ack","ok":true,"id":1})
        );
        let status = receive(&mut peer).await;
        assert_eq!(status["type"], "status");
        assert_eq!(status["text"], format!("reconnect-{reconnect}"));
        assert_eq!(receive(&mut peer).await["type"], "projects");
        let activity = receive(&mut peer).await;
        let items = activity["items"].as_array().unwrap();
        assert_eq!(items.len(), ACTIVITY_CAP);
        assert_eq!(
            items[0]["name"],
            format!("file-{}.rvt", ACTIVITY_CAP * 3 - 1)
        );
        assert_eq!(
            items[ACTIVITY_CAP - 1]["name"],
            format!("file-{}.rvt", ACTIVITY_CAP * 2)
        );
        assert!(matches!(rx.try_recv(), Ok(TrayCmd::Hello)));
        assert!(rx.try_recv().is_err());
        drop(peer);
        server.finish().await;
        assert_eq!(hub.client_count(), 0);
    }
}

#[tokio::test]
async fn resilience_slow_reader_recovers_after_broadcast_lag_with_fresh_snapshot() {
    let hub = hub();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (peer, mut server) = session(&hub, tx, 64).await;
    let mut peer = BufReader::new(peer);

    // Start an event larger than the transport buffer, then stop reading until the event
    // ring has overflowed. This deterministically exercises client()'s Lagged branch.
    hub.on_msg(&activity(0));
    tokio::task::yield_now().await;
    for index in 1..=EVENT_CAP * 3 {
        hub.on_msg(&activity(index));
    }
    hub.on_msg(&UiMsg::Status(Status {
        text: "latest-after-lag".into(),
        state: "synced",
        ..Default::default()
    }));
    let first = receive(&mut peer).await;
    assert_eq!(first["type"], "activity_item");
    assert_eq!(first["item"]["name"], "file-0.rvt");
    let status = receive(&mut peer).await;
    assert_eq!(status["type"], "status");
    assert_eq!(status["text"], "latest-after-lag");
    assert_eq!(receive(&mut peer).await["type"], "projects");
    let snapshot = receive(&mut peer).await;
    assert_eq!(snapshot["type"], "activity");
    assert_eq!(snapshot["items"].as_array().unwrap().len(), ACTIVITY_CAP);
    assert_eq!(
        snapshot["items"][0]["name"],
        format!("file-{}.rvt", EVENT_CAP * 3)
    );

    send(peer.get_mut(), b"{\"cmd\":\"sync_now\",\"id\":17}\n").await;
    assert_eq!(
        receive(&mut peer).await,
        json!({"type":"ack","ok":true,"id":17}),
        "retained stale events must not replay after a fresh recovery snapshot"
    );
    assert_sync_commands(&mut rx, 1);
    hub.on_msg(&UiMsg::Status(Status {
        text: "new-after-recovery".into(),
        state: "synced",
        ..Default::default()
    }));
    assert_eq!(receive(&mut peer).await["text"], "new-after-recovery");
    drop(peer);
    server.finish().await;
    assert_eq!(hub.client_count(), 0);
}

#[test]
fn resilience_closed_engine_queue_returns_failure_including_hello() {
    let hub = hub();
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    for cmd in [
        "sync_now",
        "hello",
        "refresh_projects",
        "sign_in",
        "retry_failed",
    ] {
        let replies = handle_line(json!({"cmd":cmd,"id":41}).to_string().as_bytes(), &hub, &tx);
        assert_eq!(
            replies.len(),
            1,
            "{cmd} reported cached state after engine failure"
        );
        let reply: Value = serde_json::from_str(&replies[0]).unwrap();
        assert_eq!(
            reply,
            json!({"type":"ack","id":41,"ok":false,"error":"motorn kör inte"})
        );
    }
}

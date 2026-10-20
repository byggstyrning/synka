//! Account-free fault injection over real loopback sockets. Compiled only by `cargo test`.
//! No global proxy, environment override, system network change, or external listener.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone)]
pub(crate) enum Fault {
    Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Send precisely these bytes, including deliberately broken HTTP/framing, then close.
    Raw(Vec<u8>),
    Disconnect,
    Stall,
    Delay(Duration, Box<Fault>),
    /// Send a response prefix then leave its advertised body unfinished.
    OpenBody(Vec<u8>),
}

impl Fault {
    pub fn reply(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::Reply {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let Self::Reply { headers, .. } = &mut self {
            headers.push((name.to_string(), value.to_string()));
        }
        self
    }
}

pub(crate) struct Server {
    pub origin: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl Server {
    pub async fn scripted(script: Vec<Fault>) -> Self {
        Self::new(move |_, n| {
            script
                .get(n)
                .cloned()
                .unwrap_or_else(|| Fault::reply(599, "unexpected extra request"))
        })
        .await
    }

    pub async fn new(handler: impl Fn(&Request, usize) -> Fault + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind ephemeral loopback listener");
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            // Dropping/aborting this parent also aborts every stalled connection.
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break; };
                        let seen = seen.clone();
                        let handler = handler.clone();
                        connections.spawn(async move {
                            let request = tokio::time::timeout(Duration::from_secs(5), read_request(&mut stream)).await;
                            let Ok(Some(request)) = request else { return; };
                            let n = {
                                let mut requests = seen.lock().unwrap();
                                let n = requests.len();
                                requests.push(request.clone());
                                n
                            };
                            send_fault(stream, handler(&request, n)).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            origin,
            requests,
            task,
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut bytes = Vec::new();
    let head_end = loop {
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 || bytes.len() + n > 64 * 1024 {
            return None;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(i) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = std::str::from_utf8(&bytes[..head_end]).ok()?;
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_string(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
        .transpose()
        .ok()?
        .unwrap_or(0);
    if length > 2 * 1024 * 1024 {
        return None;
    }
    let mut body = bytes[head_end..].to_vec();
    while body.len() < length {
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(length);
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

async fn send_fault(mut stream: TcpStream, mut fault: Fault) {
    loop {
        match fault {
            Fault::Delay(wait, next) => {
                tokio::time::sleep(wait).await;
                fault = *next;
            }
            Fault::Disconnect => return,
            Fault::Stall => {
                std::future::pending::<()>().await;
                return;
            }
            Fault::OpenBody(bytes) => {
                let _ = stream.write_all(&bytes).await;
                std::future::pending::<()>().await;
                return;
            }
            Fault::Raw(bytes) => {
                let _ = stream.write_all(&bytes).await;
                let _ = stream.shutdown().await;
                return;
            }
            Fault::Reply {
                status,
                headers,
                body,
            } => {
                let mut head = format!(
                    "HTTP/1.1 {status} Injected\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (key, value) in headers {
                    head.push_str(&format!("{key}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.shutdown().await;
                return;
            }
        }
    }
}

/// Runner-controlled volume, bounded even if its environment is misconfigured.
pub(crate) fn stress_operations() -> usize {
    std::env::var("SYNKA_STRESS_OPS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1000)
        .clamp(1, 10_000)
}

pub(crate) fn test_seed() -> u32 {
    std::env::var("SYNKA_TEST_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0xBDC)
}

pub(crate) fn project() -> crate::model::Project {
    crate::model::Project {
        pid: 1,
        hub_id: "b.synthetic-hub".into(),
        hub_name: "Synthetic Hub".into(),
        project_id: "b.synthetic-project".into(),
        name: "Synthetic Project".into(),
        root_folder_urn: "urn:adsk.wipemea:fs.folder:co.ROOT".into(),
        urn_ns: "wipemea".into(),
        tops: vec![],
        oss_scopes: None,
        change_source: crate::model::ChangeSourceKind::Unknown,
        cursor: None,
        enabled: true,
        last_sync_ms: None,
        provider: crate::model::Provider::Acc,
        meta: None,
    }
}

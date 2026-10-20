//! Named-pipe server for the WinUI window. docs/IPC.md is the contract.
//!
//! Pipe `\\.\pipe\synka-ui-<user SID>`, DACL "D:P(A;;GA;;;<SID>)" (current user only).
//! UTF-8, one JSON object per line in both directions. Commands are forwarded to the engine
//! through `TrayCmd`. Events come from `IpcHub` (a broadcast channel). The hub also caches the
//! last status, settings and projects and keeps the last 200 activity items, so `hello` can be
//! answered without asking the engine.

use crate::app::{
    AccountStatus, ActivityItem, HubList, Secret, SettingsView, Status, StreamBimLogin, TrayCmd,
    UiMsg,
};
use crate::config;
use crate::model::{HubInfo, ProjectInfo, Provider};
use crate::sync::sanitize_name;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

/// Longest accepted command line (bytes, without the newline).
pub const MAX_LINE: usize = 64 * 1024;
const ACTIVITY_CAP: usize = 200;
const EVENT_CAP: usize = 256;
const CLIENT_VALIDATION_CAP: usize = 16;

/// Commands from the UI. `id` is read separately (optional, echoed in the ack).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Hello {
        #[serde(default)]
        version: Option<String>,
    },
    /// ACC/Interaxo need no fields. StreamBIM needs `region`, `username` and `password`.
    SignIn {
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<Secret>,
    },
    SignInMfa {
        provider: String,
        code: Secret,
    },
    CancelSignIn {
        provider: String,
    },
    SignOut {
        #[serde(default)]
        provider: Option<String>,
    },
    #[serde(rename = "streambim_regions")]
    StreamBimRegions,
    TestApsClientId {
        client_id: String,
        request_id: String,
    },
    CancelApsClientIdTest {
        request_id: String,
    },
    SyncNow,
    OpenFolder {
        #[serde(default)]
        path: Option<String>,
    },
    OpenLog,
    RefreshProjects,
    SetProject {
        hub_id: String,
        project_id: String,
        subscribe: bool,
    },
    SetSetting {
        key: String,
        value: Value,
    },
    FreeUpSpace {
        #[serde(default)]
        project_id: Option<String>,
    },
    CheckUpdates,
    InstallUpdate,
    RetryFailed,
    ClearActivity,
    ExportDiagnostics,
    Reset {
        #[serde(default)]
        project_id: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    ConfirmDeletes {
        project_id: String,
        confirm: bool,
    },
    ResetAll,
    MoveSyncRoot {
        path: String,
    },
    QuitEngine,
}

/// Event fan-out and the caches that answer `hello`.
pub struct IpcHub {
    events: broadcast::Sender<Arc<str>>,
    sync_root: PathBuf,
    cache: Mutex<Cache>,
    clients: AtomicUsize,
}

#[derive(Default)]
struct Cache {
    status: Option<Value>,
    settings: Option<Value>,
    projects: Option<Value>,
    /// The pending `sign_in_progress` (`browser` or `mfa_required`) of each provider. A reconnecting
    /// window needs it to reopen the MFA dialog or show the wait for the browser.
    sign_in: HashMap<Provider, Value>,
    aps_client_id_validation: VecDeque<Value>,
    /// Newest first.
    activity: VecDeque<ActivityItem>,
}

impl IpcHub {
    pub fn new(sync_root: PathBuf) -> IpcHub {
        let (events, _) = broadcast::channel(EVENT_CAP);
        IpcHub {
            events,
            sync_root,
            cache: Mutex::new(Cache::default()),
            clients: AtomicUsize::new(0),
        }
    }

    /// Number of connected UI clients (the tray shows a fallback message box when 0).
    pub fn client_count(&self) -> usize {
        self.clients.load(Ordering::Relaxed)
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Called by `UiSender::send` for every engine message.
    pub fn on_msg(&self, m: &UiMsg) {
        match m {
            UiMsg::Status(s) => {
                let v = status_json(s);
                self.cache().status = Some(v.clone());
                self.emit(&v);
            }
            UiMsg::Projects { hubs, loading } => {
                let v = projects_json(hubs, *loading, &self.sync_root);
                self.cache().projects = Some(v.clone());
                self.emit(&v);
            }
            UiMsg::Notify {
                title,
                body,
                severity,
            } => {
                self.emit(&json!({
                    "type": "notify",
                    "title": title,
                    "body": body,
                    "severity": severity.as_str(),
                }));
            }
            UiMsg::Update {
                available,
                version,
                error,
                installing,
            } => {
                self.emit(&json!({
                    "type": "update",
                    "available": available,
                    "version": version,
                    "error": error,
                    "installing": installing,
                }));
            }
            UiMsg::Settings(s) => {
                let v = settings_json(s);
                self.cache().settings = Some(v.clone());
                self.emit(&v);
            }
            UiMsg::SignInProgress {
                provider,
                state,
                error,
            } => {
                let v = json!({
                    "type": "sign_in_progress",
                    "provider": provider.as_str(),
                    "state": state,
                    "error": error,
                });
                {
                    let mut cache = self.cache();
                    if matches!(*state, "browser" | "mfa_required") {
                        cache.sign_in.insert(*provider, v.clone());
                    } else {
                        // signed_in, failed or cancelled: the wait is over.
                        cache.sign_in.remove(provider);
                    }
                }
                self.emit(&v);
            }
            UiMsg::StreamBimRegions(regions) => {
                self.emit(&json!({
                    "type": "streambim_regions",
                    "regions": regions,
                }));
            }
            UiMsg::ApsClientIdValidation {
                request_id,
                state,
                error,
            } => {
                let v = json!({
                    "type": "aps_client_id_validation",
                    "request_id": request_id,
                    "state": state,
                    "error": error,
                });
                {
                    let mut cache = self.cache();
                    cache
                        .aps_client_id_validation
                        .retain(|event| event["request_id"] != *request_id);
                    cache.aps_client_id_validation.push_back(v.clone());
                    while cache.aps_client_id_validation.len() > CLIENT_VALIDATION_CAP {
                        cache.aps_client_id_validation.pop_front();
                    }
                }
                self.emit(&v);
            }
            UiMsg::Activity(item) => {
                {
                    let mut c = self.cache();
                    c.activity.push_front(item.clone());
                    c.activity.truncate(ACTIVITY_CAP);
                }
                self.emit(&json!({ "type": "activity_item", "item": item }));
            }
        }
    }

    fn emit(&self, v: &Value) {
        let mut line = v.to_string();
        line.push('\n');
        // No receivers (no UI open) is normal: ignore the error.
        let _ = self.events.send(Arc::from(line));
    }

    /// Cancellation is idempotent: a late cancel must not replace an already saved result.
    pub(crate) fn replay_client_validation_result(&self, request_id: &str) -> bool {
        let result = self
            .cache()
            .aps_client_id_validation
            .iter()
            .rev()
            .find(|event| {
                event["request_id"] == request_id
                    && matches!(
                        event["state"].as_str(),
                        Some("saved" | "failed" | "cancelled")
                    )
            })
            .cloned();
        if let Some(result) = result {
            self.emit(&result);
            true
        } else {
            false
        }
    }

    fn clear_activity(&self) {
        self.cache().activity.clear();
        self.emit(&json!({ "type": "activity", "items": [] }));
    }

    /// The `hello` replies: status, pending sign-in progress, settings (when known), projects,
    /// activity (newest first).
    pub fn snapshot(&self) -> Vec<String> {
        let c = self.cache();
        let status = c.status.clone().unwrap_or_else(|| {
            status_json(&Status {
                text: "Logga in".into(),
                state: "signed_out",
                sync_root: self.sync_root.display().to_string(),
                engine_version: env!("CARGO_PKG_VERSION").into(),
                accounts: Provider::ALL.map(AccountStatus::signed_out).into(),
                ..Default::default()
            })
        });
        let mut out = vec![status];
        out.extend(
            Provider::ALL
                .iter()
                .filter_map(|p| c.sign_in.get(p).cloned()),
        );
        if let Some(s) = &c.settings {
            out.push(s.clone());
        }
        out.push(
            c.projects
                .clone()
                .unwrap_or_else(|| json!({ "type": "projects", "loading": false, "hubs": [] })),
        );
        out.extend(c.aps_client_id_validation.iter().cloned());
        out.push(json!({ "type": "activity", "items": c.activity.iter().collect::<Vec<_>>() }));
        out.into_iter()
            .map(|v| {
                let mut s = v.to_string();
                s.push('\n');
                s
            })
            .collect()
    }
}

fn status_json(s: &Status) -> Value {
    json!({
        "type": "status",
        "signed_in": s.accounts.iter().any(|a| a.signed_in),
        "user": s.user.as_ref().map(|u| json!({ "name": u.name, "email": u.email })),
        "state": s.state,
        "text": s.text,
        "error": s.error,
        "pending_jobs": s.pending_jobs,
        "failed_jobs": s.failed_jobs,
        "transfers": s.transfers,
        "sync_root": s.sync_root,
        "held_deletes": s.held_deletes,
        "local_bytes": s.local_bytes,
        "engine_version": s.engine_version,
        "accounts": s.accounts.iter().map(account_json).collect::<Vec<Value>>(),
    })
}

/// One `status.accounts` entry. The user is name and e-mail only, never the user id.
fn account_json(a: &AccountStatus) -> Value {
    json!({
        "provider": a.provider.as_str(),
        "label": a.label,
        "configured": a.configured,
        "signed_in": a.signed_in,
        "signing_in": a.signing_in,
        "user": a.user.as_ref().map(|u| json!({ "name": u.name, "email": u.email })),
        "host": a.host,
    })
}

fn settings_json(s: &SettingsView) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    v["type"] = json!("settings");
    v
}

fn projects_json(hubs: &HubList, loading: bool, root: &Path) -> Value {
    let mut sorted: Vec<&(HubInfo, Vec<(ProjectInfo, bool)>)> = hubs.iter().collect();
    sorted.sort_by_key(|(h, _)| h.name.to_lowercase());
    let hubs_json: Vec<Value> = sorted
        .into_iter()
        .map(|(h, ps)| {
            let hub_dir = root.join(sanitize_name(&h.name));
            let mut ps: Vec<&(ProjectInfo, bool)> = ps.iter().collect();
            ps.sort_by_key(|(p, _)| p.name.to_lowercase());
            let projects: Vec<Value> = ps
                .into_iter()
                .map(|(p, subscribed)| {
                    let local_path = if *subscribed {
                        Some(hub_dir.join(sanitize_name(&p.name)).display().to_string())
                    } else {
                        None
                    };
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "subscribed": subscribed,
                        "local_path": local_path,
                    })
                })
                .collect();
            json!({ "id": h.id, "name": h.name, "provider": h.provider.as_str(), "projects": projects })
        })
        .collect();
    json!({ "type": "projects", "loading": loading, "hubs": hubs_json })
}

/// `\\.\pipe\synka-ui-<SID>` for the current user.
pub fn pipe_name() -> anyhow::Result<String> {
    Ok(format!(
        r"\\.\pipe\synka-ui-{}",
        crate::cf::current_user_sid()?
    ))
}

/// Owned security descriptor from SDDL (freed on drop).
struct OwnedSd(PSECURITY_DESCRIPTOR);

// The descriptor is only read by CreateNamedPipe; it is not shared mutably.
unsafe impl Send for OwnedSd {}

impl Drop for OwnedSd {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

fn current_user_sd(sid: &str) -> anyhow::Result<OwnedSd> {
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
    }
    Ok(OwnedSd(sd))
}

/// Pipe-name fragments Synka must never serve: Autodesk's own local endpoints, e.g. the
/// Desktop Connector service that Revit uses for linked models. Emulating them is not to
/// be implemented without written approval from Autodesk (DESIGN.md).
const FORBIDDEN_PIPE_FRAGMENTS: &[&str] = &["autodesk", "desktopconnector"];

fn check_pipe_name(name: &str) -> io::Result<()> {
    let lower = name.to_ascii_lowercase();
    if let Some(f) = FORBIDDEN_PIPE_FRAGMENTS.iter().find(|f| lower.contains(*f)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to create pipe {name}: names containing '{f}' are reserved for Autodesk software"),
        ));
    }
    Ok(())
}

/// Creates one pipe instance with the given security descriptor (address kept as usize so the
/// serving future stays Send; the descriptor is alive for the whole serve loop).
fn create_instance(name: &str, sd: usize, first: bool) -> io::Result<NamedPipeServer> {
    check_pipe_name(name)?;
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd as *mut c_void,
        bInheritHandle: false.into(),
    };
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first);
    // Safety: `sa` lives for the call and points at the live descriptor owned by `serve`.
    unsafe {
        opts.create_with_security_attributes_raw(
            name,
            &mut sa as *mut SECURITY_ATTRIBUTES as *mut c_void,
        )
    }
}

/// Accept clients forever. Returns Err only when the pipe cannot be created (for example
/// another engine instance already serves it). The engine keeps running without the UI pipe.
pub async fn serve(hub: Arc<IpcHub>, cmds: UnboundedSender<TrayCmd>) -> anyhow::Result<()> {
    let sid = crate::cf::current_user_sid()?;
    let name = pipe_name()?;
    let sd = current_user_sd(&sid)?;
    let sd_addr = sd.0 .0 as usize;

    let mut server = create_instance(&name, sd_addr, true)?;
    tracing::info!("ipc: lyssnar på {name}");
    loop {
        let ready = server.connect().await.is_ok();
        // Create the next instance before handing this one to a client, so a new UI never
        // finds the pipe missing.
        let next = create_instance(&name, sd_addr, false)?;
        let current = std::mem::replace(&mut server, next);
        if ready {
            tokio::spawn(client(current, hub.clone(), cmds.clone()));
        }
    }
}

struct ClientGuard(Arc<IpcHub>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn client<S>(pipe: S, hub: Arc<IpcHub>, cmds: UnboundedSender<TrayCmd>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    hub.clients.fetch_add(1, Ordering::Relaxed);
    let _guard = ClientGuard(hub.clone());
    let (rd, mut wr) = tokio::io::split(pipe);
    let mut rd = BufReader::new(rd);
    let mut events = hub.events.subscribe();
    let mut line: Vec<u8> = Vec::new();
    loop {
        tokio::select! {
            r = read_line(&mut rd, &mut line) => match r {
                Ok(0) => break,
                Ok(_) => {
                    let replies = handle_line(&line, &hub, &cmds);
                    line.clear();
                    if write_all_lines(&mut wr, &replies).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("ipc: {e}");
                    break;
                }
            },
            ev = events.recv() => match ev {
                Ok(l) => {
                    if wr.write_all(l.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Too slow: the client missed events. Resend the full current state.
                    tracing::debug!("ipc: klient hann inte med ({n} händelser), skickar allt");
                    // Discard retained events older than the snapshot. Replaying them would
                    // overwrite the freshly restored UI state with stale status/activity.
                    // Subscribe first so events emitted while writing the snapshot survive.
                    events = hub.events.subscribe();
                    if write_all_lines(&mut wr, &hub.snapshot()).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// Reads one `\n`-terminated line into `buf` (newline included). Ok(0) = the client closed the
/// pipe. Cancel-safe: partial input stays in `buf`. Lines over MAX_LINE are an error.
async fn read_line<R>(rd: &mut BufReader<R>, buf: &mut Vec<u8>) -> io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    loop {
        if buf.last() == Some(&b'\n') {
            return Ok(buf.len());
        }
        if buf.len() > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ipc: raden är för lång",
            ));
        }
        let room = (MAX_LINE + 1 - buf.len()) as u64;
        let n = (&mut *rd).take(room).read_until(b'\n', buf).await?;
        if n == 0 {
            return Ok(0);
        }
    }
}

async fn write_all_lines<W>(w: &mut W, lines: &[String]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    for l in lines {
        w.write_all(l.as_bytes()).await?;
    }
    w.flush().await
}

/// Parses one command line and returns the lines to send back (ack first, then any snapshot).
fn handle_line(line: &[u8], hub: &IpcHub, cmds: &UnboundedSender<TrayCmd>) -> Vec<String> {
    let text = line.trim_ascii();
    if text.is_empty() {
        return Vec::new();
    }
    let value: Value = match serde_json::from_slice(text) {
        Ok(v) => v,
        Err(e) => {
            return ack(None, Err(format!("ogiltig JSON: {e}")))
                .into_iter()
                .collect()
        }
    };
    let id = value.get("id").and_then(Value::as_i64);
    let cmd: Command = match serde_json::from_value(value) {
        Ok(c) => c,
        Err(e) => {
            return ack(id, Err(format!("okänt eller ogiltigt kommando: {e}")))
                .into_iter()
                .collect()
        }
    };
    match dispatch(cmd, hub, cmds) {
        Ok(extra) => {
            let mut out: Vec<String> = ack(id, Ok(())).into_iter().collect();
            out.extend(extra);
            out
        }
        Err(e) => ack(id, Err(e)).into_iter().collect(),
    }
}

/// `{"type":"ack",...}` line. Successes without an id are silent (nothing to correlate).
fn ack(id: Option<i64>, res: Result<(), String>) -> Option<String> {
    let mut v = match &res {
        Ok(()) if id.is_none() => return None,
        Ok(()) => json!({ "type": "ack", "ok": true }),
        Err(e) => json!({ "type": "ack", "ok": false, "error": e }),
    };
    if let Some(id) = id {
        v["id"] = json!(id);
    }
    let mut s = v.to_string();
    s.push('\n');
    Some(s)
}

/// The provider named in a command. Unknown names are refused with a fixed text.
fn parse_provider(name: &str) -> Result<Provider, String> {
    Provider::parse(name)
        .ok_or_else(|| "Okänd leverantör. Använd acc, streambim eller interaxo.".to_string())
}

/// The StreamBIM sign-in fields, all required. The region is a subdomain such as "app".
fn streambim_login(
    region: Option<String>,
    username: Option<String>,
    password: Option<Secret>,
) -> Result<StreamBimLogin, String> {
    let region = region.unwrap_or_default();
    let region_ok = (1..=32).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !region_ok {
        return Err("Ange en giltig StreamBIM-region.".into());
    }
    let username = username.unwrap_or_default().trim().to_string();
    if username.is_empty() {
        return Err("Ange e-postadress för StreamBIM.".into());
    }
    match password {
        Some(password) if !password.0.is_empty() => Ok(StreamBimLogin {
            region,
            username,
            password,
        }),
        _ => Err("Ange lösenordet för StreamBIM.".into()),
    }
}

/// Correlation IDs are public UUIDs generated by the UI, never arbitrary log/error text.
fn check_validation_request_id(id: &str) -> Result<(), String> {
    if id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        Ok(())
    } else {
        Err("request_id måste vara ett UUID.".into())
    }
}

/// Executes one command. Ok(extra lines) for commands answered here (`hello`).
fn dispatch(
    cmd: Command,
    hub: &IpcHub,
    cmds: &UnboundedSender<TrayCmd>,
) -> Result<Vec<String>, String> {
    let tray = match cmd {
        Command::Hello { version } => {
            tracing::info!("ipc: hello från UI {}", version.unwrap_or_default());
            cmds.send(TrayCmd::Hello)
                .map_err(|_| "motorn kör inte".to_string())?;
            return Ok(hub.snapshot());
        }
        Command::SignIn {
            provider,
            region,
            username,
            password,
        } => {
            let provider = parse_provider(provider.as_deref().unwrap_or("acc"))?;
            let streambim = match provider {
                Provider::StreamBim => Some(streambim_login(region, username, password)?),
                _ => None,
            };
            TrayCmd::SignIn {
                provider,
                streambim,
            }
        }
        Command::SignInMfa { provider, code } => {
            let provider = parse_provider(&provider)?;
            if provider != Provider::StreamBim {
                return Err("Engångskod används bara för StreamBIM.".into());
            }
            if code.0.len() != 6 || !code.0.bytes().all(|b| b.is_ascii_digit()) {
                return Err("Koden ska vara 6 siffror.".into());
            }
            TrayCmd::SignInMfa { provider, code }
        }
        Command::CancelSignIn { provider } => TrayCmd::CancelSignIn {
            provider: parse_provider(&provider)?,
        },
        Command::SignOut { provider: None } => TrayCmd::SignOut { provider: None },
        Command::SignOut {
            provider: Some(provider),
        } => TrayCmd::SignOut {
            provider: Some(parse_provider(&provider)?),
        },
        Command::StreamBimRegions => TrayCmd::StreamBimRegions,
        Command::TestApsClientId {
            client_id,
            request_id,
        } => {
            check_validation_request_id(&request_id)?;
            config::check_setting("aps_client_id", &json!(client_id))?;
            let client_id = client_id.trim().to_string();
            if client_id.is_empty() {
                return Err("Ange aps_client_id att testa, eller återställ standardvärdet.".into());
            }
            TrayCmd::TestApsClientId {
                client_id,
                request_id,
            }
        }
        Command::CancelApsClientIdTest { request_id } => {
            check_validation_request_id(&request_id)?;
            TrayCmd::CancelApsClientIdTest { request_id }
        }
        Command::SyncNow => TrayCmd::SyncNow,
        Command::OpenFolder { path: None } => TrayCmd::OpenFolder,
        Command::OpenFolder { path: Some(p) } => {
            let path = PathBuf::from(&p);
            if !path.is_absolute() {
                return Err("sökvägen måste vara absolut".into());
            }
            TrayCmd::OpenPath(path)
        }
        Command::OpenLog => TrayCmd::OpenLog,
        Command::RefreshProjects => TrayCmd::RefreshProjects,
        Command::SetProject {
            hub_id,
            project_id,
            subscribe,
        } => TrayCmd::SetProject {
            hub_id,
            project_id,
            subscribe,
        },
        Command::SetSetting { key, value } => {
            config::check_setting(&key, &value)?;
            if key == "aps_client_id" && value.as_str().is_some_and(|id| !id.trim().is_empty()) {
                return Err("aps_client_id måste testas innan det sparas. Använd Testa och spara i en uppdaterad Synka.".into());
            }
            TrayCmd::SetSetting { key, value }
        }
        Command::FreeUpSpace { project_id } => TrayCmd::FreeUpSpace { project_id },
        Command::CheckUpdates => TrayCmd::CheckUpdates,
        Command::InstallUpdate => TrayCmd::InstallUpdate,
        Command::RetryFailed => TrayCmd::RetryFailed,
        Command::ClearActivity => {
            hub.clear_activity();
            return Ok(Vec::new());
        }
        Command::ExportDiagnostics => TrayCmd::ExportDiagnostics,
        Command::Reset { project_id, path } => {
            let path = path.map(PathBuf::from);
            if path.as_ref().is_some_and(|p| !p.is_absolute()) {
                return Err("sökvägen måste vara absolut".into());
            }
            if project_id.is_none() && path.is_none() {
                return Err("reset behöver project_id eller path".into());
            }
            TrayCmd::Reset { project_id, path }
        }
        Command::ConfirmDeletes {
            project_id,
            confirm,
        } => TrayCmd::ConfirmDeletes {
            project_id,
            confirm,
        },
        Command::ResetAll => TrayCmd::ResetAll,
        Command::MoveSyncRoot { path } => {
            let path = PathBuf::from(path.trim());
            // Answered at once so the window can show why; the engine checks again.
            crate::sync::check_new_root(&hub.sync_root, &path)?;
            TrayCmd::MoveSyncRoot { path }
        }
        Command::QuitEngine => TrayCmd::Quit,
    };
    cmds.send(tray).map_err(|_| "motorn kör inte".to_string())?;
    Ok(Vec::new())
}

#[cfg(test)]
mod resilience_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Severity, Transfer};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;

    fn parse(s: &str) -> Command {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_every_command_from_the_contract() {
        assert_eq!(
            parse(r#"{"cmd":"hello","version":"0.2.0"}"#),
            Command::Hello {
                version: Some("0.2.0".into())
            }
        );
        assert_eq!(
            parse(r#"{"cmd":"sign_in"}"#),
            Command::SignIn {
                provider: None,
                region: None,
                username: None,
                password: None,
            }
        );
        assert_eq!(
            parse(r#"{"cmd":"sign_out","id":2}"#),
            Command::SignOut { provider: None }
        );
        assert_eq!(parse(r#"{"cmd":"sync_now"}"#), Command::SyncNow);
        assert_eq!(
            parse(r#"{"cmd":"open_folder"}"#),
            Command::OpenFolder { path: None }
        );
        assert_eq!(
            parse(r#"{"cmd":"open_folder","path":"C:\\Synka\\x"}"#),
            Command::OpenFolder {
                path: Some("C:\\Synka\\x".into())
            }
        );
        assert_eq!(parse(r#"{"cmd":"open_log"}"#), Command::OpenLog);
        assert_eq!(
            parse(r#"{"cmd":"refresh_projects"}"#),
            Command::RefreshProjects
        );
        assert_eq!(
            parse(r#"{"cmd":"set_project","hub_id":"b.h1","project_id":"b.p1","subscribe":true}"#),
            Command::SetProject {
                hub_id: "b.h1".into(),
                project_id: "b.p1".into(),
                subscribe: true
            }
        );
        assert_eq!(
            parse(r#"{"cmd":"set_setting","key":"max_downloads","value":4}"#),
            Command::SetSetting {
                key: "max_downloads".into(),
                value: json!(4)
            }
        );
        assert_eq!(
            parse(r#"{"cmd":"set_setting","key":"log_level","value":"debug"}"#),
            Command::SetSetting {
                key: "log_level".into(),
                value: json!("debug")
            }
        );
        assert_eq!(
            parse(r#"{"cmd":"free_up_space"}"#),
            Command::FreeUpSpace { project_id: None }
        );
        assert_eq!(
            parse(r#"{"cmd":"free_up_space","project_id":"b.p1"}"#),
            Command::FreeUpSpace {
                project_id: Some("b.p1".into())
            }
        );
        assert_eq!(parse(r#"{"cmd":"check_updates"}"#), Command::CheckUpdates);
        assert_eq!(parse(r#"{"cmd":"install_update"}"#), Command::InstallUpdate);
        assert_eq!(parse(r#"{"cmd":"reset_all"}"#), Command::ResetAll);
        assert_eq!(
            parse(r#"{"cmd":"move_sync_root","path":"D:\\Projekt\\Synka"}"#),
            Command::MoveSyncRoot {
                path: "D:\\Projekt\\Synka".into()
            }
        );
        assert_eq!(parse(r#"{"cmd":"retry_failed"}"#), Command::RetryFailed);
        assert_eq!(parse(r#"{"cmd":"clear_activity"}"#), Command::ClearActivity);
        assert_eq!(
            parse(r#"{"cmd":"export_diagnostics"}"#),
            Command::ExportDiagnostics
        );
        assert_eq!(
            parse(r#"{"cmd":"reset","project_id":"b.p1"}"#),
            Command::Reset {
                project_id: Some("b.p1".into()),
                path: None
            }
        );
        assert_eq!(
            parse(r#"{"cmd":"confirm_deletes","project_id":"b.p1","confirm":false}"#),
            Command::ConfirmDeletes {
                project_id: "b.p1".into(),
                confirm: false
            }
        );
        assert_eq!(parse(r#"{"cmd":"quit_engine"}"#), Command::QuitEngine);
    }

    #[test]
    fn command_round_trip() {
        for s in [
            r#"{"cmd":"set_project","hub_id":"b.h1","project_id":"b.p1","subscribe":false}"#,
            r#"{"cmd":"set_setting","key":"poll_active_secs","value":60}"#,
            r#"{"cmd":"set_setting","key":"start_at_login","value":true}"#,
        ] {
            let c = parse(s);
            let back: Command = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn unknown_or_broken_commands_are_rejected() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let r = handle_line(br#"{"cmd":"nope","id":7}"#, &hub, &tx);
        assert_eq!(r.len(), 1);
        let v: Value = serde_json::from_str(&r[0]).unwrap();
        assert_eq!(v["type"], "ack");
        assert_eq!(v["id"], 7);
        assert_eq!(v["ok"], false);

        let r = handle_line(
            br#"{"cmd":"set_setting","id":8,"key":"max_uploads","value":9}"#,
            &hub,
            &tx,
        );
        let v: Value = serde_json::from_str(&r[0]).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["id"], 8);

        let r = handle_line(
            br#"{"cmd":"open_folder","id":9,"path":"relativ"}"#,
            &hub,
            &tx,
        );
        let v: Value = serde_json::from_str(&r[0]).unwrap();
        assert_eq!(v["ok"], false);

        // A move into the current root is refused in the ack, with the reason.
        let r = handle_line(
            br#"{"cmd":"move_sync_root","id":10,"path":"C:\\Synka\\Ny"}"#,
            &hub,
            &tx,
        );
        let v: Value = serde_json::from_str(&r[0]).unwrap();
        assert_eq!(
            (v["ok"].clone(), v["id"].clone()),
            (json!(false), json!(10))
        );
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("nuvarande synkmappen"),
            "{v}"
        );
    }

    #[test]
    fn client_id_override_commands_are_validated_before_reaching_the_engine() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        for value in [json!(""), json!(" \t\r\n")] {
            let line = json!({
                "cmd": "set_setting", "id": 21, "key": "aps_client_id", "value": value
            })
            .to_string();
            let replies = handle_line(line.as_bytes(), &hub, &tx);
            assert_eq!(
                serde_json::from_str::<Value>(&replies[0]).unwrap(),
                json!({"type": "ack", "ok": true, "id": 21})
            );
            assert!(matches!(
                rx.try_recv(),
                Ok(TrayCmd::SetSetting { key, value: forwarded })
                    if key == "aps_client_id" && forwarded == value
            ));
        }

        for value in [
            Value::Null,
            json!(false),
            json!("valid-but-untested-client"),
            json!("https://aps.autodesk.com/app/test"),
            json!("client id"),
            json!("client\tid"),
            json!("client\u{0000}id"),
            json!("x".repeat(257)),
        ] {
            let line = json!({
                "cmd": "set_setting", "id": 22, "key": "aps_client_id", "value": value
            })
            .to_string();
            let replies = handle_line(line.as_bytes(), &hub, &tx);
            let reply: Value = serde_json::from_str(&replies[0]).unwrap();
            assert_eq!(reply["type"], "ack");
            assert_eq!(reply["id"], 22);
            assert_eq!(reply["ok"], false, "accepted {value}");
            assert!(reply["error"].as_str().unwrap().contains("aps_client_id"));
            assert!(rx.try_recv().is_err(), "forwarded {value}");
        }
    }

    #[test]
    fn client_validation_commands_require_valid_ids_and_preserve_correlation() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let request_id = "82e8eba6-50d1-4be0-a81f-c84f32777725";
        for command in ["test_aps_client_id", "cancel_aps_client_id_test"] {
            let line = json!({"cmd": command, "id": 31, "client_id": " test-client ", "request_id": request_id}).to_string();
            let replies = handle_line(line.as_bytes(), &hub, &tx);
            assert_eq!(
                serde_json::from_str::<Value>(&replies[0]).unwrap()["ok"],
                true
            );
            match rx.try_recv().unwrap() {
                TrayCmd::TestApsClientId {
                    client_id,
                    request_id: id,
                } => {
                    assert_eq!(client_id, "test-client");
                    assert_eq!(id, request_id);
                }
                TrayCmd::CancelApsClientIdTest { request_id: id } => assert_eq!(id, request_id),
                other => panic!("wrong forwarded command: {other:?}"),
            }
            for invalid in [
                "",
                "not-a-uuid",
                "x".repeat(1000).as_str(),
                "82e8eba6-50d1-4be0-a81f-c84f3277772!",
            ] {
                let line = json!({"cmd": command, "id": 32, "client_id": "test-client", "request_id": invalid}).to_string();
                let replies = handle_line(line.as_bytes(), &hub, &tx);
                assert_eq!(
                    serde_json::from_str::<Value>(&replies[0]).unwrap()["ok"],
                    false
                );
                assert!(rx.try_recv().is_err());
            }
        }
        for invalid_client in [
            "",
            " ",
            "bad client",
            "https://example.invalid",
            "x".repeat(257).as_str(),
        ] {
            let line = json!({"cmd": "test_aps_client_id", "id": 33, "client_id": invalid_client, "request_id": request_id}).to_string();
            let replies = handle_line(line.as_bytes(), &hub, &tx);
            assert_eq!(
                serde_json::from_str::<Value>(&replies[0]).unwrap()["ok"],
                false
            );
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn client_validation_outcome_replays_after_reconnect_without_credentials() {
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let request_id = "82e8eba6-50d1-4be0-a81f-c84f32777725";
        for state in ["testing", "saved", "failed", "cancelled"] {
            let mut live = hub.events.subscribe();
            let error = (state == "failed").then(|| "HTTP 401: invalid_client".into());
            hub.on_msg(&UiMsg::ApsClientIdValidation {
                request_id: request_id.into(),
                state,
                error: error.clone(),
            });
            let event: Value = serde_json::from_str(&live.try_recv().unwrap()).unwrap();
            drop(live);
            let replay = hub
                .snapshot()
                .into_iter()
                .map(|line| serde_json::from_str::<Value>(&line).unwrap())
                .find(|v| v["type"] == "aps_client_id_validation")
                .unwrap();
            assert_eq!(replay, event);
            assert_eq!(
                replay,
                json!({"type": "aps_client_id_validation", "request_id": request_id, "state": state, "error": error})
            );
        }
        assert_eq!(
            hub.snapshot()
                .iter()
                .filter(|line| line.contains("aps_client_id_validation"))
                .count(),
            1
        );
    }

    #[test]
    fn client_validation_cancel_replays_only_terminal_results() {
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let mut events = hub.events.subscribe();
        let request_id = "82e8eba6-50d1-4be0-a81f-c84f32777725";
        assert!(!hub.replay_client_validation_result(request_id));
        hub.on_msg(&UiMsg::ApsClientIdValidation {
            request_id: request_id.into(),
            state: "testing",
            error: None,
        });
        events.try_recv().unwrap();
        assert!(!hub.replay_client_validation_result(request_id));
        assert!(events.try_recv().is_err());
        for state in ["saved", "failed", "cancelled"] {
            hub.on_msg(&UiMsg::ApsClientIdValidation {
                request_id: request_id.into(),
                state,
                error: None,
            });
            let original = events.try_recv().unwrap();
            assert!(hub.replay_client_validation_result(request_id));
            assert_eq!(events.try_recv().unwrap(), original);
        }
    }

    #[test]
    fn client_validation_replay_keeps_recent_windows_and_has_a_fixed_bound() {
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        for operation in 0..17 {
            hub.on_msg(&UiMsg::ApsClientIdValidation {
                request_id: format!("82e8eba6-50d1-4be0-a81f-{operation:012x}"),
                state: "cancelled",
                error: None,
            });
        }
        let events = hub
            .snapshot()
            .into_iter()
            .map(|line| serde_json::from_str::<Value>(&line).unwrap())
            .filter(|event| event["type"] == "aps_client_id_validation")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), CLIENT_VALIDATION_CAP);
        assert_eq!(
            events.first().unwrap()["request_id"],
            "82e8eba6-50d1-4be0-a81f-000000000001"
        );
        assert_eq!(
            events.last().unwrap()["request_id"],
            "82e8eba6-50d1-4be0-a81f-000000000010"
        );
        assert!(events.iter().all(|event| event["state"] == "cancelled"));
    }

    #[test]
    fn a_pending_sign_in_wait_is_replayed_to_a_new_window_until_it_ends() {
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let (cmds, _rx) = tokio::sync::mpsc::unbounded_channel();
        let hello = |hub: &IpcHub| -> Vec<Value> {
            handle_line(br#"{"cmd":"hello"}"#, hub, &cmds)
                .iter()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect()
        };
        let sign_in =
            |hub: &IpcHub, provider: Provider, state: &'static str, error: Option<&str>| {
                hub.on_msg(&UiMsg::SignInProgress {
                    provider,
                    state,
                    error: error.map(str::to_string),
                });
            };
        let pending = |lines: &[Value]| -> Vec<Value> {
            lines
                .iter()
                .filter(|v| v["type"] == "sign_in_progress")
                .cloned()
                .collect()
        };
        hub.on_msg(&UiMsg::Status(Status {
            text: "Logga in".into(),
            state: "signed_out",
            sync_root: r"C:\Synka".into(),
            engine_version: "test".into(),
            ..Default::default()
        }));
        sign_in(&hub, Provider::StreamBim, "mfa_required", None);
        let reply = hello(&hub);
        assert_eq!(reply[0]["type"], "status", "the replay follows status");
        assert_eq!(
            pending(&reply),
            vec![
                json!({"type": "sign_in_progress", "provider": "streambim", "state": "mfa_required", "error": null})
            ]
        );
        assert_eq!(reply[1]["type"], "sign_in_progress");

        // Two providers waiting at once: one entry each, in account order.
        sign_in(&hub, Provider::Acc, "browser", None);
        let states: Vec<(String, String)> = pending(&hello(&hub))
            .iter()
            .map(|v| {
                (
                    v["provider"].as_str().unwrap().into(),
                    v["state"].as_str().unwrap().into(),
                )
            })
            .collect();
        assert_eq!(
            states,
            vec![
                ("acc".into(), "browser".into()),
                ("streambim".into(), "mfa_required".into())
            ]
        );

        // A terminal state ends the wait: it is not replayed any more.
        sign_in(
            &hub,
            Provider::StreamBim,
            "cancelled",
            Some("Tiden för koden gick ut – logga in igen."),
        );
        assert_eq!(pending(&hello(&hub)).len(), 1);
        sign_in(&hub, Provider::Acc, "signed_in", None);
        assert!(pending(&hello(&hub)).is_empty());

        sign_in(&hub, Provider::Interaxo, "browser", None);
        sign_in(&hub, Provider::Interaxo, "failed", Some("HTTP 401"));
        assert!(pending(&hello(&hub)).is_empty());
    }

    #[test]
    fn a_checked_move_reaches_the_engine() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let base = std::env::temp_dir().join(format!("synka-ipc-move-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let hub = IpcHub::new(base.join("Synka"));
        let to = base.join("Ny plats").join("Synka");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        let line =
            json!({"cmd": "move_sync_root", "id": 11, "path": to.to_string_lossy()}).to_string();
        let r = handle_line(line.as_bytes(), &hub, &tx);
        let v: Value = serde_json::from_str(&r[0]).unwrap();
        // Wine and some CI disks are not NTFS: then the reason names the drive, not the path.
        match v["ok"].as_bool() {
            Some(true) => {
                assert!(matches!(rx.try_recv(), Ok(TrayCmd::MoveSyncRoot { path }) if path == to))
            }
            _ => assert!(v["error"].as_str().unwrap().contains("disk"), "{v}"),
        }
        assert!(handle_line(br#"{"cmd":"reset_all","id":12}"#, &hub, &tx).len() == 1);
        assert!(matches!(rx.try_recv(), Ok(TrayCmd::ResetAll)));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn commands_reach_the_engine_and_are_acked() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let r = handle_line(br#"{"cmd":"sync_now","id":3}"#, &hub, &tx);
        assert_eq!(r.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&r[0]).unwrap(),
            json!({"type":"ack","ok":true,"id":3})
        );
        assert!(matches!(rx.try_recv(), Ok(TrayCmd::SyncNow)));

        // hello is answered here; the engine only hears that a window connected.
        let r = handle_line(br#"{"cmd":"hello","id":4,"version":"x"}"#, &hub, &tx);
        assert!(matches!(rx.try_recv(), Ok(TrayCmd::Hello)));
        assert!(rx.try_recv().is_err());
        assert_eq!(
            serde_json::from_str::<Value>(&r[0]).unwrap(),
            json!({"type":"ack","ok":true,"id":4})
        );
        assert!(r.len() >= 3, "ack + projects + activity");
    }

    #[test]
    fn status_event_shape() {
        let s = Status {
            user: Some(crate::auth::UserInfo {
                user_id: "u".into(),
                name: "Anna".into(),
                email: "a@x.se".into(),
            }),
            text: "Synkar…".into(),
            state: "syncing",
            error: None,
            pending_jobs: 2,
            failed_jobs: 1,
            transfers: vec![Transfer {
                name: "plan.rvt".into(),
                project: "Projekt 1".into(),
                direction: "down",
                done: 10,
                total: 100,
            }],
            sync_root: r"C:\Synka".into(),
            held_deletes: vec![],
            local_bytes: None,
            engine_version: "0.1.0".into(),
            accounts: vec![AccountStatus {
                provider: Provider::Acc,
                label: "ACC",
                configured: true,
                signed_in: true,
                signing_in: false,
                user: Some(crate::auth::UserInfo {
                    user_id: "u".into(),
                    name: "Anna".into(),
                    email: "a@x.se".into(),
                }),
                host: None,
            }],
        };
        let v = status_json(&s);
        assert_eq!(v["type"], "status");
        assert_eq!(v["signed_in"], true);
        assert_eq!(v["accounts"][0]["provider"], "acc");
        assert_eq!(v["accounts"][0]["signed_in"], true);
        assert!(
            !v.to_string().contains("\"u\""),
            "user ids never reach the UI"
        );
        assert_eq!(v["user"], json!({"name":"Anna","email":"a@x.se"}));
        assert_eq!(v["state"], "syncing");
        assert!(v["error"].is_null());
        assert_eq!(v["pending_jobs"], 2);
        assert_eq!(v["failed_jobs"], 1);
        assert_eq!(
            v["transfers"][0],
            json!({"name":"plan.rvt","project":"Projekt 1","direction":"down","done":10,"total":100})
        );
        assert!(v["local_bytes"].is_null());
        assert_eq!(v["engine_version"], "0.1.0");
        assert!(v["user"].get("user_id").is_none());
    }

    #[test]
    fn projects_event_shape_and_local_path() {
        let hub = HubInfo {
            id: "b.h1".into(),
            name: "Byggfirma: Syd".into(),
            region: None,
            provider: crate::model::Provider::Acc,
        };
        let p1 = ProjectInfo {
            id: "b.p1".into(),
            hub_id: "b.h1".into(),
            name: "Kv 5".into(),
            root_folder_urn: None,
            scopes: vec![],
            provider: crate::model::Provider::Acc,
            meta: None,
        };
        let p2 = ProjectInfo {
            id: "b.p2".into(),
            hub_id: "b.h1".into(),
            name: "Aa".into(),
            root_folder_urn: None,
            scopes: vec![],
            provider: crate::model::Provider::Acc,
            meta: None,
        };
        let hubs: HubList = vec![(hub, vec![(p1, true), (p2, false)])];
        let v = projects_json(&hubs, false, Path::new(r"C:\Synka"));
        assert_eq!(v["type"], "projects");
        assert_eq!(v["loading"], false);
        let h = &v["hubs"][0];
        assert_eq!(h["id"], "b.h1");
        // projects sorted by name: "Aa" (not subscribed) first
        assert_eq!(h["projects"][0]["name"], "Aa");
        assert_eq!(h["projects"][0]["subscribed"], false);
        assert!(h["projects"][0]["local_path"].is_null());
        assert_eq!(h["projects"][1]["subscribed"], true);
        assert_eq!(
            h["projects"][1]["local_path"],
            r"C:\Synka\Byggfirma_ Syd\Kv 5"
        );
    }

    #[test]
    fn notify_update_settings_and_activity_shapes() {
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let mut rx = hub.events.subscribe();

        hub.on_msg(&UiMsg::notify(Severity::Warning, "Kan inte avsluta"));
        let v: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"type":"notify","title":"Synka","body":"Kan inte avsluta","severity":"warning"})
        );

        hub.on_msg(&UiMsg::Update {
            available: true,
            version: Some("0.3.0".into()),
            error: None,
            installing: false,
        });
        let v: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"type":"update","available":true,"version":"0.3.0","error":null,"installing":false})
        );

        hub.on_msg(&UiMsg::Settings(SettingsView {
            propagate_deletes: true,
            max_downloads: 4,
            max_uploads: 2,
            poll_active_secs: 30,
            log_level: "info".into(),
            start_at_login: false,
            auto_update: true,
            update_channel_configured: false,
            sync_root: r"C:\Synka".into(),
            client_id_configured: false,
            aps_client_id: None,
            aps_client_id_override_supported: true,
            aps_client_id_validation_supported: true,
            providers: vec!["acc", "streambim", "interaxo"],
            interaxo_client_id: None,
            interaxo_redirect_uri: "http://localhost:47864/".into(),
            interaxo_redirect_uri_default: "http://localhost:47864/",
            interaxo_client_secret_set: false,
        }));
        let v: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(v["type"], "settings");
        assert_eq!(v["max_downloads"], 4);
        assert_eq!(v["client_id_configured"], false);
        assert!(v["aps_client_id"].is_null());
        assert_eq!(v["aps_client_id_override_supported"], true);

        for i in 0..205 {
            hub.on_msg(&UiMsg::Activity(ActivityItem {
                ts_ms: i,
                kind: "download",
                name: format!("f{i}"),
                project: "P".into(),
                detail: String::new(),
            }));
        }
        let v: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(v["type"], "activity_item");
        assert_eq!(v["item"]["name"], "f0");
        // The ring keeps the newest 200, newest first.
        let snap = hub.snapshot();
        let act: Value = serde_json::from_str(snap.last().unwrap()).unwrap();
        assert_eq!(act["type"], "activity");
        assert_eq!(act["items"].as_array().unwrap().len(), 200);
        assert_eq!(act["items"][0]["name"], "f204");
        assert_eq!(act["items"][199]["name"], "f5");
    }

    #[test]
    fn settings_snapshot_exposes_only_the_normalized_saved_client_id_override() {
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let mut rx = hub.events.subscribe();
        let mut config = config::Config::default();
        for input in [None, Some(" \ttest-client_ID_123\r\n"), Some(" \t\r\n")] {
            config.aps_client_id = input.map(str::to_string);
            hub.on_msg(&UiMsg::Settings(SettingsView {
                propagate_deletes: config.propagate_deletes,
                max_downloads: config.max_downloads,
                max_uploads: config.max_uploads,
                poll_active_secs: config.poll_active_secs,
                log_level: config.log_level.clone(),
                start_at_login: false,
                auto_update: config.auto_update,
                update_channel_configured: !config.update_channel.trim().is_empty(),
                sync_root: r"C:\Synka".into(),
                client_id_configured: config.client_id_configured(),
                aps_client_id: config.explicit_client_id_override().map(str::to_string),
                aps_client_id_override_supported: true,
                aps_client_id_validation_supported: true,
                providers: vec!["acc", "streambim", "interaxo"],
                interaxo_client_id: None,
                interaxo_redirect_uri: "http://localhost:47864/".into(),
                interaxo_redirect_uri_default: "http://localhost:47864/",
                interaxo_client_secret_set: false,
            }));
            let event: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(event["aps_client_id_override_supported"], true);
            assert_eq!(event["client_id_configured"], true);
            if input.is_some_and(|id| !id.trim().is_empty()) {
                assert_eq!(event["aps_client_id"], "test-client_ID_123");
            } else {
                assert!(event["aps_client_id"].is_null());
            }
            let cached = hub
                .snapshot()
                .into_iter()
                .map(|line| serde_json::from_str::<Value>(&line).unwrap())
                .find(|value| value["type"] == "settings")
                .unwrap();
            assert_eq!(cached, event);
        }
    }

    #[test]
    fn sign_in_commands_parse_with_defaults_and_validate_each_field() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));

        // ACC needs no fields.
        let r = handle_line(br#"{"cmd":"sign_in","id":1}"#, &hub, &tx);
        assert_eq!(
            serde_json::from_str::<Value>(&r[0]).unwrap(),
            json!({"type":"ack","ok":true,"id":1})
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(TrayCmd::SignIn {
                provider: Provider::Acc,
                streambim: None
            })
        ));

        // StreamBIM with everything it needs; the username is trimmed.
        let r = handle_line(
            br#"{"cmd":"sign_in","id":2,"provider":"streambim","region":"app","username":" anna@example.se ","password":"pw-one"}"#,
            &hub,
            &tx,
        );
        assert_eq!(
            serde_json::from_str::<Value>(&r[0]).unwrap()["ok"],
            json!(true)
        );
        match rx.try_recv() {
            Ok(TrayCmd::SignIn {
                provider: Provider::StreamBim,
                streambim: Some(login),
            }) => {
                assert_eq!(login.region, "app");
                assert_eq!(login.username, "anna@example.se");
                assert_eq!(login.password.0, "pw-one");
            }
            other => panic!("{other:?}"),
        }

        // Each missing or invalid StreamBIM field, and an unknown provider, is refused in the ack.
        for line in [
            r#"{"cmd":"sign_in","id":3,"provider":"streambim","username":"a@x.se","password":"p"}"#,
            r#"{"cmd":"sign_in","id":4,"provider":"streambim","region":"App","username":"a@x.se","password":"p"}"#,
            r#"{"cmd":"sign_in","id":5,"provider":"streambim","region":"app.se","username":"a@x.se","password":"p"}"#,
            r#"{"cmd":"sign_in","id":6,"provider":"streambim","region":"app","username":"  ","password":"p"}"#,
            r#"{"cmd":"sign_in","id":7,"provider":"streambim","region":"app","username":"a@x.se"}"#,
            r#"{"cmd":"sign_in","id":8,"provider":"streambim","region":"app","username":"a@x.se","password":""}"#,
            r#"{"cmd":"sign_in","id":9,"provider":"nope","region":"app"}"#,
        ] {
            let r = handle_line(line.as_bytes(), &hub, &tx);
            let v: Value = serde_json::from_str(&r[0]).unwrap();
            assert_eq!(v["ok"], false, "{line}: {v}");
            assert_eq!(v["type"], "ack");
            assert!(
                rx.try_recv().is_err(),
                "refused line reached the engine: {line}"
            );
        }
    }

    #[test]
    fn mfa_cancel_sign_out_and_regions_commands_validate_their_fields() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));

        let r = handle_line(
            br#"{"cmd":"sign_in_mfa","id":1,"provider":"streambim","code":"123456"}"#,
            &hub,
            &tx,
        );
        assert_eq!(
            serde_json::from_str::<Value>(&r[0]).unwrap()["ok"],
            json!(true)
        );
        match rx.try_recv() {
            Ok(TrayCmd::SignInMfa {
                provider: Provider::StreamBim,
                code,
            }) => assert_eq!(code.0, "123456"),
            other => panic!("{other:?}"),
        }
        for line in [
            r#"{"cmd":"sign_in_mfa","id":2,"provider":"streambim","code":"12345"}"#,
            r#"{"cmd":"sign_in_mfa","id":3,"provider":"streambim","code":"1234567"}"#,
            r#"{"cmd":"sign_in_mfa","id":4,"provider":"streambim","code":"12a456"}"#,
            r#"{"cmd":"sign_in_mfa","id":5,"provider":"acc","code":"123456"}"#,
            r#"{"cmd":"sign_in_mfa","id":6,"provider":"streambim","code":123456}"#,
            r#"{"cmd":"sign_in_mfa","id":7,"provider":"nope","code":"123456"}"#,
        ] {
            let r = handle_line(line.as_bytes(), &hub, &tx);
            let v: Value = serde_json::from_str(&r[0]).unwrap();
            assert_eq!(v["ok"], false, "{line}: {v}");
            assert!(
                rx.try_recv().is_err(),
                "refused line reached the engine: {line}"
            );
        }

        // Cancel is valid for every provider.
        handle_line(
            br#"{"cmd":"cancel_sign_in","provider":"interaxo"}"#,
            &hub,
            &tx,
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(TrayCmd::CancelSignIn {
                provider: Provider::Interaxo
            })
        ));
        let r = handle_line(
            br#"{"cmd":"cancel_sign_in","id":8,"provider":"x"}"#,
            &hub,
            &tx,
        );
        assert_eq!(
            serde_json::from_str::<Value>(&r[0]).unwrap()["ok"],
            json!(false)
        );

        // Sign-out with and without a provider.
        handle_line(br#"{"cmd":"sign_out"}"#, &hub, &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(TrayCmd::SignOut { provider: None })
        ));
        handle_line(br#"{"cmd":"sign_out","provider":"streambim"}"#, &hub, &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(TrayCmd::SignOut {
                provider: Some(Provider::StreamBim)
            })
        ));
        let r = handle_line(br#"{"cmd":"sign_out","id":9,"provider":"x"}"#, &hub, &tx);
        assert_eq!(
            serde_json::from_str::<Value>(&r[0]).unwrap()["ok"],
            json!(false)
        );
        assert!(rx.try_recv().is_err());

        handle_line(br#"{"cmd":"streambim_regions"}"#, &hub, &tx);
        assert!(matches!(rx.try_recv(), Ok(TrayCmd::StreamBimRegions)));
    }

    #[test]
    fn passwords_and_codes_never_reach_an_ack_or_a_debug_string() {
        const PASSWORD: &str = "Hemligt-Lösen-7731";
        const CODE: &str = "548213";
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = IpcHub::new(PathBuf::from(r"C:\Synka"));
        let lines = [
            // accepted: the password reaches the engine, but never a text
            format!(r#"{{"cmd":"sign_in","id":1,"provider":"streambim","region":"app","username":"a@x.se","password":"{pw}"}}"#, pw = PASSWORD),
            // a password that is not text: serde would echo the value, the reader must not
            r#"{"cmd":"sign_in","id":2,"provider":"streambim","region":"app","username":"a@x.se","password":4711123}"#.to_string(),
            // a code that is not text
            format!(r#"{{"cmd":"sign_in_mfa","id":3,"provider":"streambim","code":{code}}}"#, code = CODE),
            // a code for another provider
            format!(r#"{{"cmd":"sign_in_mfa","id":4,"provider":"acc","code":"{code}"}}"#, code = CODE),
            // broken JSON that still contains the password
            format!(r#"{{"cmd":"sign_in","id":5,"provider":"streambim","password":"{pw}","extra":[}}"#, pw = PASSWORD),
        ];
        let mut seen: Vec<String> = Vec::new();
        for line in &lines {
            for reply in handle_line(line.as_bytes(), &hub, &tx) {
                seen.push(reply);
            }
        }
        while let Ok(cmd) = rx.try_recv() {
            seen.push(format!("{cmd:?}"));
        }
        assert!(seen.len() >= 4, "{seen:?}");
        for text in &seen {
            assert!(!text.contains(PASSWORD), "password leaked: {text}");
            assert!(!text.contains(CODE), "code leaked: {text}");
            assert!(!text.contains("4711123"), "password value leaked: {text}");
        }
        // The parsed command, too: its Debug output is what a log line would show.
        let parsed: Command = serde_json::from_str(&lines[0]).unwrap();
        assert!(!format!("{parsed:?}").contains(PASSWORD));
    }

    #[test]
    fn projects_event_names_the_provider_of_each_hub() {
        let hubs: HubList = vec![(
            HubInfo {
                id: "interaxo:c1".into(),
                name: "Interaxo – Byggstyrning".into(),
                region: Some("https://api.interaxo.se/v1".into()),
                provider: Provider::Interaxo,
            },
            Vec::new(),
        )];
        let v = projects_json(&hubs, false, Path::new(r"C:\Synka"));
        assert_eq!(v["hubs"][0]["provider"], "interaxo");
    }

    #[test]
    fn status_lists_every_account_in_order_without_user_ids() {
        let accounts = vec![
            AccountStatus {
                provider: Provider::Acc,
                label: "ACC",
                configured: true,
                signed_in: false,
                signing_in: false,
                user: None,
                host: None,
            },
            AccountStatus {
                provider: Provider::StreamBim,
                label: "StreamBIM",
                configured: true,
                signed_in: true,
                signing_in: false,
                user: Some(crate::auth::UserInfo {
                    user_id: "sb-secret-id".into(),
                    name: "Sven".into(),
                    email: "s@x.se".into(),
                }),
                host: Some("app.streambim.com".into()),
            },
            AccountStatus {
                provider: Provider::Interaxo,
                label: "Interaxo",
                configured: false,
                signed_in: false,
                signing_in: true,
                user: None,
                host: None,
            },
        ];
        let s = Status {
            accounts,
            text: "Synkad".into(),
            state: "synced",
            sync_root: r"C:\Synka".into(),
            engine_version: "x".into(),
            ..Default::default()
        };
        let v = status_json(&s);
        assert_eq!(v["signed_in"], true);
        let providers: Vec<&str> = v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["provider"].as_str().unwrap())
            .collect();
        assert_eq!(providers, ["acc", "streambim", "interaxo"]);
        assert_eq!(
            v["accounts"][1]["user"],
            json!({"name":"Sven","email":"s@x.se"})
        );
        assert_eq!(v["accounts"][1]["host"], "app.streambim.com");
        assert_eq!(v["accounts"][2]["signing_in"], true);
        assert!(v["accounts"][0]["user"].is_null());
        assert!(
            !v.to_string().contains("sb-secret-id"),
            "user ids never reach the UI"
        );
    }

    #[test]
    fn line_limit_is_enforced() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let big = vec![b'a'; MAX_LINE + 10];
            let (mut a, b) = tokio::io::duplex(MAX_LINE * 2);
            tokio::spawn(async move {
                let _ = a.write_all(&big).await;
            });
            let mut rd = BufReader::new(b);
            let mut buf = Vec::new();
            assert!(read_line(&mut rd, &mut buf).await.is_err());
        });
    }

    /// Needs the real named pipe for the current user: cargo test -- --ignored
    #[tokio::test]
    #[ignore = "opens the real named pipe of the current user"]
    async fn pipe_hello_gets_status() {
        let hub = Arc::new(IpcHub::new(PathBuf::from(r"C:\Synka")));
        hub.on_msg(&UiMsg::Status(Status {
            text: "Synkad".into(),
            state: "synced",
            sync_root: r"C:\Synka".into(),
            engine_version: "test".into(),
            ..Default::default()
        }));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve(hub.clone(), tx));

        let name = pipe_name().unwrap();
        let mut client = None;
        for _ in 0..50 {
            match ClientOptions::new().open(&name) {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let client = client.expect("could not open the pipe");
        let (rd, mut wr) = tokio::io::split(client);
        wr.write_all(b"{\"cmd\":\"hello\",\"id\":1,\"version\":\"test\"}\n")
            .await
            .unwrap();

        let mut lines = BufReader::new(rd).lines();
        let mut got_status = false;
        for _ in 0..8 {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("timeout waiting for the pipe")
                .unwrap()
                .expect("pipe closed");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["type"] == "status" {
                assert_eq!(v["text"], "Synkad");
                got_status = true;
                break;
            }
        }
        server.abort();
        assert!(got_status);
    }

    #[test]
    fn pipe_guard_refuses_autodesk_names() {
        for ok in [
            r"\\.\pipe\synka-ui-S-1-5-21-1-2-3-1001",
            pipe_name().unwrap().as_str(),
        ] {
            assert!(check_pipe_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            r"\\.\pipe\Autodesk.DesktopConnector.Service",
            r"\\.\pipe\DesktopConnectorService",
            r"\\.\pipe\AUTODESK",
            r"\\.\pipe\x-desktopconnector-y",
        ] {
            let err = check_pipe_name(bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{bad}");
        }
        // The guard runs before any pipe is created, so this must fail without side effects.
        let err = create_instance(r"\\.\pipe\DesktopConnectorService-synka-test", 0, true)
            .err()
            .unwrap();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}

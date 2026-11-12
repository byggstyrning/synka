#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Module docs are plain text laid out for reading in the source, not Markdown lists.
#![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]
//! Synka — syncs ACC Docs files to a local folder. See DESIGN.md.
//!
//! Startup: single-instance (named mutex "Local\\synka-engine-single-instance"; if taken, exit 0),
//! cf::expose_placeholders(), paths::resolve, Config::load(state_dir\settings.json), logging
//! (tracing-appender daily rolling in log_dir, keep 7 files, level from config, no ANSI),
//! panic hook that logs, Store::open, http::build_client, Auth::new(cfg.client_id()), Api,
//! UiSender with the main thread id (GetCurrentThreadId), Ctx, shortcut::ensure_once,
//! tokio runtime (multi-thread, 2 workers, thread name "synka-rt") running Engine::run and the
//! UI named-pipe server (ipc::serve) on a background std thread; when the engine returns,
//! PostThreadMessageW(main, WM_QUIT). The engine keeps running without a UI connected.
//! Main thread: tray::run_message_loop. On exit: drop runtime with shutdown_timeout(3 s).
//!
//! Explorer's COM server (`crate::shell`) is this same process: `-Embedding` starts are
//! normal starts, and the class objects are registered on the main thread before the message
//! loop in both modes. An `-Embedding` start that finds the mutex taken exits at once: the
//! running engine already serves the classes.

mod api;
mod app;
mod auth;
mod cf;
mod config;
mod diag;
mod http;
mod ipc;
mod model;
mod paths;
mod remote;
mod shell;
mod shortcut;
mod store;
mod streambim_auth;
mod sync;
#[cfg(test)]
mod test_support;
mod tray;
mod update;

use app::{Ctx, TrayCmd, UiMsg, UiSender};
use auth::Auth;
use config::Config;
use paths::Paths;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use store::Store;
use streambim_auth::StreamBimAccounts;
use sync::Engine;
use tokio::sync::mpsc::UnboundedSender;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, LPARAM, WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Recovery::{
    RegisterApplicationRestart, REGISTER_APPLICATION_RESTART_FLAGS,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, PostThreadMessageW, MB_ICONERROR, MB_OK, WM_QUIT,
};

fn main() -> anyhow::Result<()> {
    let result = run();
    if let Err(e) = &result {
        // windows_subsystem = "windows": there is no console, so show the failure.
        show_error(&format!("{e:#}"));
    }
    result
}

fn run() -> anyhow::Result<()> {
    // COM/WinRT for the main thread (paths, shortcut, tray). Balanced by process exit.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let embedding = shell::is_embedding();
    // A restart after a sync root move: the stopping engine may still hold the mutex briefly.
    let restarted = std::env::args().any(|a| a == RESTARTED_ARG);
    let _mutex = match acquire_single_instance(restarted)? {
        Some(h) => h,
        // Already running; with -Embedding that instance holds the class registrations.
        None => return Ok(()),
    };

    cf::expose_placeholders();

    let mut paths = paths::resolve(None)?;
    let mut cfg = Config::load(&paths.state_dir.join("settings.json"));
    cfg.acc_only = config::ACC_ONLY;
    if cfg.sync_root.is_some() {
        paths = paths::resolve(cfg.sync_root.as_deref())?;
    }

    let _log_guard = init_logging(&paths, &cfg)?;
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
    }));
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        acc_only = cfg.acc_only,
        packaged = paths.packaged,
        embedding,
        state_dir = %paths.state_dir.display(),
        sync_root = %paths.sync_root.display(),
        "engine starting"
    );
    if paths.packaged {
        // Windows restarts a registered process after it was stopped for a package update
        // (and after a crash or hang), so updates and failures never leave sync off.
        if let Err(e) = unsafe {
            RegisterApplicationRestart(PCWSTR::null(), REGISTER_APPLICATION_RESTART_FLAGS(0))
        } {
            tracing::warn!("RegisterApplicationRestart: {e}");
        }
    }

    let store = Arc::new(Store::open(&paths.db_file)?);
    let http = http::build_client();
    let auth = Arc::new(Auth::new(cfg.client_id(), http.clone(), &paths.state_dir));
    let api = http::Api::new(http.clone(), auth.clone());
    // One account per StreamBIM server; each account has its own API client (streambim_auth).
    // The ACC-only build reads no StreamBIM session from disk.
    let streambim = Arc::new(if cfg.acc_only {
        StreamBimAccounts::none(http.clone(), &paths.state_dir)
    } else {
        StreamBimAccounts::load(http.clone(), &paths.state_dir)
    });
    // Not configured (empty client id) until the user gives one; applying the settings at
    // runtime comes in a later step.
    let interaxo = Arc::new(Auth::interaxo(
        cfg.interaxo_client(),
        http.clone(),
        &paths.state_dir,
    ));
    let interaxo_api = http::Api::new(http, interaxo.clone());

    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiMsg>();
    let main_thread_id = unsafe { GetCurrentThreadId() };
    let hub = Arc::new(ipc::IpcHub::offering(
        paths.sync_root.clone(),
        !cfg.acc_only,
        cfg.interaxo_on(),
    ));
    let ui = UiSender {
        tx: ui_tx,
        main_thread_id,
        hub: hub.clone(),
    };

    shortcut::ensure_once(&store, &paths);

    let settings = RwLock::new(cfg.clone());
    let ctx = Arc::new(Ctx {
        cfg,
        settings,
        paths: paths.clone(),
        store: store.clone(),
        auth,
        api,
        streambim,
        interaxo,
        interaxo_api,
        ui,
    });
    shell::set_context(ctx.clone());

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TrayCmd>();
    shell::set_command_sender(cmd_tx.clone());
    // Main (STA) thread, before the message loop pumps the queued activation requests.
    let class_objects = shell::com::ClassObjects::register();
    let engine = Engine::new(ctx);
    let engine_thread = std::thread::Builder::new()
        .name("synka-engine".into())
        .spawn({
            let ipc_cmds = cmd_tx.clone();
            let hub = hub.clone();
            move || {
                run_engine(engine, cmd_rx, ipc_cmds, hub, main_thread_id);
            }
        })?;

    let loop_result = tray::run_message_loop(ui_rx, cmd_tx.clone(), hub, paths.clone());
    // No new Explorer calls once the loop has stopped pumping.
    drop(class_objects);

    // The engine normally ends on TrayCmd::Quit; make sure it is asked to stop, then wait.
    let _ = cmd_tx.send(TrayCmd::Quit);
    if engine_thread.join().is_err() {
        tracing::error!("engine thread panicked");
    }
    tracing::info!("engine stopped");
    if app::restart_requested() {
        match std::env::current_exe()
            .and_then(|exe| std::process::Command::new(exe).arg(RESTARTED_ARG).spawn())
        {
            Ok(_) => tracing::info!("engine restarting"),
            Err(e) => tracing::error!("could not restart the engine: {e}"),
        }
    }
    loop_result
}

/// Passed to the engine it starts again after a sync root move.
const RESTARTED_ARG: &str = "--restarted";

/// Runs the engine and the UI pipe server on their own tokio runtime; posts WM_QUIT to the main
/// thread when the engine is done.
fn run_engine(
    engine: Arc<Engine>,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<TrayCmd>,
    ipc_cmds: UnboundedSender<TrayCmd>,
    hub: Arc<ipc::IpcHub>,
    main_thread_id: u32,
) {
    match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("synka-rt")
        .enable_all()
        .build()
    {
        Ok(rt) => {
            rt.spawn(async move {
                if let Err(e) = ipc::serve(hub, ipc_cmds).await {
                    tracing::error!("ipc: {e:#}");
                }
            });
            if let Err(e) = rt.block_on(engine.run(cmd_rx)) {
                tracing::error!("engine stopped with error: {e:#}");
            }
            rt.shutdown_timeout(Duration::from_secs(3));
        }
        Err(e) => tracing::error!("could not start tokio runtime: {e}"),
    }
    unsafe {
        let _ = PostThreadMessageW(main_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

/// Returns the mutex handle (keep alive for the process lifetime), or None if another
/// instance already holds it. `wait`: keep trying for up to 30 s (the previous engine is
/// still shutting down).
fn acquire_single_instance(wait: bool) -> anyhow::Result<Option<HANDLE>> {
    for _ in 0..if wait { 60 } else { 1 } {
        unsafe {
            let handle = CreateMutexW(None, false, w!("Local\\synka-engine-single-instance"))?;
            if GetLastError() != ERROR_ALREADY_EXISTS {
                return Ok(Some(handle));
            }
            let _ = CloseHandle(handle);
        }
        if wait {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    Ok(None)
}

fn init_logging(paths: &Paths, cfg: &Config) -> anyhow::Result<WorkerGuard> {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("synka")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&paths.log_dir)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_new(&cfg.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(guard)
}

fn show_error(msg: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(msg), w!("Synka"), MB_OK | MB_ICONERROR);
    }
}

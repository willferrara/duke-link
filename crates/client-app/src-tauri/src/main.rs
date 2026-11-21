#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duke_link_client::launcher::{
    detect_minecraft_installs, launch_and_watch_horizon, validate_username, DetectedInstall,
    LaunchOutcome,
};
use duke_link_client::settings::{
    load_or_create_client_key, load_settings, resolve_data_dir, save_settings_atomic,
    ClientSettings,
};
use duke_link_client::sync::{sync_pack_over_quic, SyncSummary};
use duke_link_client::tunnel::{
    build_endpoint_addr, poll_status, start_tunnel_to_addr, ConnectionPathKind, TunnelHandle,
    TunnelStatusSnapshot,
};
use iroh::SecretKey;
use serde::Serialize;
use shared_protocol::allowlist::InviteTicket;
use tauri::{AppHandle, Emitter, RunEvent, State};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[cfg(windows)]
mod win_crash {
    use std::ffi::c_void;

    type ExceptionHandler = unsafe extern "system" fn(*mut c_void) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetUnhandledExceptionFilter(
            lp_top_level_exception_filter: Option<ExceptionHandler>,
        ) -> Option<ExceptionHandler>;
    }

    unsafe extern "system" fn unhandled_exception_handler(info: *mut c_void) -> i32 {
        let msg = format!(
            "Unhandled Win32 SEH exception (EXCEPTION_POINTERS={:?})\n",
            info
        );
        crate::append_crash_log(&msg);
        // EXCEPTION_CONTINUE_SEARCH = 0
        0
    }

    pub fn install() {
        // SAFETY: Registers a process-wide Win32 SEH top-level exception filter.
        unsafe {
            SetUnhandledExceptionFilter(Some(unhandled_exception_handler));
        }
    }
}

fn crash_log_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("duke-link-crash.log"));
        }
    }
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        if !local_app_data.trim().is_empty() {
            paths.push(
                PathBuf::from(local_app_data)
                    .join("duke-link")
                    .join("crash.log"),
            );
        }
    }
    paths
}

pub(crate) fn append_crash_log(message: &str) {
    for path in crash_log_paths() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(message.as_bytes());
            let _ = file.flush();
        }
    }
}

fn install_crash_handlers() {
    std::panic::set_hook(Box::new(|panic_info| {
        let loc = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_owned());
        let msg = format!("PANIC at {loc}: {panic_info}\n");
        append_crash_log(&msg);
    }));

    #[cfg(windows)]
    win_crash::install();
}

struct AppState {
    data_dir: PathBuf,
    secret_key: SecretKey,
    settings: Mutex<ClientSettings>,
    tunnel: Arc<AsyncMutex<Option<TunnelHandle>>>,
}

#[derive(Debug, Clone, Serialize)]
struct ClientBootstrapState {
    client_endpoint_id: String,
    data_dir: String,
    settings: ClientSettings,
    server_name: Option<String>,
    detected_installs: Vec<DetectedInstall>,
    tunnel_active: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PlayResult {
    sync: SyncSummary,
    launch: LaunchOutcome,
}

fn snapshot_settings(state: &AppState) -> Result<ClientSettings, String> {
    let guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?;
    Ok(guard.clone())
}

#[tauri::command]
async fn get_client_state(state: State<'_, AppState>) -> Result<ClientBootstrapState, String> {
    let settings = snapshot_settings(&state)?;
    let server_name = settings
        .invite_ticket
        .as_deref()
        .and_then(|raw| InviteTicket::decode(raw).ok())
        .map(|t| t.server_name);
    let tunnel_active = state.tunnel.lock().await.is_some();

    Ok(ClientBootstrapState {
        client_endpoint_id: state.secret_key.public().to_string(),
        data_dir: state.data_dir.display().to_string(),
        settings,
        server_name,
        detected_installs: detect_minecraft_installs(),
        tunnel_active,
    })
}

#[tauri::command]
fn save_config(
    invite_ticket: String,
    username: String,
    state: State<'_, AppState>,
) -> Result<ClientBootstrapState, String> {
    let ticket = InviteTicket::decode(&invite_ticket).map_err(|e| e.to_string())?;
    validate_username(&username).map_err(|e| e.to_string())?;

    let updated = {
        let mut guard = state
            .settings
            .lock()
            .map_err(|_| "settings lock poisoned".to_owned())?;
        guard.invite_ticket = Some(invite_ticket.trim().to_owned());
        guard.username = username.trim().to_owned();
        guard.clone()
    };

    save_settings_atomic(&state.data_dir, &updated).map_err(|e| e.to_string())?;

    Ok(ClientBootstrapState {
        client_endpoint_id: state.secret_key.public().to_string(),
        data_dir: state.data_dir.display().to_string(),
        settings: updated,
        server_name: Some(ticket.server_name),
        detected_installs: detect_minecraft_installs(),
        tunnel_active: false,
    })
}

async fn ensure_tunnel_started(
    state: &AppState,
    ticket: &InviteTicket,
    local_port: u16,
) -> Result<(iroh::Endpoint, iroh::EndpointAddr, String), String> {
    let server_addr = build_endpoint_addr(ticket).map_err(|e| e.to_string())?;
    let mut guard = state.tunnel.lock().await;

    if let Some(ref handle) = *guard {
        return Ok((
            handle.endpoint().clone(),
            handle.server_addr().clone(),
            handle.local_addr().to_string(),
        ));
    }

    let bind_addr = format!("127.0.0.1:{local_port}");
    let handle = start_tunnel_to_addr(state.secret_key.clone(), server_addr, &bind_addr)
        .await
        .map_err(|e| e.to_string())?;

    let ep = handle.endpoint().clone();
    let addr = handle.server_addr().clone();
    let local_str = handle.local_addr().to_string();
    *guard = Some(handle);
    Ok((ep, addr, local_str))
}

#[tauri::command]
async fn poll_server_status(state: State<'_, AppState>) -> Result<TunnelStatusSnapshot, String> {
    let settings = snapshot_settings(&state)?;
    let client_id = state.secret_key.public().to_string();

    let Some(raw_ticket) = settings.invite_ticket.as_deref() else {
        return Ok(TunnelStatusSnapshot {
            connected: false,
            local_bind_addr: format!("127.0.0.1:{}", settings.local_mc_port),
            server_name: "Not configured".to_owned(),
            server_endpoint_id: String::new(),
            client_endpoint_id: client_id,
            path_kind: ConnectionPathKind::Unknown,
            rtt_ms: None,
            server_online: false,
            players_online: 0,
            max_players: 0,
            motd: "Paste an invite ticket to connect".to_owned(),
            last_error: None,
        });
    };

    let ticket = InviteTicket::decode(raw_ticket).map_err(|e| e.to_string())?;
    let (ep, server_addr, local_bind_addr) =
        ensure_tunnel_started(&state, &ticket, settings.local_mc_port).await?;

    match poll_status(&ep, &server_addr).await {
        Ok((snap, path_kind, rtt_ms)) => Ok(TunnelStatusSnapshot {
            connected: true,
            local_bind_addr,
            server_name: ticket.server_name,
            server_endpoint_id: ticket.server_endpoint_id.to_string(),
            client_endpoint_id: client_id,
            path_kind,
            rtt_ms,
            server_online: snap.online,
            players_online: snap.players_online,
            max_players: snap.max_players,
            motd: snap.motd,
            last_error: None,
        }),
        Err(err) => Ok(TunnelStatusSnapshot {
            connected: false,
            local_bind_addr,
            server_name: ticket.server_name,
            server_endpoint_id: ticket.server_endpoint_id.to_string(),
            client_endpoint_id: client_id,
            path_kind: ConnectionPathKind::Unknown,
            rtt_ms: None,
            server_online: false,
            players_online: 0,
            max_players: 0,
            motd: "Relay unreachable".to_owned(),
            last_error: Some(err.to_string()),
        }),
    }
}

#[tauri::command]
async fn sync_and_play(app: AppHandle, state: State<'_, AppState>) -> Result<PlayResult, String> {
    let settings = snapshot_settings(&state)?;
    let raw_ticket = settings
        .invite_ticket
        .as_deref()
        .ok_or_else(|| "Please save a duke1: invite ticket first".to_owned())?;
    let ticket = InviteTicket::decode(raw_ticket).map_err(|e| e.to_string())?;
    validate_username(&settings.username).map_err(|e| e.to_string())?;

    let (ep, server_addr, _) =
        ensure_tunnel_started(&state, &ticket, settings.local_mc_port).await?;
    let instance_dir = settings
        .instance_dir
        .unwrap_or_else(|| state.data_dir.join("instance"));

    let sync_summary = sync_pack_over_quic(&ep, &server_addr, &instance_dir, |evt| {
        let _ = app.emit("sync-progress", evt);
    })
    .await
    .map_err(|e| e.to_string())?;

    let launch_outcome = launch_and_watch_horizon(
        &instance_dir,
        &settings.username,
        &ticket.server_name,
        settings.local_mc_port,
        &ep,
        &server_addr,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(PlayResult {
        sync: sync_summary,
        launch: launch_outcome,
    })
}

#[tauri::command]
async fn stop_tunnel(state: State<'_, AppState>) -> Result<(), String> {
    let maybe_handle = {
        let mut guard = state.tunnel.lock().await;
        guard.take()
    };
    if let Some(handle) = maybe_handle {
        handle.stop().await;
    }
    Ok(())
}

fn main() {
    // Line 1: install panic hook and Win32 SEH filter before anything else executes.
    install_crash_handlers();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let data_dir = resolve_data_dir();
    if let Err(err) = fs::create_dir_all(&data_dir) {
        error!(error = %err, dir = %data_dir.display(), "failed to create data directory");
        append_crash_log(&format!(
            "FATAL create_dir_all({}): {err}\n",
            data_dir.display()
        ));
        std::process::exit(1);
    }

    let secret_key = match load_or_create_client_key(&data_dir) {
        Ok(k) => k,
        Err(err) => {
            error!(error = %err, "failed to load or create client identity key");
            append_crash_log(&format!("FATAL load_or_create_client_key: {err}\n"));
            std::process::exit(1);
        }
    };

    let settings = match load_settings(&data_dir) {
        Ok(s) => s,
        Err(err) => {
            warn!(error = %err, "failed to load settings.json, falling back to defaults");
            ClientSettings::default()
        }
    };

    info!(
        client_id = %secret_key.public(),
        data_dir = %data_dir.display(),
        "starting duke-link client"
    );

    let tunnel = Arc::new(AsyncMutex::new(None));
    let tunnel_for_exit = Arc::clone(&tunnel);

    let app_state = AppState {
        data_dir,
        secret_key,
        settings: Mutex::new(settings),
        tunnel,
    };

    let app = match tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_client_state,
            save_config,
            poll_server_status,
            sync_and_play,
            stop_tunnel
        ])
        .build(tauri::generate_context!())
    {
        Ok(app) => app,
        Err(err) => {
            error!(error = %err, "failed to build Tauri application");
            append_crash_log(&format!("FATAL tauri::Builder::build: {err}\n"));
            std::process::exit(1);
        }
    };

    app.run(move |_app_handle, event| {
        if let RunEvent::Exit = event {
            let tunnel_ref = Arc::clone(&tunnel_for_exit);
            tauri::async_runtime::block_on(async move {
                let maybe_handle = {
                    let mut guard = tunnel_ref.lock().await;
                    guard.take()
                };
                if let Some(handle) = maybe_handle {
                    handle.stop().await;
                }
            });
        }
    });
}

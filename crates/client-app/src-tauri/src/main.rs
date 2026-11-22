#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duke_link_client::launcher::{
    detect_horizon_installs, detect_minecraft_installs, launch_and_watch_horizon,
    resolve_horizon_layout, spawn_horizon_client, validate_username, DetectedInstall,
    HorizonLayout, LaunchOutcome, SpawnedHorizon,
};
use duke_link_client::settings::{
    load_or_create_client_key, load_profile, load_settings, resolve_data_dir, save_profile_atomic,
    save_settings_atomic, ClientSettings, PlayerProfile,
};
use duke_link_client::sync::{sync_pack_over_quic, SyncSummary};
use duke_link_client::tunnel::{
    build_endpoint_addr, poll_status, start_tunnel_to_addr, ConnectionPathKind, TunnelHandle,
    TunnelStatusSnapshot,
};
use iroh::SecretKey;
use serde::Serialize;
use shared_protocol::allowlist::InviteTicket;
use shared_protocol::manifest::{DEFAULT_HORIZON_RELEASE_TAG, DEFAULT_HORIZON_REV};
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
    profile: Mutex<PlayerProfile>,
    diagnostic_logs: Mutex<Vec<String>>,
    tunnel: Arc<AsyncMutex<Option<TunnelHandle>>>,
}

#[derive(Debug, Clone, Serialize)]
struct ClientBootstrapState {
    client_endpoint_id: String,
    data_dir: String,
    settings: ClientSettings,
    profile: PlayerProfile,
    server_name: Option<String>,
    horizon_rev: String,
    horizon_release_tag: String,
    detected_installs: Vec<DetectedInstall>,
    detected_horizon_installs: Vec<HorizonLayout>,
    tunnel_active: bool,
    diagnostic_logs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PlayResult {
    sync: SyncSummary,
    launch: LaunchOutcome,
}

fn push_diag_log(state: &AppState, line: impl Into<String>) {
    if let Ok(mut logs) = state.diagnostic_logs.lock() {
        logs.push(line.into());
        if logs.len() > 200 {
            let overflow = logs.len() - 200;
            logs.drain(0..overflow);
        }
    }
}

fn snapshot_settings(state: &AppState) -> Result<ClientSettings, String> {
    let guard = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?;
    Ok(guard.clone())
}

fn snapshot_profile(state: &AppState) -> Result<PlayerProfile, String> {
    let guard = state
        .profile
        .lock()
        .map_err(|_| "profile lock poisoned".to_owned())?;
    Ok(guard.clone())
}

fn snapshot_logs(state: &AppState) -> Vec<String> {
    state
        .diagnostic_logs
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

#[tauri::command]
async fn get_client_state(state: State<'_, AppState>) -> Result<ClientBootstrapState, String> {
    let settings = snapshot_settings(&state)?;
    let profile = snapshot_profile(&state)?;
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
        profile,
        server_name,
        horizon_rev: DEFAULT_HORIZON_REV.to_owned(),
        horizon_release_tag: DEFAULT_HORIZON_RELEASE_TAG.to_owned(),
        detected_installs: detect_minecraft_installs(),
        detected_horizon_installs: detect_horizon_installs(&state.data_dir),
        tunnel_active,
        diagnostic_logs: snapshot_logs(&state),
    })
}

#[tauri::command]
fn save_config(
    invite_ticket: String,
    username: String,
    horizon_install_dir: Option<String>,
    direct_server_addr: Option<String>,
    state: State<'_, AppState>,
) -> Result<ClientBootstrapState, String> {
    let trimmed_ticket = invite_ticket.trim();
    let server_name = if !trimmed_ticket.is_empty() {
        let ticket = InviteTicket::decode(trimmed_ticket).map_err(|e| e.to_string())?;
        Some(ticket.server_name)
    } else {
        None
    };
    validate_username(&username).map_err(|e| e.to_string())?;

    let cleaned_install = horizon_install_dir
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let cleaned_direct = direct_server_addr
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    let updated_settings = {
        let mut guard = state
            .settings
            .lock()
            .map_err(|_| "settings lock poisoned".to_owned())?;
        guard.invite_ticket = if trimmed_ticket.is_empty() {
            None
        } else {
            Some(trimmed_ticket.to_owned())
        };
        guard.username = username.trim().to_owned();
        guard.instance_dir = cleaned_install.clone();
        guard.direct_server_addr = cleaned_direct.clone();
        guard.clone()
    };

    let updated_profile = {
        let mut guard = state
            .profile
            .lock()
            .map_err(|_| "profile lock poisoned".to_owned())?;
        guard.player_name = username.trim().to_owned();
        guard.horizon_install_dir = cleaned_install;
        guard.direct_server_addr = cleaned_direct;
        guard.clone()
    };

    save_settings_atomic(&state.data_dir, &updated_settings).map_err(|e| e.to_string())?;
    save_profile_atomic(&state.data_dir, &updated_profile).map_err(|e| e.to_string())?;
    push_diag_log(
        &state,
        format!(
            "Saved profile.toml (player_name={}) and settings.json",
            updated_profile.player_name
        ),
    );

    Ok(ClientBootstrapState {
        client_endpoint_id: state.secret_key.public().to_string(),
        data_dir: state.data_dir.display().to_string(),
        settings: updated_settings,
        profile: updated_profile,
        server_name,
        horizon_rev: DEFAULT_HORIZON_REV.to_owned(),
        horizon_release_tag: DEFAULT_HORIZON_RELEASE_TAG.to_owned(),
        detected_installs: detect_minecraft_installs(),
        detected_horizon_installs: detect_horizon_installs(&state.data_dir),
        tunnel_active: false,
        diagnostic_logs: snapshot_logs(&state),
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
            tps: None,
            horizon_rev: Some(DEFAULT_HORIZON_REV.to_owned()),
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
            tps: snap.tps,
            horizon_rev: snap
                .horizon_rev
                .or_else(|| Some(DEFAULT_HORIZON_REV.to_owned())),
            last_error: None,
        }),
        Err(err) => {
            push_diag_log(&state, format!("Status poll failed: {err}"));
            Ok(TunnelStatusSnapshot {
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
                tps: None,
                horizon_rev: Some(DEFAULT_HORIZON_REV.to_owned()),
                last_error: Some(err.to_string()),
            })
        }
    }
}

#[tauri::command]
async fn launch_horizon_client(
    player_name: Option<String>,
    install_dir: Option<String>,
    server_addr: Option<String>,
    state: State<'_, AppState>,
) -> Result<SpawnedHorizon, String> {
    let settings = snapshot_settings(&state)?;
    let profile = snapshot_profile(&state)?;

    let effective_name = player_name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| profile.player_name.clone());
    validate_username(&effective_name).map_err(|e| e.to_string())?;

    let install_root = install_dir
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or(profile.horizon_install_dir.clone())
        .or(settings.instance_dir.clone())
        .or_else(|| {
            detect_horizon_installs(&state.data_dir)
                .into_iter()
                .next()
                .map(|h| h.install_root)
        })
        .unwrap_or_else(|| state.data_dir.join("instance"));

    let (target_server, server_name) = if let Some(explicit) = server_addr
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or(profile.direct_server_addr.clone())
        .or(settings.direct_server_addr.clone())
    {
        (explicit, "Dukes Horizon MC 26.3".to_owned())
    } else if let Some(raw_ticket) = settings.invite_ticket.as_deref() {
        let ticket = InviteTicket::decode(raw_ticket).map_err(|e| e.to_string())?;
        let (_, _, local_bind) =
            ensure_tunnel_started(&state, &ticket, settings.local_mc_port).await?;
        (local_bind, ticket.server_name)
    } else {
        (
            format!("127.0.0.1:{}", settings.local_mc_port),
            "Dukes Horizon MC 26.3".to_owned(),
        )
    };

    let spawned =
        spawn_horizon_client(&install_root, &effective_name, &server_name, &target_server)
            .map_err(|e| e.to_string())?;

    push_diag_log(
        &state,
        format!(
            "Launched horizon-client (pid={:?}, name={}, server={})",
            spawned.pid, spawned.player_name, spawned.server_addr
        ),
    );
    Ok(spawned)
}

#[tauri::command]
fn get_diagnostic_logs(state: State<'_, AppState>) -> Vec<String> {
    snapshot_logs(&state)
}

#[tauri::command]
async fn sync_and_play(app: AppHandle, state: State<'_, AppState>) -> Result<PlayResult, String> {
    let settings = snapshot_settings(&state)?;
    let profile = snapshot_profile(&state)?;
    let raw_ticket = settings
        .invite_ticket
        .as_deref()
        .ok_or_else(|| "Please save a duke1: invite ticket first".to_owned())?;
    let ticket = InviteTicket::decode(raw_ticket).map_err(|e| e.to_string())?;
    let player_name = if !profile.player_name.trim().is_empty() {
        profile.player_name.clone()
    } else {
        settings.username.clone()
    };
    validate_username(&player_name).map_err(|e| e.to_string())?;

    let (ep, server_addr, local_bind) =
        ensure_tunnel_started(&state, &ticket, settings.local_mc_port).await?;
    let install_root = profile
        .horizon_install_dir
        .or(settings.instance_dir)
        .or_else(|| {
            detect_horizon_installs(&state.data_dir)
                .into_iter()
                .next()
                .map(|h| h.install_root)
        })
        .unwrap_or_else(|| state.data_dir.join("instance"));

    let layout = resolve_horizon_layout(&install_root);
    push_diag_log(
        &state,
        format!(
            "Syncing Horizon pack into {} via {}",
            layout.game_dir.display(),
            local_bind
        ),
    );

    let sync_summary = sync_pack_over_quic(&ep, &server_addr, &layout.game_dir, |evt| {
        let _ = app.emit("sync-progress", evt);
    })
    .await
    .map_err(|e| e.to_string())?;

    push_diag_log(
        &state,
        format!(
            "Sync complete: {} downloaded, {} skipped ({} bytes)",
            sync_summary.downloaded_files,
            sync_summary.skipped_files,
            sync_summary.bytes_downloaded
        ),
    );

    let launch_outcome = launch_and_watch_horizon(
        &install_root,
        &player_name,
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

    let mut profile = match load_profile(&data_dir) {
        Ok(p) => p,
        Err(err) => {
            warn!(error = %err, "failed to load profile.toml, falling back to defaults");
            PlayerProfile::default()
        }
    };
    if profile.player_name == "Player" && settings.username != "Player" {
        profile.player_name = settings.username.clone();
    }

    info!(
        client_id = %secret_key.public(),
        data_dir = %data_dir.display(),
        "starting duke-link client"
    );

    let tunnel = Arc::new(AsyncMutex::new(None));
    let tunnel_for_exit = Arc::clone(&tunnel);

    let initial_logs = vec![format!(
        "duke-link initialized (horizon_rev={}, endpoint={})",
        DEFAULT_HORIZON_REV,
        secret_key.public()
    )];

    let app_state = AppState {
        data_dir,
        secret_key,
        settings: Mutex::new(settings),
        profile: Mutex::new(profile),
        diagnostic_logs: Mutex::new(initial_logs),
        tunnel,
    };

    let app = match tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_client_state,
            save_config,
            poll_server_status,
            launch_horizon_client,
            get_diagnostic_logs,
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

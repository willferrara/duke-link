use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use iroh::{Endpoint, EndpointAddr};
use serde::{Deserialize, Serialize};
use shared_protocol::manifest::{atomic_write_bytes, ManifestError};
use thiserror::Error;
use tokio::process::Command;
use tracing::{info, warn};

use crate::tunnel::upload_crash_log;

/// Supported existing Minecraft launcher installations usable by `horizon-prepare`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherKind {
    VanillaMinecraft,
    PrismLauncher,
    ModrinthApp,
}

/// Detected local Minecraft / launcher directory with optional assets path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedInstall {
    pub kind: LauncherKind,
    pub root_dir: PathBuf,
    pub assets_dir: Option<PathBuf>,
}

/// Resolved directory layout for a Horizon MC 26.3 installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HorizonLayout {
    pub install_root: PathBuf,
    pub client_bin: Option<PathBuf>,
    pub resources_dir: PathBuf,
    pub game_dir: PathBuf,
}

/// Metadata returned after spawning `horizon-client` directly via Tauri IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnedHorizon {
    pub pid: Option<u32>,
    pub client_bin: PathBuf,
    pub resources_dir: PathBuf,
    pub game_dir: PathBuf,
    pub player_name: String,
    pub server_addr: String,
}

/// Result returned when `horizon-client` finishes execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchOutcome {
    pub exit_code: i32,
    pub crash_log_uploaded: Option<String>,
}

/// Typed errors for launcher detection, `servers.dat` writing, and process execution.
#[derive(Debug, Error)]
pub enum LauncherError {
    #[error("invalid username '{0}': must be 3-16 ASCII letters, digits, or underscores")]
    InvalidUsername(String),
    #[error("atomic file write failed: {0}")]
    Manifest(#[from] ManifestError),
    #[error("horizon-client executable not found in {0}")]
    ClientBinaryNotFound(PathBuf),
    #[error("failed to spawn horizon-client: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Validates a Minecraft/Horizon player username (3..=16 ASCII alphanumeric or `_`).
pub fn validate_username(username: &str) -> Result<(), LauncherError> {
    let trimmed = username.trim();
    if trimmed.len() < 3
        || trimmed.len() > 16
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(LauncherError::InvalidUsername(username.to_owned()));
    }
    Ok(())
}

/// Pure, deterministic detector for Vanilla `.minecraft`, Prism Launcher, and Modrinth App
/// installations given optional `app_data` (`%APPDATA%`) and `home_dir` roots.
pub fn detect_minecraft_installs_in(
    app_data: Option<&Path>,
    home_dir: Option<&Path>,
) -> Vec<DetectedInstall> {
    let mut found = Vec::new();

    if let Some(roaming) = app_data {
        let candidates = [
            (LauncherKind::VanillaMinecraft, roaming.join(".minecraft")),
            (LauncherKind::PrismLauncher, roaming.join("PrismLauncher")),
            (LauncherKind::ModrinthApp, roaming.join("ModrinthApp")),
            (
                LauncherKind::ModrinthApp,
                roaming.join("com.modrinth.theseus"),
            ),
        ];
        for (kind, root) in candidates {
            if root.is_dir() && !found.iter().any(|d: &DetectedInstall| d.root_dir == root) {
                let assets = root.join("assets");
                found.push(DetectedInstall {
                    kind,
                    root_dir: root,
                    assets_dir: assets.is_dir().then_some(assets),
                });
            }
        }
    }

    if let Some(home) = home_dir {
        let local_share = home.join(".local").join("share");
        let candidates = [
            (LauncherKind::VanillaMinecraft, home.join(".minecraft")),
            (
                LauncherKind::PrismLauncher,
                local_share.join("PrismLauncher"),
            ),
            (LauncherKind::ModrinthApp, local_share.join("ModrinthApp")),
            (
                LauncherKind::ModrinthApp,
                local_share.join("com.modrinth.theseus"),
            ),
        ];
        for (kind, root) in candidates {
            if root.is_dir() && !found.iter().any(|d: &DetectedInstall| d.root_dir == root) {
                let assets = root.join("assets");
                found.push(DetectedInstall {
                    kind,
                    root_dir: root,
                    assets_dir: assets.is_dir().then_some(assets),
                });
            }
        }
    }

    found
}

/// Auto-detects installed `.minecraft`, Prism Launcher, and Modrinth App directories on this host.
pub fn detect_minecraft_installs() -> Vec<DetectedInstall> {
    let app_data = std::env::var_os("APPDATA").map(PathBuf::from);
    let home_dir = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    detect_minecraft_installs_in(app_data.as_deref(), home_dir.as_deref())
}

/// Encodes a minimal valid uncompressed Minecraft NBT `servers.dat` pointing to `host_port`.
pub fn encode_servers_dat_nbt(server_name: &str, host_port: &str) -> Vec<u8> {
    fn write_string(buf: &mut Vec<u8>, s: &str) {
        let bytes = s.as_bytes();
        let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(&bytes[..usize::from(len)]);
    }

    let mut buf = Vec::with_capacity(128);
    // Root TAG_Compound ("")
    buf.push(10);
    write_string(&mut buf, "");

    // TAG_List ("servers") of TAG_Compound (10), length 1
    buf.push(9);
    write_string(&mut buf, "servers");
    buf.push(10);
    buf.extend_from_slice(&1i32.to_be_bytes());

    // Entry compound: TAG_String("name")
    buf.push(8);
    write_string(&mut buf, "name");
    write_string(&mut buf, server_name);

    // TAG_String("ip")
    buf.push(8);
    write_string(&mut buf, "ip");
    write_string(&mut buf, host_port);

    // TAG_Byte("acceptTextures") = 1
    buf.push(1);
    write_string(&mut buf, "acceptTextures");
    buf.push(1);

    // End entry compound + End root compound
    buf.push(0);
    buf.push(0);
    buf
}

/// Atomically writes `servers.dat` inside `instance_dir` pointing to `host_port` (e.g. `127.0.0.1:25565`).
pub fn write_local_servers_dat(
    instance_dir: &Path,
    server_name: &str,
    host_port: &str,
) -> Result<PathBuf, LauncherError> {
    let path = instance_dir.join("servers.dat");
    let bytes = encode_servers_dat_nbt(server_name, host_port);
    atomic_write_bytes(&path, &bytes)?;
    Ok(path)
}

/// Locates `horizon-client.exe` (Windows) or `horizon-client` (Unix) inside `install_root`
/// (supporting `client/horizon-windows-x64/`, `horizon-windows-x64/`, `client/`, or root)
/// or beside the `duke-link` executable.
pub fn locate_horizon_client(install_root: &Path) -> Option<PathBuf> {
    let bin_name = if cfg!(windows) {
        "horizon-client.exe"
    } else {
        "horizon-client"
    };

    let candidates = [
        install_root
            .join("client")
            .join("horizon-windows-x64")
            .join(bin_name),
        install_root.join("horizon-windows-x64").join(bin_name),
        install_root.join("client").join(bin_name),
        install_root.join(bin_name),
    ];

    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [
                dir.join(bin_name),
                dir.join("client")
                    .join("horizon-windows-x64")
                    .join(bin_name),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

/// Resolves the Horizon `resources` and `game` directories and binary path for `install_root`.
pub fn resolve_horizon_layout(install_root: &Path) -> HorizonLayout {
    let client_bin = locate_horizon_client(install_root);
    let resources_dir = install_root.join("resources");
    let game_dir = if install_root.join("game").is_dir()
        || install_root.join("client").is_dir()
        || !install_root.join("horizon-mods").is_dir()
    {
        install_root.join("game")
    } else {
        install_root.to_path_buf()
    };

    HorizonLayout {
        install_root: install_root.to_path_buf(),
        client_bin,
        resources_dir,
        game_dir,
    }
}

/// Pure, deterministic detector for Horizon MC 26.3 installations given candidate roots.
pub fn detect_horizon_installs_in(roots: &[PathBuf]) -> Vec<HorizonLayout> {
    let mut found = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let layout = resolve_horizon_layout(root);
        let has_marker = layout.client_bin.is_some()
            || layout.resources_dir.is_dir()
            || layout.game_dir.join("horizon-mods").is_dir()
            || root.join("horizon-mods").is_dir();
        if has_marker
            && !found
                .iter()
                .any(|existing: &HorizonLayout| existing.install_root == layout.install_root)
        {
            found.push(layout);
        }
    }
    found
}

/// Auto-detects Horizon MC 26.3 installations in standard portable and user locations.
pub fn detect_horizon_installs(data_dir: &Path) -> Vec<HorizonLayout> {
    let mut roots = vec![data_dir.join("horizon"), data_dir.join("instance")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.join("horizon"));
            roots.push(dir.to_path_buf());
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home_path = PathBuf::from(home);
        roots.push(home_path.join("dukes-playtest"));
        roots.push(home_path.join("horizon"));
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local_app_data).join("dukes-horizon"));
    }
    detect_horizon_installs_in(&roots)
}

/// Builds the exact zero-Microsoft-login CLI arguments for `horizon-client`:
/// `--resources <dir> --game-dir <dir> --name <player_name> --server <server_addr>`.
pub fn build_horizon_launch_args(
    resources_dir: &Path,
    game_dir: &Path,
    player_name: &str,
    server_addr: &str,
) -> Vec<OsString> {
    vec![
        OsString::from("--resources"),
        resources_dir.as_os_str().to_os_string(),
        OsString::from("--game-dir"),
        game_dir.as_os_str().to_os_string(),
        OsString::from("--name"),
        OsString::from(player_name.trim()),
        OsString::from("--server"),
        OsString::from(server_addr.trim()),
    ]
}

/// Spawns `horizon-client` non-blockingly for one-click friend launch via Tauri IPC.
pub fn spawn_horizon_client(
    install_root: &Path,
    player_name: &str,
    server_name: &str,
    server_addr: &str,
) -> Result<SpawnedHorizon, LauncherError> {
    validate_username(player_name)?;
    let layout = resolve_horizon_layout(install_root);
    let client_bin = layout
        .client_bin
        .clone()
        .ok_or_else(|| LauncherError::ClientBinaryNotFound(install_root.to_path_buf()))?;

    let _ = fs::create_dir_all(&layout.game_dir);
    write_local_servers_dat(&layout.game_dir, server_name, server_addr)?;

    let args = build_horizon_launch_args(
        &layout.resources_dir,
        &layout.game_dir,
        player_name,
        server_addr,
    );

    info!(
        bin = %client_bin.display(),
        player_name = %player_name.trim(),
        server_addr = %server_addr.trim(),
        "spawning horizon-client"
    );

    let child = std::process::Command::new(&client_bin)
        .current_dir(&layout.game_dir)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(SpawnedHorizon {
        pid: Some(child.id()),
        client_bin,
        resources_dir: layout.resources_dir,
        game_dir: layout.game_dir,
        player_name: player_name.trim().to_owned(),
        server_addr: server_addr.trim().to_owned(),
    })
}

/// Spawns `horizon-client` pointed at `127.0.0.1:<local_port>`, watches its exit code, and
/// automatically uploads crash output over `duke-link/status/1` on non-zero exit.
pub async fn launch_and_watch_horizon(
    install_root: &Path,
    username: &str,
    server_name: &str,
    local_port: u16,
    endpoint: &Endpoint,
    server_addr: &EndpointAddr,
) -> Result<LaunchOutcome, LauncherError> {
    validate_username(username)?;
    let host_port = format!("127.0.0.1:{local_port}");
    let layout = resolve_horizon_layout(install_root);
    let _ = fs::create_dir_all(&layout.game_dir);
    write_local_servers_dat(&layout.game_dir, server_name, &host_port)?;

    let client_bin = layout
        .client_bin
        .ok_or_else(|| LauncherError::ClientBinaryNotFound(install_root.to_path_buf()))?;

    let args = build_horizon_launch_args(
        &layout.resources_dir,
        &layout.game_dir,
        username,
        &host_port,
    );

    info!(
        bin = %client_bin.display(),
        %username,
        %host_port,
        "launching horizon-client"
    );

    let output = Command::new(&client_bin)
        .current_dir(&layout.game_dir)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code == 0 {
        return Ok(LaunchOutcome {
            exit_code: 0,
            crash_log_uploaded: None,
        });
    }

    warn!(
        exit_code,
        "horizon-client exited with non-zero status; collecting crash report"
    );
    let crash_payload =
        collect_crash_report(&layout.game_dir, exit_code, &output.stdout, &output.stderr);
    let uploaded =
        match upload_crash_log(endpoint, server_addr, "horizon-client", &crash_payload).await {
            Ok((stored_name, bytes)) => {
                info!(%stored_name, bytes, "auto-uploaded crash log to relay-daemon");
                Some(stored_name)
            }
            Err(err) => {
                warn!(error = %err, "failed to upload crash log");
                None
            }
        };

    Ok(LaunchOutcome {
        exit_code,
        crash_log_uploaded: uploaded,
    })
}

fn collect_crash_report(
    instance_dir: &Path,
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let mut report = format!("horizon-client exit_code={exit_code}\n");

    let latest_log = instance_dir.join("logs").join("latest.log");
    if let Ok(content) = fs::read_to_string(&latest_log) {
        report.push_str("\n=== logs/latest.log ===\n");
        report.push_str(&content);
    }

    if !stderr.is_empty() {
        report.push_str("\n=== stderr ===\n");
        report.push_str(&String::from_utf8_lossy(stderr));
    }
    if !stdout.is_empty() {
        report.push_str("\n=== stdout ===\n");
        report.push_str(&String::from_utf8_lossy(stdout));
    }

    report
}

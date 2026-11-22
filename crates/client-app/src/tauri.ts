export interface ClientSettings {
  invite_ticket: string | null;
  username: string;
  local_mc_port: number;
  instance_dir: string | null;
  direct_server_addr?: string | null;
}

export interface PlayerProfile {
  player_name: string;
  horizon_install_dir?: string | null;
  direct_server_addr?: string | null;
}

export interface HorizonLayout {
  install_root: string;
  client_bin: string | null;
  resources_dir: string;
  game_dir: string;
}

export interface DetectedInstall {
  kind: "vanilla_minecraft" | "prism_launcher" | "modrinth_app";
  root_dir: string;
  assets_dir: string | null;
}

export interface ClientBootstrapState {
  client_endpoint_id: string;
  data_dir: string;
  settings: ClientSettings;
  profile: PlayerProfile;
  server_name: string | null;
  horizon_rev: string;
  horizon_release_tag: string;
  detected_installs: DetectedInstall[];
  detected_horizon_installs: HorizonLayout[];
  tunnel_active: boolean;
  diagnostic_logs: string[];
}

export interface TunnelStatusSnapshot {
  connected: boolean;
  local_bind_addr: string;
  server_name: string;
  server_endpoint_id: string;
  client_endpoint_id: string;
  path_kind: "direct" | "relay" | "unknown";
  rtt_ms: number | null;
  server_online: boolean;
  players_online: number;
  max_players: number;
  motd: string;
  tps?: number | null;
  horizon_rev?: string | null;
  last_error?: string | null;
}

export interface SyncProgressEvent {
  stage: string;
  current_file: string;
  files_done: number;
  files_total: number;
  bytes_done: number;
  bytes_total: number;
  mbps: number;
}

export interface SyncSummary {
  pack_name: string;
  pack_version: string;
  horizon_rev?: string | null;
  downloaded_files: number;
  skipped_files: number;
  preserved_user_files: number;
  bytes_downloaded: number;
}

export interface LaunchOutcome {
  exit_code: number;
  crash_log_uploaded: string | null;
}

export interface PlayResult {
  sync: SyncSummary;
  launch: LaunchOutcome;
}

export interface SpawnedHorizon {
  pid: number | null;
  client_bin: string;
  resources_dir: string;
  game_dir: string;
  player_name: string;
  server_addr: string;
}

interface TauriCore {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
}

interface TauriEventApi {
  listen<T>(
    event: string,
    handler: (evt: { payload: T }) => void
  ): Promise<() => void>;
}

interface TauriWindow extends Window {
  __TAURI__?: {
    core?: TauriCore;
    event?: TauriEventApi;
  };
}

function getCore(): TauriCore | null {
  return (window as TauriWindow).__TAURI__?.core ?? null;
}

function getEventApi(): TauriEventApi | null {
  return (window as TauriWindow).__TAURI__?.event ?? null;
}

export function isTauriAvailable(): boolean {
  return getCore() !== null;
}

export async function invokeCommand<T>(
  cmd: string,
  args?: Record<string, unknown>
): Promise<T> {
  const core = getCore();
  if (!core) {
    throw new Error("Tauri desktop IPC unavailable in browser preview.");
  }
  return core.invoke<T>(cmd, args);
}

export async function listenSyncProgress(
  handler: (evt: SyncProgressEvent) => void
): Promise<() => void> {
  const events = getEventApi();
  if (!events) {
    return () => {};
  }
  return events.listen<SyncProgressEvent>("sync-progress", (e) => {
    if (e && e.payload) {
      handler(e.payload);
    }
  });
}

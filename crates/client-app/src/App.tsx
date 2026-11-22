import { useCallback, useEffect, useMemo, useState } from "react";
import { AppShell } from "@astryxdesign/core/AppShell";
import { Badge } from "@astryxdesign/core/Badge";
import { Button } from "@astryxdesign/core/Button";
import { Card } from "@astryxdesign/core/Card";
import { Collapsible } from "@astryxdesign/core/Collapsible";
import { Dialog } from "@astryxdesign/core/Dialog";
import { Heading } from "@astryxdesign/core/Heading";
import { HStack } from "@astryxdesign/core/HStack";
import { ProgressBar } from "@astryxdesign/core/ProgressBar";
import { Text } from "@astryxdesign/core/Text";
import { TextInput } from "@astryxdesign/core/TextInput";
import { Toast } from "@astryxdesign/core/Toast";
import { VStack } from "@astryxdesign/core/VStack";

import {
  type ClientBootstrapState,
  type HorizonLayout,
  type PlayResult,
  type SpawnedHorizon,
  type SyncProgressEvent,
  type TunnelStatusSnapshot,
  invokeCommand,
  isTauriAvailable,
  listenSyncProgress,
} from "./tauri";

const DEFAULT_REV = "f037744";

interface ToastMessage {
  id: number;
  text: string;
  type: "info" | "error";
}

export function App() {
  const [bootstrap, setBootstrap] = useState<ClientBootstrapState | null>(null);
  const [status, setStatus] = useState<TunnelStatusSnapshot | null>(null);
  const [inviteTicket, setInviteTicket] = useState("");
  const [playerName, setPlayerName] = useState("Player");
  const [installDir, setInstallDir] = useState("");
  const [directServer, setDirectServer] = useState("");
  const [syncProgress, setSyncProgress] = useState<SyncProgressEvent | null>(
    null
  );
  const [logs, setLogs] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [saving, setSaving] = useState(false);
  const [diagOpen, setDiagOpen] = useState(false);
  const [logDialogOpen, setLogDialogOpen] = useState(false);
  const [toast, setToast] = useState<ToastMessage | null>(null);
  const [footnote, setFootnote] = useState(
    "Local QUIC tunnel binds 127.0.0.1:25565 on launch."
  );

  const showToast = useCallback((text: string, type: "info" | "error" = "info") => {
    setToast({ id: Date.now(), text, type });
  }, []);

  const refreshLogs = useCallback(async () => {
    if (!isTauriAvailable()) {
      return;
    }
    try {
      const next = await invokeCommand<string[]>("get_diagnostic_logs");
      setLogs(next);
    } catch {
      // Keep existing logs if IPC fails.
    }
  }, []);

  const refreshStatus = useCallback(async () => {
    if (!isTauriAvailable()) {
      return;
    }
    try {
      const snap = await invokeCommand<TunnelStatusSnapshot>(
        "poll_server_status"
      );
      setStatus(snap);
      await refreshLogs();
    } catch (err) {
      setFootnote(`Status error: ${String(err)}`);
    }
  }, [refreshLogs]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let pollTimer: number | null = null;

    async function init() {
      if (!isTauriAvailable()) {
        setFootnote("Preview mode (Tauri desktop IPC not attached).");
        return;
      }
      try {
        const state = await invokeCommand<ClientBootstrapState>(
          "get_client_state"
        );
        setBootstrap(state);
        setInviteTicket(state.settings.invite_ticket ?? "");
        setPlayerName(
          state.profile.player_name || state.settings.username || "Player"
        );
        const preferredInstall =
          state.profile.horizon_install_dir ??
          state.settings.instance_dir ??
          state.detected_horizon_installs[0]?.install_root ??
          "";
        setInstallDir(preferredInstall);
        setDirectServer(
          state.profile.direct_server_addr ??
            state.settings.direct_server_addr ??
            ""
        );
        setLogs(state.diagnostic_logs);

        unlisten = await listenSyncProgress((evt) => {
          setSyncProgress(evt);
        });

        await refreshStatus();
        pollTimer = window.setInterval(() => {
          void refreshStatus();
        }, 10000);
      } catch (err) {
        setFootnote(`Init failed: ${String(err)}`);
      }
    }

    void init();
    return () => {
      if (unlisten) {
        unlisten();
      }
      if (pollTimer !== null) {
        window.clearInterval(pollTimer);
      }
    };
  }, [refreshStatus]);

  const isOnline = Boolean(status?.connected && status?.server_online);
  const statusBadgeLabel = isOnline
    ? "Online"
    : status?.connected
      ? "Relay Ready"
      : "Offline";
  const statusBadgeVariant = isOnline
    ? "success"
    : status?.connected
      ? "warning"
      : "neutral";

  const horizonRev =
    status?.horizon_rev ?? bootstrap?.horizon_rev ?? DEFAULT_REV;

  const tpsText =
    typeof status?.tps === "number" ? status.tps.toFixed(1) : "--";
  const pingText =
    typeof status?.rtt_ms === "number" ? `${status.rtt_ms} ms` : "-- ms";
  const playersText = isOnline
    ? `${status?.players_online ?? 0} / ${status?.max_players ?? 0}`
    : "-- / --";
  const transportText =
    status?.path_kind === "direct"
      ? "Direct"
      : status?.path_kind === "relay"
        ? "Relay"
        : status?.connected
          ? "QUIC"
          : "Idle";

  const syncPercent = useMemo(() => {
    if (!syncProgress) {
      return 0;
    }
    if (syncProgress.bytes_total > 0) {
      return Math.min(
        100,
        Math.round((syncProgress.bytes_done / syncProgress.bytes_total) * 100)
      );
    }
    return syncProgress.stage === "complete" ? 100 : 0;
  }, [syncProgress]);

  const syncStageLabel = useMemo(() => {
    if (!syncProgress) {
      return "Ready to Sync (.hpack / .hmod)";
    }
    if (syncProgress.stage === "complete") {
      return "Pack Verified (BLAKE3 + SHA-256)";
    }
    if (syncProgress.stage === "downloading") {
      return "Syncing Horizon Pack";
    }
    return "Connecting to Relay";
  }, [syncProgress]);

  const handleSave = useCallback(async () => {
    if (!isTauriAvailable()) {
      showToast("Desktop IPC unavailable in preview.", "error");
      return;
    }
    setSaving(true);
    try {
      const updated = await invokeCommand<ClientBootstrapState>("save_config", {
        inviteTicket: inviteTicket.trim(),
        username: playerName.trim(),
        horizonInstallDir: installDir.trim() || null,
        directServerAddr: directServer.trim() || null,
      });
      setBootstrap(updated);
      setLogs(updated.diagnostic_logs);
      showToast("Saved profile.toml and connection settings.", "info");
      await refreshStatus();
    } catch (err) {
      showToast(String(err), "error");
    } finally {
      setSaving(false);
    }
  }, [
    directServer,
    installDir,
    inviteTicket,
    playerName,
    refreshStatus,
    showToast,
  ]);

  const handleSyncAndPlay = useCallback(async () => {
    if (!isTauriAvailable() || busy) {
      return;
    }
    setBusy(true);
    setFootnote("Connecting P2P tunnel and verifying Horizon pack files...");
    try {
      await invokeCommand<ClientBootstrapState>("save_config", {
        inviteTicket: inviteTicket.trim(),
        username: playerName.trim(),
        horizonInstallDir: installDir.trim() || null,
        directServerAddr: directServer.trim() || null,
      });
      const result = await invokeCommand<PlayResult>("sync_and_play");
      const crashNote = result.launch.crash_log_uploaded
        ? ` Crash log uploaded (${result.launch.crash_log_uploaded}).`
        : "";
      setFootnote(
        `Session ended (exit ${result.launch.exit_code}).${crashNote}`
      );
      showToast(
        `Synced ${result.sync.downloaded_files} file(s), skipped ${result.sync.skipped_files}.`,
        "info"
      );
    } catch (err) {
      setFootnote(`Launch failed: ${String(err)}`);
      showToast(String(err), "error");
    } finally {
      setBusy(false);
      await refreshStatus();
    }
  }, [
    busy,
    directServer,
    installDir,
    inviteTicket,
    playerName,
    refreshStatus,
    showToast,
  ]);

  const handleDirectLaunch = useCallback(async () => {
    if (!isTauriAvailable() || busy) {
      return;
    }
    setBusy(true);
    try {
      const spawned = await invokeCommand<SpawnedHorizon>(
        "launch_horizon_client",
        {
          playerName: playerName.trim() || null,
          installDir: installDir.trim() || null,
          serverAddr: directServer.trim() || null,
        }
      );
      setFootnote(
        `Launched horizon-client (PID ${spawned.pid ?? "?"}) as ${spawned.player_name} -> ${spawned.server_addr}`
      );
      showToast(`Horizon client started as ${spawned.player_name}`, "info");
      await refreshLogs();
    } catch (err) {
      showToast(String(err), "error");
    } finally {
      setBusy(false);
    }
  }, [busy, directServer, installDir, playerName, refreshLogs, showToast]);

  const diagnosticBundle = useMemo(() => {
    const lines = [
      `Duke Link Diagnostics (Horizon ${horizonRev})`,
      `Client Endpoint ID: ${bootstrap?.client_endpoint_id ?? "unavailable"}`,
      `Server Endpoint ID: ${status?.server_endpoint_id || "unconfigured"}`,
      `Local Tunnel Bind:  ${status?.local_bind_addr ?? "127.0.0.1:25565"}`,
      `Transport Path:     ${transportText} (RTT: ${pingText}, TPS: ${tpsText})`,
      `Player Name:        ${playerName}`,
      `Horizon Install:    ${installDir || "auto"}`,
      `Data Dir:           ${bootstrap?.data_dir ?? "portable ./data"}`,
      "--- Recent Events ---",
      ...(logs.length > 0 ? logs : ["No events recorded yet."]),
    ];
    return lines.join("\n");
  }, [
    bootstrap,
    horizonRev,
    installDir,
    logs,
    pingText,
    playerName,
    status,
    tpsText,
    transportText,
  ]);

  const handleCopyText = useCallback(
    async (content: string, label: string) => {
      if (!navigator.clipboard) {
        showToast("Clipboard unavailable.", "error");
        return;
      }
      try {
        await navigator.clipboard.writeText(content);
        showToast(`${label} copied to clipboard.`, "info");
      } catch {
        showToast(`Failed to copy ${label}.`, "error");
      }
    },
    [showToast]
  );

  const detectedInstalls: HorizonLayout[] =
    bootstrap?.detected_horizon_installs ?? [];

  return (
    <AppShell contentPadding={4} height="fill">
      <VStack gap={4} maxWidth={680} width="100%">
        {/* Connection Header */}
        <Card padding={4} elevation="low">
          <VStack gap={3}>
            <HStack justify="between" align="center" wrap="wrap" gap={2}>
              <VStack gap={0.5}>
                <HStack gap={2} align="center" wrap="wrap">
                  <Heading level={2}>
                    {status?.server_name ||
                      bootstrap?.server_name ||
                      "Duke Link"}
                  </Heading>
                  <Badge
                    label={statusBadgeLabel}
                    variant={statusBadgeVariant}
                  />
                  <Badge label={`Horizon ${horizonRev}`} variant="info" />
                </HStack>
                <Text type="supporting" color="secondary">
                  {status?.last_error ??
                    status?.motd ??
                    "Horizon MC 26.3 P2P Friend Joiner (Zero Microsoft Login)"}
                </Text>
              </VStack>
              <Button
                label="Refresh"
                variant="secondary"
                size="sm"
                onClick={() => void refreshStatus()}
              />
            </HStack>

            <HStack gap={4} wrap="wrap" justify="between">
              <VStack gap={0.5}>
                <Text type="label" color="secondary">
                  TPS
                </Text>
                <Text type="code" hasTabularNumbers>
                  {tpsText}
                </Text>
              </VStack>
              <VStack gap={0.5}>
                <Text type="label" color="secondary">
                  Ping
                </Text>
                <Text type="code" hasTabularNumbers>
                  {pingText}
                </Text>
              </VStack>
              <VStack gap={0.5}>
                <Text type="label" color="secondary">
                  Players
                </Text>
                <Text type="code" hasTabularNumbers>
                  {playersText}
                </Text>
              </VStack>
              <VStack gap={0.5}>
                <Text type="label" color="secondary">
                  Transport
                </Text>
                <Text type="code">{transportText}</Text>
              </VStack>
            </HStack>
          </VStack>
        </Card>

        {/* Quick Join Card */}
        <Card padding={4} elevation="low">
          <VStack gap={3}>
            <Heading level={3}>Quick Join</Heading>

            <TextInput
              label="Invite Ticket (duke1:...)"
              value={inviteTicket}
              onChange={(val) => setInviteTicket(val)}
              placeholder="duke1:..."
              description="Paste the invite ticket from your host."
            />

            <TextInput
              label="Minecraft Name"
              value={playerName}
              onChange={(val) => setPlayerName(val)}
              placeholder="PlayerName"
              description="Saved locally in ./data/profile.toml (3-16 chars, no Microsoft login required)."
              isRequired
            />

            <TextInput
              label="Horizon Install Folder"
              value={installDir}
              onChange={(val) => setInstallDir(val)}
              placeholder="Auto-detected Horizon folder"
              description="Contains client/, resources/, and game/horizon-mods/."
              isOptional
            />

            {detectedInstalls.length > 0 && (
              <HStack gap={2} wrap="wrap" align="center">
                <Text type="supporting" color="secondary">
                  Detected:
                </Text>
                {detectedInstalls.map((inst) => (
                  <Button
                    key={inst.install_root}
                    label={inst.install_root}
                    size="sm"
                    variant="ghost"
                    onClick={() => setInstallDir(inst.install_root)}
                  />
                ))}
              </HStack>
            )}

            <VStack gap={1.5}>
              <ProgressBar
                label={syncStageLabel}
                value={syncPercent}
                max={100}
                hasValueLabel
                variant={
                  syncProgress?.stage === "complete" ? "success" : "accent"
                }
              />
              <HStack justify="between" wrap="wrap" gap={2}>
                <Text type="supporting" color="secondary">
                  {syncProgress?.current_file ||
                    (syncProgress?.stage === "complete"
                      ? "All .hpack / .hmod files verified"
                      : "Waiting for Sync & Play")}
                </Text>
                <Text type="code" color="secondary" hasTabularNumbers>
                  {syncProgress
                    ? `${syncProgress.files_done}/${syncProgress.files_total} files · ${syncProgress.mbps.toFixed(1)} MB/s`
                    : "0/0 files · 0.0 MB/s"}
                </Text>
              </HStack>
            </VStack>

            <HStack gap={2} wrap="wrap">
              <Button
                label={busy ? "Syncing & Launching..." : "Sync & Play"}
                variant="primary"
                size="lg"
                isLoading={busy}
                onClick={() => void handleSyncAndPlay()}
              />
              <Button
                label="Launch Client"
                variant="secondary"
                size="lg"
                isDisabled={busy}
                onClick={() => void handleDirectLaunch()}
              />
              <Button
                label="Save Profile"
                variant="ghost"
                size="lg"
                isLoading={saving}
                onClick={() => void handleSave()}
              />
            </HStack>

            <Text type="supporting" color="secondary">
              {footnote}
            </Text>
          </VStack>
        </Card>

        {/* Diagnostics / Logs Drawer */}
        <Card padding={4} variant="muted">
          <Collapsible
            isOpen={diagOpen}
            onOpenChange={setDiagOpen}
            trigger={
              <HStack justify="between" align="center" width="100%">
                <Text weight="semibold">Diagnostics &amp; Tunnel Logs</Text>
                <Badge
                  label={`${logs.length} events`}
                  variant="neutral"
                />
              </HStack>
            }
          >
            <VStack gap={3} paddingBlockStart={3}>
              <VStack gap={1}>
                <Text type="label" color="secondary">
                  Your Endpoint ID (share with host allowlist)
                </Text>
                <HStack gap={2} align="center" wrap="wrap">
                  <Text type="code" wordBreak="break-all">
                    {bootstrap?.client_endpoint_id ?? "loading..."}
                  </Text>
                  <Button
                    label="Copy ID"
                    size="sm"
                    variant="secondary"
                    onClick={() =>
                      void handleCopyText(
                        bootstrap?.client_endpoint_id ?? "",
                        "Endpoint ID"
                      )
                    }
                  />
                </HStack>
              </VStack>

              <TextInput
                label="Direct Server Override (optional)"
                value={directServer}
                onChange={(val) => setDirectServer(val)}
                placeholder="Leave blank to use P2P tunnel (127.0.0.1:25565)"
                isOptional
              />

              <HStack gap={2} wrap="wrap">
                <Button
                  label="Copy Diagnostic Log"
                  variant="secondary"
                  size="sm"
                  onClick={() =>
                    void handleCopyText(diagnosticBundle, "Diagnostic log")
                  }
                />
                <Button
                  label="Inspect Full Log"
                  variant="ghost"
                  size="sm"
                  onClick={() => setLogDialogOpen(true)}
                />
              </HStack>

              <VStack gap={1}>
                {logs.slice(-6).map((line, idx) => (
                  <Text key={`${idx}-${line}`} type="code" color="secondary">
                    {line}
                  </Text>
                ))}
              </VStack>
            </VStack>
          </Collapsible>
        </Card>

        {toast && (
          <Toast
            key={toast.id}
            body={toast.text}
            type={toast.type}
            isAutoHide
            autoHideDuration={4000}
            onDismiss={() => setToast(null)}
          />
        )}

        <Dialog
          isOpen={logDialogOpen}
          onOpenChange={setLogDialogOpen}
          width={560}
          padding={4}
        >
          <VStack gap={3}>
            <Heading level={3}>Tunnel &amp; Sync Diagnostics</Heading>
            <Text type="code" wordBreak="break-word">
              {diagnosticBundle}
            </Text>
            <HStack gap={2} justify="end">
              <Button
                label="Copy Log"
                variant="primary"
                size="sm"
                onClick={() =>
                  void handleCopyText(diagnosticBundle, "Diagnostic log")
                }
              />
              <Button
                label="Close"
                variant="secondary"
                size="sm"
                onClick={() => setLogDialogOpen(false)}
              />
            </HStack>
          </VStack>
        </Dialog>
      </VStack>
    </AppShell>
  );
}

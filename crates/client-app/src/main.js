"use strict";

const tauriCore = window.__TAURI__ && window.__TAURI__.core ? window.__TAURI__.core : null;
const tauriEvent = window.__TAURI__ && window.__TAURI__.event ? window.__TAURI__.event : null;

const el = {
  serverName: document.getElementById("server-name"),
  serverMotd: document.getElementById("server-motd"),
  statusDot: document.getElementById("status-dot"),
  statusPill: document.getElementById("status-pill"),
  metricPlayers: document.getElementById("metric-players"),
  metricPath: document.getElementById("metric-path"),
  metricPing: document.getElementById("metric-ping"),
  setupCard: document.getElementById("setup-card"),
  setupToggle: document.getElementById("setup-toggle"),
  setupSummary: document.getElementById("setup-summary"),
  inputTicket: document.getElementById("input-ticket"),
  inputUsername: document.getElementById("input-username"),
  clientEndpointId: document.getElementById("client-endpoint-id"),
  btnCopyId: document.getElementById("btn-copy-id"),
  btnSave: document.getElementById("btn-save"),
  setupFeedback: document.getElementById("setup-feedback"),
  syncStage: document.getElementById("sync-stage"),
  syncStats: document.getElementById("sync-stats"),
  syncBarTrack: document.getElementById("sync-bar-track"),
  syncBarFill: document.getElementById("sync-bar-fill"),
  syncFile: document.getElementById("sync-file"),
  btnPlay: document.getElementById("btn-play"),
  btnPlayLabel: document.getElementById("btn-play-label"),
  btnRefresh: document.getElementById("btn-refresh"),
  globalStatus: document.getElementById("global-status"),
};

let pollTimer = null;
let busy = false;

function setCollapsed(collapsed) {
  el.setupCard.classList.toggle("collapsed", collapsed);
  el.setupToggle.setAttribute("aria-expanded", String(!collapsed));
}

function setFeedback(text, kind) {
  el.setupFeedback.textContent = text;
  el.setupFeedback.className = "feedback" + (kind ? " " + kind : "");
}

function renderStatus(snap) {
  el.serverName.textContent = snap.server_name || "Not configured";
  el.serverMotd.textContent = snap.last_error ? snap.last_error : snap.motd || "";

  const isOnline = Boolean(snap.connected && snap.server_online);
  el.statusDot.className = "status-dot " + (isOnline ? "online" : "offline");
  el.statusPill.className = "pill " + (isOnline ? "online" : "offline");
  el.statusPill.textContent = isOnline ? "Online" : snap.connected ? "Relay Ready" : "Offline";

  el.metricPlayers.textContent = isOnline
    ? `${snap.players_online} / ${snap.max_players}`
    : "-- / --";

  const pathLabel =
    snap.path_kind === "direct"
      ? "Direct"
      : snap.path_kind === "relay"
        ? "Relay"
        : snap.connected
          ? "QUIC"
          : "Idle";
  el.metricPath.textContent = pathLabel;
  el.metricPing.textContent = typeof snap.rtt_ms === "number" ? `${snap.rtt_ms} ms` : "-- ms";
}

function renderProgress(evt) {
  const pct =
    evt.bytes_total > 0
      ? Math.min(100, Math.round((evt.bytes_done / evt.bytes_total) * 100))
      : evt.stage === "complete"
        ? 100
        : 0;
  el.syncStage.textContent =
    evt.stage === "complete"
      ? "Pack Up to Date"
      : evt.stage === "downloading"
        ? "Syncing Pack"
        : "Connecting to Relay";
  el.syncStats.textContent = `${evt.files_done} / ${evt.files_total} files · ${evt.mbps.toFixed(1)} MB/s`;
  el.syncBarFill.style.width = `${pct}%`;
  el.syncBarTrack.setAttribute("aria-valuenow", String(pct));
  el.syncFile.textContent = evt.current_file || (evt.stage === "complete" ? "All files verified (BLAKE3)" : "Negotiating manifest...");
}

async function refreshStatus() {
  if (!tauriCore) {
    return;
  }
  try {
    const snap = await tauriCore.invoke("poll_server_status");
    renderStatus(snap);
  } catch (err) {
    el.globalStatus.textContent = `Status error: ${String(err)}`;
  }
}

async function bootstrap() {
  if (!tauriCore) {
    el.globalStatus.textContent = "Tauri IPC unavailable (running outside desktop shell).";
    return;
  }

  try {
    const state = await tauriCore.invoke("get_client_state");
    el.clientEndpointId.textContent = state.client_endpoint_id;
    el.inputUsername.value = state.settings.username || "Player";
    if (state.settings.invite_ticket) {
      el.inputTicket.value = state.settings.invite_ticket;
      el.setupSummary.textContent = state.settings.username;
      el.serverName.textContent = state.server_name || "Configured Server";
      setCollapsed(true);
    } else {
      el.setupSummary.textContent = "Setup required";
      setCollapsed(false);
    }

    if (tauriEvent) {
      await tauriEvent.listen("sync-progress", (event) => {
        if (event && event.payload) {
          renderProgress(event.payload);
        }
      });
    }

    await refreshStatus();
    if (pollTimer) {
      clearInterval(pollTimer);
    }
    pollTimer = setInterval(refreshStatus, 10000);
  } catch (err) {
    el.globalStatus.textContent = `Init failed: ${String(err)}`;
  }
}

el.setupToggle.addEventListener("click", () => {
  const isCollapsed = el.setupCard.classList.contains("collapsed");
  setCollapsed(!isCollapsed);
});

el.btnCopyId.addEventListener("click", async () => {
  const idText = el.clientEndpointId.textContent || "";
  if (!idText || !navigator.clipboard) {
    return;
  }
  try {
    await navigator.clipboard.writeText(idText);
    setFeedback("Endpoint ID copied", "ok");
  } catch (_err) {
    setFeedback("Copy failed", "error");
  }
});

el.btnSave.addEventListener("click", async () => {
  if (!tauriCore) {
    return;
  }
  const ticket = el.inputTicket.value.trim();
  const username = el.inputUsername.value.trim();
  setFeedback("Validating...", "");
  try {
    const updated = await tauriCore.invoke("save_config", {
      inviteTicket: ticket,
      username,
    });
    el.setupSummary.textContent = updated.settings.username;
    el.serverName.textContent = updated.server_name || "Configured Server";
    setFeedback("Saved", "ok");
    setCollapsed(true);
    await refreshStatus();
  } catch (err) {
    setFeedback(String(err), "error");
  }
});

el.btnRefresh.addEventListener("click", () => {
  void refreshStatus();
});

el.btnPlay.addEventListener("click", async () => {
  if (!tauriCore || busy) {
    return;
  }
  busy = true;
  el.btnPlay.disabled = true;
  el.btnPlayLabel.textContent = "Syncing & Launching...";
  el.globalStatus.textContent = "Connecting P2P tunnel and verifying pack files...";

  try {
    const result = await tauriCore.invoke("sync_and_play");
    const crashNote = result.launch.crash_log_uploaded
      ? ` Crash log uploaded (${result.launch.crash_log_uploaded}).`
      : "";
    el.globalStatus.textContent = `Session ended (exit ${result.launch.exit_code}).${crashNote}`;
  } catch (err) {
    el.globalStatus.textContent = `Launch failed: ${String(err)}`;
  } finally {
    busy = false;
    el.btnPlay.disabled = false;
    el.btnPlayLabel.textContent = "Play";
    void refreshStatus();
  }
});

document.addEventListener("DOMContentLoaded", () => {
  void bootstrap();
});

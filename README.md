# duke-link

Open-source, one-click Rust P2P friend-joiner for **Horizon MC** — no VPN installs, no port forwarding, and no manual modpack zip wrangling.

## What It Solves

Hosting a modded Minecraft server for friends normally forces everyone to install a VPN, configure port forwarding, or manually copy mod folders while clobbering their own keybinds and graphics tweaks.

`duke-link` pairs a lightweight cross-platform relay daemon (`relay-daemon`) with a native Tauri v2 desktop client (`duke-link-client`) over **Iroh 1.5.0 QUIC**:
- **Zero VPN / Zero Port Forwarding:** Hole-punched P2P QUIC connections dialed directly by Ed25519 `EndpointId` with automatic relay fallback.
- **BLAKE3 Delta Mod Sync:** Downloads only missing or changed pack files into a canonicalized managed instance directory.
- **3-Way Config Overlay:** Overwrites `EnforcedServerState` mods/configs while preserving each friend's local edits on `InitialDefault` files (keybinds, FOV, LOD settings) via a local baseline hash store.
- **One-Click Launch & Crash Telemetry:** Binds `127.0.0.1:25565`, writes `servers.dat`, launches `horizon-client`, and automatically uploads non-zero exit crash logs (capped at 2 MiB) to the host.

## Architecture

```text
+-------------------------------+         Iroh 1.5.0 QUIC (Ed25519 mTLS)         +---------------------------------+
|   Friend Desktop (Win11)      | <============================================> |   Server Host (Linux / Win)     |
|   duke-link-client (Tauri v2) |                                                |   relay-daemon                  |
|                               |  ALPN: duke-link/mc-tunnel/1                   |                                 |
|  horizon-client -> 127.0.0.1  | <------------ Bidirectional TCP -------------> | -> 127.0.0.1:25565 (Horizon MC) |
|                    :25565     |                                                |                                 |
|                               |  ALPN: duke-link/sync/1                        |                                 |
|  BLAKE3 Delta + 3-Way Overlay | <---- PackManifest + 64 KiB File Chunks -----> | -> pack_dir/ (canonicalized)    |
|                               |                                                |                                 |
|                               |  ALPN: duke-link/status/1                      |                                 |
|  Live Ping / Players / Crash  | <---- /api/v1/status + 2 MiB Crash Log ------> | -> 127.0.0.1:8080 + crash-dir/  |
+-------------------------------+                                                +---------------------------------+
```

## Friend Setup (3 Steps)

1. Download and launch **Duke Link** (`duke-link-client.exe`).
2. Copy your **Endpoint ID** from the *Connection & Profile* card and send it to your server host so they can add you to `allowlist.toml`.
3. Paste the host's `duke1:...` **Invite Ticket**, choose your in-game username, click **Save Settings**, and press **Play**.

## Server Host Quickstart

1. Build or download `duke-link-relay` (`crates/relay-daemon`) and copy [`deploy/relay.example.toml`](deploy/relay.example.toml) as a reference.
2. Generate your persistent Ed25519 identity and print your shareable invite ticket:
   ```bash
   ./duke-link-relay --key-path /opt/duke-link/duke-relay.key --server-name "Horizon SMP" --print-invite
   ```
3. Add each friend's Ed25519 `EndpointId` to `/opt/duke-link/allowlist.toml`:
   ```toml
   allowed_endpoints = [
       "<friend-1-endpoint-id>",
       "<friend-2-endpoint-id>",
   ]
   ```
4. Install [`deploy/duke-link-relay.service`](deploy/duke-link-relay.service) on Ubuntu/Pop!_OS:
   ```bash
   sudo cp deploy/duke-link-relay.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now duke-link-relay
   ```

## Security Model

- **Strict Ed25519 Allowlist:** Every incoming QUIC connection is cryptographically authenticated via Iroh TLS and immediately closed (`403`) if the peer's `EndpointId` is not present in `allowlist.toml`.
- **Canonicalized Path-Traversal Guards:** Both `relay-daemon` (`sync/1`) and `duke-link-client` validate relative paths and canonicalize targets so reads and writes can never escape `pack_dir` or the managed instance directory.
- **Bounded Crash Uploads:** Uploaded crash logs are strictly capped at 2 MiB with sanitized peer and timestamp filename components.
- **Panic-Free Production Rust:** Workspace Clippy lints deny `unwrap_used` and `expect_used` outside tests; all fallible operations propagate typed `thiserror` errors.

## Local Build & Verification

Prerequisites: Rust stable toolchain (`1.91+`), Node.js `20+`, and (on Linux) GTK3/WebKitGTK development headers.

```powershell
# Run formatting, strict clippy, and all unit + Iroh loopback QUIC tests:
.\scripts\verify-hygiene.ps1

# Validate frontend syntax:
node --check crates\client-app\src\main.js
```

## License

Dual-friendly open source licensed under the [MIT License](LICENSE-MIT).

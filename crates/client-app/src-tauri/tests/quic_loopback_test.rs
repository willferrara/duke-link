use std::path::PathBuf;
use std::time::Duration;

use duke_link_client::tunnel::start_tunnel_to_addr;
use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use relay_daemon::handlers::RelayConfig;
use relay_daemon::start_relay;
use shared_protocol::allowlist::Allowlist;
use shared_protocol::wire::ALPN_MC_TUNNEL;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Real in-process Iroh 1.5.0 loopback QUIC integration test:
/// 1. Starts a local mock TCP echo server.
/// 2. Starts a real `relay-daemon` Iroh 1.5.0 endpoint with an explicit Ed25519 allowlist.
/// 3. Connects an allowlisted `duke-link-client` tunnel over `duke-link/mc-tunnel/1` and
///    verifies end-to-end TCP -> QUIC -> TCP echo roundtrip.
/// 4. Connects an unlisted peer and asserts the relay rejects it cleanly.
#[tokio::test]
async fn test_real_iroh_loopback_quic_tunnel_and_allowlist() {
    // 1. Mock local Horizon MC TCP echo server
    let echo_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock echo TCP listener");
    let echo_addr = echo_listener.local_addr().expect("echo local_addr");

    let echo_task = tokio::spawn(async move {
        if let Ok((mut sock, _)) = echo_listener.accept().await {
            let mut buf = [0u8; 1024];
            while let Ok(n) = sock.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if sock.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    });

    // 2. Start real in-process relay-daemon Iroh 1.5.0 endpoint
    let relay_key = SecretKey::generate();
    let allowed_client_key = SecretKey::generate();
    let unlisted_client_key = SecretKey::generate();

    let allowlist = Allowlist::from_ids([allowed_client_key.public()]);
    let temp_root =
        std::env::temp_dir().join(format!("duke_link_quic_loopback_{}", std::process::id()));
    let relay_cfg = RelayConfig {
        mc_addr: echo_addr.to_string(),
        status_addr: "127.0.0.1:1".to_owned(),
        pack_dir: temp_root.join("pack"),
        manifest_path: temp_root.join("pack").join("manifest.json"),
        crash_dir: temp_root.join("crash-reports"),
    };

    let relay = start_relay(relay_key, allowlist, relay_cfg)
        .await
        .expect("start real iroh relay");
    let server_addr = relay.endpoint_addr();

    // 3. Allowlisted client-app tunnel echoes bytes over real QUIC mc-tunnel/1
    let tunnel = start_tunnel_to_addr(allowed_client_key, server_addr.clone(), "127.0.0.1:0")
        .await
        .expect("start client tunnel");

    let mut tcp = TcpStream::connect(tunnel.local_addr())
        .await
        .expect("connect to local tunnel TCP port");

    let payload = b"horizon-mc-handshake-over-iroh-1.5.0-quic";
    tcp.write_all(payload).await.expect("write payload");

    let mut echoed = vec![0u8; payload.len()];
    tokio::time::timeout(Duration::from_secs(10), tcp.read_exact(&mut echoed))
        .await
        .expect("echo timed out")
        .expect("read echoed bytes");
    assert_eq!(echoed.as_slice(), payload);

    drop(tcp);

    // 4. Unlisted peer MUST be rejected by the relay-daemon allowlist
    let intruder_ep = Endpoint::builder(presets::N0)
        .secret_key(unlisted_client_key)
        .bind()
        .await
        .expect("bind intruder endpoint");

    let rejected = tokio::time::timeout(Duration::from_secs(10), async {
        match intruder_ep.connect(server_addr, ALPN_MC_TUNNEL).await {
            Err(_) => true,
            Ok(conn) => match conn.open_bi().await {
                Err(_) => true,
                Ok((mut send, mut recv)) => {
                    let _ = send.write_all(b"intruder-probe").await;
                    let _ = send.finish();
                    let mut buf = [0u8; 32];
                    match recv.read(&mut buf).await {
                        Ok(Some(n)) => n == 0,
                        Ok(None) | Err(_) => true,
                    }
                }
            },
        }
    })
    .await
    .expect("intruder rejection check timed out");

    assert!(
        rejected,
        "unlisted peer must be rejected by relay allowlist"
    );

    intruder_ep.close().await;
    tunnel.stop().await;
    relay.shutdown().await;
    let _ = echo_task.await;
    let _ = std::fs::remove_dir_all(PathBuf::from(&temp_root));
}

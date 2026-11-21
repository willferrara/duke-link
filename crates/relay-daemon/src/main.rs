use std::path::PathBuf;

use clap::Parser;
use relay_daemon::handlers::RelayConfig;
use relay_daemon::{load_or_create_secret_key, start_relay, DaemonError};
use shared_protocol::allowlist::{Allowlist, InviteTicket};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "duke-relay-daemon",
    about = "Cross-platform iroh P2P relay daemon for Horizon MC"
)]
struct Cli {
    /// Path to persistent Ed25519 secret key file (32 raw bytes).
    #[arg(long, default_value = "duke-relay.key")]
    key_path: PathBuf,

    /// Path to TOML EndpointId allowlist file.
    #[arg(long, default_value = "allowlist.toml")]
    allowlist: PathBuf,

    /// Directory containing pack files served over sync/1.
    #[arg(long, default_value = "pack")]
    pack_dir: PathBuf,

    /// Path to PackManifest JSON served over sync/1.
    #[arg(long, default_value = "pack/manifest.json")]
    manifest_path: PathBuf,

    /// Directory where uploaded client crash logs are stored.
    #[arg(long, default_value = "crash-reports")]
    crash_dir: PathBuf,

    /// Local Horizon MC server TCP address for mc-tunnel/1.
    #[arg(long, default_value = "127.0.0.1:25565")]
    mc_addr: String,

    /// Local Horizon MC REST API address for status/1 (/api/v1/status).
    #[arg(long, default_value = "127.0.0.1:8080")]
    status_addr: String,

    /// Human-readable server name embedded in invite tickets.
    #[arg(long, default_value = "Horizon MC Server")]
    server_name: String,

    /// Optional explicit relay URL embedded in invite tickets.
    #[arg(long)]
    relay_url: Option<String>,

    /// Print the shareable invite ticket for this daemon's EndpointId and exit.
    #[arg(long)]
    print_invite: bool,
}

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Err(err) = run().await {
        error!(error = %err, "relay-daemon exited with error");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), DaemonError> {
    let cli = Cli::parse();
    let secret_key = load_or_create_secret_key(&cli.key_path)?;
    let endpoint_id = secret_key.public();

    if cli.print_invite {
        let ticket = InviteTicket {
            server_endpoint_id: endpoint_id,
            server_name: cli.server_name,
            relay_url: cli.relay_url,
        };
        let encoded = ticket.encode()?;
        println!("{encoded}");
        return Ok(());
    }

    let allowlist = Allowlist::load(&cli.allowlist)?;
    let relay_cfg = RelayConfig {
        mc_addr: cli.mc_addr,
        status_addr: cli.status_addr,
        pack_dir: cli.pack_dir,
        manifest_path: cli.manifest_path,
        crash_dir: cli.crash_dir,
    };

    let running = start_relay(secret_key, allowlist, relay_cfg).await?;
    let _ = tokio::signal::ctrl_c().await;
    info!("received Ctrl+C, shutting down relay-daemon");
    running.shutdown().await;
    Ok(())
}

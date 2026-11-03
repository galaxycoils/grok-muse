mod server;

use clap::Parser;
use gm_core::config::GmConfig;
use gm_core::error::Result;
use gm_platform::MockPlatform;
use server::HelperServer;
use std::path::PathBuf;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "gm-cu-helper", about = "Grok-Muse Computer Use Native Helper")]
struct Args {
    #[arg(short, long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let config = GmConfig::default();
    let socket_path = args.socket.unwrap_or(config.socket_path);

    info!("Starting gm-cu-helper daemon at {:?}", socket_path);

    // In v1 initial scaffold, instantiate platform (mock/native fallback)
    let platform = MockPlatform::new();
    let server = HelperServer::new(platform);
    server.run(socket_path).await?;
    Ok(())
}

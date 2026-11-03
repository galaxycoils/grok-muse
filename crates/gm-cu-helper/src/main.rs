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
    #[arg(
        long,
        help = "Force mock platform instead of native macOS ScreenCaptureKit/AX"
    )]
    mock: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let config = GmConfig::default();
    let socket_path = args.socket.unwrap_or(config.socket_path);

    info!("Starting gm-cu-helper daemon at {:?}", socket_path);

    if args.mock {
        info!("Using MockPlatform as requested by --mock flag");
        let platform = MockPlatform::new();
        let server = HelperServer::new(platform);
        server.run(socket_path).await?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        info!("Instantiating native MacPlatform (ScreenCaptureKit + AXUIElement + CGEvent)");
        let platform = gm_platform::MacPlatform::new()?;
        let server = HelperServer::new(platform);
        server.run(socket_path).await?;
    }

    #[cfg(target_os = "linux")]
    {
        info!("Instantiating native LinuxPlatform (AT-SPI + X11/Wayland)");
        let platform = gm_platform::LinuxPlatform::new()?;
        let server = HelperServer::new(platform);
        server.run(socket_path).await?;
    }

    #[cfg(target_os = "windows")]
    {
        info!("Instantiating native WindowsPlatform (UI Automation + DXGI/WGC)");
        let platform = gm_platform::WindowsPlatform::new()?;
        let server = HelperServer::new(platform);
        server.run(socket_path).await?;
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        info!("Instantiating MockPlatform on unknown target");
        let platform = MockPlatform::new();
        let server = HelperServer::new(platform);
        server.run(socket_path).await?;
    }

    Ok(())
}

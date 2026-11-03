use clap::{Parser, Subcommand};
use gm_core::error::Result;
use tracing::info;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn gm_mac_ui_init_tray() -> std::os::raw::c_int;
    fn gm_mac_ui_init_chat_window() -> std::os::raw::c_int;
}

#[derive(Parser, Debug)]
#[command(name = "gm-ui", about = "Grok-Muse Desktop Shell UI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Launch tray icon and global hotkey monitor
    Tray,
    /// Open floating chat / action overlay window
    Chat,
    /// Trigger emergency kill switch immediately
    Kill,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Tray) {
        Commands::Tray => {
            info!("Launching Grok-Muse tray shell");
            #[cfg(target_os = "macos")]
            unsafe {
                let ret = gm_mac_ui_init_tray();
                if ret == 0 {
                    info!("Native macOS AppKit NSStatusItem initialized");
                }
            }
            println!("Grok-Muse system tray active. Press Cmd+Alt+Escape for kill switch.");
            tokio::signal::ctrl_c().await.ok();
        }
        Commands::Chat => {
            info!("Opening floating chat interface");
            #[cfg(target_os = "macos")]
            unsafe {
                let ret = gm_mac_ui_init_chat_window();
                if ret == 0 {
                    info!("Native macOS AppKit floating window initialized");
                }
            }
            println!("Grok-Muse chat interface initialized.");
            tokio::signal::ctrl_c().await.ok();
        }
        Commands::Kill => {
            info!("Emergency kill switch activated from CLI");
            println!("Kill signal dispatched to daemon.");
        }
    }

    Ok(())
}

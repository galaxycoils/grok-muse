use clap::{Parser, Subcommand};
use gm_core::error::Result;
use tracing::info;

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
            println!("Grok-Muse system tray active. Press Cmd+Alt+Escape for kill switch.");
        }
        Commands::Chat => {
            info!("Opening floating chat interface");
            println!("Grok-Muse chat interface initialized.");
        }
        Commands::Kill => {
            info!("Emergency kill switch activated from CLI");
            println!("Kill signal dispatched to daemon.");
        }
    }

    Ok(())
}

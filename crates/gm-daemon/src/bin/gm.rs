use clap::Parser;
use gm_core::error::Result;
use gm_daemon::{
    Cli, Commands, run_daemon, run_do, run_dogfood, run_kill, run_onboard, run_permissions,
    run_review, run_service, run_stats, run_status,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Permissions {
            open_missing,
            open,
            json,
        }) => {
            run_permissions(open_missing, open, json).await?;
        }
        Some(Commands::Onboard { check_only }) => {
            run_onboard(check_only).await?;
        }
        Some(Commands::Do { instruction }) => {
            run_do(&instruction, cli.config).await?;
        }
        Some(Commands::Daemon) => {
            run_daemon(cli.config, cli.task).await?;
        }
        Some(Commands::Status) => {
            run_status().await?;
        }
        Some(Commands::Kill) => {
            run_kill().await?;
        }
        Some(Commands::Service { action }) => {
            run_service(action).await?;
        }
        Some(Commands::Dogfood { action, json }) => {
            run_dogfood(action, json).await?;
        }
        Some(Commands::Stats { json }) => {
            run_stats(json).await?;
        }
        Some(Commands::Review { date }) => {
            run_review(date).await?;
        }
        None => {
            if let Some(task) = cli.task {
                run_do(&task, cli.config).await?;
            } else {
                run_onboard(true).await?;
            }
        }
    }

    Ok(())
}

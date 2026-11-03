use clap::Parser;
use gm_agent::{AgentLoop, AgentLoopConfig};
use gm_core::config::GmConfig;
use gm_core::error::Result;
use gm_goals::{ProactiveScheduler, QuietGate, QuietGateConfig};
use gm_llm::{BudgetManager, HybridLlmClient, LlmRouter, MockLlmBackend};
use gm_memory::PersonalMemoryStore;
use gm_platform::IpcPlatformClient;
use gm_safety::{AuditLogger, BlocklistManager, KillSwitch};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "gm-daemon", about = "Grok-Muse Hybrid Agent Background Daemon")]
struct Args {
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run a single task and exit
    #[arg(long)]
    task: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let config = GmConfig::default();

    info!("Starting gm-daemon...");
    let audit_log = AuditLogger::in_memory();
    let kill_switch = KillSwitch::new();
    let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let router = LlmRouter::new(config.providers.clone());

    // Connect to cu-helper IPC client
    let platform = Arc::new(IpcPlatformClient::new(&config.socket_path));
    let llm_backend = Arc::new(MockLlmBackend::new());
    let llm_client = Arc::new(HybridLlmClient::new(llm_backend, budget));

    // Memory store
    let _memory = PersonalMemoryStore::in_memory()?;

    // Proactive engine setup
    let _quiet_gate = QuietGate::new(QuietGateConfig::default());
    let scheduler = ProactiveScheduler::new();

    // Register proactive patrol job
    scheduler.register_job("patrol_calendar_reminders", 300, || {
        info!("Running proactive calendar and reminders patrol");
    });

    if let Some(task_instruction) = args.task {
        info!("Running one-shot task: {}", task_instruction);
        let agent = AgentLoop::new(
            platform,
            llm_client,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        );

        match agent.run_task("cli_task", &task_instruction).await {
            Ok(summary) => {
                println!("Task completed: {summary}");
                audit_log.log(
                    "task_done",
                    None,
                    None,
                    None,
                    None,
                    Some(summary),
                    serde_json::json!({}),
                );
            }
            Err(e) => {
                eprintln!("Task failed: {e}");
                return Err(e);
            }
        }
    } else {
        info!("gm-daemon running in background mode.");
    }

    Ok(())
}

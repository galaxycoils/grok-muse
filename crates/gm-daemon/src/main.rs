use clap::Parser;
use gm_agent::{AgentLoop, AgentLoopConfig};
use gm_core::config::GmConfig;
use gm_core::error::Result;
use gm_goals::{ProactiveEngine, QuietGateConfig};
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
    let llm_client = Arc::new(HybridLlmClient::new(llm_backend, Arc::clone(&budget)));

    // Memory store
    let _memory = PersonalMemoryStore::in_memory()?;

    // Proactive engine setup connecting scheduler, quiet gate, and daily spend cap tracking
    let proactive_engine = Arc::new(ProactiveEngine::new(
        QuietGateConfig::default(),
        Arc::clone(&budget),
    ));

    // Register proactive patrol job connecting EventKit and UserNotifications
    proactive_engine.schedule_patrol(Arc::clone(&platform), 300);

    // Spawn proactive scheduler tick task so background patrol jobs actually execute
    let tick_engine = Arc::clone(&proactive_engine);
    let tick_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            tick_engine.tick(chrono::Utc::now());
        }
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
                tick_task.abort();
                return Err(e);
            }
        }
    } else {
        info!("gm-daemon running in background mode.");
        tokio::signal::ctrl_c().await.ok();
    }

    tick_task.abort();
    Ok(())
}

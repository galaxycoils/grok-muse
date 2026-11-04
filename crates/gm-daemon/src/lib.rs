use clap::{Parser, Subcommand};
use gm_agent::{AgentLoop, AgentLoopConfig};
use gm_core::config::GmConfig;
use gm_core::error::{GmError, Result};
use gm_core::ipc::{
    DaemonRequest, DaemonResponse, IpcMessage, read_ipc_message, write_ipc_message,
};
use gm_goals::{ProactiveEngine, QuietGateConfig};
use gm_llm::{
    BudgetManager, ChatMessage, HybridLlmClient, KeychainSecrets, LlmBackend, LlmRouter,
    META_KEY_ACCOUNT, MockLlmBackend, ReqwestLlmBackend, TaskType, XAI_KEY_ACCOUNT,
};
use gm_memory::PersonalMemoryStore;
use gm_platform::permissions::{PrivacyPane, check_all_permissions};
use gm_platform::{IpcPlatformClient, Platform};
use gm_safety::{AuditLogger, BlocklistManager, KillSwitch};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

pub mod control_server;
pub mod service;
pub mod updater;

pub use control_server::*;
pub use service::*;
pub use updater::*;

struct UnavailableLlmBackend;
#[async_trait::async_trait]
impl LlmBackend for UnavailableLlmBackend {
    async fn complete(
        &self,
        _target: &gm_llm::ModelTarget,
        _messages: &[ChatMessage],
    ) -> Result<gm_llm::ChatCompletionResponse> {
        Err(GmError::Config("No model is configured. Add a provider API key or install a compatible checkpoint and tokenizer. Demo inference requires explicit GM_FORCE_MOCK_LLM=1.".into()))
    }
}

pub fn build_llm_backend(secrets: &KeychainSecrets) -> Arc<dyn LlmBackend> {
    if std::env::var("GM_FORCE_MOCK_LLM")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        warn!("GM_FORCE_MOCK_LLM is set, forcing MockLlmBackend");
        return Arc::new(MockLlmBackend::new());
    }

    let meta_key = secrets
        .get("META_API_KEY")
        .or_else(|| secrets.get_meta_key().ok())
        .or_else(|| std::env::var("META_API_KEY").ok())
        .filter(|k| !k.trim().is_empty());

    let xai_key = secrets
        .get("XAI_API_KEY")
        .or_else(|| secrets.get_xai_key().ok())
        .or_else(|| std::env::var("XAI_API_KEY").ok())
        .filter(|k| !k.trim().is_empty());

    if meta_key.is_some() || xai_key.is_some() {
        Arc::new(ReqwestLlmBackend::with_keys(meta_key, xai_key))
    } else if let Some(local_engine) =
        gm_vlm_candle::ModelRegistry::default_registry().find_installed_engine()
    {
        info!(
            "No cloud API keys found; using installed local Candle VLM ({}) for offline inference",
            local_engine.config().model_id
        );
        Arc::new(gm_llm::LocalVlmLlmAdapter::new(Arc::new(local_engine)))
    } else {
        warn!("No provider key or usable local VLM configured; inference is unavailable");
        Arc::new(UnavailableLlmBackend)
    }
}

pub async fn ensure_helper_running(config: &GmConfig) -> Result<()> {
    let client = IpcPlatformClient::new(&config.socket_path);
    if let Ok(Ok(())) =
        tokio::time::timeout(tokio::time::Duration::from_millis(250), client.ping()).await
    {
        return Ok(());
    }

    let (_, helper_bin) = service::resolve_binaries(None);
    if !helper_bin.exists() {
        warn!(
            "gm-cu-helper binary not found at {:?}; skipping auto-spawn",
            helper_bin
        );
        return Err(GmError::Platform(format!(
            "gm-cu-helper binary not found at {:?}",
            helper_bin
        )));
    }

    info!(
        "Auto-spawning gm-cu-helper at {:?} with socket {:?}",
        helper_bin, config.socket_path
    );
    let mut cmd = std::process::Command::new(&helper_bin);
    cmd.arg("--socket").arg(&config.socket_path);

    let _child = cmd
        .spawn()
        .map_err(|e| GmError::Platform(format!("Failed to auto-spawn gm-cu-helper: {e}")))?;

    let start = tokio::time::Instant::now();
    let timeout = tokio::time::Duration::from_millis(1500);
    while start.elapsed() < timeout {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        if let Ok(Ok(())) =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), client.ping()).await
        {
            info!("gm-cu-helper auto-spawned successfully and responding to Ping");
            return Ok(());
        }
    }

    Err(GmError::Platform(
        "gm-cu-helper spawned but did not respond to Ping within 1.5s".into(),
    ))
}

#[derive(Parser, Debug)]
#[command(
    name = "gm",
    about = "Grok-Muse Hybrid Agent: fast computer-use + proactive personal agent"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Run a single agent task directly (shorthand for `gm do`)
    #[arg(long)]
    pub task: Option<String>,

    /// Path to custom config file
    #[arg(short, long)]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Inspect or request macOS permissions (Screen Recording, Accessibility, EventKit)
    Permissions {
        /// Automatically open missing permission panes in macOS System Settings
        #[arg(long)]
        open_missing: bool,
        /// Open specific privacy pane: screen, accessibility, calendar, reminders
        #[arg(long)]
        open: Option<String>,
        /// Output live status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Interactive onboarding walkthrough
    Onboard {
        /// Check-only mode without opening windows
        #[arg(long)]
        check_only: bool,
    },
    /// Run an agent computer-use task
    Do {
        /// Instruction for computer-use agent
        instruction: String,
    },
    /// Run background daemon process
    Daemon,
    /// Show system and agent health status
    Status,
    /// Trigger emergency kill switch
    Kill,
    /// Service management and auto-start (LaunchAgent install/uninstall/status)
    Service {
        #[command(subcommand)]
        action: ServiceSubcommand,
    },
    /// Proactive dogfood rating & metrics tracking (G6 gate)
    Dogfood {
        #[command(subcommand)]
        action: Option<DogfoodSubcommand>,
        /// Output metrics as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show system health, permissions, and dogfood statistics
    Stats {
        /// Output statistics as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run structured nightly review synthesis
    Review {
        /// Date to review (YYYY-MM-DD, defaults to today)
        #[arg(long)]
        date: Option<String>,
    },
    /// Synthesize weekly or monthly safety, spend, CU actions, and task success audit report
    Audit {
        /// Audit aggregation period: weekly or monthly
        #[arg(long, default_value = "weekly")]
        period: String,
        /// Output audit metrics as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run memory upkeep to deduplicate facts, cluster knowledge, and prune stale observations
    Upkeep {
        /// Days after which transient observations expire (default: 14)
        #[arg(long)]
        stale_days: Option<i64>,
        /// Output report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Stream recent observations, patrol insights, and completed tasks
    Feed {
        /// Number of feed items to retrieve
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Output feed as JSON
        #[arg(long)]
        json: bool,
    },
    /// Proactive suggestions derived from calendar, unread threads, and personal memory
    Ideas {
        /// Output suggestions as JSON
        #[arg(long)]
        json: bool,
    },
    /// Interactive terminal REPL for conversation, memory retrieval, status inspection, and inline task dispatch
    Chat {
        /// Optional initial system prompt override
        #[arg(long)]
        system: Option<String>,
    },
    /// Check for updates or download newer release DMG with checksum verification (Action A11)
    Update {
        /// Check only for updates without downloading
        #[arg(long)]
        check: bool,
        /// Force download even if current version is up to date
        #[arg(long)]
        force: bool,
        /// Target GitHub repository (default: galaxycoils/grok-muse)
        #[arg(long)]
        repo: Option<String>,
        /// Output update status as JSON
        #[arg(long)]
        json: bool,
        /// Custom download destination directory
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Manage offline local Vision-Language Models (Qwen2-VL-2B / SmolVLM2)
    Models {
        #[command(subcommand)]
        action: Option<ModelsSubcommand>,
        /// Output model registry status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage OpenBot persistent agent roster (muse, researcher, coder, guardian)
    Bots {
        #[command(subcommand)]
        action: Option<BotsSubcommand>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage OpenBot crash-safe SQLite task queue (list/add/pause/resume/cancel/reassign/backup/restore)
    Queue {
        #[command(subcommand)]
        action: Option<QueueSubcommand>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage OpenBot multi-bot channels (#general, #research, #ops)
    Channels {
        #[command(subcommand)]
        action: Option<ChannelsSubcommand>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run loopback OpenBot Companion Web / AG-UI / A2A server (default: 127.0.0.1:7433)
    Serve {
        /// Loopback bind address
        #[arg(long, default_value = "127.0.0.1:7433")]
        bind: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum BotsSubcommand {
    /// List all configured bots and their isolated workspaces
    List,
    /// Show detailed profile and workspace paths for a bot
    Show { id: String },
    /// Create or update a custom bot profile
    Create {
        id: String,
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "Custom Specialist")]
        role: String,
        #[arg(long, default_value = "You are a helpful specialist agent.")]
        prompt: String,
    },
    /// Remove a custom bot profile
    Rm { id: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum QueueSubcommand {
    /// List queued, running, paused, and completed tasks
    List {
        /// Optional bot filter
        #[arg(long)]
        bot: Option<String>,
    },
    /// Enqueue a new task for a bot
    Add {
        /// Target bot ID (e.g., muse, coder, researcher, guardian)
        #[arg(long, default_value = "muse")]
        bot: String,
        /// Task instruction
        instruction: String,
        /// Priority (higher runs first)
        #[arg(long, default_value = "0")]
        priority: i32,
    },
    /// Pause a task
    Pause { id: String },
    /// Resume a paused or human-handoff task
    Resume {
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Cancel a task
    Cancel { id: String },
    /// Reassign a task to another bot
    Reassign { id: String, bot: String },
    /// Export queue backup to JSONL file
    Backup { path: PathBuf },
    /// Restore queue backup from JSONL file
    Restore { path: PathBuf },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ChannelsSubcommand {
    /// List all multi-bot channels
    List,
    /// Post a message to a channel (triggers @bot mentions automatically)
    Post {
        channel: String,
        message: String,
        #[arg(long, default_value = "user")]
        author: String,
    },
    /// Show recent messages in a channel
    History {
        channel: String,
        #[arg(long, default_value = "25")]
        limit: usize,
    },
    /// Assign lead owner bot for a channel
    Assign { channel: String, bot: String },
    /// Archive a channel
    Archive { channel: String },
    /// Restore an archived channel
    Restore { channel: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ModelsSubcommand {
    /// List available and installed local VLM models
    List,
    /// Pull open-weight VLM safetensors & tokenizer from HuggingFace Hub (default: Qwen/Qwen2-VL-2B-Instruct)
    Pull {
        /// Model ID or alias (qwen2-vl-2b, smolvlm2-2.2b, smolvlm, or <org>/<repo>)
        model: Option<String>,
    },
    /// Remove an installed local VLM from disk
    Rm {
        /// Model ID or alias to remove
        model: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ServiceSubcommand {
    /// Install and bootstrap LaunchAgent plists for gm-daemon and gm-cu-helper
    Install {
        /// Optional custom binary directory
        #[arg(long)]
        bin_dir: Option<PathBuf>,
    },
    /// Bootout and remove LaunchAgent plists
    Uninstall,
    /// Show LaunchAgent registration and process status
    Status,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DogfoodSubcommand {
    /// Print dogfood metrics report and G6 gate status
    Summary,
    /// Rate a notice useful or not useful
    Rate {
        notice_id: String,
        #[arg(long)]
        useful: bool,
    },
    /// Record user feedback action for a notice (done, later, stop)
    Feedback {
        notice_id: String,
        #[arg(long)]
        topic: String,
        #[arg(long)]
        action: String,
    },
}

pub async fn run_permissions(
    open_missing: bool,
    open_pane: Option<String>,
    json_format: bool,
) -> Result<()> {
    let status = check_all_permissions();

    if let Some(ref pane_str) = open_pane {
        let pane = PrivacyPane::parse(pane_str).ok_or_else(|| {
            GmError::Config(format!(
                "Unknown privacy pane '{pane_str}'. Valid options: screen, accessibility, calendar, reminders, or Privacy_<Name>"
            ))
        })?;
        println!("Opening System Settings for {}...", pane.title());
        let opened = pane.open();
        if opened {
            println!("==> Successfully launched System Settings: {}", pane.url());
        } else {
            eprintln!("==> Failed to launch System Settings URL: {}", pane.url());
        }
        return Ok(());
    }

    if open_missing {
        if !status.screen_recording {
            println!("Opening Screen Recording settings...");
            PrivacyPane::ScreenRecording.open();
        }
        if !status.accessibility {
            println!("Opening Accessibility settings...");
            PrivacyPane::Accessibility.open();
        }
        if !status.calendar {
            println!("Opening Calendars settings...");
            PrivacyPane::Calendars.open();
        }
        if !status.reminders {
            println!("Opening Reminders settings...");
            PrivacyPane::Reminders.open();
        }
    }

    if json_format {
        let json_val = serde_json::json!({
            "screen_recording": status.screen_recording,
            "accessibility": status.accessibility,
            "calendar": status.calendar,
            "reminders": status.reminders,
            "all_granted": status.all_granted(),
            "cu_granted": status.cu_granted(),
            "deep_links": {
                "screen_recording": PrivacyPane::ScreenRecording.url(),
                "accessibility": PrivacyPane::Accessibility.url(),
                "calendar": PrivacyPane::Calendars.url(),
                "reminders": PrivacyPane::Reminders.url(),
            }
        });
        println!("{}", serde_json::to_string_pretty(&json_val)?);
        return Ok(());
    }

    println!("\n============================================================");
    println!("             Grok-Muse macOS Permissions Status             ");
    println!("============================================================");

    let format_row = |name: &str, granted: bool, pane: PrivacyPane| {
        let badge = if granted { " [✔] " } else { " [✘] " };
        let state = if granted {
            "Granted".to_string()
        } else {
            "NOT GRANTED".to_string()
        };
        println!("{:<7}{:<20}: {}", badge, name, state);
        if !granted {
            println!("      -> Deep Link: {}", pane.url());
            println!(
                "      -> Open via:  gm permissions --open {}",
                pane.pane_name()
            );
        }
    };

    format_row(
        "Screen Recording",
        status.screen_recording,
        PrivacyPane::ScreenRecording,
    );
    format_row(
        "Accessibility",
        status.accessibility,
        PrivacyPane::Accessibility,
    );
    format_row("Calendars", status.calendar, PrivacyPane::Calendars);
    format_row("Reminders", status.reminders, PrivacyPane::Reminders);

    println!("------------------------------------------------------------");
    if status.all_granted() {
        println!("Overall Status: ALL REQUIRED PERMISSIONS GRANTED (Ready)");
    } else if status.cu_granted() {
        println!("Overall Status: Computer-Use Ready (Proactive EventKit permissions partial)");
    } else {
        println!("Overall Status: ACTION REQUIRED (Run 'gm permissions --open-missing')");
    }
    println!("============================================================\n");

    Ok(())
}

pub async fn run_onboard(check_only: bool) -> Result<()> {
    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║           Grok-Muse Interactive Onboarding Guide         ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!("Grok-Muse combines low-latency macOS computer-use with a");
    println!("proactive personal agent. Let's verify your system setup.\n");

    // 1. TCC Permissions
    println!("▶ Step 1: Checking macOS System Permissions (TCC)");
    let perms = check_all_permissions();

    let check_and_prompt = |name: &str, granted: bool, pane: PrivacyPane| {
        if granted {
            println!("  ✔ {:<18}: Granted", name);
        } else {
            println!("  ✘ {:<18}: NOT GRANTED", name);
            println!("    Purpose: {}", pane.description());
            println!("    Deep Link: {}", pane.url());
            if !check_only {
                println!("    --> Opening System Settings now...");
                pane.open();
            }
        }
    };

    check_and_prompt(
        "Screen Recording",
        perms.screen_recording,
        PrivacyPane::ScreenRecording,
    );
    check_and_prompt(
        "Accessibility",
        perms.accessibility,
        PrivacyPane::Accessibility,
    );
    check_and_prompt("Calendars", perms.calendar, PrivacyPane::Calendars);
    check_and_prompt("Reminders", perms.reminders, PrivacyPane::Reminders);

    // 2. Keychain API Keys
    println!("\n▶ Step 2: Checking LLM Provider API Keys (macOS Keychain)");
    let keychain = KeychainSecrets::default();

    let meta_key = keychain.get_secret(META_KEY_ACCOUNT).ok();
    if meta_key.is_some() {
        println!("  ✔ Meta Model API key : Found (Keychain or META_API_KEY)");
    } else {
        println!("  ○ Meta Model API key : Not configured (Optional: export META_API_KEY=...)");
    }

    let xai_key = keychain.get_secret(XAI_KEY_ACCOUNT).ok();
    if xai_key.is_some() {
        println!("  ✔ xAI Grok API key   : Found (Keychain or XAI_API_KEY)");
    } else {
        println!("  ○ xAI Grok API key   : Not configured (Optional: export XAI_API_KEY=...)");
    }

    // 3. Helper & Daemon status
    println!("\n▶ Step 3: Checking Background Helper & Daemon Architecture");
    let config = GmConfig::load(None)?;
    let helper_socket = &config.socket_path;
    if helper_socket.exists() {
        println!("  ✔ CU Helper socket   : Active at {:?}", helper_socket);
    } else {
        println!(
            "  ○ CU Helper socket   : Idle (Started automatically or via GrokMuseCUHelper.app)"
        );
    }

    // 4. Summary
    println!("\n════════════════════════════════════════════════════════════");
    if perms.all_granted() {
        println!("🎉 Onboarding complete! Grok-Muse is fully configured and ready.");
    } else {
        println!("⚠️  Some permissions are pending. After toggling switches in");
        println!("    System Settings, verify anytime with: gm permissions");
    }
    println!("\nUseful Commands:");
    println!("  • Execute a task:       gm do \"Open Notes and add meeting notes\"");
    println!("  • Desktop Shell UI:     cargo run -p gm-ui -- tray");
    println!("  • Permissions Dialog:   cargo run -p gm-ui -- onboard");
    println!("  • Emergency Kill:       Cmd+Alt+Escape or gm kill");
    println!("════════════════════════════════════════════════════════════\n");

    Ok(())
}

async fn submit_to_running_daemon(config: &GmConfig, instruction: &str) -> Result<Option<String>> {
    async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        mut stream: S,
        instruction: &str,
    ) -> Result<Option<String>> {
        gm_core::ipc::write_ipc_message(
            &mut stream,
            &gm_core::ipc::IpcMessage {
                id: 1,
                payload: gm_core::ipc::DaemonRequest::SubmitTask {
                    instruction: instruction.into(),
                },
            },
        )
        .await?;
        let response: gm_core::ipc::IpcMessage<gm_core::ipc::DaemonResponse> =
            gm_core::ipc::read_ipc_message(&mut stream).await?;
        match response.payload {
            gm_core::ipc::DaemonResponse::Accepted { id } => Ok(Some(id)),
            gm_core::ipc::DaemonResponse::Error(message) => Err(GmError::Ipc(message)),
            _ => Err(GmError::Ipc("Unexpected task submission response".into())),
        }
    }
    #[cfg(unix)]
    let connection = tokio::net::UnixStream::connect(&config.daemon_socket_path).await;
    #[cfg(windows)]
    let connection =
        tokio::net::windows::named_pipe::ClientOptions::new().open(&config.daemon_socket_path);
    match connection {
        Ok(stream) => tokio::time::timeout(
            std::time::Duration::from_secs(5),
            exchange(stream, instruction),
        )
        .await
        .map_err(|_| GmError::Timeout("Daemon did not acknowledge task submission".into()))?,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

async fn wait_for_queued_task(
    supervisor: &gm_openbot::OpenBotSupervisor,
    id: &str,
) -> Result<String> {
    loop {
        let task = supervisor
            .queue()
            .get_task(id)?
            .ok_or_else(|| GmError::Config("Task disappeared".into()))?;
        match task.status {
            gm_openbot::QueuedTaskStatus::Completed => {
                return Ok(task.result_summary.unwrap_or_default());
            }
            gm_openbot::QueuedTaskStatus::Failed | gm_openbot::QueuedTaskStatus::Cancelled => {
                return Err(GmError::Provider(
                    task.result_summary
                        .unwrap_or_else(|| task.status.as_str().into()),
                ));
            }
            gm_openbot::QueuedTaskStatus::Paused
            | gm_openbot::QueuedTaskStatus::WaitingForHuman => {
                return Ok(format!(
                    "Task {} needs human input: {}",
                    id,
                    task.blocker_note.unwrap_or_default()
                ));
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

pub async fn run_do(instruction: &str, config_path: Option<PathBuf>) -> Result<()> {
    let config = GmConfig::load(config_path.as_deref())?;
    if let Some(id) = submit_to_running_daemon(&config, instruction).await? {
        println!("Task queued in the running daemon: {id}");
        return Ok(());
    }
    let _runtime_lease = gm_openbot::runtime::RuntimeLease::acquire(&config.storage_dir)?;
    let _ = ensure_helper_running(&config).await;

    println!(
        "==> Running Grok-Muse computer-use task: \"{}\"",
        instruction
    );
    let audit_log = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());
    let kill_switch = KillSwitch::new();
    let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let router = LlmRouter::new(config.providers.clone());

    let platform = Arc::new(IpcPlatformClient::new(&config.socket_path));
    let secrets = KeychainSecrets::default();
    let llm_backend = build_llm_backend(&secrets);
    let llm_client = Arc::new(HybridLlmClient::new(llm_backend, Arc::clone(&budget)));

    let mut agent = AgentLoop::new(
        platform,
        llm_client,
        router,
        kill_switch,
        blocklist,
        AgentLoopConfig::default(),
    )
    .with_capabilities(gm_agent::AgentCapabilities {
        allow_gui: config.safety.allow_host_desktop,
        allow_shell: false,
        allow_network: true,
    })
    .with_audit(Arc::new(audit_log.clone()));

    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        let handler = Arc::new(
            gm_agent::CallbackInteractionHandler::new()
                .with_approval_callback(|req| {
                    use std::io::Write;
                    println!(
                        "\n⚠️  [USER CONFIRMATION REQUIRED]\nAction: {:?}\nTarget: {:?}\nReason: {}\nApprove execution? [y/N]: ",
                        req.action, req.target_label, req.reason
                    );
                    let _ = std::io::stdout().flush();
                    let mut input = String::new();
                    let _ = std::io::stdin().read_line(&mut input);
                    let trimmed = input.trim().to_lowercase();
                    trimmed == "y" || trimmed == "yes"
                })
                .with_ask_user_callback(|question| {
                    use std::io::Write;
                    println!("\n❓ [AGENT ASKS]: {question}\nYour answer: ");
                    let _ = std::io::stdout().flush();
                    let mut input = String::new();
                    let _ = std::io::stdin().read_line(&mut input);
                    Some(input.trim().to_string())
                }),
        );
        agent = agent.with_interaction_handler(handler);
    }

    match agent.run_task("cli_do", instruction).await {
        Ok(summary) => {
            println!("Task succeeded: {summary}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Task failed: {e}");
            Err(e)
        }
    }
}

pub async fn run_status() -> Result<()> {
    println!("\n============================================================");
    println!("                Grok-Muse System Status                     ");
    println!("============================================================");

    let perms = check_all_permissions();
    println!("Permissions:");
    println!(
        "  Screen Recording : {}",
        if perms.screen_recording {
            "OK"
        } else {
            "Missing"
        }
    );
    println!(
        "  Accessibility    : {}",
        if perms.accessibility { "OK" } else { "Missing" }
    );
    println!(
        "  Calendars        : {}",
        if perms.calendar { "OK" } else { "Missing" }
    );
    println!(
        "  Reminders        : {}",
        if perms.reminders { "OK" } else { "Missing" }
    );

    let config = GmConfig::load(None)?;
    let helper_socket = &config.socket_path;
    println!(
        "\nHelper Socket: {}",
        if helper_socket.exists() {
            format!("Connected ({})", helper_socket.display())
        } else {
            "Not running".to_string()
        }
    );

    println!("============================================================\n");
    Ok(())
}

pub async fn run_kill() -> Result<()> {
    let config = GmConfig::load(None)?;
    run_kill_with_config(&config).await
}

pub async fn run_kill_with_config(config: &GmConfig) -> Result<()> {
    println!("🚨 Grok-Muse Emergency Kill Switch Triggered");

    let daemon_kill = async {
        #[cfg(unix)]
        if config.daemon_socket_path.exists()
            && let Ok(Ok(mut stream)) = tokio::time::timeout(
                tokio::time::Duration::from_millis(40),
                tokio::net::UnixStream::connect(&config.daemon_socket_path),
            )
            .await
        {
            let req = IpcMessage {
                id: 999,
                payload: DaemonRequest::EmergencyKill,
            };
            if let Ok(Ok(())) = tokio::time::timeout(
                tokio::time::Duration::from_millis(20),
                write_ipc_message(&mut stream, &req),
            )
            .await
            {
                if let Ok(Ok(resp)) = tokio::time::timeout(
                    tokio::time::Duration::from_millis(20),
                    read_ipc_message::<_, IpcMessage<DaemonResponse>>(&mut stream),
                )
                .await
                {
                    if matches!(resp.payload, DaemonResponse::Killed) {
                        println!("✔ Daemon confirmed emergency kill.");
                    }
                } else {
                    println!("✔ Daemon confirmed emergency kill.");
                }
            }
        }
    };

    let helper_kill = async {
        #[cfg(unix)]
        if config.socket_path.exists() {
            println!(
                "Connecting to CU Helper socket ({:?}) to drop active actions...",
                config.socket_path
            );
            let platform = IpcPlatformClient::new(&config.socket_path);
            match tokio::time::timeout(tokio::time::Duration::from_millis(40), platform.cancel())
                .await
            {
                Ok(Ok(())) => {
                    println!("✔ CU Helper confirmed: active actions cancelled and dropped.")
                }
                Ok(Err(e)) => eprintln!("⚠ Notice: Could not send cancel over socket: {e}"),
                Err(_) => eprintln!("⚠ Notice: Helper cancel timed out"),
            }
        } else {
            println!("○ CU Helper socket is not active (no background actions queued).");
        }
    };

    tokio::join!(daemon_kill, helper_kill);
    println!("Emergency stop completed.");
    Ok(())
}

pub async fn run_chat(system_override: Option<String>, config_path: Option<PathBuf>) -> Result<()> {
    let stdin = std::io::stdin();
    let reader = stdin.lock();
    let mut stdout = std::io::stdout();
    run_chat_loop(reader, &mut stdout, system_override, config_path).await
}

pub async fn run_chat_loop<R: BufRead, W: Write>(
    mut reader: R,
    writer: &mut W,
    system_override: Option<String>,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let config = GmConfig::load(config_path.as_deref())?;

    let keychain = KeychainSecrets::default();
    let meta_key = keychain.get_meta_key().ok();
    let xai_key = keychain.get_xai_key().ok();

    let backend = build_llm_backend(&keychain);
    let backend_name = if meta_key.is_some() || xai_key.is_some() {
        "Live API (Keychain / Environment)"
    } else {
        "Model unavailable (configure a provider or compatible local model)"
    };

    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let llm_client = Arc::new(HybridLlmClient::new(backend, Arc::clone(&budget)));
    let router = LlmRouter::new(config.providers.clone());
    let memory = Arc::new(
        PersonalMemoryStore::default_store()
            .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap()),
    );
    let audit_log = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());

    let system_prompt = system_override.unwrap_or_else(|| {
        "You are Grok-Muse, a fast computer-use personal assistant for macOS. \
        You have direct access to local memory, system permissions, and desktop automation tools. \
        Respond concisely, accurately, and politely."
            .into()
    });

    let mut history: Vec<ChatMessage> = vec![ChatMessage::text("system", system_prompt)];

    writeln!(
        writer,
        "\n╔═════════════════════════════════════════════════════════════════════════════╗"
    )?;
    writeln!(
        writer,
        "║                 Grok-Muse Interactive Terminal REPL                         ║"
    )?;
    writeln!(
        writer,
        "╚═════════════════════════════════════════════════════════════════════════════╝"
    )?;
    writeln!(writer, "  Active Backend : {}", backend_name)?;
    writeln!(
        writer,
        "  Helper Socket  : {}",
        config.socket_path.display()
    )?;
    writeln!(writer, "\nCommands:")?;

    writeln!(
        writer,
        "  • /memory <query>  : Search personal knowledge base (FTS5)"
    )?;
    writeln!(
        writer,
        "  • /status          : Inspect system health, permissions, and budget"
    )?;
    writeln!(
        writer,
        "  • /do <task>       : Dispatch an inline computer-use task directly"
    )?;
    writeln!(writer, "  • /clear           : Clear conversation history")?;
    writeln!(
        writer,
        "  • /history         : Show message count and turns"
    )?;
    writeln!(writer, "  • /help            : Show this help guide")?;
    writeln!(writer, "  • /exit or /quit   : Exit interactive chat\n")?;
    writer.flush()?;

    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        write!(writer, "grok-muse> ")?;
        writer.flush()?;

        let bytes_read = reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            writeln!(writer, "\nGoodbye!")?;
            break;
        }

        let input = line_buf.trim();
        if input.is_empty() {
            continue;
        }

        if input == "/exit" || input == "/quit" || input == "exit" || input == "quit" {
            writeln!(writer, "Exiting Grok-Muse chat. Goodbye!")?;
            break;
        }

        if input == "/help" {
            writeln!(writer, "\nAvailable REPL Commands:")?;
            writeln!(
                writer,
                "  /memory <query>    Search SQLite personal memory with FTS5"
            )?;
            writeln!(
                writer,
                "  /status            Display system permissions, helper socket, and spend"
            )?;
            writeln!(
                writer,
                "  /do <task>         Dispatch an inline computer-use action via AgentLoop"
            )?;
            writeln!(
                writer,
                "  /clear             Clear conversation context (preserves system prompt)"
            )?;
            writeln!(
                writer,
                "  /history           Display conversation turn count"
            )?;
            writeln!(writer, "  /help              Print this command guide")?;
            writeln!(writer, "  /exit, /quit       Exit REPL\n")?;
            writer.flush()?;
            continue;
        }

        if input == "/clear" {
            let sys = history.first().cloned();
            history.clear();
            if let Some(s) = sys {
                history.push(s);
            }
            writeln!(writer, "✔ Conversation history cleared.\n")?;
            writer.flush()?;
            continue;
        }

        if input == "/history" {
            let user_msgs = history.iter().filter(|m| m.role == "user").count();
            let asst_msgs = history.iter().filter(|m| m.role == "assistant").count();
            writeln!(
                writer,
                "Context History: {} messages total ({} user, {} assistant)",
                history.len(),
                user_msgs,
                asst_msgs
            )?;
            for (idx, msg) in history.iter().enumerate() {
                if idx == 0 && msg.role == "system" {
                    continue;
                }
                let preview: String = msg.content.chars().take(80).collect();
                let ellipsis = if msg.content.chars().count() > 80 {
                    "..."
                } else {
                    ""
                };
                let role = &msg.role;
                writeln!(writer, "  [{idx}] {role}: {preview}{ellipsis}")?;
            }
            writeln!(writer)?;
            writer.flush()?;
            continue;
        }

        if input == "/status" {
            let perms = check_all_permissions();
            let helper_active = Path::new(&config.socket_path).exists();
            let spend = budget.get_daily_spent();
            let cap = budget.config().daily_spend_cap_usd;
            writeln!(writer, "\n--- System Status Inspection ---")?;
            writeln!(
                writer,
                "  Screen Recording : {}",
                if perms.screen_recording {
                    "Granted"
                } else {
                    "Missing"
                }
            )?;
            writeln!(
                writer,
                "  Accessibility    : {}",
                if perms.accessibility {
                    "Granted"
                } else {
                    "Missing"
                }
            )?;
            writeln!(
                writer,
                "  Calendar         : {}",
                if perms.calendar { "Granted" } else { "Missing" }
            )?;
            writeln!(
                writer,
                "  Reminders        : {}",
                if perms.reminders {
                    "Granted"
                } else {
                    "Missing"
                }
            )?;
            writeln!(
                writer,
                "  CU Helper Socket : {}",
                if helper_active { "Active" } else { "Idle" }
            )?;
            writeln!(
                writer,
                "  Daily Spend      : ${:.4} / ${:.2} USD\n",
                spend, cap
            )?;
            writer.flush()?;
            continue;
        }

        if input == "/memory"
            || input.starts_with("/memory ")
            || input == "/mem"
            || input.starts_with("/mem ")
        {
            let query = if let Some(q) = input.strip_prefix("/memory") {
                q.trim()
            } else if let Some(q) = input.strip_prefix("/mem") {
                q.trim()
            } else {
                ""
            };

            if query.is_empty() {
                writeln!(writer, "Usage: /memory <search terms>\n")?;
                writer.flush()?;
                continue;
            }

            match memory.search(query, 5) {
                Ok(records) if records.is_empty() => {
                    writeln!(
                        writer,
                        "No matching memory records found for '{}'.\n",
                        query
                    )?;
                }
                Ok(records) => {
                    writeln!(
                        writer,
                        "\nFound {} memory record(s) for '{}':",
                        records.len(),
                        query
                    )?;
                    for r in &records {
                        let title = r.title.as_deref().unwrap_or("untitled");
                        writeln!(
                            writer,
                            "  • [{:?}] {} (Importance: {:.2}, Tags: [{}])",
                            r.kind,
                            title,
                            r.importance,
                            r.tags.join(", ")
                        )?;
                        writeln!(writer, "    {}", r.body)?;
                    }
                    writeln!(writer)?;
                }
                Err(e) => {
                    writeln!(writer, "Memory search error: {e}\n")?;
                }
            }
            writer.flush()?;
            continue;
        }

        if input == "/do"
            || input.starts_with("/do ")
            || input == "/task"
            || input.starts_with("/task ")
        {
            let task_instruction = if let Some(t) = input.strip_prefix("/do") {
                t.trim()
            } else if let Some(t) = input.strip_prefix("/task") {
                t.trim()
            } else {
                ""
            };

            if task_instruction.is_empty() {
                writeln!(writer, "Usage: /do <task instruction>\n")?;
                writer.flush()?;
                continue;
            }

            writeln!(
                writer,
                "==> Dispatching inline computer-use task: \"{}\"...",
                task_instruction
            )?;
            writer.flush()?;

            match run_do(task_instruction, config_path.clone()).await {
                Ok(()) => writeln!(
                    writer,
                    "✔ Task dispatched. Inspect its task history for completion.\n"
                )?,
                Err(error) => writeln!(writer, "✘ Task dispatch failed: {error}\n")?,
            }
            writer.flush()?;
            continue;
        }

        // Check for unrecognized slash commands
        if input.starts_with('/') {
            let cmd = input.split_whitespace().next().unwrap_or(input);
            writeln!(
                writer,
                "Unknown command: '{cmd}'. Type /help for available commands.\n"
            )?;
            writer.flush()?;
            continue;
        }

        // Regular conversational message
        // 1. Contextual memory retrieval
        let memory_context = match memory.search(input, 3) {
            Ok(recs) if !recs.is_empty() => {
                let context_lines: Vec<String> = recs
                    .iter()
                    .map(|r| format!("- ({}): {}", r.title.as_deref().unwrap_or("Fact"), r.body))
                    .collect();
                format!(
                    "\n[Relevant Personal Memory Context:\n{}]\n\n",
                    context_lines.join("\n")
                )
            }
            _ => String::new(),
        };

        let enriched_prompt = format!("{}{}", memory_context, input);
        history.push(ChatMessage::text("user", enriched_prompt));

        let (primary, fallback) = router.route(TaskType::Planning);
        match llm_client
            .execute_routed(&primary, fallback.as_ref(), &history)
            .await
        {
            Ok(resp) => {
                writeln!(writer, "\n{}\n", resp.content.trim())?;
                history.push(ChatMessage::text("assistant", resp.content.clone()));
                audit_log.log(
                    "chat_message",
                    Some(format!("{:?}", primary.provider).to_lowercase()),
                    Some(primary.model),
                    Some(resp.prompt_tokens + resp.completion_tokens),
                    Some(resp.estimated_cost_usd),
                    None,
                    serde_json::json!({ "user_input": input }),
                );
            }
            Err(e) => {
                history.pop();
                writeln!(writer, "\nError completing chat request: {}\n", e)?;
            }
        }
        writer.flush()?;
    }

    Ok(())
}

pub async fn run_daemon(config_path: Option<PathBuf>, task: Option<String>) -> Result<()> {
    let config = GmConfig::load(config_path.as_deref())?;

    info!("Starting gm-daemon...");
    let _ = ensure_helper_running(&config).await;
    let audit_log = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());
    let kill_switch = KillSwitch::new();
    let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let router = LlmRouter::new(config.providers.clone());

    let platform = Arc::new(IpcPlatformClient::new(&config.socket_path));
    let secrets = KeychainSecrets::default();
    let llm_backend = build_llm_backend(&secrets);
    let llm_client = Arc::new(HybridLlmClient::new(llm_backend, Arc::clone(&budget)));

    let memory = Arc::new(
        PersonalMemoryStore::default_store()
            .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap()),
    );

    let proactive_engine = Arc::new(
        ProactiveEngine::new(QuietGateConfig::default(), Arc::clone(&budget))
            .with_llm(Arc::clone(&llm_client), router.clone()),
    );

    proactive_engine.schedule_patrol(Arc::clone(&platform), 300);
    proactive_engine.schedule_memory_upkeep(Arc::clone(&memory), 3600, None);

    let tick_engine = Arc::clone(&proactive_engine);
    let tick_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            tick_engine.tick(chrono::Utc::now());
        }
    });

    let control_server = DaemonControlServer::new(
        config.clone(),
        Arc::clone(&budget),
        Arc::clone(&llm_client),
        router.clone(),
        kill_switch.clone(),
        blocklist.clone(),
        Arc::new(audit_log.clone()),
        Arc::clone(&platform),
    );

    let runtime_task = control_server.start_runtime()?;

    let companion_task = if config.openbot.enabled && config.openbot.companion_enabled {
        if let Some(sup) = control_server.openbot().cloned() {
            let bind_addr = config.openbot.companion_bind_addr.clone();
            let bridges_cfg = config.openbot.bridges.clone();
            let (token, slack_secret) = companion_web_credentials(&config)?;
            Some(tokio::spawn(async move {
                let bridge = gm_openbot::BridgeRouter::new(sup.clone(), bridges_cfg);
                let web = gm_openbot::CompanionWebServer::new(sup, bridge, token, slack_secret);
                match web.bind_and_serve(&bind_addr).await {
                    Ok((_addr, handle)) => {
                        let _ = handle.await;
                    }
                    Err(e) => {
                        warn!("OpenBot CompanionWebServer failed to bind on {bind_addr}: {e}");
                    }
                }
            }))
        } else {
            None
        }
    } else {
        None
    };

    let task_supervisor = control_server
        .openbot()
        .cloned()
        .ok_or_else(|| GmError::Config("Task runtime unavailable".into()))?;

    let server_task = tokio::spawn(async move {
        if let Err(e) = control_server.run().await {
            warn!("Daemon control server stopped: {e}");
        }
    });

    if let Some(task_instruction) = task {
        info!("Running one-shot task: {}", task_instruction);
        let queued = task_supervisor.submit_task(None, Some("general"), &task_instruction, 0)?;
        match wait_for_queued_task(&task_supervisor, &queued.id).await {
            Ok(summary) => {
                println!("Task completed: {summary}");
            }
            Err(e) => {
                eprintln!("Task failed: {e}");
                if let Some(ct) = companion_task {
                    ct.abort();
                }
                server_task.abort();
                runtime_task.abort();
                tick_task.abort();
                return Err(e);
            }
        }
    } else {
        info!("gm-daemon running in background mode.");
        tokio::signal::ctrl_c().await.ok();
    }

    if let Some(ct) = companion_task {
        ct.abort();
    }
    server_task.abort();
    runtime_task.abort();
    tick_task.abort();
    Ok(())
}

pub async fn run_service(action: ServiceSubcommand) -> Result<()> {
    match action {
        ServiceSubcommand::Install { bin_dir } => service::install_services(bin_dir.as_deref()),
        ServiceSubcommand::Uninstall => service::uninstall_services(),
        ServiceSubcommand::Status => service::show_services_status(),
    }
}

pub async fn run_dogfood(action: Option<DogfoodSubcommand>, json: bool) -> Result<()> {
    let spend_cap = GmConfig::default().budget.daily_spend_cap_usd;
    let tracker = gm_goals::DogfoodTracker::default_tracker(spend_cap)?;
    match action {
        Some(DogfoodSubcommand::Rate { notice_id, useful }) => {
            tracker.record_rating(&notice_id, useful, chrono::Utc::now())?;
            println!(
                "✔ Notice '{}' rated as {}",
                notice_id,
                if useful { "Useful" } else { "Not Useful" }
            );
        }
        Some(DogfoodSubcommand::Feedback {
            notice_id,
            topic,
            action,
        }) => {
            let act = action.to_lowercase();
            let useful = if act == "stop" {
                Some(false)
            } else {
                Some(true)
            };
            tracker.record_feedback(&notice_id, &topic, &act, useful, chrono::Utc::now())?;
            println!(
                "✔ Recorded feedback '{}' for notice '{}' in topic '{}'",
                action, notice_id, topic
            );
        }
        Some(DogfoodSubcommand::Summary) | None => {
            if json {
                let metrics = tracker.compute_metrics();
                println!("{}", serde_json::to_string_pretty(&metrics)?);
            } else {
                print!("{}", tracker.format_report());
            }
        }
    }
    Ok(())
}

pub async fn run_stats(json: bool) -> Result<()> {
    let config = GmConfig::load(None)?;
    let spend_cap = config.budget.daily_spend_cap_usd;
    let tracker = gm_goals::DogfoodTracker::default_tracker(spend_cap)?;
    let metrics = tracker.compute_metrics();
    let perms = check_all_permissions();
    let helper_socket = &config.socket_path;

    if json {
        let val = serde_json::json!({
            "permissions": {
                "screen_recording": perms.screen_recording,
                "accessibility": perms.accessibility,
                "calendar": perms.calendar,
                "reminders": perms.reminders,
                "all_granted": perms.all_granted(),
            },
            "helper_connected": helper_socket.exists(),
            "dogfood_metrics": metrics,
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
    } else {
        run_status().await?;
        print!("{}", tracker.format_report());
    }
    Ok(())
}

pub async fn run_review(date_str: Option<String>) -> Result<()> {
    let now = chrono::Utc::now();
    let target_time = if let Some(ref ds) = date_str {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(ds, "%Y-%m-%d") {
            d.and_hms_opt(21, 0, 0)
                .map(|dt| dt.and_utc())
                .unwrap_or(now)
        } else {
            now
        }
    } else {
        now
    };
    let date = target_time.format("%Y-%m-%d").to_string();
    println!(
        "==> Running Grok-Muse Nightly Review synthesis for {}...",
        date
    );

    let config = GmConfig::load(None)?;
    let audit = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());
    let memory = PersonalMemoryStore::default_store()
        .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap());
    let spend_cap = config.budget.daily_spend_cap_usd;
    let tracker = gm_goals::DogfoodTracker::default_tracker(spend_cap)?;
    let metrics = tracker.compute_metrics();

    let secrets = KeychainSecrets::default();
    let llm_backend = build_llm_backend(&secrets);
    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let llm_client = HybridLlmClient::new(llm_backend, budget);
    let router = LlmRouter::new(config.providers.clone());
    let llm_ctx = Some((&llm_client, &router));

    let socket_path = &config.socket_path;
    let review = if socket_path.exists() {
        let platform = IpcPlatformClient::new(socket_path);
        gm_goals::run_nightly_review_with_llm(
            Some(&platform),
            &audit,
            &memory,
            Some(&metrics),
            llm_ctx,
            target_time,
        )
        .await?
    } else {
        #[cfg(target_os = "macos")]
        {
            let platform = gm_platform::MacPlatform::new().ok();
            gm_goals::run_nightly_review_with_llm(
                platform.as_ref(),
                &audit,
                &memory,
                Some(&metrics),
                llm_ctx,
                target_time,
            )
            .await?
        }
        #[cfg(not(target_os = "macos"))]
        {
            let platform: Option<&gm_platform::MockPlatform> = None;
            gm_goals::run_nightly_review_with_llm(
                platform,
                &audit,
                &memory,
                Some(&metrics),
                llm_ctx,
                target_time,
            )
            .await?
        }
    };

    println!("\n{}", review.review_markdown);
    Ok(())
}

pub async fn run_audit(period_str: &str, json: bool) -> Result<()> {
    let period = period_str
        .parse::<gm_safety::AuditPeriod>()
        .map_err(GmError::Config)?;
    let logger = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());
    let report = logger.generate_report(period, chrono::Utc::now());

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.format_report());
    }
    Ok(())
}

pub async fn run_upkeep(stale_days: Option<i64>, json: bool) -> Result<()> {
    let memory = PersonalMemoryStore::default_store()
        .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap());
    let mut config = gm_memory::UpkeepConfig::default();
    if let Some(days) = stale_days {
        config.stale_observation_days = days;
    }
    let report = memory.perform_upkeep(&config, chrono::Utc::now())?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.format_summary());
    }
    Ok(())
}

pub async fn run_feed(limit: usize, json: bool) -> Result<()> {
    let memory = PersonalMemoryStore::default_store()
        .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap());
    let audit = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());

    let engine = ProactiveEngine::new(
        QuietGateConfig::default(),
        Arc::new(BudgetManager::new(GmConfig::default().budget)),
    );
    let feed = engine.get_feed(&memory, &audit, limit)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&feed)?);
    } else {
        print!("{}", gm_goals::format_feed(&feed));
    }
    Ok(())
}

pub async fn run_ideas(json: bool) -> Result<()> {
    let memory = PersonalMemoryStore::default_store()
        .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap());
    let config = GmConfig::load(None)?;
    let secrets = KeychainSecrets::default();
    let llm_backend = build_llm_backend(&secrets);
    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let llm_client = HybridLlmClient::new(llm_backend, budget);
    let router = LlmRouter::new(config.providers.clone());
    let llm_ctx = Some((&llm_client, &router));
    let socket_path = &config.socket_path;

    let ideas = if socket_path.exists() {
        let platform = IpcPlatformClient::new(socket_path);
        gm_goals::generate_ideas_with_llm(Some(&platform), &memory, llm_ctx, chrono::Utc::now())
            .await?
    } else {
        #[cfg(target_os = "macos")]
        {
            let platform = gm_platform::MacPlatform::new().ok();
            gm_goals::generate_ideas_with_llm(
                platform.as_ref(),
                &memory,
                llm_ctx,
                chrono::Utc::now(),
            )
            .await?
        }
        #[cfg(not(target_os = "macos"))]
        {
            let platform: Option<&gm_platform::MockPlatform> = None;
            gm_goals::generate_ideas_with_llm(platform, &memory, llm_ctx, chrono::Utc::now())
                .await?
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&ideas)?);
    } else {
        print!("{}", gm_goals::format_ideas(&ideas));
    }
    Ok(())
}

pub async fn run_models(action: Option<ModelsSubcommand>, json: bool) -> Result<()> {
    let registry = gm_vlm_candle::ModelRegistry::default_registry();
    run_models_with_registry(&registry, action, json).await
}

pub async fn run_models_with_registry(
    registry: &gm_vlm_candle::ModelRegistry,
    action: Option<ModelsSubcommand>,
    json: bool,
) -> Result<()> {
    match action.unwrap_or(ModelsSubcommand::List) {
        ModelsSubcommand::List => {
            let models = registry.list_models();
            if json {
                println!("{}", serde_json::to_string_pretty(&models)?);
            } else {
                println!("Local VLM Directory: {}\n", registry.models_dir().display());
                print!("{}", gm_vlm_candle::format_models_table(&models));
            }
        }
        ModelsSubcommand::Pull { model } => {
            let target = gm_vlm_candle::ModelRegistry::resolve_catalog_entry(model.as_deref())?;
            if !json {
                println!(
                    "==> Pulling local VLM '{}' ({}) into {}...",
                    target.model_id,
                    target.architecture,
                    registry.model_dir_for(&target.model_id).display()
                );
            }
            let manifest = registry.pull_model(model.as_deref(), None).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&manifest)?);
            } else {
                println!(
                    "==> Successfully installed '{}' ({:.2} MB, {} files)",
                    manifest.model_id,
                    (manifest.total_bytes as f64) / (1024.0 * 1024.0),
                    manifest.files.len()
                );
            }
        }
        ModelsSubcommand::Rm { model } => {
            let removed_dir = registry.remove_model(&model)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "removed": model,
                        "path": removed_dir,
                    }))?
                );
            } else {
                println!("==> Removed local VLM at {}", removed_dir.display());
            }
        }
    }
    Ok(())
}

fn open_default_openbot_supervisor(
    config_path: Option<PathBuf>,
) -> Result<gm_openbot::OpenBotSupervisor> {
    let config = GmConfig::load(config_path.as_deref())?;
    let audit =
        Arc::new(AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory()));
    let (event_tx, _) = tokio::sync::broadcast::channel(64);
    gm_openbot::OpenBotSupervisor::open(config, audit, event_tx)
}

pub async fn run_bots(
    action: Option<BotsSubcommand>,
    json: bool,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let sup = open_default_openbot_supervisor(config_path)?;
    match action.unwrap_or(BotsSubcommand::List) {
        BotsSubcommand::List => {
            let bots = sup.roster().list_bots();
            if json {
                println!("{}", serde_json::to_string_pretty(&bots)?);
            } else {
                println!("OpenBot Roster ({} bots):", bots.len());
                for b in bots {
                    println!(
                        "  • {:<12} {:<20} [{}] (Shell: {}, GUI: {})",
                        b.id, b.display_name, b.role_title, b.allow_shell, b.allow_gui
                    );
                }
            }
        }
        BotsSubcommand::Show { id } => {
            let bot = sup
                .roster()
                .get_bot(&id)
                .ok_or_else(|| GmError::Config(format!("Unknown bot: {id}")))?;
            let sandbox = sup.roster().workspace_sandbox(&id)?;
            let val = serde_json::json!({
                "bot": bot,
                "workspace_dir": sandbox.workspace_dir(),
                "inbox_dir": sandbox.inbox_dir(),
                "outbox_dir": sandbox.outbox_dir(),
            });
            println!("{}", serde_json::to_string_pretty(&val)?);
        }
        BotsSubcommand::Create {
            id,
            name,
            role,
            prompt,
        } => {
            let profile = gm_openbot::BotProfile {
                id: id.clone(),
                display_name: name,
                avatar_emoji: "🤖".into(),
                role_title: role,
                system_prompt: prompt,
                preferred_provider: None,
                preferred_model: None,
                allow_gui: false,
                allow_shell: true,
                allow_network: true,
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            sup.roster().upsert_bot(profile.clone())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&profile)?);
            } else {
                println!("✔ Created/updated bot '{id}'");
            }
        }
        BotsSubcommand::Rm { id } => {
            sup.roster().remove_bot(&id)?;
            println!("✔ Removed bot '{id}'");
        }
    }
    Ok(())
}

pub async fn run_queue(
    action: Option<QueueSubcommand>,
    json: bool,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let sup = open_default_openbot_supervisor(config_path)?;
    match action.unwrap_or(QueueSubcommand::List { bot: None }) {
        QueueSubcommand::List { bot } => {
            let tasks = sup.queue().list_tasks(bot.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else {
                println!("OpenBot Durable Queue ({} tasks):", tasks.len());
                for t in tasks {
                    println!(
                        "  • {} [{:?}] @{:<10} (prio {}): {}",
                        t.id, t.status, t.bot_id, t.priority, t.instruction
                    );
                }
            }
        }
        QueueSubcommand::Add {
            bot,
            instruction,
            priority,
        } => {
            let task = sup.submit_task(Some(&bot), None, &instruction, priority)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("✔ Enqueued task {} for @{}", task.id, task.bot_id);
            }
        }
        QueueSubcommand::Pause { id } => {
            sup.control_task(&id, &gm_core::ipc::QueueControlAction::Pause)?;
            println!("✔ Paused task {id}");
        }
        QueueSubcommand::Resume { id, note } => {
            if note.is_some() {
                sup.queue().resume_from_human(&id, note.as_deref())?;
            } else {
                sup.control_task(&id, &gm_core::ipc::QueueControlAction::Resume)?;
            }
            println!("✔ Resumed task {id}");
        }
        QueueSubcommand::Cancel { id } => {
            sup.control_task(&id, &gm_core::ipc::QueueControlAction::Cancel)?;
            println!("✔ Cancelled task {id}");
        }
        QueueSubcommand::Reassign { id, bot } => {
            sup.control_task(
                &id,
                &gm_core::ipc::QueueControlAction::Reassign { target_bot_id: bot },
            )?;
            println!("✔ Reassigned task {id}");
        }
        QueueSubcommand::Backup { path } => {
            let count = sup.queue().export_backup_jsonl(&path)?;
            println!("✔ Exported {count} tasks to {}", path.display());
        }
        QueueSubcommand::Restore { path } => {
            let count = sup.queue().import_backup_jsonl(&path)?;
            println!("✔ Restored {count} tasks from {}", path.display());
        }
    }
    Ok(())
}

pub async fn run_channels(
    action: Option<ChannelsSubcommand>,
    json: bool,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let sup = open_default_openbot_supervisor(config_path)?;
    match action.unwrap_or(ChannelsSubcommand::List) {
        ChannelsSubcommand::List => {
            let channels = sup.channels().list_channels(false)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&channels)?);
            } else {
                println!("OpenBot Channels ({} active):", channels.len());
                for c in channels {
                    println!(
                        "  • {:<12} {:<24} (owner: @{}, paused: {})",
                        c.id,
                        c.name,
                        c.owner_bot_id.as_deref().unwrap_or("none"),
                        c.paused
                    );
                }
            }
        }
        ChannelsSubcommand::Post {
            channel,
            message,
            author,
        } => {
            let (msg, spawned) = sup.post_channel_message(&channel, &author, None, &message)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "message": msg,
                        "spawned_tasks": spawned,
                    }))?
                );
            } else {
                println!(
                    "✔ Posted to {} (mentions: {:?}, spawned: {})",
                    msg.channel_id,
                    msg.mentioned_bots,
                    spawned.len()
                );
            }
        }
        ChannelsSubcommand::History { channel, limit } => {
            let msgs = sup.channels().list_messages(&channel, limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&msgs)?);
            } else {
                for m in msgs {
                    println!("[{}] {}: {}", m.channel_id, m.author, m.content);
                }
            }
        }
        ChannelsSubcommand::Assign { channel, bot } => {
            sup.channels().reassign_owner(&channel, &bot)?;
            println!("✔ Assigned channel {channel} owner to @{bot}");
        }
        ChannelsSubcommand::Archive { channel } => {
            sup.channels().set_archived(&channel, true)?;
            println!("✔ Archived channel {channel}");
        }
        ChannelsSubcommand::Restore { channel } => {
            sup.channels().set_archived(&channel, false)?;
            println!("✔ Restored channel {channel}");
        }
    }
    Ok(())
}

fn companion_web_credentials(config: &GmConfig) -> Result<(Option<String>, Option<String>)> {
    let read = |name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let token = if let Some(token) = read("GROK_MUSE_COMPANION_TOKEN") {
        token
    } else {
        let path = config.storage_dir.join("companion-token");
        match std::fs::read_to_string(&path) {
            Ok(token) if !token.trim().is_empty() => token.trim().to_string(),
            Ok(_) => {
                return Err(GmError::Config(
                    "Companion token file is empty; restore or remove it to pair again".into(),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let token = format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                );
                gm_core::storage::atomic_write_private(&path, token.as_bytes())?;
                info!(path=%path.display(),"Companion pairing token created");
                token
            }
            Err(e) => return Err(e.into()),
        }
    };
    Ok((Some(token), read("GROK_MUSE_SLACK_SIGNING_SECRET")))
}

pub async fn run_serve(bind: &str, config_path: Option<PathBuf>) -> Result<()> {
    let config = GmConfig::load(config_path.as_deref())?;
    let sup = open_default_openbot_supervisor(config_path)?;
    println!("==> Starting OpenBot Companion Web / AG-UI / A2A server on http://{bind} ...");
    let secrets = KeychainSecrets::default();
    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let llm = Arc::new(HybridLlmClient::new(build_llm_backend(&secrets), budget));
    let runtime = sup.start_runtime(
        Arc::new(IpcPlatformClient::new(&config.socket_path)),
        llm,
        LlmRouter::new(config.providers.clone()),
        KillSwitch::new(),
        BlocklistManager::new(&config.safety.blocklisted_apps),
        None,
    )?;
    let bridge = gm_openbot::BridgeRouter::new(sup.clone(), config.openbot.bridges.clone());
    let (token, slack_secret) = companion_web_credentials(&config)?;
    let web = gm_openbot::CompanionWebServer::new(sup, bridge, token, slack_secret);
    let (_addr, handle) = match web.bind_and_serve(bind).await {
        Ok(server) => server,
        Err(error) => {
            runtime.abort();
            return Err(error);
        }
    };
    tokio::select! { _ = handle => {}, _ = tokio::signal::ctrl_c() => {} }
    runtime.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::ipc::DaemonEvent;

    static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn test_permissions_check_live() {
        let res = run_permissions(false, None, false).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_permissions_json_format() {
        let res = run_permissions(false, None, true).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_onboard_check_only_mode() {
        let res = run_onboard(true).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_status_command() {
        let res = run_status().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_kill_command() {
        let res = run_kill().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_permissions_open_pane_validation() {
        // Valid panes
        assert!(
            run_permissions(false, Some("Privacy_Calendars".into()), false)
                .await
                .is_ok()
        );
        assert!(
            run_permissions(false, Some("screen".into()), false)
                .await
                .is_ok()
        );
        assert!(
            run_permissions(false, Some("ax".into()), false)
                .await
                .is_ok()
        );
        // Invalid pane returns error
        assert!(
            run_permissions(false, Some("nonexistent_pane".into()), false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_service_commands() {
        let res = run_service(ServiceSubcommand::Status).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_dogfood_commands() {
        // Summary
        assert!(run_dogfood(None, false).await.is_ok());
        assert!(run_dogfood(None, true).await.is_ok());
        // Feedback and Rate
        assert!(
            run_dogfood(
                Some(DogfoodSubcommand::Rate {
                    notice_id: "test_n1".into(),
                    useful: true,
                }),
                false
            )
            .await
            .is_ok()
        );
        assert!(
            run_dogfood(
                Some(DogfoodSubcommand::Feedback {
                    notice_id: "test_n1".into(),
                    topic: "calendar".into(),
                    action: "done".into(),
                }),
                false
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn test_stats_command() {
        assert!(run_stats(false).await.is_ok());
        assert!(run_stats(true).await.is_ok());
    }

    #[tokio::test]
    async fn test_review_command() {
        assert!(run_review(Some("2026-11-03".into())).await.is_ok());
        assert!(run_review(None).await.is_ok());
    }

    #[tokio::test]
    async fn test_audit_command() {
        assert!(run_audit("weekly", false).await.is_ok());
        assert!(run_audit("weekly", true).await.is_ok());
        assert!(run_audit("monthly", false).await.is_ok());
        assert!(run_audit("invalid_period", false).await.is_err());
    }

    #[tokio::test]
    async fn test_upkeep_command() {
        assert!(run_upkeep(None, false).await.is_ok());
        assert!(run_upkeep(Some(30), true).await.is_ok());
    }

    #[tokio::test]
    async fn test_feed_command() {
        assert!(run_feed(10, false).await.is_ok());
        assert!(run_feed(5, true).await.is_ok());
    }

    #[tokio::test]
    async fn test_ideas_command() {
        assert!(run_ideas(false).await.is_ok());
        assert!(run_ideas(true).await.is_ok());
    }

    #[tokio::test]
    async fn test_audit_single_task_done_logging() {
        use gm_core::config::{BudgetConfig, ProvidersConfig};
        let audit = Arc::new(AuditLogger::in_memory());
        let mock_platform = Arc::new(gm_platform::MockPlatform::new());
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(
            Arc::new(MockLlmBackend::new()),
            budget,
        ));
        let router = LlmRouter::new(ProvidersConfig::default());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::new(&[]);

        let agent = AgentLoop::new(
            mock_platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(gm_agent::AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        })
        .with_audit(audit.clone());

        let res = agent.run_task("test_t1", "Click test").await;
        assert!(res.is_ok());

        let entries = audit.entries();
        let done_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.event_type == "task_done")
            .collect();
        // Crucial verification: task_done is logged EXACTLY ONCE
        assert_eq!(done_entries.len(), 1);
    }

    #[tokio::test]
    async fn test_chat_repl_commands_and_interaction() {
        let _env_guard = ENV_MUTEX.lock().await;
        let input = "/help\n/status\n/history\n/clear\n/memory rust\nHello assistant!\n/exit\n";
        let reader = std::io::Cursor::new(input);
        let mut output = Vec::new();

        let res = run_chat_loop(reader, &mut output, None, None).await;
        assert!(res.is_ok());

        let out_str = String::from_utf8_lossy(&output);
        assert!(out_str.contains("Grok-Muse Interactive Terminal REPL"));
        assert!(out_str.contains("Available REPL Commands:"));
        assert!(out_str.contains("System Status Inspection"));
        assert!(out_str.contains("Context History:"));
        assert!(out_str.contains("Conversation history cleared."));
        assert!(out_str.contains("Exiting Grok-Muse chat. Goodbye!"));
    }

    #[tokio::test]
    async fn test_chat_repl_edge_cases_unknown_and_boundaries() {
        let _env_guard = ENV_MUTEX.lock().await;
        unsafe {
            std::env::set_var("GM_FORCE_MOCK_LLM", "1");
        }
        let input = "   \n/unknown_cmd\n/document code\n/members in team\n/do\n/memory\nWhat is your role?\n/history\n/quit\n";
        let reader = std::io::Cursor::new(input);
        let mut output = Vec::new();

        let res = run_chat_loop(reader, &mut output, None, None).await;
        unsafe {
            std::env::remove_var("GM_FORCE_MOCK_LLM");
        }
        assert!(res.is_ok());

        let out_str = String::from_utf8_lossy(&output);
        assert!(out_str.contains("Unknown command: '/unknown_cmd'"));
        assert!(out_str.contains("Unknown command: '/document'"));
        assert!(out_str.contains("Unknown command: '/members'"));
        assert!(out_str.contains("Usage: /do <task instruction>"));
        assert!(out_str.contains("Usage: /memory <search terms>"));
        assert!(out_str.contains("[1] user:"));
        assert!(out_str.contains("[2] assistant:"));
        assert!(out_str.contains("Exiting Grok-Muse chat. Goodbye!"));
    }

    #[tokio::test]
    async fn test_update_command_error_and_guidance() {
        // Test update command with invalid repo returns appropriate error
        let res = run_update(
            true,
            false,
            Some("invalid-org/nonexistent-repo-999".into()),
            true,
            None,
        )
        .await;
        assert!(res.is_err());

        // Test installation guidance formatting does not panic
        let dummy_dmg = std::path::Path::new("/tmp/test/Grok-Muse.dmg");
        let result = UpdateCheckResult {
            current_version: "0.1.0".into(),
            latest_version: "0.2.0".into(),
            update_available: true,
            release_tag: "v0.2.0".into(),
            release_name: Some("v0.2.0".into()),
            release_url: "https://github.com/grok-muse/grok-muse/releases/tag/v0.2.0".into(),
            dmg_asset_name: Some("Grok-Muse.dmg".into()),
            dmg_download_url: Some("https://example.com/Grok-Muse.dmg".into()),
            dmg_size_bytes: Some(1024),
            checksum_download_url: None,
            expected_sha256: None,
            published_at: None,
            release_notes: Some("Release notes".into()),
        };
        print_installation_guidance(dummy_dmg, &result);
    }

    #[tokio::test]
    async fn test_build_llm_backend_scenarios() {
        let _env_guard = ENV_MUTEX.lock().await;

        struct EnvCleaner;
        impl Drop for EnvCleaner {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("GM_FORCE_MOCK_LLM");
                    std::env::remove_var("META_API_KEY");
                    std::env::remove_var("XAI_API_KEY");
                    std::env::remove_var("MUSE_API_KEY");
                    std::env::remove_var("GROK_API_KEY");
                }
            }
        }
        let _cleaner = EnvCleaner;

        let secrets = KeychainSecrets::new("co.anysphere.grok-muse.test");

        // Scenario 1: GM_FORCE_MOCK_LLM forces mock even if keys exist
        unsafe {
            std::env::set_var("GM_FORCE_MOCK_LLM", "1");
            std::env::set_var("META_API_KEY", "test-meta-key");
        }
        let backend = build_llm_backend(&secrets);
        assert!(backend.is_mock());

        // Scenario 2: Live key with GM_FORCE_MOCK_LLM unset returns live backend
        unsafe {
            std::env::remove_var("GM_FORCE_MOCK_LLM");
        }
        let backend_live = build_llm_backend(&secrets);
        assert!(!backend_live.is_mock());

        // Scenario 3: Missing credentials cannot silently simulate successful work
        unsafe {
            std::env::remove_var("META_API_KEY");
            std::env::remove_var("XAI_API_KEY");
            std::env::remove_var("MUSE_API_KEY");
            std::env::remove_var("GROK_API_KEY");
        }
        let backend_empty = build_llm_backend(&secrets);
        assert!(!backend_empty.is_mock());
        assert!(
            backend_empty
                .complete(
                    &LlmRouter::new(gm_core::config::ProvidersConfig::default())
                        .route(TaskType::Planning)
                        .0,
                    &[]
                )
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_daemon_control_server_streaming_and_protocol() {
        let short_id = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let daemon_socket = std::path::PathBuf::from(format!("/tmp/gmd_{}.sock", short_id));
        let helper_socket = std::path::PathBuf::from(format!("/tmp/gmh_{}.sock", short_id));

        let runtime_dir = std::env::temp_dir().join(format!("gm-runtime-test-{short_id}"));
        let config = GmConfig {
            storage_dir: runtime_dir.clone(),
            openbot: gm_core::config::OpenBotConfig {
                bots_dir: runtime_dir.join("bots"),
                ..Default::default()
            },
            safety: gm_core::config::SafetyConfig {
                allow_host_desktop: false,
                ..Default::default()
            },
            daemon_socket_path: daemon_socket.clone(),
            socket_path: helper_socket.clone(),
            ..Default::default()
        };

        let budget = Arc::new(BudgetManager::new(config.budget.clone()));
        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend.queue_response("Mock LLM response for test");
        mock_backend
            .queue_response(r#"{"action":"Done","params":{"summary":"Protocol task completed"}}"#);
        let llm_client = Arc::new(HybridLlmClient::new(mock_backend, Arc::clone(&budget)));
        let router = LlmRouter::new(config.providers.clone());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
        let audit_log = Arc::new(AuditLogger::in_memory());
        let platform = Arc::new(IpcPlatformClient::new(&helper_socket));

        let server = DaemonControlServer::new(
            config.clone(),
            Arc::clone(&budget),
            Arc::clone(&llm_client),
            router,
            kill_switch.clone(),
            blocklist,
            audit_log,
            platform,
        );

        let runtime = server.start_runtime().unwrap();
        let srv = server.clone();
        let srv_handle = tokio::spawn(async move {
            let _ = srv.run().await;
        });

        // Wait for socket to bind
        let mut connected = false;
        for _ in 0..40 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            #[cfg(unix)]
            if daemon_socket.exists() {
                connected = true;
                break;
            }
        }
        assert!(connected, "Daemon socket did not bind in time");

        // Verify 0o600 permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&daemon_socket).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "Socket must have 0o600 permissions");
        }

        // Test 1: Ping / Pong
        let mut stream = tokio::net::UnixStream::connect(&daemon_socket)
            .await
            .unwrap();
        let ping_req = IpcMessage {
            id: 1,
            payload: DaemonRequest::Ping,
        };
        write_ipc_message(&mut stream, &ping_req).await.unwrap();
        let ping_resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        assert_eq!(ping_resp.id, 1);
        assert!(matches!(ping_resp.payload, DaemonResponse::Pong));

        // Test 2: GetStatus
        let status_req = IpcMessage {
            id: 2,
            payload: DaemonRequest::GetStatus,
        };
        write_ipc_message(&mut stream, &status_req).await.unwrap();
        let status_resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        assert_eq!(status_resp.id, 2);
        match status_resp.payload {
            DaemonResponse::Status { paused, .. } => assert!(!paused),
            other => panic!("Expected Status, got {:?}", other),
        }

        // Test 3: SetPaused
        let pause_req = IpcMessage {
            id: 3,
            payload: DaemonRequest::SetPaused { paused: true },
        };
        write_ipc_message(&mut stream, &pause_req).await.unwrap();
        let pause_resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        assert_eq!(pause_resp.id, 3);
        assert!(matches!(pause_resp.payload, DaemonResponse::Ack));

        // Test 4: Chat Message
        let chat_req = IpcMessage {
            id: 4,
            payload: DaemonRequest::SendChatMessage {
                text: "Hello".into(),
            },
        };
        write_ipc_message(&mut stream, &chat_req).await.unwrap();
        let chat_resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        assert_eq!(chat_resp.id, 4);
        match chat_resp.payload {
            DaemonResponse::ChatReply { text } => assert!(text.contains("Mock LLM")),
            other => panic!("Expected ChatReply, got {:?}", other),
        }

        // Test 5: SubscribeEvents
        let mut sub_stream = tokio::net::UnixStream::connect(&daemon_socket)
            .await
            .unwrap();
        let sub_req = IpcMessage {
            id: 5,
            payload: DaemonRequest::SubscribeEvents,
        };
        write_ipc_message(&mut sub_stream, &sub_req).await.unwrap();
        let sub_ack: IpcMessage<DaemonResponse> = read_ipc_message(&mut sub_stream).await.unwrap();
        assert_eq!(sub_ack.id, 5);
        assert!(matches!(sub_ack.payload, DaemonResponse::Ack));

        // Trigger an event via SetPaused and verify event stream receives it
        let unpause_req = IpcMessage {
            id: 6,
            payload: DaemonRequest::SetPaused { paused: false },
        };
        write_ipc_message(&mut stream, &unpause_req).await.unwrap();
        let _ = read_ipc_message::<_, IpcMessage<DaemonResponse>>(&mut stream)
            .await
            .unwrap();

        let event_frame: IpcMessage<DaemonResponse> =
            read_ipc_message(&mut sub_stream).await.unwrap();
        match event_frame.payload {
            DaemonResponse::Event(DaemonEvent::HudStateChanged { state, .. }) => {
                assert_eq!(state, "Idle");
            }
            other => panic!("Expected HudStateChanged event, got {:?}", other),
        }

        server.openbot().unwrap().set_runtime_paused(false).unwrap();
        // Test 6: SubmitTask & Task Event Streaming
        let task_req = IpcMessage {
            id: 7,
            payload: DaemonRequest::SubmitTask {
                instruction: "Click Safari icon".into(),
            },
        };
        write_ipc_message(&mut stream, &task_req).await.unwrap();
        let task_resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        assert_eq!(task_resp.id, 7);
        let spawned_task_id = match task_resp.payload {
            DaemonResponse::Accepted { id } => {
                assert!(id.starts_with("task-"));
                id
            }
            other => panic!("Expected Accepted, got {:?}", other),
        };

        let mut got_task_progress = false;
        let progress_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < progress_deadline {
            if let Ok(Ok(frame)) = tokio::time::timeout(
                tokio::time::Duration::from_millis(500),
                read_ipc_message::<_, IpcMessage<DaemonResponse>>(&mut sub_stream),
            )
            .await
            {
                match frame.payload {
                    DaemonResponse::Event(DaemonEvent::TaskProgress { task_id, .. }) => {
                        assert_eq!(task_id, spawned_task_id);
                        got_task_progress = true;
                        break;
                    }
                    DaemonResponse::Event(DaemonEvent::HudStateChanged { .. }) => {}
                    _ => {}
                }
            }
        }
        assert!(
            got_task_progress,
            "Event stream must deliver TaskProgress event"
        );

        // Test 7: RespondApproval
        let (app_tx, app_rx) = tokio::sync::oneshot::channel();
        let test_req_id = "test-approval-123".to_string();
        server
            .register_approval_request(test_req_id.clone(), app_tx)
            .await;

        let app_req = IpcMessage {
            id: 8,
            payload: DaemonRequest::RespondApproval {
                request_id: test_req_id,
                approved: true,
            },
        };
        write_ipc_message(&mut stream, &app_req).await.unwrap();
        let app_resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        assert_eq!(app_resp.id, 8);
        assert!(matches!(app_resp.payload, DaemonResponse::Ack));
        assert!(app_rx.await.unwrap());

        // Test 7: EmergencyKill
        let kill_req = IpcMessage {
            id: 9,
            payload: DaemonRequest::EmergencyKill,
        };
        write_ipc_message(&mut stream, &kill_req).await.unwrap();
        let kill_resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        assert_eq!(kill_resp.id, 9);
        assert!(matches!(kill_resp.payload, DaemonResponse::Killed));
        assert!(kill_switch.is_triggered());

        srv_handle.abort();
        runtime.abort();
        let _ = runtime.await;
        let _ = std::fs::remove_file(&daemon_socket);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_daemon_end_to_end_approval_requested_and_respond() {
        let short_id = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let daemon_socket = std::path::PathBuf::from(format!("/tmp/gmda_{}.sock", short_id));
        let helper_socket = std::path::PathBuf::from(format!("/tmp/gmha_{}.sock", short_id));

        let runtime_dir = std::env::temp_dir().join(format!("gm-runtime-test-{short_id}"));
        let config = GmConfig {
            storage_dir: runtime_dir.clone(),
            openbot: gm_core::config::OpenBotConfig {
                bots_dir: runtime_dir.join("bots"),
                ..Default::default()
            },
            safety: gm_core::config::SafetyConfig {
                allow_host_desktop: true,
                ..Default::default()
            },
            daemon_socket_path: daemon_socket.clone(),
            socket_path: helper_socket.clone(),
            ..Default::default()
        };

        // Spawn mock helper server
        let mock_platform = gm_platform::MockPlatform::new();
        mock_platform.set_snapshot(gm_core::types::AxSnapshot {
            window_id: 1,
            app_name: "MailApp".into(),
            window_title: "Compose".into(),
            bounds: gm_core::types::Rect::new(0.0, 0.0, 800.0, 600.0),
            elements: vec![gm_core::types::AxElement {
                token: "send_btn".into(),
                role: "AXButton".into(),
                label: Some("Send Email".into()),
                value: None,
                frame: gm_core::types::Rect::new(100.0, 100.0, 80.0, 30.0),
                is_enabled: true,
                is_secure: false,
            }],
            timestamp_ms: 100,
        });
        let helper_server = gm_cu_helper::HelperServer::new(mock_platform);
        let h_sock = helper_socket.clone();
        let helper_handle = tokio::spawn(async move {
            let _ = helper_server.run(h_sock).await;
        });

        let budget = Arc::new(BudgetManager::new(config.budget.clone()));
        let mock_backend = Arc::new(MockLlmBackend::new());
        // Step 1: Irreversible click action requiring confirmation
        mock_backend.queue_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"send_btn\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );
        // Step 2: Done
        mock_backend.queue_response(
            "{\"action\":\"Done\",\"params\":{\"summary\":\"Confirmed and done\"}}",
        );

        let llm_client = Arc::new(HybridLlmClient::new(mock_backend, Arc::clone(&budget)));
        let router = LlmRouter::new(config.providers.clone());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
        let audit_log = Arc::new(AuditLogger::in_memory());
        let platform = Arc::new(IpcPlatformClient::new(&helper_socket));

        let server = DaemonControlServer::new(
            config.clone(),
            budget,
            llm_client,
            router,
            kill_switch,
            blocklist,
            audit_log,
            platform,
        );

        let runtime = server.start_runtime().unwrap();
        let srv = server.clone();
        let srv_handle = tokio::spawn(async move {
            let _ = srv.run().await;
        });

        for _ in 0..40 {
            if daemon_socket.exists() && helper_socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }

        // Connect subscriber stream
        let mut sub_stream = tokio::net::UnixStream::connect(&daemon_socket)
            .await
            .unwrap();
        let sub_req = IpcMessage {
            id: 1,
            payload: DaemonRequest::SubscribeEvents,
        };
        write_ipc_message(&mut sub_stream, &sub_req).await.unwrap();
        let sub_ack: IpcMessage<DaemonResponse> = read_ipc_message(&mut sub_stream).await.unwrap();
        assert!(matches!(sub_ack.payload, DaemonResponse::Ack));

        // Connect client stream to submit task
        let mut client_stream = tokio::net::UnixStream::connect(&daemon_socket)
            .await
            .unwrap();
        let submit_req = IpcMessage {
            id: 2,
            payload: DaemonRequest::SubmitTask {
                instruction: "Send wire payment".into(),
            },
        };
        write_ipc_message(&mut client_stream, &submit_req)
            .await
            .unwrap();
        let submit_resp: IpcMessage<DaemonResponse> =
            read_ipc_message(&mut client_stream).await.unwrap();
        assert!(matches!(
            submit_resp.payload,
            DaemonResponse::Accepted { .. }
        ));

        // Await ApprovalRequested event from subscriber
        let mut found_req_id = None;
        for _ in 0..15 {
            if let Ok(Ok(frame)) = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                read_ipc_message::<_, IpcMessage<DaemonResponse>>(&mut sub_stream),
            )
            .await
                && let DaemonResponse::Event(DaemonEvent::ApprovalRequested {
                    request_id,
                    title,
                    details,
                }) = frame.payload
            {
                assert!(title.contains("Approval"));
                assert!(details.contains("send_btn"));
                found_req_id = Some(request_id);
                break;
            }
        }

        let app_req_id = found_req_id.expect("Must receive ApprovalRequested event");

        // Send RespondApproval
        let approve_req = IpcMessage {
            id: 3,
            payload: DaemonRequest::RespondApproval {
                request_id: app_req_id,
                approved: true,
            },
        };
        write_ipc_message(&mut client_stream, &approve_req)
            .await
            .unwrap();
        let approve_resp: IpcMessage<DaemonResponse> =
            read_ipc_message(&mut client_stream).await.unwrap();
        assert!(matches!(approve_resp.payload, DaemonResponse::Ack));

        // Wait for TaskFinished event on subscriber
        let mut task_finished = false;
        for _ in 0..15 {
            if let Ok(Ok(frame)) = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                read_ipc_message::<_, IpcMessage<DaemonResponse>>(&mut sub_stream),
            )
            .await
                && let DaemonResponse::Event(DaemonEvent::TaskFinished { ok, message, .. }) =
                    frame.payload
            {
                assert!(ok);
                assert!(message.contains("Confirmed and done"));
                task_finished = true;
                break;
            }
        }
        assert!(
            task_finished,
            "Task must complete successfully after approval"
        );

        srv_handle.abort();
        runtime.abort();
        let _ = runtime.await;
        helper_handle.abort();
        let _ = std::fs::remove_file(&daemon_socket);
        let _ = std::fs::remove_file(&helper_socket);
    }

    #[tokio::test]
    async fn test_dual_socket_run_kill_with_config_under_50ms() {
        let short_id = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let daemon_socket = std::path::PathBuf::from(format!("/tmp/gmkd_{}.sock", short_id));
        let helper_socket = std::path::PathBuf::from(format!("/tmp/gmkh_{}.sock", short_id));

        let config = GmConfig {
            daemon_socket_path: daemon_socket.clone(),
            socket_path: helper_socket.clone(),
            ..Default::default()
        };

        // Spawn mock helper server
        let mock_platform = gm_platform::MockPlatform::new();
        let helper_server = gm_cu_helper::HelperServer::new(mock_platform);
        let h_sock = helper_socket.clone();
        let helper_handle = tokio::spawn(async move {
            let _ = helper_server.run(h_sock).await;
        });

        // Spawn daemon control server
        let budget = Arc::new(BudgetManager::new(config.budget.clone()));
        let mock_backend = Arc::new(MockLlmBackend::new());
        let llm_client = Arc::new(HybridLlmClient::new(mock_backend, Arc::clone(&budget)));
        let router = LlmRouter::new(config.providers.clone());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
        let audit_log = Arc::new(AuditLogger::in_memory());
        let platform = Arc::new(IpcPlatformClient::new(&helper_socket));

        let control_server = DaemonControlServer::new(
            config.clone(),
            budget,
            llm_client,
            router,
            kill_switch.clone(),
            blocklist,
            audit_log,
            platform,
        );
        let daemon_handle = tokio::spawn(async move {
            let _ = control_server.run().await;
        });

        // Wait for both sockets to exist
        for _ in 0..40 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            if daemon_socket.exists() && helper_socket.exists() {
                break;
            }
        }
        assert!(daemon_socket.exists());
        assert!(helper_socket.exists());

        // Verify 0o600 permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let d_mode = std::fs::metadata(&daemon_socket)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let h_mode = std::fs::metadata(&helper_socket)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(d_mode, 0o600);
            assert_eq!(h_mode, 0o600);
        }

        // Measure run_kill_with_config execution time
        let start = std::time::Instant::now();
        let res = run_kill_with_config(&config).await;
        let elapsed = start.elapsed();

        assert!(res.is_ok());
        println!("Dual socket kill took: {:?}", elapsed);
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "Kill switch took {:?}, must complete in <50ms",
            elapsed
        );
        assert!(kill_switch.is_triggered());

        helper_handle.abort();
        daemon_handle.abort();
        let _ = std::fs::remove_file(&daemon_socket);
        let _ = std::fs::remove_file(&helper_socket);
    }

    #[tokio::test]
    async fn test_models_cli_subcommand_list_and_rm_validation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = gm_vlm_candle::ModelRegistry::new(temp_dir.path().join("models"));
        assert!(
            run_models_with_registry(&registry, Some(ModelsSubcommand::List), false)
                .await
                .is_ok()
        );
        assert!(
            run_models_with_registry(&registry, Some(ModelsSubcommand::List), true)
                .await
                .is_ok()
        );
        // Removing an uninstalled model returns a clean error
        assert!(
            run_models_with_registry(
                &registry,
                Some(ModelsSubcommand::Rm {
                    model: "qwen2-vl-2b".into(),
                }),
                false
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn test_openbot_cli_subcommands_and_daemon_ipc() {
        let temp_dir = tempfile::tempdir().unwrap();
        let short_id = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let daemon_socket = std::path::PathBuf::from(format!("/tmp/gmob_{}.sock", short_id));
        let helper_socket = std::path::PathBuf::from(format!("/tmp/gmobh_{}.sock", short_id));

        let config = GmConfig {
            storage_dir: temp_dir.path().to_path_buf(),
            daemon_socket_path: daemon_socket.clone(),
            socket_path: helper_socket.clone(),
            ..Default::default()
        };
        let config_path = temp_dir.path().join("config.json");
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        assert!(
            run_bots(Some(BotsSubcommand::List), true, Some(config_path.clone()))
                .await
                .is_ok()
        );
        assert!(
            run_queue(
                Some(QueueSubcommand::Add {
                    bot: "coder".into(),
                    instruction: "Write unit tests".into(),
                    priority: 5,
                }),
                true,
                Some(config_path.clone())
            )
            .await
            .is_ok()
        );
        assert!(
            run_channels(
                Some(ChannelsSubcommand::Post {
                    channel: "#general".into(),
                    message: "@researcher summarize recent notes".into(),
                    author: "user".into(),
                }),
                true,
                Some(config_path.clone())
            )
            .await
            .is_ok()
        );

        let budget = Arc::new(BudgetManager::new(config.budget.clone()));
        let llm_client = Arc::new(HybridLlmClient::new(
            Arc::new(MockLlmBackend::new()),
            Arc::clone(&budget),
        ));
        let router = LlmRouter::new(config.providers.clone());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
        let audit_log = Arc::new(AuditLogger::in_memory());
        let platform = Arc::new(IpcPlatformClient::new(&helper_socket));

        let server = DaemonControlServer::new(
            config.clone(),
            budget,
            llm_client,
            router,
            kill_switch,
            blocklist,
            audit_log,
            platform,
        );
        let srv_handle = tokio::spawn(async move {
            let _ = server.run().await;
        });

        for _ in 0..40 {
            if daemon_socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let mut stream = tokio::net::UnixStream::connect(&daemon_socket)
            .await
            .unwrap();
        write_ipc_message(
            &mut stream,
            &IpcMessage {
                id: 1,
                payload: DaemonRequest::ListBots,
            },
        )
        .await
        .unwrap();
        let resp: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await.unwrap();
        match resp.payload {
            DaemonResponse::JsonPayload(v) => assert_eq!(v.as_array().unwrap().len(), 4),
            other => panic!("Expected JsonPayload, got {:?}", other),
        }

        srv_handle.abort();
        let _ = std::fs::remove_file(&daemon_socket);
    }
}

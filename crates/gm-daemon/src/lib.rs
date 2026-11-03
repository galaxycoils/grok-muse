use clap::{Parser, Subcommand};
use gm_agent::{AgentLoop, AgentLoopConfig};
use gm_core::config::GmConfig;
use gm_core::error::{GmError, Result};
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
use tracing::info;

pub mod service;
pub use service::*;

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
    let helper_socket = Path::new("/tmp/grok-muse-helper.sock");
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

pub async fn run_do(instruction: &str, config_path: Option<PathBuf>) -> Result<()> {
    let config = if let Some(p) = config_path {
        if let Ok(content) = std::fs::read_to_string(&p) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            GmConfig::default()
        }
    } else {
        GmConfig::default()
    };

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
    let llm_backend = Arc::new(MockLlmBackend::new());
    let llm_client = Arc::new(HybridLlmClient::new(llm_backend, Arc::clone(&budget)));

    let agent = AgentLoop::new(
        platform,
        llm_client,
        router,
        kill_switch,
        blocklist,
        AgentLoopConfig::default(),
    )
    .with_audit(Arc::new(audit_log.clone()));

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

    let helper_socket = Path::new("/tmp/grok-muse-helper.sock");
    println!(
        "\nHelper Socket: {}",
        if helper_socket.exists() {
            "Connected (/tmp/grok-muse-helper.sock)"
        } else {
            "Not running"
        }
    );

    println!("============================================================\n");
    Ok(())
}

pub async fn run_kill() -> Result<()> {
    println!("🚨 Grok-Muse Emergency Kill Switch Triggered");
    let helper_socket = Path::new("/tmp/grok-muse-helper.sock");
    if helper_socket.exists() {
        println!(
            "Connecting to CU Helper socket ({:?}) to drop active actions...",
            helper_socket
        );
        let platform = IpcPlatformClient::new("/tmp/grok-muse-helper.sock");
        match platform.cancel().await {
            Ok(()) => println!("✔ CU Helper confirmed: active actions cancelled and dropped."),
            Err(e) => eprintln!("⚠ Notice: Could not send cancel over socket: {e}"),
        }
    } else {
        println!("○ CU Helper socket is not active (no background actions queued).");
    }
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
    let config = if let Some(p) = config_path {
        if let Ok(content) = std::fs::read_to_string(&p) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            GmConfig::default()
        }
    } else {
        GmConfig::default()
    };

    let keychain = KeychainSecrets::default();
    let meta_key = keychain.get_meta_key().ok();
    let xai_key = keychain.get_xai_key().ok();

    let (backend, backend_name): (Arc<dyn LlmBackend>, &str) = if meta_key.is_some()
        || xai_key.is_some()
    {
        (
            Arc::new(ReqwestLlmBackend::with_keys(meta_key, xai_key)),
            "Live API (Keychain / Environment)",
        )
    } else {
        let mock = MockLlmBackend::new();
        mock.set_response("I am ready to assist you. You can ask me questions, search memory with /memory, check system status with /status, or dispatch actions with /do.");
        (Arc::new(mock), "Offline / Conversational Simulator")
    };

    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let llm_client = Arc::new(HybridLlmClient::new(backend, Arc::clone(&budget)));
    let router = LlmRouter::new(config.providers.clone());
    let memory = Arc::new(
        PersonalMemoryStore::default_store()
            .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap()),
    );
    let audit_log = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());
    let kill_switch = KillSwitch::new();
    let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);

    let system_prompt = system_override.unwrap_or_else(|| {
        "You are Grok-Muse, a fast computer-use personal assistant for macOS. \
        You have direct access to local memory, system permissions, and desktop automation tools. \
        Respond concisely, accurately, and politely."
            .into()
    });

    let mut history: Vec<ChatMessage> = vec![ChatMessage {
        role: "system".into(),
        content: system_prompt,
    }];

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

            let platform = Arc::new(IpcPlatformClient::new(&config.socket_path));
            let agent = AgentLoop::new(
                platform,
                llm_client.clone(),
                router.clone(),
                kill_switch.clone(),
                blocklist.clone(),
                AgentLoopConfig::default(),
            )
            .with_audit(Arc::new(audit_log.clone()));

            match agent.run_task("repl_inline", task_instruction).await {
                Ok(summary) => {
                    writeln!(writer, "✔ Task succeeded: {}\n", summary)?;
                    history.push(ChatMessage {
                        role: "system".into(),
                        content: format!("Computer-use action executed: {summary}"),
                    });
                }
                Err(e) => {
                    writeln!(writer, "✘ Task failed: {}\n", e)?;
                }
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
        history.push(ChatMessage {
            role: "user".into(),
            content: enriched_prompt,
        });

        let (primary, fallback) = router.route(TaskType::Planning);
        match llm_client
            .execute_routed(&primary, fallback.as_ref(), &history)
            .await
        {
            Ok(resp) => {
                writeln!(writer, "\n{}\n", resp.content.trim())?;
                history.push(ChatMessage {
                    role: "assistant".into(),
                    content: resp.content.clone(),
                });
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
    let config = if let Some(p) = config_path {
        if let Ok(content) = std::fs::read_to_string(&p) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            GmConfig::default()
        }
    } else {
        GmConfig::default()
    };

    info!("Starting gm-daemon...");
    let audit_log = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());
    let kill_switch = KillSwitch::new();
    let blocklist = BlocklistManager::new(&config.safety.blocklisted_apps);
    let budget = Arc::new(BudgetManager::new(config.budget.clone()));
    let router = LlmRouter::new(config.providers.clone());

    let platform = Arc::new(IpcPlatformClient::new(&config.socket_path));
    let llm_backend = Arc::new(MockLlmBackend::new());
    let llm_client = Arc::new(HybridLlmClient::new(llm_backend, Arc::clone(&budget)));

    let memory = Arc::new(
        PersonalMemoryStore::default_store()
            .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap()),
    );

    let proactive_engine = Arc::new(ProactiveEngine::new(
        QuietGateConfig::default(),
        Arc::clone(&budget),
    ));

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

    if let Some(task_instruction) = task {
        info!("Running one-shot task: {}", task_instruction);
        let agent = AgentLoop::new(
            platform,
            llm_client,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_audit(Arc::new(audit_log.clone()));

        match agent.run_task("cli_task", &task_instruction).await {
            Ok(summary) => {
                println!("Task completed: {summary}");
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
    let spend_cap = GmConfig::default().budget.daily_spend_cap_usd;
    let tracker = gm_goals::DogfoodTracker::default_tracker(spend_cap)?;
    let metrics = tracker.compute_metrics();
    let perms = check_all_permissions();
    let helper_socket = Path::new("/tmp/grok-muse-helper.sock");

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

    let audit = AuditLogger::default_logger().unwrap_or_else(|_| AuditLogger::in_memory());
    let memory = PersonalMemoryStore::default_store()
        .unwrap_or_else(|_| PersonalMemoryStore::in_memory().unwrap());
    let spend_cap = GmConfig::default().budget.daily_spend_cap_usd;
    let tracker = gm_goals::DogfoodTracker::default_tracker(spend_cap)?;
    let metrics = tracker.compute_metrics();

    let socket_path = Path::new("/tmp/grok-muse-helper.sock");
    let review = if socket_path.exists() {
        let platform = IpcPlatformClient::new(socket_path);
        gm_goals::run_nightly_review(
            Some(&platform),
            &audit,
            &memory,
            Some(&metrics),
            target_time,
        )
        .await?
    } else {
        #[cfg(target_os = "macos")]
        {
            let platform = gm_platform::MacPlatform::new().ok();
            gm_goals::run_nightly_review(
                platform.as_ref(),
                &audit,
                &memory,
                Some(&metrics),
                target_time,
            )
            .await?
        }
        #[cfg(not(target_os = "macos"))]
        {
            let platform: Option<&gm_platform::MockPlatform> = None;
            gm_goals::run_nightly_review(platform, &audit, &memory, Some(&metrics), target_time)
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
    let socket_path = Path::new("/tmp/grok-muse-helper.sock");

    let ideas = if socket_path.exists() {
        let platform = IpcPlatformClient::new(socket_path);
        gm_goals::generate_ideas(Some(&platform), &memory, chrono::Utc::now()).await?
    } else {
        #[cfg(target_os = "macos")]
        {
            let platform = gm_platform::MacPlatform::new().ok();
            gm_goals::generate_ideas(platform.as_ref(), &memory, chrono::Utc::now()).await?
        }
        #[cfg(not(target_os = "macos"))]
        {
            let platform: Option<&gm_platform::MockPlatform> = None;
            gm_goals::generate_ideas(platform, &memory, chrono::Utc::now()).await?
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&ideas)?);
    } else {
        print!("{}", gm_goals::format_ideas(&ideas));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let input = "   \n/unknown_cmd\n/document code\n/members in team\n/do\n/memory\nWhat is your role?\n/history\n/quit\n";
        let reader = std::io::Cursor::new(input);
        let mut output = Vec::new();

        let res = run_chat_loop(reader, &mut output, None, None).await;
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
}

mod native;
use clap::{Parser, Subcommand};
use gm_core::config::GmConfig;
use gm_core::error::Result;
use gm_core::ipc::DaemonRequest;
#[cfg(unix)]
use gm_core::ipc::{DaemonEvent, DaemonResponse, IpcMessage, read_ipc_message, write_ipc_message};
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::path::Path;
use std::sync::OnceLock;
use tracing::{info, warn};

#[cfg(target_os = "macos")]
#[allow(dead_code)]
unsafe extern "C" {
    fn gm_mac_ui_init_tray() -> std::os::raw::c_int;
    fn gm_mac_ui_init_chat_window() -> std::os::raw::c_int;
    fn gm_mac_ui_show_chat_window() -> std::os::raw::c_int;
    fn gm_mac_ui_init_action_overlay() -> std::os::raw::c_int;
    fn gm_mac_ui_set_overlay_state(
        state: std::os::raw::c_int,
        status_text: *const std::os::raw::c_char,
        detail_text: *const std::os::raw::c_char,
    );
    fn gm_ui_set_hud_state(
        state: std::os::raw::c_int,
        status_text: *const std::os::raw::c_char,
        detail_text: *const std::os::raw::c_char,
    );
    fn gm_ui_append_chat_message(
        role: *const std::os::raw::c_char,
        message: *const std::os::raw::c_char,
    );
    fn gm_ui_append_chat_delta(delta: *const std::os::raw::c_char);
    fn gm_ui_present_approval_sheet(
        request_id: *const std::os::raw::c_char,
        title: *const std::os::raw::c_char,
        details: *const std::os::raw::c_char,
    );
    fn gm_ui_set_approval_callback(approval_cb: extern "C" fn(*const std::os::raw::c_char, bool));
    fn gm_mac_ui_show_overlay(show: std::os::raw::c_int);
    fn gm_mac_ui_show_onboarding() -> std::os::raw::c_int;
    fn gm_mac_ui_refresh_onboarding_permissions();
    fn gm_mac_ui_init_global_hotkeys() -> std::os::raw::c_int;
    fn gm_mac_ui_run_app();
    fn gm_mac_ui_stop_app();
    fn gm_mac_ui_set_callbacks(
        hotkey_cb: extern "C" fn(std::os::raw::c_int),
        task_cb: extern "C" fn(*const std::os::raw::c_char),
        pause_cb: extern "C" fn(std::os::raw::c_int),
    );
}

#[derive(Parser, Debug)]
#[command(
    name = "gm-ui",
    about = "Grok-Muse Desktop Shell UI (Tray, Action Overlay, Onboarding & Hotkeys)"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Launch the shared native desktop application
    Native,
    /// Launch tray icon, action overlay, and global hotkey monitor
    Tray,
    /// Open floating chat / action window
    Chat,
    /// Open Onboarding & Permissions dialog
    Onboard,
    /// Test or display floating visual action overlay
    Overlay {
        /// Overlay state: idle, acting, paused, or stopped
        #[arg(long, default_value = "acting")]
        state: String,
        /// Detail text description
        #[arg(long)]
        detail: Option<String>,
    },
    /// Trigger emergency kill switch immediately
    Kill,
}

#[allow(dead_code)]
enum UiCommand {
    SubmitTaskOrChat(String),
    TogglePause(bool),
    ApprovalDecision { request_id: String, approved: bool },
}

#[allow(dead_code)]
static UI_CMD_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<UiCommand>> = OnceLock::new();
#[allow(dead_code)]
static CONFIG: OnceLock<GmConfig> = OnceLock::new();
#[allow(dead_code)]
static TOKIO_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

#[cfg(target_os = "macos")]
extern "C" fn hotkey_callback(hotkey_id: std::os::raw::c_int) {
    if hotkey_id == 1 {
        info!("Hotkey fired: Summon (Cmd+Shift+Space)");
        println!("[HOTKEY] Summon Assistant activated (Cmd+Shift+Space)");
    } else if hotkey_id == 2 {
        info!("Hotkey fired: Emergency Kill Switch (Cmd+Alt+Escape)");
        eprintln!("[HOTKEY] EMERGENCY KILL SWITCH ACTIVATED (Cmd+Alt+Escape)");

        // Update HUD to Stopped immediately (<1ms)
        let status = CString::new("🛑 Grok-Muse: Stopped").unwrap();
        let detail = CString::new("Stopped (Emergency Kill)").unwrap();
        unsafe {
            gm_ui_set_hud_state(3, status.as_ptr(), detail.as_ptr());
        }

        // Concurrently kill daemon and helper in background runtime (<50ms)
        if let Some(cfg) = CONFIG.get() {
            let cfg = cfg.clone();
            if let Some(handle) = TOKIO_HANDLE.get() {
                handle.spawn(async move {
                    let _ = gm_daemon::run_kill_with_config(&cfg).await;
                });
            } else {
                tokio::spawn(async move {
                    let _ = gm_daemon::run_kill_with_config(&cfg).await;
                });
            }
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn task_submitted_callback(text_ptr: *const std::os::raw::c_char) {
    if !text_ptr.is_null() {
        let task_str = unsafe { std::ffi::CStr::from_ptr(text_ptr) }
            .to_string_lossy()
            .to_string();
        info!("Task instruction submitted from UI: {task_str}");
        println!("[TASK SUBMITTED] {task_str}");
        if let Some(tx) = UI_CMD_TX.get() {
            let _ = tx.send(UiCommand::SubmitTaskOrChat(task_str));
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn pause_toggled_callback(is_paused: std::os::raw::c_int) {
    let paused = is_paused == 1;
    info!("Agent coexistence pause changed: is_paused={paused}");
    println!(
        "[COEXISTENCE] Agent execution {}",
        if paused { "PAUSED" } else { "RESUMED" }
    );
    if let Some(tx) = UI_CMD_TX.get() {
        let _ = tx.send(UiCommand::TogglePause(paused));
    }
}

#[cfg(target_os = "macos")]
extern "C" fn approval_decision_callback(
    request_id_ptr: *const std::os::raw::c_char,
    approved: bool,
) {
    if !request_id_ptr.is_null() {
        let req_id = unsafe { std::ffi::CStr::from_ptr(request_id_ptr) }
            .to_string_lossy()
            .to_string();
        info!("Approval decision received from UI: req_id={req_id}, approved={approved}");
        if let Some(tx) = UI_CMD_TX.get() {
            let _ = tx.send(UiCommand::ApprovalDecision {
                request_id: req_id,
                approved,
            });
        }
    }
}

#[cfg(unix)]
fn handle_daemon_event(ev: DaemonEvent) {
    #[cfg(target_os = "macos")]
    match ev {
        DaemonEvent::HudStateChanged {
            state,
            detail,
            daily_spend_usd,
        } => {
            let state_code = match state.to_lowercase().as_str() {
                "idle" => 0,
                "acting" | "thinking" => 1,
                "paused" => 2,
                "stopped" | "kill" => 3,
                _ => 1,
            };
            let status_text = format!("Grok-Muse: {state} (${daily_spend_usd:.2})");
            let c_status = CString::new(status_text.replace('\0', "")).unwrap_or_default();
            let c_detail = CString::new(detail.replace('\0', "")).unwrap_or_default();
            unsafe {
                gm_ui_set_hud_state(state_code, c_status.as_ptr(), c_detail.as_ptr());
            }
        }
        DaemonEvent::ChatToken { delta, done } | DaemonEvent::ChatTokenV1 { delta, done, .. } => {
            let c_delta = CString::new(delta.replace('\0', "")).unwrap_or_default();
            unsafe {
                gm_ui_append_chat_delta(c_delta.as_ptr());
                if done {
                    let c_nl = CString::new("\n").unwrap_or_default();
                    gm_ui_append_chat_delta(c_nl.as_ptr());
                }
            }
        }
        DaemonEvent::TaskProgress {
            step,
            action_summary,
            ..
        } => {
            let c_status = CString::new("⚡️ Grok-Muse: Acting").unwrap_or_default();
            let c_detail =
                CString::new(format!("Step {step}: {}", action_summary.replace('\0', "")))
                    .unwrap_or_default();
            unsafe {
                gm_ui_set_hud_state(1, c_status.as_ptr(), c_detail.as_ptr());
            }
        }
        DaemonEvent::ApprovalRequested {
            request_id,
            title,
            details,
        } => {
            let c_req = CString::new(request_id.replace('\0', "")).unwrap_or_default();
            let c_title = CString::new(title.replace('\0', "")).unwrap_or_default();
            let c_details = CString::new(details.replace('\0', "")).unwrap_or_default();
            unsafe {
                gm_ui_present_approval_sheet(c_req.as_ptr(), c_title.as_ptr(), c_details.as_ptr());
            }
        }
        DaemonEvent::TaskFinished { ok, message, .. } => {
            let (code, status) = if ok {
                (0, "Grok-Muse: Finished")
            } else {
                (3, "Grok-Muse: Stopped")
            };
            let c_status = CString::new(status).unwrap_or_default();
            let clean_msg = message.replace('\0', "");
            let c_detail = CString::new(clean_msg.clone()).unwrap_or_default();
            unsafe {
                gm_ui_set_hud_state(code, c_status.as_ptr(), c_detail.as_ptr());
                let role = CString::new("System").unwrap_or_default();
                let msg = CString::new(format!("Task outcome: {clean_msg}")).unwrap_or_default();
                gm_ui_append_chat_message(role.as_ptr(), msg.as_ptr());
            }
        }
        DaemonEvent::WheelHandoffRequested {
            task_id,
            bot_id,
            blocker,
            instructions,
        } => {
            let c_status = CString::new("🛞 Take the Wheel").unwrap_or_default();
            let c_detail =
                CString::new(format!("@{bot_id} [{blocker}]: {instructions}").replace('\0', ""))
                    .unwrap_or_default();
            let c_req = CString::new(format!("wheel:{task_id}")).unwrap_or_default();
            let c_title =
                CString::new(format!("🛞 Take the Wheel (@{bot_id})")).unwrap_or_default();
            unsafe {
                gm_ui_set_hud_state(2, c_status.as_ptr(), c_detail.as_ptr());
                gm_ui_present_approval_sheet(c_req.as_ptr(), c_title.as_ptr(), c_detail.as_ptr());
            }
        }
        DaemonEvent::WheelHandoffResumed { task_id, bot_id } => {
            let c_status = CString::new("⚡️ Grok-Muse: Resumed").unwrap_or_default();
            let c_detail =
                CString::new(format!("Resumed @{bot_id} task {task_id}")).unwrap_or_default();
            unsafe {
                gm_ui_set_hud_state(1, c_status.as_ptr(), c_detail.as_ptr());
            }
        }
        DaemonEvent::QueueUpdated {
            bot_id,
            queued,
            running_task_id,
        } => {
            let role = CString::new("OpenBot").unwrap_or_default();
            let msg = CString::new(format!(
                "Queue update: @{bot_id} ({queued} queued, running: {:?})",
                running_task_id
            ))
            .unwrap_or_default();
            unsafe {
                gm_ui_append_chat_message(role.as_ptr(), msg.as_ptr());
            }
        }
        DaemonEvent::ApprovalResolved { .. } => {}
        DaemonEvent::ChannelMessage {
            channel_id,
            author,
            content,
            ..
        } => {
            let role = CString::new(format!("{channel_id} ({author})")).unwrap_or_default();
            let msg = CString::new(content.replace('\0', "")).unwrap_or_default();
            unsafe {
                gm_ui_append_chat_message(role.as_ptr(), msg.as_ptr());
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = ev;
    }
}

async fn ensure_daemon_running(config: &GmConfig, config_path: Option<&Path>) {
    {
        if native::is_running(config).await {
            return;
        }

        let (daemon_bin, _) = gm_daemon::service::resolve_binaries(None);
        if !daemon_bin.exists() {
            warn!(
                "gm-daemon binary not found at {:?}; skipping auto-spawn",
                daemon_bin
            );
            return;
        }

        info!("Auto-spawning gm-daemon at {:?}", daemon_bin);
        let mut cmd = std::process::Command::new(&daemon_bin);
        if let Some(path) = config_path {
            cmd.arg("--config").arg(path);
        }
        cmd.arg("daemon");
        if let Ok(_child) = cmd.spawn() {
            let start = tokio::time::Instant::now();
            while start.elapsed() < tokio::time::Duration::from_millis(2000) {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                if native::is_running(config).await {
                    info!("gm-daemon auto-spawned and responding");
                    return;
                }
            }
        }
    }
}

async fn send_daemon_request(path: &Path, req: DaemonRequest) {
    #[cfg(unix)]
    {
        match tokio::net::UnixStream::connect(path).await {
            Ok(mut stream) => {
                let msg = IpcMessage {
                    id: 1,
                    payload: req,
                };
                if let Err(e) = write_ipc_message(&mut stream, &msg).await {
                    warn!("Failed to send daemon request: {e}");
                    return;
                }
                // Await response so the connection is closed gracefully by client rather than causing BrokenPipe
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_secs(30),
                    read_ipc_message::<_, IpcMessage<DaemonResponse>>(&mut stream),
                )
                .await;
            }
            Err(e) => {
                warn!("Failed to connect to daemon socket at {:?}: {e}", path);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, req);
    }
}

async fn run_ui_tokio_client(
    config: GmConfig,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
) {
    // 1. Ensure daemon is running
    ensure_daemon_running(&config, None).await;

    // 2. Background event subscriber with auto-reconnect
    #[cfg(unix)]
    {
        let daemon_socket = config.daemon_socket_path.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(mut stream) = tokio::net::UnixStream::connect(&daemon_socket).await {
                    info!("Connected to gm-daemon event bus");
                    let sub_req = IpcMessage {
                        id: 1,
                        payload: DaemonRequest::SubscribeEvents,
                    };
                    if write_ipc_message(&mut stream, &sub_req).await.is_ok() {
                        // Read initial Ack
                        let _ack: Result<IpcMessage<DaemonResponse>> =
                            read_ipc_message(&mut stream).await;
                        loop {
                            match read_ipc_message::<_, IpcMessage<DaemonResponse>>(&mut stream)
                                .await
                            {
                                Ok(msg) => {
                                    if let DaemonResponse::Event(ev) = msg.payload {
                                        handle_daemon_event(ev);
                                    }
                                }
                                Err(e) => {
                                    warn!("Daemon event stream disconnected: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        });
    }

    // 3. Process commands dispatched from AppKit callbacks
    while let Some(cmd) = cmd_rx.recv().await {
        let req = match cmd {
            UiCommand::SubmitTaskOrChat(text) => {
                let trimmed = text.trim();
                let lower = trimmed.to_lowercase();
                if let Some(rest) = trimmed.strip_prefix('@')
                    && let Some((bot, instr)) = rest.split_once(' ')
                {
                    DaemonRequest::QueueTask {
                        bot_id: Some(bot.trim().to_lowercase()),
                        channel_id: Some("#general".into()),
                        instruction: instr.trim().to_string(),
                        priority: 5,
                    }
                } else if let Some(instruction) = trimmed.strip_prefix("/do ") {
                    DaemonRequest::SubmitTask {
                        instruction: instruction.trim().to_string(),
                    }
                } else if let Some(instruction) = trimmed.strip_prefix("do: ") {
                    DaemonRequest::SubmitTask {
                        instruction: instruction.trim().to_string(),
                    }
                } else if trimmed.starts_with("/do") {
                    let instruction = trimmed.trim_start_matches("/do").trim().to_string();
                    DaemonRequest::SubmitTask { instruction }
                } else if let Some(instruction) = trimmed.strip_prefix("/task ") {
                    DaemonRequest::SubmitTask {
                        instruction: instruction.trim().to_string(),
                    }
                } else if let Some(instruction) = trimmed.strip_prefix("task: ") {
                    DaemonRequest::SubmitTask {
                        instruction: instruction.trim().to_string(),
                    }
                } else if lower.starts_with("open ")
                    || lower.starts_with("click ")
                    || lower.starts_with("type ")
                    || lower.starts_with("press ")
                    || lower.starts_with("tap ")
                    || lower.starts_with("scroll ")
                    || lower.starts_with("navigate ")
                    || lower.starts_with("launch ")
                    || lower.starts_with("close ")
                    || lower.starts_with("switch ")
                    || lower.starts_with("select ")
                    || lower.starts_with("drag ")
                {
                    DaemonRequest::SubmitTask {
                        instruction: trimmed.to_string(),
                    }
                } else {
                    DaemonRequest::SendChatMessage {
                        text: trimmed.to_string(),
                    }
                }
            }
            UiCommand::TogglePause(paused) => DaemonRequest::SetPaused { paused },
            UiCommand::ApprovalDecision {
                request_id,
                approved,
            } => {
                if let Some(task_id) = request_id.strip_prefix("wheel:") {
                    DaemonRequest::TakeTheWheelResume {
                        task_id: task_id.to_string(),
                        note: Some(format!("Resumed from UI sheet (approved={approved})")),
                    }
                } else {
                    DaemonRequest::RespondApproval {
                        request_id,
                        approved,
                    }
                }
            }
        };

        send_daemon_request(&config.daemon_socket_path, req).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let cmd = cli.command.unwrap_or(Commands::Native);

    let config = GmConfig::load(cli.config.as_deref())?;
    if matches!(cmd, Commands::Native) {
        ensure_daemon_running(&config, cli.config.as_deref()).await;
        return native::run(
            config,
            cli.config
                .clone()
                .unwrap_or_else(gm_core::config::default_config_path),
        );
    }
    let _ = CONFIG.set(config.clone());
    let _ = TOKIO_HANDLE.set(tokio::runtime::Handle::current());

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = UI_CMD_TX.set(cmd_tx);

    tokio::spawn(run_ui_tokio_client(config.clone(), cmd_rx));

    if matches!(cmd, Commands::Kill) {
        info!("Emergency kill switch activated from CLI");
        #[cfg(target_os = "macos")]
        unsafe {
            gm_ui_set_hud_state(
                3,
                CString::new("🛑 Grok-Muse: Stopped").unwrap().as_ptr(),
                CString::new("CLI Kill Signal").unwrap().as_ptr(),
            );
        }
        gm_daemon::run_kill_with_config(&config).await?;
        println!("Emergency kill signal dispatched.");
        return Ok(());
    }

    // Spawn background task to stop AppKit event loop on Ctrl+C signal
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.ok();
        #[cfg(target_os = "macos")]
        unsafe {
            gm_mac_ui_stop_app();
        }
    });

    match cmd {
        Commands::Native => unreachable!(),
        Commands::Tray => {
            info!("Launching Grok-Muse tray desktop shell");
            #[cfg(target_os = "macos")]
            unsafe {
                gm_mac_ui_set_callbacks(
                    hotkey_callback,
                    task_submitted_callback,
                    pause_toggled_callback,
                );
                gm_ui_set_approval_callback(approval_decision_callback);
                let ret = gm_mac_ui_init_tray();
                if ret == 0 {
                    info!("Native macOS AppKit NSStatusItem & Action Overlay initialized");
                }
            }
            println!("Grok-Muse system tray active.");
            println!("  • Instant Summon: Cmd+Shift+Space");
            println!("  • Emergency Kill Switch: Cmd+Alt+Escape");
            println!("  • Tray menu: Summon, Pause/Resume, Run Task, Permissions, Quit");
        }
        Commands::Chat => {
            info!("Opening floating chat interface");
            #[cfg(target_os = "macos")]
            unsafe {
                gm_mac_ui_set_callbacks(
                    hotkey_callback,
                    task_submitted_callback,
                    pause_toggled_callback,
                );
                gm_ui_set_approval_callback(approval_decision_callback);
                let ret = gm_mac_ui_init_chat_window();
                if ret == 0 {
                    info!("Native macOS AppKit floating window initialized");
                }
                gm_mac_ui_show_chat_window();
            }
            println!("Grok-Muse chat interface initialized.");
        }
        Commands::Onboard => {
            info!("Opening Onboarding & Permissions dialog");
            #[cfg(target_os = "macos")]
            unsafe {
                let ret = gm_mac_ui_show_onboarding();
                if ret == 0 {
                    info!("Native macOS AppKit onboarding dialog initialized");
                }
            }
            println!("Grok-Muse permissions & onboarding dialog active.");
        }
        Commands::Overlay { state, detail } => {
            info!("Initializing floating visual action overlay (state={state})");
            #[cfg(target_os = "macos")]
            unsafe {
                gm_mac_ui_init_action_overlay();
                let state_code = match state.to_lowercase().as_str() {
                    "idle" => 0,
                    "acting" => 1,
                    "paused" => 2,
                    "stopped" | "kill" => 3,
                    _ => 1,
                };
                let c_detail = detail
                    .map(|d| CString::new(d).unwrap())
                    .unwrap_or_else(|| CString::new("Inspecting target window...").unwrap());
                gm_ui_set_hud_state(state_code, std::ptr::null(), c_detail.as_ptr());
                gm_mac_ui_show_overlay(1);
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (&state, &detail);
            }
            println!("Visual action overlay displayed. Press Ctrl+C or dismiss to exit.");
        }
        Commands::Kill => unreachable!(),
    }

    #[cfg(target_os = "macos")]
    unsafe {
        gm_mac_ui_run_app();
    }
    #[cfg(not(target_os = "macos"))]
    tokio::signal::ctrl_c().await.ok();

    Ok(())
}

use clap::{Parser, Subcommand};
use gm_core::error::Result;
use std::ffi::CString;
use tracing::info;

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
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
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

extern "C" fn hotkey_callback(hotkey_id: std::os::raw::c_int) {
    if hotkey_id == 1 {
        info!("Hotkey fired: Summon (Cmd+Shift+Space)");
        println!("[HOTKEY] Summon Assistant activated (Cmd+Shift+Space)");
    } else if hotkey_id == 2 {
        info!("Hotkey fired: Emergency Kill Switch (Cmd+Alt+Escape)");
        eprintln!("[HOTKEY] EMERGENCY KILL SWITCH ACTIVATED (Cmd+Alt+Escape)");
    }
}

extern "C" fn task_submitted_callback(text_ptr: *const std::os::raw::c_char) {
    if !text_ptr.is_null() {
        let task_str = unsafe { std::ffi::CStr::from_ptr(text_ptr) }.to_string_lossy();
        info!("Task instruction submitted from UI: {task_str}");
        println!("[TASK SUBMITTED] {task_str}");
    }
}

extern "C" fn pause_toggled_callback(is_paused: std::os::raw::c_int) {
    info!("Agent coexistence pause changed: is_paused={is_paused}");
    println!(
        "[COEXISTENCE] Agent execution {}",
        if is_paused == 1 { "PAUSED" } else { "RESUMED" }
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let cmd = cli.command.unwrap_or(Commands::Tray);

    if matches!(cmd, Commands::Kill) {
        info!("Emergency kill switch activated from CLI");
        #[cfg(target_os = "macos")]
        unsafe {
            gm_mac_ui_set_overlay_state(
                3,
                CString::new("🛑 Grok-Muse: Stopped").unwrap().as_ptr(),
                CString::new("CLI Kill Signal").unwrap().as_ptr(),
            );
        }
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
        Commands::Tray => {
            info!("Launching Grok-Muse tray desktop shell");
            #[cfg(target_os = "macos")]
            unsafe {
                gm_mac_ui_set_callbacks(
                    hotkey_callback,
                    task_submitted_callback,
                    pause_toggled_callback,
                );
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
                gm_mac_ui_set_overlay_state(state_code, std::ptr::null(), c_detail.as_ptr());
                gm_mac_ui_show_overlay(1);
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

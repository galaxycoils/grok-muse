#[cfg(not(target_os = "macos"))]
pub use crate::user_services::{install_services, show_services_status, uninstall_services};
#[cfg(target_os = "macos")]
use gm_core::error::{GmError, Result};
#[cfg(target_os = "macos")]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

pub const DAEMON_LABEL: &str = "co.anysphere.grok-muse.daemon";
pub const HELPER_LABEL: &str = "co.anysphere.grok-muse.helper";

pub fn launch_agents_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Library/LaunchAgents")
    } else {
        PathBuf::from("/tmp/LaunchAgents")
    }
}

pub fn logs_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Library/Logs/GrokMuse")
    } else {
        PathBuf::from("/tmp/GrokMuseLogs")
    }
}

pub fn daemon_plist_path() -> PathBuf {
    launch_agents_dir().join(format!("{DAEMON_LABEL}.plist"))
}

pub fn helper_plist_path() -> PathBuf {
    launch_agents_dir().join(format!("{HELPER_LABEL}.plist"))
}

pub fn helper_short_plist_path() -> PathBuf {
    launch_agents_dir().join("helper.plist")
}

pub fn generate_daemon_plist(daemon_bin: &Path, log_stdout: &Path, log_stderr: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>daemon</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        DAEMON_LABEL,
        daemon_bin.display(),
        home,
        log_stdout.display(),
        log_stderr.display()
    )
}

pub fn generate_helper_plist(helper_bin: &Path, log_stdout: &Path, log_stderr: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        HELPER_LABEL,
        helper_bin.display(),
        home,
        log_stdout.display(),
        log_stderr.display()
    )
}

pub fn get_user_id() -> u32 {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn getuid() -> u32;
        }
        unsafe { getuid() }
    }
    #[cfg(not(unix))]
    {
        501
    }
}

pub fn resolve_binaries(custom_bin_dir: Option<&Path>) -> (PathBuf, PathBuf) {
    if let Some(dir) = custom_bin_dir {
        let d = dir.join(format!("gm-daemon{}", std::env::consts::EXE_SUFFIX));
        let h = dir.join(format!("gm-cu-helper{}", std::env::consts::EXE_SUFFIX));
        return (d, h);
    }

    // 1. Check standard /Applications bundle locations
    let app_daemon = PathBuf::from("/Applications/GrokMuse.app/Contents/MacOS/gm-daemon");
    let app_helper =
        PathBuf::from("/Applications/GrokMuseCUHelper.app/Contents/MacOS/gm-cu-helper");
    if app_daemon.exists() && app_helper.exists() {
        return (app_daemon, app_helper);
    }

    // 2. Check directory of current executable
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let d = parent.join(format!("gm-daemon{}", std::env::consts::EXE_SUFFIX));
        let h = parent.join(format!("gm-cu-helper{}", std::env::consts::EXE_SUFFIX));
        if d.exists() && h.exists() {
            return (d, h);
        }
    }

    // 3. Check workspace target/release or target/debug
    let candidates = [
        ("target/release/gm-daemon", "target/release/gm-cu-helper"),
        ("target/debug/gm-daemon", "target/debug/gm-cu-helper"),
        (
            "grok-muse/target/release/gm-daemon",
            "grok-muse/target/release/gm-cu-helper",
        ),
        (
            "grok-muse/target/debug/gm-daemon",
            "grok-muse/target/debug/gm-cu-helper",
        ),
        (
            "../target/release/gm-daemon",
            "../target/release/gm-cu-helper",
        ),
        ("../target/debug/gm-daemon", "../target/debug/gm-cu-helper"),
    ];
    for (d, h) in candidates {
        let dp = PathBuf::from(d);
        let hp = PathBuf::from(h);
        if dp.exists() && hp.exists() {
            return (
                std::fs::canonicalize(&dp).unwrap_or(dp),
                std::fs::canonicalize(&hp).unwrap_or(hp),
            );
        }
    }

    (app_daemon, app_helper)
}

fn run_launchctl(args: &[&str]) -> bool {
    #[cfg(target_os = "macos")]
    {
        match Command::new("launchctl").args(args).output() {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        false
    }
}

pub fn bootstrap_service(plist_path: &Path, label: &str) -> bool {
    let uid = get_user_id();
    let domain = format!("gui/{uid}");
    let service_target = format!("{domain}/{label}");

    // Bootout previous registration if any
    let _ = run_launchctl(&["bootout", &service_target]);

    // Bootstrap modern launch agent
    let plist_str = plist_path.to_string_lossy();
    if run_launchctl(&["bootstrap", &domain, &plist_str]) {
        return true;
    }

    // Fallback to legacy load
    run_launchctl(&["load", "-w", &plist_str])
}

pub fn bootout_service(plist_path: &Path, label: &str) -> bool {
    let uid = get_user_id();
    let domain = format!("gui/{uid}");
    let service_target = format!("{domain}/{label}");

    let res = run_launchctl(&["bootout", &service_target]);
    if !res {
        let plist_str = plist_path.to_string_lossy();
        let _ = run_launchctl(&["unload", "-w", &plist_str]);
    }
    true
}

#[derive(Debug, Clone)]
pub struct ServiceStatus {
    pub label: String,
    pub plist_installed: bool,
    pub plist_path: PathBuf,
    pub is_running: bool,
    pub pid: Option<u32>,
}

pub fn inspect_service(label: &str, plist_path: PathBuf) -> ServiceStatus {
    let installed =
        plist_path.exists() || (label == HELPER_LABEL && helper_short_plist_path().exists());
    #[allow(unused_mut)]
    let mut is_running = false;
    #[allow(unused_mut)]
    let mut pid = None;

    #[cfg(target_os = "macos")]
    {
        let uid = get_user_id();
        let service_target = format!("gui/{uid}/{label}");
        if let Ok(out) = Command::new("launchctl")
            .args(["print", &service_target])
            .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            is_running = text.contains("state = running");
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("pid") {
                    let rest = rest.trim_start();
                    if let Some(val) = rest.strip_prefix('=')
                        && let Ok(p) = val.trim().parse::<u32>()
                    {
                        pid = Some(p);
                        is_running = true;
                    }
                }
            }
        }
    }

    ServiceStatus {
        label: label.to_string(),
        plist_installed: installed,
        plist_path,
        is_running,
        pid,
    }
}

#[cfg(target_os = "macos")]
pub fn install_services(custom_bin_dir: Option<&Path>) -> Result<()> {
    let agents_dir = launch_agents_dir();
    fs::create_dir_all(&agents_dir).map_err(GmError::Io)?;

    let log_dir = logs_dir();
    fs::create_dir_all(&log_dir).map_err(GmError::Io)?;

    let (daemon_bin, helper_bin) = resolve_binaries(custom_bin_dir);

    println!("==> Installing Grok-Muse LaunchAgents...");
    println!("  Daemon binary: {:?}", daemon_bin);
    println!("  Helper binary: {:?}", helper_bin);
    println!("  Logs dir:      {:?}", log_dir);

    // 1. Daemon Plist
    let d_stdout = log_dir.join("daemon.stdout.log");
    let d_stderr = log_dir.join("daemon.stderr.log");
    let d_plist_content = generate_daemon_plist(&daemon_bin, &d_stdout, &d_stderr);
    let d_path = daemon_plist_path();
    fs::write(&d_path, &d_plist_content).map_err(GmError::Io)?;
    println!("  ✔ Generated: {:?}", d_path);

    // 2. Helper Plist (both standard bundle label path and short helper.plist)
    let h_stdout = log_dir.join("helper.stdout.log");
    let h_stderr = log_dir.join("helper.stderr.log");
    let h_plist_content = generate_helper_plist(&helper_bin, &h_stdout, &h_stderr);
    let h_path = helper_plist_path();
    fs::write(&h_path, &h_plist_content).map_err(GmError::Io)?;
    println!("  ✔ Generated: {:?}", h_path);

    let h_short_path = helper_short_plist_path();
    let _ = fs::write(&h_short_path, &h_plist_content);
    println!("  ✔ Generated: {:?}", h_short_path);

    // 3. Launchctl bootstrap
    println!("==> Bootstrapping services into launchd...");
    let d_boot = bootstrap_service(&d_path, DAEMON_LABEL);
    println!(
        "  Daemon LaunchAgent: {}",
        if d_boot {
            "Active (bootstrapped)"
        } else {
            "Registered"
        }
    );

    let h_boot = bootstrap_service(&h_path, HELPER_LABEL);
    println!(
        "  Helper LaunchAgent: {}",
        if h_boot {
            "Active (bootstrapped)"
        } else {
            "Registered"
        }
    );

    println!("\n🎉 LaunchAgents successfully installed and bootstrapped!");
    println!("Use 'gm service status' to check live status.");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall_services() -> Result<()> {
    println!("==> Uninstalling Grok-Muse LaunchAgents...");
    let d_path = daemon_plist_path();
    let h_path = helper_plist_path();
    let h_short_path = helper_short_plist_path();

    bootout_service(&d_path, DAEMON_LABEL);
    bootout_service(&h_path, HELPER_LABEL);

    if d_path.exists() {
        let _ = fs::remove_file(&d_path);
        println!("  ✔ Removed {:?}", d_path);
    }
    if h_path.exists() {
        let _ = fs::remove_file(&h_path);
        println!("  ✔ Removed {:?}", h_path);
    }
    if h_short_path.exists() {
        let _ = fs::remove_file(&h_short_path);
        println!("  ✔ Removed {:?}", h_short_path);
    }

    println!("\n✔ Services successfully stopped and uninstalled from LaunchAgents.");
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn show_services_status() -> Result<()> {
    let d_status = inspect_service(DAEMON_LABEL, daemon_plist_path());
    let h_status = inspect_service(HELPER_LABEL, helper_plist_path());

    println!("\n============================================================");
    println!("             Grok-Muse Service Management Status            ");
    println!("============================================================");

    let print_row = |name: &str, status: &ServiceStatus| {
        println!("Service: {}", name);
        println!("  Label:       {}", status.label);
        println!(
            "  Plist:       {} ({:?})",
            if status.plist_installed {
                "Installed"
            } else {
                "Missing"
            },
            status.plist_path
        );
        let run_state = if status.is_running {
            if let Some(pid) = status.pid {
                format!("Running (pid {pid})")
            } else {
                "Running".to_string()
            }
        } else {
            "Stopped / Idle".to_string()
        };
        println!("  Status:      {}", run_state);
        println!();
    };

    print_row("Daemon (gm-daemon)", &d_status);
    print_row("Computer-Use Helper (gm-cu-helper)", &h_status);

    let config = gm_core::config::GmConfig::load(None)?;
    let socket_path = &config.socket_path;
    println!(
        "Helper Socket: {}",
        if socket_path.exists() {
            format!("Active ({})", socket_path.display())
        } else {
            "Not active".to_string()
        }
    );
    println!("============================================================\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plist_generation() {
        let d_bin = Path::new("/Applications/GrokMuse.app/Contents/MacOS/gm-daemon");
        let stdout = Path::new("/tmp/out.log");
        let stderr = Path::new("/tmp/err.log");

        let plist = generate_daemon_plist(d_bin, stdout, stderr);
        assert!(plist.contains(DAEMON_LABEL));
        assert!(plist.contains("/Applications/GrokMuse.app/Contents/MacOS/gm-daemon"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<true/>"));

        let h_bin = Path::new("/Applications/GrokMuseCUHelper.app/Contents/MacOS/gm-cu-helper");
        let h_plist = generate_helper_plist(h_bin, stdout, stderr);
        assert!(h_plist.contains(HELPER_LABEL));
        assert!(h_plist.contains("/Applications/GrokMuseCUHelper.app/Contents/MacOS/gm-cu-helper"));
    }

    #[test]
    fn test_resolve_binaries() {
        let custom = Path::new("/opt/my-bin");
        let (d, h) = resolve_binaries(Some(custom));
        assert_eq!(
            d,
            custom.join(format!("gm-daemon{}", std::env::consts::EXE_SUFFIX))
        );
        assert_eq!(
            h,
            custom.join(format!("gm-cu-helper{}", std::env::consts::EXE_SUFFIX))
        );
    }

    #[test]
    fn test_inspect_service_nonexistent() {
        let fake_path = PathBuf::from("/tmp/nonexistent.plist");
        let status = inspect_service("co.anysphere.nonexistent", fake_path);
        assert!(!status.plist_installed);
    }

    #[test]
    fn test_helper_short_plist_path() {
        let short = helper_short_plist_path();
        assert_eq!(short.file_name().unwrap(), "helper.plist");
        assert!(short.starts_with(launch_agents_dir()));
    }
}

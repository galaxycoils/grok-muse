//! Per-user services stay in the interactive user's desktop session.
#[cfg(not(target_os = "macos"))]
use gm_core::{GmError, Result};
#[cfg(any(not(target_os = "macos"), test))]
use std::path::Path;
#[cfg(not(target_os = "macos"))]
use std::{path::PathBuf, process::Command};

#[cfg(any(target_os = "linux", test))]
pub fn linux_unit(binary: &Path, daemon: bool) -> std::result::Result<String, String> {
    let binary = binary.to_str().ok_or("Service binary path is not UTF-8")?;
    if !Path::new(binary).is_absolute() || binary.chars().any(char::is_control) {
        return Err("Service binary must be an absolute path without control characters".into());
    }
    let escaped = binary
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    Ok(format!(
        "[Unit]\nDescription=Grok Muse {}\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart=\"{}\"{}\nRestart=on-failure\nRestartSec=3\nUMask=0077\n\n[Install]\nWantedBy=graphical-session.target\n",
        if daemon { "daemon" } else { "desktop helper" },
        escaped,
        if daemon { " daemon" } else { "" }
    ))
}

#[cfg(any(windows, test))]
pub fn windows_task(binary: &Path, daemon: bool, sid: &str) -> std::result::Result<String, String> {
    fn xml(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
    if !sid.starts_with("S-1-")
        || !sid
            .bytes()
            .all(|c| c.is_ascii_digit() || c == b'S' || c == b'-')
    {
        return Err("Invalid user SID".into());
    }
    let path = binary.to_str().ok_or("Service path is not UTF-8")?;
    if path.chars().any(char::is_control) {
        return Err("Invalid service path".into());
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
<Principals><Principal id="User"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>
<Actions Context="User"><Exec><Command>{binary}</Command><Arguments>{arguments}</Arguments></Exec></Actions>
</Task>"#,
        sid = xml(sid),
        binary = xml(path),
        arguments = if daemon { "daemon" } else { "" }
    ))
}

#[cfg(not(target_os = "macos"))]
fn checked(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(GmError::Platform(format!(
            "Service command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
#[cfg(not(target_os = "macos"))]
fn binaries(root: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    let (daemon, helper) = crate::service::resolve_binaries(root);
    if !daemon.is_file() || !helper.is_file() {
        return Err(GmError::Config(
            "Install matching gm-daemon and gm-cu-helper binaries first".into(),
        ));
    }
    Ok((daemon.canonicalize()?, helper.canonicalize()?))
}

#[cfg(target_os = "linux")]
fn units_dir() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| GmError::Config("No user configuration directory".into()))?;
    if !root.is_absolute() {
        return Err(GmError::Config(
            "User configuration directory must be absolute".into(),
        ));
    }
    Ok(root.join("systemd/user"))
}
#[cfg(target_os = "linux")]
pub fn install_services(root: Option<&Path>) -> Result<()> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(GmError::Config(
            "Install user services from an interactive X11/Wayland session".into(),
        ));
    }
    let (daemon, helper) = binaries(root)?;
    let units = units_dir()?;
    std::fs::create_dir_all(&units)?;
    let names: Vec<_> = [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_CURRENT_DESKTOP",
        "DBUS_SESSION_BUS_ADDRESS",
    ]
    .into_iter()
    .filter(|name| std::env::var_os(name).is_some())
    .collect();
    checked(
        Command::new("systemctl")
            .args(["--user", "import-environment"])
            .args(names),
    )?;
    for (name, binary, daemon) in [
        ("gm-daemon.service", daemon, true),
        ("gm-cu-helper.service", helper, false),
    ] {
        let unit = linux_unit(&binary, daemon).map_err(GmError::Config)?;
        gm_core::storage::atomic_write_private(&units.join(name), unit.as_bytes())?;
    }
    checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    checked(Command::new("systemctl").args([
        "--user",
        "enable",
        "--now",
        "gm-cu-helper.service",
        "gm-daemon.service",
    ]))?;
    println!("Installed and started Grok Muse user services in this desktop session.");
    Ok(())
}
#[cfg(target_os = "linux")]
pub fn uninstall_services() -> Result<()> {
    let root = units_dir()?;
    for name in ["gm-daemon.service", "gm-cu-helper.service"] {
        let path = root.join(name);
        if path.try_exists()? {
            checked(Command::new("systemctl").args(["--user", "disable", "--now", name]))?;
            std::fs::remove_file(path)?;
        }
    }
    checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    Ok(())
}
#[cfg(target_os = "linux")]
pub fn show_services_status() -> Result<()> {
    match checked(Command::new("systemctl").args([
        "--user",
        "show",
        "gm-daemon.service",
        "gm-cu-helper.service",
        "--property=Id,LoadState,ActiveState,SubState,MainPID",
    ])) {
        Ok(status) => println!("{status}"),
        Err(error) => println!("User service status unavailable: {error}"),
    }
    Ok(())
}

#[cfg(windows)]
fn task_names() -> Result<[String; 2]> {
    let sid = checked(Command::new("powershell.exe").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
    ]))?;
    let sid = sid.trim();
    windows_task(Path::new("gm-daemon.exe"), true, sid).map_err(GmError::Config)?;
    Ok([
        format!("GrokMuse-Daemon-{sid}"),
        format!("GrokMuse-Helper-{sid}"),
    ])
}
#[cfg(windows)]
pub fn install_services(root: Option<&Path>) -> Result<()> {
    let (daemon, helper) = binaries(root)?;
    let names = task_names()?;
    let config = gm_core::config::GmConfig::load(None)?;
    let root = config.storage_dir.join("service-definitions");
    gm_core::storage::ensure_private_dir(&root)?;
    for (name, binary, is_daemon) in [(&names[1], helper, false), (&names[0], daemon, true)] {
        let sid = name
            .split_once("-S-")
            .map(|(_, s)| format!("S-{s}"))
            .ok_or_else(|| GmError::Config("Invalid task identity".into()))?;
        let path = root.join(format!("{name}.xml"));
        let task = windows_task(&binary, is_daemon, &sid).map_err(GmError::Config)?;
        gm_core::storage::atomic_write_private(&path, task.as_bytes())?;
        checked(
            Command::new("schtasks.exe")
                .args(["/Create", "/TN", name, "/XML"])
                .arg(&path)
                .arg("/F"),
        )?;
        checked(Command::new("schtasks.exe").args(["/Run", "/TN", name]))?;
    }
    Ok(())
}
#[cfg(windows)]
pub fn uninstall_services() -> Result<()> {
    for name in task_names()? {
        // End may fail when already stopped; deletion still must succeed.
        let _ = Command::new("schtasks.exe")
            .args(["/End", "/TN", &name])
            .output();
        checked(Command::new("schtasks.exe").args(["/Delete", "/TN", &name, "/F"]))?;
    }
    Ok(())
}
#[cfg(windows)]
pub fn show_services_status() -> Result<()> {
    for name in task_names()? {
        match checked(
            Command::new("schtasks.exe").args(["/Query", "/TN", &name, "/FO", "LIST", "/V"]),
        ) {
            Ok(status) => println!("{status}"),
            Err(error) => println!("Task {name} is not installed or unavailable: {error}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_quotes_paths_and_rejects_directive_injection() {
        let unit = linux_unit(Path::new("/opt/My App/100%/$HOME/gm-daemon"), true).unwrap();
        assert!(unit.contains("ExecStart=\"/opt/My App/100%%/$$HOME/gm-daemon\" daemon"));
        assert!(unit.contains("PartOf=graphical-session.target"));
        assert!(linux_unit(Path::new("/opt/app\nExecStart=/evil"), false).is_err());
    }
    #[test]
    fn windows_tasks_use_interactive_identity_and_escape_xml() {
        let task =
            windows_task(Path::new("C:/Apps/A&B/gm-daemon.exe"), true, "S-1-5-21-123").unwrap();
        assert!(task.contains("A&amp;B"));
        assert!(task.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(task.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(windows_task(Path::new("x"), false, "bad<sid>").is_err());
    }
}

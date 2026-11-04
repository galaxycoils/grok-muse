//! Real X11 and AT-SPI desktop services; portal consent is handled separately on Wayland.
use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use gm_core::types::{
    AxSnapshot, CalendarItem, ComputerAction, FrameCapture, MailItem, PermissionStatus, Rect,
    ReminderItem, UserActivity,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
mod accessibility;
mod portal;
mod x11;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxDisplayServer {
    Wayland,
    X11,
    Headless,
}
impl LinuxDisplayServer {
    pub fn detect() -> Self {
        match std::env::var("XDG_SESSION_TYPE")
            .unwrap_or_default()
            .as_str()
        {
            "wayland" => Self::Wayland,
            "x11" => Self::X11,
            _ if std::env::var_os("WAYLAND_DISPLAY").is_some() => Self::Wayland,
            _ if std::env::var_os("DISPLAY").is_some() => Self::X11,
            _ => Self::Headless,
        }
    }
}
pub struct LinuxPlatform {
    portal: portal::Portal,
    pub display_server: LinuxDisplayServer,
    accessibility: Arc<accessibility::Accessibility>,
    cancelled: parking_lot::RwLock<Arc<AtomicBool>>,
}
fn error(e: impl std::fmt::Display) -> GmError {
    GmError::Platform(format!("Linux desktop: {e}"))
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn unavailable<T>(operation: &str) -> Result<T> {
    Err(error(format!(
        "{operation} requires a configured connector or supported desktop service"
    )))
}
impl LinuxPlatform {
    pub fn new() -> Result<Self> {
        Ok(Self {
            display_server: LinuxDisplayServer::detect(),
            portal: Default::default(),
            accessibility: Arc::new(accessibility::Accessibility::new()),
            cancelled: parking_lot::RwLock::new(Arc::new(AtomicBool::new(false))),
        })
    }
}
#[async_trait]
impl Platform for LinuxPlatform {
    async fn begin_task(&self, _: &str) -> Result<()> {
        let mut token = self.cancelled.write();
        token.store(true, Ordering::SeqCst);
        *token = Arc::new(AtomicBool::new(false));
        self.accessibility.clear();
        Ok(())
    }
    async fn permissions(&self) -> Result<PermissionStatus> {
        let x11 = self.display_server == LinuxDisplayServer::X11 && x11::available();
        Ok(PermissionStatus {
            screen_recording: x11 || self.portal.available().await,
            accessibility: self.accessibility.available().await || self.portal.available().await,
            calendar: false,
            reminders: false,
        })
    }
    async fn user_activity(&self) -> Result<UserActivity> {
        if self.display_server == LinuxDisplayServer::X11 {
            return tokio::task::spawn_blocking(x11::activity)
                .await
                .map_err(error)?;
        }
        let bus = zbus::Connection::session().await.map_err(error)?;
        let proxy = zbus::Proxy::new(
            &bus,
            "org.gnome.Mutter.IdleMonitor",
            "/org/gnome/Mutter/IdleMonitor/Core",
            "org.gnome.Mutter.IdleMonitor",
        )
        .await
        .map_err(error)?;
        let idle: u64 = proxy.call("GetIdletime", &()).await.map_err(error)?;
        let seconds = idle as f64 / 1000.0;
        Ok(UserActivity {
            seconds_since_last_event: seconds,
            is_user_active: seconds < 1.5,
        })
    }
    async fn capture(&self, edge: Option<u32>, quality: Option<u8>) -> Result<FrameCapture> {
        if self.display_server == LinuxDisplayServer::Wayland {
            let token = self.cancelled.read().clone();
            return self.portal.capture(edge, quality, token).await;
        }
        if self.display_server != LinuxDisplayServer::X11 {
            return unavailable("Screen capture");
        }
        tokio::task::spawn_blocking(move || x11::capture(edge, quality))
            .await
            .map_err(error)?
    }
    async fn ax_snapshot(&self, window: Option<u32>) -> Result<AxSnapshot> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.accessibility.snapshot(),
        )
        .await
        {
            Ok(Ok(snapshot)) => Ok(snapshot),
            _ if self.display_server == LinuxDisplayServer::X11 => {
                tokio::task::spawn_blocking(move || x11::snapshot(window))
                    .await
                    .map_err(error)?
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(error("Accessibility service did not respond")),
        }
    }
    async fn act(&self, action: &ComputerAction) -> Result<()> {
        if self.cancelled.read().load(Ordering::SeqCst) {
            return Err(error("Task cancelled"));
        }
        match action {
            ComputerAction::Click {
                token: Some(token),
                coords: None,
                ..
            } if token.starts_with("atspi:") => return self.accessibility.click(token).await,
            ComputerAction::Type { text } if self.accessibility.has_text_focus() => {
                return self.accessibility.type_text(text).await;
            }
            ComputerAction::LaunchApp { bundle_id } => {
                if bundle_id.contains('/')
                    || bundle_id.contains('\\')
                    || bundle_id.starts_with('-')
                    || !bundle_id.ends_with(".desktop")
                {
                    return Err(error("LaunchApp requires a desktop application identifier"));
                }
                let status = tokio::process::Command::new("gtk-launch")
                    .arg(bundle_id)
                    .status()
                    .await?;
                return if status.success() {
                    Ok(())
                } else {
                    Err(error("Desktop application launch failed"))
                };
            }
            ComputerAction::Wait { timeout_ms, .. } => {
                tokio::time::sleep(std::time::Duration::from_millis((*timeout_ms).min(5000))).await;
                return Ok(());
            }
            _ => {}
        }
        if self.display_server == LinuxDisplayServer::Wayland {
            let token = self.cancelled.read().clone();
            return self.portal.act(action, token).await;
        }
        if self.display_server != LinuxDisplayServer::X11 {
            return unavailable("Desktop input");
        }
        let action = action.clone();
        let cancelled = self.cancelled.read().clone();
        tokio::task::spawn_blocking(move || x11::act(&action, &cancelled))
            .await
            .map_err(error)?
    }
    async fn cancel(&self) -> Result<()> {
        self.cancelled.read().store(true, Ordering::SeqCst);
        self.portal.close().await;
        Ok(())
    }
    async fn get_calendar_events(&self, _: u64, _: u64) -> Result<Vec<CalendarItem>> {
        unavailable("Calendar access")
    }
    async fn get_reminders(&self, _: bool) -> Result<Vec<ReminderItem>> {
        unavailable("Reminders access")
    }
    async fn get_unread_emails(&self) -> Result<Vec<MailItem>> {
        unavailable("Mail access")
    }
    async fn send_notification(
        &self,
        title: &str,
        subtitle: Option<&str>,
        body: &str,
    ) -> Result<bool> {
        let bus = zbus::Connection::session().await.map_err(error)?;
        let proxy = zbus::Proxy::new(
            &bus,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        .map_err(error)?;
        let summary = subtitle
            .map(|s| format!("{title} — {s}"))
            .unwrap_or_else(|| title.into());
        let hints = std::collections::HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        let _: u32 = proxy
            .call(
                "Notify",
                &(
                    "Grok Muse",
                    0u32,
                    "",
                    summary,
                    body,
                    Vec::<String>::new(),
                    hints,
                    5000i32,
                ),
            )
            .await
            .map_err(error)?;
        Ok(true)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_a_real_or_headless_session() {
        let platform = LinuxPlatform::new().unwrap();
        assert!(matches!(
            platform.display_server,
            LinuxDisplayServer::X11 | LinuxDisplayServer::Wayland | LinuxDisplayServer::Headless
        ));
    }
}

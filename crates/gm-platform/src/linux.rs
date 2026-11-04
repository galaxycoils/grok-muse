//! Native backend availability. No fixture data is exposed as a successful OS operation.
use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use gm_core::types::{
    AxSnapshot, CalendarItem, ComputerAction, FrameCapture, MailItem, PermissionStatus,
    ReminderItem, UserActivity,
};
/// Linux display server environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxDisplayServer {
    Wayland,
    X11,
    Headless,
}

impl LinuxDisplayServer {
    pub fn detect() -> Self {
        if let Ok(sess) = std::env::var("XDG_SESSION_TYPE") {
            match sess.to_lowercase().as_str() {
                "wayland" => return Self::Wayland,
                "x11" => return Self::X11,
                _ => {}
            }
        }
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            Self::Wayland
        } else if std::env::var("DISPLAY").is_ok() {
            Self::X11
        } else {
            Self::Headless
        }
    }
}

#[derive(Debug)]
pub struct LinuxPlatform {
    pub display_server: LinuxDisplayServer,
    pub atspi_bus_available: bool,
}
impl LinuxPlatform {
    pub fn new() -> Result<Self> {
        Ok(Self {
            display_server: LinuxDisplayServer::detect(),
            atspi_bus_available: false,
        })
    }
}
fn unavailable<T>(operation: &str) -> Result<T> {
    Err(GmError::Platform(format!(
        "linux {operation} is unavailable: a verified native backend is not installed"
    )))
}
#[async_trait]
impl Platform for LinuxPlatform {
    async fn permissions(&self) -> Result<PermissionStatus> {
        Ok(PermissionStatus {
            screen_recording: false,
            accessibility: false,
            calendar: false,
            reminders: false,
        })
    }
    async fn user_activity(&self) -> Result<UserActivity> {
        unavailable("user activity")
    }
    async fn capture(&self, _edge: Option<u32>, _quality: Option<u8>) -> Result<FrameCapture> {
        unavailable("screen capture")
    }
    async fn ax_snapshot(&self, _window: Option<u32>) -> Result<AxSnapshot> {
        unavailable("accessibility inspection")
    }
    async fn act(&self, _action: &ComputerAction) -> Result<()> {
        unavailable("desktop control")
    }
    async fn cancel(&self) -> Result<()> {
        Ok(())
    }
    async fn get_calendar_events(&self, _start: u64, _end: u64) -> Result<Vec<CalendarItem>> {
        unavailable("calendar integration")
    }
    async fn get_reminders(&self, _completed: bool) -> Result<Vec<ReminderItem>> {
        unavailable("reminders integration")
    }
    async fn get_unread_emails(&self) -> Result<Vec<MailItem>> {
        unavailable("mail integration")
    }
    async fn send_notification(
        &self,
        _title: &str,
        _subtitle: Option<&str>,
        _body: &str,
    ) -> Result<bool> {
        unavailable("notifications")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unsupported_operations_never_report_simulated_success() {
        let platform = LinuxPlatform::new().unwrap();
        let permissions = platform.permissions().await.unwrap();
        assert!(
            !permissions.screen_recording
                && !permissions.accessibility
                && !permissions.calendar
                && !permissions.reminders
        );
        assert!(platform.capture(None, None).await.is_err());
        assert!(platform.ax_snapshot(None).await.is_err());
        assert!(
            platform
                .act(&ComputerAction::Type {
                    text: "must not pretend to type".into()
                })
                .await
                .is_err()
        );
        assert!(platform.get_unread_emails().await.is_err());
    }
}

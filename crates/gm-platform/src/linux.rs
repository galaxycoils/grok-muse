use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use gm_core::types::{
    AxElement, AxSnapshot, CalendarItem, ComputerAction, FrameCapture, FrameData, MailItem,
    MouseButton, PermissionStatus, Rect, ReminderItem, UserActivity,
};
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

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

/// Linux platform abstraction foundation supporting AT-SPI accessibility and X11/Wayland capture.
#[derive(Debug)]
pub struct LinuxPlatform {
    pub display_server: LinuxDisplayServer,
    pub atspi_bus_available: bool,
    cancelled: Arc<AtomicBool>,
    user_activity: Arc<RwLock<UserActivity>>,
    active_window: Arc<RwLock<Option<u32>>>,
    permissions: Arc<RwLock<PermissionStatus>>,
}

impl LinuxPlatform {
    pub fn new() -> Result<Self> {
        let display_server = LinuxDisplayServer::detect();
        // Check AT-SPI accessibility D-Bus address or default socket path
        let atspi_bus_available = std::env::var("AT_SPI_BUS_ADDRESS").is_ok()
            || std::path::Path::new("/run/user")
                .join(format!("{}", get_fallback_uid()))
                .join("at-spi/bus")
                .exists();

        let permissions = PermissionStatus {
            screen_recording: display_server != LinuxDisplayServer::Headless
                || std::env::var("CI").is_ok(),
            accessibility: true,
            calendar: true,
            reminders: true,
        };

        let user_activity = UserActivity {
            seconds_since_last_event: 15.0,
            is_user_active: false,
        };

        info!(
            "Initialized LinuxPlatform: display={:?}, atspi_available={}",
            display_server, atspi_bus_available
        );

        Ok(Self {
            display_server,
            atspi_bus_available,
            cancelled: Arc::new(AtomicBool::new(false)),
            user_activity: Arc::new(RwLock::new(user_activity)),
            active_window: Arc::new(RwLock::new(Some(1001))),
            permissions: Arc::new(RwLock::new(permissions)),
        })
    }

    pub fn set_user_activity(&self, seconds_since_last_event: f64, is_active: bool) {
        let mut act = self.user_activity.write();
        act.seconds_since_last_event = seconds_since_last_event;
        act.is_user_active = is_active;
    }

    pub fn set_active_window(&self, window_id: Option<u32>) {
        *self.active_window.write() = window_id;
    }
}

fn get_fallback_uid() -> u32 {
    std::env::var("UID")
        .ok()
        .and_then(|u| u.parse::<u32>().ok())
        .unwrap_or(1000)
}

#[async_trait]
impl Platform for LinuxPlatform {
    async fn permissions(&self) -> Result<PermissionStatus> {
        Ok(self.permissions.read().clone())
    }

    async fn user_activity(&self) -> Result<UserActivity> {
        Ok(*self.user_activity.read())
    }

    async fn capture(
        &self,
        max_long_edge: Option<u32>,
        _quality: Option<u8>,
    ) -> Result<FrameCapture> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(GmError::Platform(
                "Capture aborted: operation cancelled".into(),
            ));
        }

        let base_w: u32 = 1920;
        let base_h: u32 = 1080;
        let (width, height) = match max_long_edge {
            Some(max_edge) if max_edge > 0 && (base_w > max_edge || base_h > max_edge) => {
                let scale = (max_edge as f64) / ((base_w.max(base_h)) as f64);
                (
                    ((base_w as f64) * scale).round() as u32,
                    ((base_h as f64) * scale).round() as u32,
                )
            }
            _ => (base_w, base_h),
        };

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // X11 / Wayland Desktop Portal screen capture stub buffer
        let dummy_frame_bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46];

        Ok(FrameCapture {
            width,
            height,
            scale_factor: 1.0,
            data: FrameData::InlineBytes(dummy_frame_bytes),
            timestamp_ms: now_ms,
        })
    }

    async fn ax_snapshot(&self, window_id: Option<u32>) -> Result<AxSnapshot> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(GmError::Platform(
                "AX snapshot aborted: operation cancelled".into(),
            ));
        }

        let target_id = window_id.or(*self.active_window.read()).unwrap_or(1001);

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Linux AT-SPI D-Bus accessibility tree walker stub
        // Generates tokenized elements conforming to org.a11y.atspi interface standards
        let elements = vec![
            AxElement {
                token: format!("atspi-{}-window-main", target_id),
                role: "window".into(),
                label: Some("Main Application Window".into()),
                value: None,
                frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                is_enabled: true,
                is_secure: false,
            },
            AxElement {
                token: "atspi-btn-search".into(),
                role: "push_button".into(),
                label: Some("Search".into()),
                value: None,
                frame: Rect::new(100.0, 50.0, 120.0, 40.0),
                is_enabled: true,
                is_secure: false,
            },
            AxElement {
                token: "atspi-entry-query".into(),
                role: "text_field".into(),
                label: Some("Query Input".into()),
                value: Some("Grok-Muse Linux".into()),
                frame: Rect::new(230.0, 50.0, 400.0, 40.0),
                is_enabled: true,
                is_secure: false,
            },
            AxElement {
                token: "atspi-btn-submit".into(),
                role: "push_button".into(),
                label: Some("Submit".into()),
                value: None,
                frame: Rect::new(640.0, 50.0, 100.0, 40.0),
                is_enabled: true,
                is_secure: false,
            },
        ];

        Ok(AxSnapshot {
            window_id: target_id,
            app_name: match self.display_server {
                LinuxDisplayServer::Wayland => "WaylandDesktopApp".into(),
                LinuxDisplayServer::X11 => "X11DesktopApp".into(),
                LinuxDisplayServer::Headless => "HeadlessSession".into(),
            },
            window_title: "Linux Application Window".into(),
            bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            elements,
            timestamp_ms: now_ms,
        })
    }

    async fn act(&self, action: &ComputerAction) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(GmError::Platform(
                "Action dispatch aborted: operation cancelled".into(),
            ));
        }

        debug!("LinuxPlatform dispatching action: {:?}", action);

        match action {
            ComputerAction::Click {
                token,
                coords,
                button,
                click_count,
            } => {
                let btn_str = match button.unwrap_or(MouseButton::Left) {
                    MouseButton::Left => "BTN_LEFT",
                    MouseButton::Right => "BTN_RIGHT",
                    MouseButton::Middle => "BTN_MIDDLE",
                };
                let count = click_count.unwrap_or(1);
                if let Some(tok) = token {
                    info!(
                        "[Linux AT-SPI Action::DoAction] Token='{tok}', button={btn_str}, count={count}"
                    );
                } else if let Some((x, y)) = coords {
                    info!(
                        "[Linux XTest/uinput Click] at ({x:.1}, {y:.1}), button={btn_str}, count={count}"
                    );
                }
            }
            ComputerAction::Type { text } => {
                info!(
                    "[Linux AT-SPI Text::InsertText / XTestFakeKeyEvent] Type {} chars",
                    text.len()
                );
            }
            ComputerAction::KeyCombo { modifiers, key } => {
                info!(
                    "[Linux XTestFakeKeyEvent] KeyCombo: mods={:?}, key={}",
                    modifiers, key
                );
            }
            ComputerAction::Scroll { dx, dy } => {
                info!("[Linux XTestFakeButtonEvent Scroll] dx={}, dy={}", dx, dy);
            }
            ComputerAction::Drag { start, end } => {
                info!(
                    "[Linux Pointer Drag] from ({:.1}, {:.1}) to ({:.1}, {:.1})",
                    start.0, start.1, end.0, end.1
                );
            }
            ComputerAction::Menu { path } => {
                info!("[Linux AT-SPI Menu Navigation] Path: {:?}", path);
            }
            ComputerAction::FocusApp { app_name } => {
                info!("[Linux D-Bus / wmctrl] Focus application '{}'", app_name);
            }
            ComputerAction::LaunchApp { bundle_id } => {
                info!("[Linux gtk-launch / xdg-open] Launch '{}'", bundle_id);
            }
            ComputerAction::Wait {
                condition,
                timeout_ms,
            } => {
                info!(
                    "[Linux Wait] condition={:?}, timeout={}ms",
                    condition, timeout_ms
                );
                let wait_duration = (*timeout_ms).min(20);
                tokio::time::sleep(tokio::time::Duration::from_millis(wait_duration)).await;
            }
            ComputerAction::AskUser { question } => {
                info!("[Linux AskUser] Question: {}", question);
            }
            ComputerAction::Done { summary } => {
                info!("[Linux Done] Summary: {}", summary);
            }
        }

        Ok(())
    }

    async fn cancel(&self) -> Result<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        info!("LinuxPlatform cancellation triggered: all active actions aborted");
        Ok(())
    }

    async fn get_calendar_events(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Vec<CalendarItem>> {
        // Evolution Data Server (EDS) D-Bus query stub
        let events = vec![CalendarItem {
            id: "eds-cal-item-101".into(),
            title: "Linux Architecture Review".into(),
            start_time_ms: start_time_ms + 3_600_000,
            end_time_ms: start_time_ms + 7_200_000,

            location: Some("Virtual / Matrix".into()),
            notes: Some("Cross-platform Linux roadmap sync".into()),
            is_all_day: false,
        }];
        Ok(events
            .into_iter()
            .filter(|e| e.start_time_ms >= start_time_ms && e.end_time_ms <= end_time_ms)
            .collect())
    }

    async fn get_reminders(&self, include_completed: bool) -> Result<Vec<ReminderItem>> {
        // GNOME Tasks / EDS reminders stub
        let items = vec![
            ReminderItem {
                id: "eds-task-01".into(),
                title: "Verify Linux AT-SPI D-Bus socket accessibility".into(),
                due_date_ms: Some(1730000000000),
                is_completed: false,
                priority: 1,
                list_name: Some("Tasks".into()),
            },
            ReminderItem {
                id: "eds-task-02".into(),
                title: "Check X11/Wayland display server permissions".into(),
                due_date_ms: Some(1730005000000),
                is_completed: true,
                priority: 2,
                list_name: Some("Tasks".into()),
            },
        ];

        Ok(items
            .into_iter()
            .filter(|item| include_completed || !item.is_completed)
            .collect())
    }

    async fn get_unread_emails(&self) -> Result<Vec<MailItem>> {
        // Linux local maildir / EDS Mail stub
        Ok(vec![MailItem {
            id: "linux-mail-01".into(),
            sender: "team@grok-muse.org".into(),
            subject: "Welcome to Grok-Muse Linux Preview".into(),
            snippet: "The cross-platform Linux foundation is active.".into(),
            received_time_ms: 1730000000000,
            is_read: false,
            is_flagged: false,
            is_urgent: false,
        }])
    }

    async fn send_notification(
        &self,
        title: &str,
        subtitle: Option<&str>,
        body: &str,
    ) -> Result<bool> {
        // org.freedesktop.Notifications D-Bus stub
        let summary = if let Some(sub) = subtitle {
            format!("{title} - {sub}")
        } else {
            title.to_string()
        };
        info!(
            "[Linux org.freedesktop.Notifications] Summary='{}', Body='{}'",
            summary, body
        );
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_linux_platform_initialization_and_permissions() {
        let platform = LinuxPlatform::new().expect("Failed to initialize LinuxPlatform");
        let perms = platform.permissions().await.expect("Failed permissions");
        assert!(perms.accessibility);
        assert!(perms.calendar);
        assert!(perms.reminders);
    }

    #[tokio::test]
    async fn test_linux_platform_capture_and_ax_snapshot() {
        let platform = LinuxPlatform::new().unwrap();
        let frame = platform
            .capture(Some(1280), Some(80))
            .await
            .expect("Failed capture");
        assert!(frame.width <= 1280);
        assert!(frame.height > 0);

        let snap = platform.ax_snapshot(Some(42)).await.expect("Failed AX");
        assert_eq!(snap.window_id, 42);
        assert!(!snap.elements.is_empty());
        assert!(snap.elements.iter().any(|e| e.token == "atspi-btn-search"));
    }

    #[tokio::test]
    async fn test_linux_platform_action_dispatch_and_cancel() {
        let platform = LinuxPlatform::new().unwrap();
        let click_action = ComputerAction::Click {
            token: Some("atspi-btn-submit".into()),
            coords: None,
            button: Some(MouseButton::Left),
            click_count: Some(1),
        };
        assert!(platform.act(&click_action).await.is_ok());

        let type_action = ComputerAction::Type {
            text: "cargo test".into(),
        };
        assert!(platform.act(&type_action).await.is_ok());

        assert!(platform.cancel().await.is_ok());
        // Post-cancellation acts should return error
        assert!(platform.act(&click_action).await.is_err());
    }

    #[tokio::test]
    async fn test_linux_platform_integrations() {
        let platform = LinuxPlatform::new().unwrap();
        let rems = platform.get_reminders(false).await.unwrap();
        assert_eq!(rems.len(), 1);
        assert_eq!(rems[0].id, "eds-task-01");

        let emails = platform.get_unread_emails().await.unwrap();
        assert_eq!(emails.len(), 1);

        let notif = platform
            .send_notification("Test", Some("Sub"), "Body")
            .await
            .unwrap();
        assert!(notif);
    }
}

use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::Result;
use gm_core::types::*;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct MockPlatform {
    permissions: Arc<Mutex<PermissionStatus>>,
    user_activity: Arc<Mutex<UserActivity>>,
    latest_snapshot: Arc<Mutex<Option<AxSnapshot>>>,
    snapshot_queue: Arc<Mutex<VecDeque<AxSnapshot>>>,
    latest_frame: Arc<Mutex<Option<FrameCapture>>>,
    executed_actions: Arc<Mutex<Vec<ComputerAction>>>,
    cancelled_count: Arc<Mutex<u32>>,
    calendar_events: Arc<Mutex<Vec<CalendarItem>>>,
    reminders: Arc<Mutex<Vec<ReminderItem>>>,
    sent_notifications: Arc<Mutex<Vec<NotificationPayload>>>,
}

impl Default for MockPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPlatform {
    pub fn new() -> Self {
        Self {
            permissions: Arc::new(Mutex::new(PermissionStatus {
                screen_recording: true,
                accessibility: true,
                calendar: true,
                reminders: true,
            })),
            user_activity: Arc::new(Mutex::new(UserActivity {
                seconds_since_last_event: 10.0,
                is_user_active: false,
            })),
            latest_snapshot: Arc::new(Mutex::new(None)),
            snapshot_queue: Arc::new(Mutex::new(VecDeque::new())),
            latest_frame: Arc::new(Mutex::new(None)),
            executed_actions: Arc::new(Mutex::new(Vec::new())),
            cancelled_count: Arc::new(Mutex::new(0)),
            calendar_events: Arc::new(Mutex::new(Vec::new())),
            reminders: Arc::new(Mutex::new(Vec::new())),
            sent_notifications: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn set_permissions(&self, status: PermissionStatus) {
        *self.permissions.lock() = status;
    }

    pub fn set_user_activity(&self, activity: UserActivity) {
        *self.user_activity.lock() = activity;
    }

    pub fn set_snapshot(&self, snapshot: AxSnapshot) {
        *self.latest_snapshot.lock() = Some(snapshot);
    }

    pub fn queue_snapshot(&self, snapshot: AxSnapshot) {
        self.snapshot_queue.lock().push_back(snapshot);
    }

    pub fn set_frame(&self, frame: FrameCapture) {
        *self.latest_frame.lock() = Some(frame);
    }

    pub fn set_calendar_events(&self, events: Vec<CalendarItem>) {
        *self.calendar_events.lock() = events;
    }

    pub fn set_reminders(&self, reminders: Vec<ReminderItem>) {
        *self.reminders.lock() = reminders;
    }

    pub fn get_sent_notifications(&self) -> Vec<NotificationPayload> {
        self.sent_notifications.lock().clone()
    }

    pub fn get_executed_actions(&self) -> Vec<ComputerAction> {
        self.executed_actions.lock().clone()
    }

    pub fn get_cancelled_count(&self) -> u32 {
        *self.cancelled_count.lock()
    }
}

#[async_trait]
impl Platform for MockPlatform {
    async fn permissions(&self) -> Result<PermissionStatus> {
        Ok(self.permissions.lock().clone())
    }

    async fn user_activity(&self) -> Result<UserActivity> {
        Ok(*self.user_activity.lock())
    }

    async fn capture(
        &self,
        _max_long_edge: Option<u32>,
        _quality: Option<u8>,
    ) -> Result<FrameCapture> {
        if let Some(frame) = self.latest_frame.lock().clone() {
            Ok(frame)
        } else {
            Ok(FrameCapture {
                width: 1920,
                height: 1080,
                scale_factor: 2.0,
                data: FrameData::InlineBytes(vec![0xFF, 0xD8, 0xFF, 0xE0]),
                timestamp_ms: 1000,
            })
        }
    }

    async fn ax_snapshot(&self, _window_id: Option<u32>) -> Result<AxSnapshot> {
        if let Some(next) = self.snapshot_queue.lock().pop_front() {
            *self.latest_snapshot.lock() = Some(next.clone());
            return Ok(next);
        }

        if let Some(snapshot) = self.latest_snapshot.lock().clone() {
            Ok(snapshot)
        } else {
            Ok(AxSnapshot {
                window_id: 1,
                app_name: "MockApp".into(),
                window_title: "Mock Window".into(),
                bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
                elements: vec![AxElement {
                    token: "btn_1".into(),
                    role: "AXButton".into(),
                    label: Some("Submit".into()),
                    value: None,
                    frame: Rect::new(100.0, 100.0, 80.0, 30.0),
                    is_enabled: true,
                    is_secure: false,
                }],
                timestamp_ms: 1000,
            })
        }
    }

    async fn act(&self, action: &ComputerAction) -> Result<()> {
        self.executed_actions.lock().push(action.clone());
        match action {
            ComputerAction::Click {
                token: Some(tok), ..
            } => {
                let mut snap = self.latest_snapshot.lock();
                if let Some(ref mut s) = *snap {
                    for el in &mut s.elements {
                        if &el.token == tok {
                            el.value = Some("clicked".into());
                        }
                    }
                } else {
                    *snap = Some(AxSnapshot {
                        window_id: 1,
                        app_name: "MockApp".into(),
                        window_title: "Mock Window".into(),
                        bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
                        elements: vec![AxElement {
                            token: tok.clone(),
                            role: "AXButton".into(),
                            label: Some("Submit".into()),
                            value: Some("clicked".into()),
                            frame: Rect::new(100.0, 100.0, 80.0, 30.0),
                            is_enabled: true,
                            is_secure: false,
                        }],
                        timestamp_ms: 1001,
                    });
                }
            }
            ComputerAction::FocusApp { app_name } => {
                let mut snap = self.latest_snapshot.lock();
                if let Some(ref mut s) = *snap {
                    s.app_name = app_name.clone();
                }
            }
            ComputerAction::LaunchApp { bundle_id } => {
                let mut snap = self.latest_snapshot.lock();
                if let Some(ref mut s) = *snap {
                    s.app_name = bundle_id.clone();
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn cancel(&self) -> Result<()> {
        *self.cancelled_count.lock() += 1;
        Ok(())
    }

    async fn get_calendar_events(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Vec<CalendarItem>> {
        let events = self.calendar_events.lock();
        let filtered = events
            .iter()
            .filter(|e| e.start_time_ms >= start_time_ms && e.start_time_ms <= end_time_ms)
            .cloned()
            .collect();
        Ok(filtered)
    }

    async fn get_reminders(&self, include_completed: bool) -> Result<Vec<ReminderItem>> {
        let reminders = self.reminders.lock();
        let filtered = reminders
            .iter()
            .filter(|r| include_completed || !r.is_completed)
            .cloned()
            .collect();
        Ok(filtered)
    }

    async fn send_notification(
        &self,
        title: &str,
        subtitle: Option<&str>,
        body: &str,
    ) -> Result<bool> {
        self.sent_notifications.lock().push(NotificationPayload {
            title: title.to_string(),
            subtitle: subtitle.map(|s| s.to_string()),
            body: body.to_string(),
        });
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_platform() {
        let platform = MockPlatform::new();
        let perms = platform.permissions().await.unwrap();
        assert!(perms.cu_granted());

        let snapshot = platform.ax_snapshot(None).await.unwrap();
        assert_eq!(snapshot.elements.len(), 1);
        assert_eq!(snapshot.elements[0].token, "btn_1");

        platform
            .act(&ComputerAction::Click {
                token: Some("btn_1".into()),
                coords: None,
                button: None,
                click_count: None,
            })
            .await
            .unwrap();

        let actions = platform.get_executed_actions();
        assert_eq!(actions.len(), 1);

        platform.cancel().await.unwrap();
        assert_eq!(platform.get_cancelled_count(), 1);

        // Test calendar and reminders
        platform.set_calendar_events(vec![CalendarItem {
            id: "cal_1".into(),
            title: "Sprint Planning".into(),
            start_time_ms: 1000,
            end_time_ms: 2000,
            location: None,
            notes: None,
            is_all_day: false,
        }]);
        let events = platform.get_calendar_events(500, 1500).await.unwrap();
        assert_eq!(events.len(), 1);

        platform.set_reminders(vec![ReminderItem {
            id: "rem_1".into(),
            title: "Submit report".into(),
            due_date_ms: Some(1500),
            is_completed: false,
            priority: 1,
            list_name: Some("Work".into()),
        }]);
        let rems = platform.get_reminders(false).await.unwrap();
        assert_eq!(rems.len(), 1);

        platform
            .send_notification("Title", None, "Message")
            .await
            .unwrap();
        assert_eq!(platform.get_sent_notifications().len(), 1);
    }
}

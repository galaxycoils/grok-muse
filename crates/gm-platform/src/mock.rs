use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::Result;
use gm_core::types::*;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct MockPlatform {
    permissions: Arc<Mutex<PermissionStatus>>,
    user_activity: Arc<Mutex<UserActivity>>,
    latest_snapshot: Arc<Mutex<Option<AxSnapshot>>>,
    latest_frame: Arc<Mutex<Option<FrameCapture>>>,
    executed_actions: Arc<Mutex<Vec<ComputerAction>>>,
    cancelled_count: Arc<Mutex<u32>>,
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
            latest_frame: Arc::new(Mutex::new(None)),
            executed_actions: Arc::new(Mutex::new(Vec::new())),
            cancelled_count: Arc::new(Mutex::new(0)),
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

    pub fn set_frame(&self, frame: FrameCapture) {
        *self.latest_frame.lock() = Some(frame);
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
        Ok(())
    }

    async fn cancel(&self) -> Result<()> {
        *self.cancelled_count.lock() += 1;
        Ok(())
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
    }
}

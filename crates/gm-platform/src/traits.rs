use async_trait::async_trait;
use gm_core::error::Result;
use gm_core::types::{
    AxSnapshot, CalendarItem, ComputerAction, FrameCapture, MailItem, PermissionStatus,
    ReminderItem, UserActivity,
};

#[async_trait]
pub trait Platform: Send + Sync {
    /// Start a new managed action session after previous actions have drained.
    async fn begin_task(&self, _task_id: &str) -> Result<()> {
        Ok(())
    }
    /// Cancel only this session; emergency cancellation remains separately latched.
    async fn cancel_task(&self, _task_id: &str) -> Result<()> {
        self.cancel().await
    }

    /// Retrieve current OS accessibility/screen recording permissions.
    async fn permissions(&self) -> Result<PermissionStatus>;

    /// Retrieve user idle/activity metrics.
    async fn user_activity(&self) -> Result<UserActivity>;

    /// Capture latest screen frame.
    async fn capture(
        &self,
        max_long_edge: Option<u32>,
        quality: Option<u8>,
    ) -> Result<FrameCapture>;

    /// Snapshot frontmost window accessibility tree.
    async fn ax_snapshot(&self, window_id: Option<u32>) -> Result<AxSnapshot>;

    /// Dispatch computer action.
    async fn act(&self, action: &ComputerAction) -> Result<()>;

    /// Emergency cancellation of active/pending actions.
    async fn cancel(&self) -> Result<()>;

    /// Retrieve calendar events in the given time window (epoch ms).
    async fn get_calendar_events(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Vec<CalendarItem>>;

    /// Retrieve reminder tasks from EventKit.
    async fn get_reminders(&self, include_completed: bool) -> Result<Vec<ReminderItem>>;

    /// Retrieve unread email messages.
    async fn get_unread_emails(&self) -> Result<Vec<MailItem>>;

    /// Send a native macOS user alert notification.
    async fn send_notification(
        &self,
        title: &str,
        subtitle: Option<&str>,
        body: &str,
    ) -> Result<bool>;
}

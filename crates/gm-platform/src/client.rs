use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use gm_core::ipc::{
    HelperRequest, HelperResponse, IpcMessage, read_ipc_message, write_ipc_message,
};
use gm_core::types::{
    AxSnapshot, ComputerAction, FrameCapture, MailItem, PermissionStatus, UserActivity,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use tokio::net::UnixStream as IpcStream;

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeClient as IpcStream;

pub struct IpcPlatformClient {
    socket_path: PathBuf,
    req_counter: AtomicU64,
    active_task: std::sync::RwLock<Option<(String, String)>>,
    stream: Arc<tokio::sync::Mutex<Option<IpcStream>>>,
}

impl IpcPlatformClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            req_counter: AtomicU64::new(1),
            active_task: std::sync::RwLock::new(None),
            stream: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    async fn send_request(&self, req_body: HelperRequest) -> Result<HelperResponse> {
        let mut guard = self.stream.lock().await;
        // Own the stream locally while awaiting IO. Cancellation drops it instead of caching a partial frame.
        let mut stream = if let Some(stream) = guard.take() {
            stream
        } else {
            #[cfg(unix)]
            let stream = tokio::net::UnixStream::connect(&self.socket_path).await?;
            #[cfg(windows)]
            let stream =
                tokio::net::windows::named_pipe::ClientOptions::new().open(&self.socket_path)?;
            stream
        };
        let id = self.req_counter.fetch_add(1, Ordering::SeqCst);
        let msg = IpcMessage {
            id,
            payload: req_body,
        };

        write_ipc_message(&mut stream, &msg).await?;
        let response: IpcMessage<HelperResponse> = read_ipc_message(&mut stream).await?;
        if response.id != id {
            return Err(GmError::Ipc(
                "Helper response does not match the outstanding request".into(),
            ));
        }
        *guard = Some(stream);

        match response.payload {
            HelperResponse::Error(msg) => Err(GmError::Platform(msg)),
            other => Ok(other),
        }
    }

    /// Ping the helper to verify connectivity.
    pub async fn ping(&self) -> Result<()> {
        match self.send_request(HelperRequest::Ping).await? {
            HelperResponse::Pong => Ok(()),
            other => Err(GmError::Ipc(format!(
                "Unexpected response to Ping: {other:?}"
            ))),
        }
    }
}

#[async_trait]
impl Platform for IpcPlatformClient {
    async fn begin_task(&self, task_id: &str) -> Result<()> {
        let session_id = format!("{task_id}:{}", uuid::Uuid::new_v4());
        match self
            .send_request(HelperRequest::BeginTask {
                task_id: session_id.clone(),
            })
            .await?
        {
            HelperResponse::ActionExecuted { success: true, .. } => {
                *self
                    .active_task
                    .write()
                    .map_err(|_| GmError::Ipc("Task session lock failed".into()))? =
                    Some((task_id.into(), session_id));
                Ok(())
            }
            _ => Err(GmError::Ipc(
                "Helper could not begin task session; update/restart matching helper binaries"
                    .into(),
            )),
        }
    }
    async fn cancel_task(&self, task_id: &str) -> Result<()> {
        let session = self
            .active_task
            .read()
            .map_err(|_| GmError::Ipc("Task session lock failed".into()))?
            .clone();
        let Some((logical, session)) = session else {
            return Ok(());
        };
        if logical != task_id {
            return Ok(());
        }
        match IpcPlatformClient::new(&self.socket_path)
            .send_request(HelperRequest::CancelTask { task_id: session })
            .await?
        {
            HelperResponse::Cancelled => Ok(()),
            _ => Err(GmError::Ipc(
                "Helper did not acknowledge task cancellation".into(),
            )),
        }
    }
    async fn permissions(&self) -> Result<PermissionStatus> {
        match self.send_request(HelperRequest::GetPermissions).await? {
            HelperResponse::Permissions(status) => Ok(status),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for permissions: {other:?}"
            ))),
        }
    }

    async fn user_activity(&self) -> Result<UserActivity> {
        match self.send_request(HelperRequest::GetUserActivity).await? {
            HelperResponse::UserActivity(act) => Ok(act),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for user_activity: {other:?}"
            ))),
        }
    }

    async fn capture(
        &self,
        max_long_edge: Option<u32>,
        quality: Option<u8>,
    ) -> Result<FrameCapture> {
        match self
            .send_request(HelperRequest::CaptureLatestFrame {
                max_long_edge,
                quality,
            })
            .await?
        {
            HelperResponse::Frame(frame) => Ok(frame),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for capture: {other:?}"
            ))),
        }
    }

    async fn ax_snapshot(&self, window_id: Option<u32>) -> Result<AxSnapshot> {
        match self
            .send_request(HelperRequest::GetAxSnapshot { window_id })
            .await?
        {
            HelperResponse::AxSnapshot(snap) => Ok(snap),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for ax_snapshot: {other:?}"
            ))),
        }
    }

    async fn act(&self, action: &ComputerAction) -> Result<()> {
        let task_id = self
            .active_task
            .read()
            .map_err(|_| GmError::Ipc("Task session lock failed".into()))?
            .clone();
        let request = match task_id {
            Some((_, task_id)) => HelperRequest::ExecuteTaskAction {
                task_id,
                action: action.clone(),
            },
            None => HelperRequest::ExecuteAction(action.clone()),
        };
        match self.send_request(request).await? {
            HelperResponse::ActionExecuted { success: true, .. } => Ok(()),
            HelperResponse::ActionExecuted {
                success: false,
                message,
            } => Err(GmError::Platform(
                message.unwrap_or_else(|| "Action execution failed".into()),
            )),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for act: {other:?}"
            ))),
        }
    }

    async fn cancel(&self) -> Result<()> {
        match IpcPlatformClient::new(&self.socket_path)
            .send_request(HelperRequest::CancelActions)
            .await?
        {
            HelperResponse::Cancelled => Ok(()),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for cancel: {other:?}"
            ))),
        }
    }

    async fn get_calendar_events(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Vec<gm_core::types::CalendarItem>> {
        match self
            .send_request(HelperRequest::GetCalendarEvents {
                start_time_ms,
                end_time_ms,
            })
            .await?
        {
            HelperResponse::CalendarEvents(events) => Ok(events),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for get_calendar_events: {other:?}"
            ))),
        }
    }

    async fn get_reminders(
        &self,
        include_completed: bool,
    ) -> Result<Vec<gm_core::types::ReminderItem>> {
        match self
            .send_request(HelperRequest::GetReminders { include_completed })
            .await?
        {
            HelperResponse::Reminders(rems) => Ok(rems),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for get_reminders: {other:?}"
            ))),
        }
    }

    async fn get_unread_emails(&self) -> Result<Vec<MailItem>> {
        match self.send_request(HelperRequest::GetUnreadEmails).await? {
            HelperResponse::UnreadEmails(emails) => Ok(emails),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for get_unread_emails: {other:?}"
            ))),
        }
    }

    async fn send_notification(
        &self,
        title: &str,
        subtitle: Option<&str>,
        body: &str,
    ) -> Result<bool> {
        match self
            .send_request(HelperRequest::SendNotification {
                title: title.to_string(),
                subtitle: subtitle.map(|s| s.to_string()),
                body: body.to_string(),
            })
            .await?
        {
            HelperResponse::NotificationSent(sent) => Ok(sent),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for send_notification: {other:?}"
            ))),
        }
    }
}

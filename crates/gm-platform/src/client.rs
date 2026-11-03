use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use gm_core::ipc::{
    HelperRequest, HelperResponse, IpcMessage, read_ipc_message, write_ipc_message,
};
use gm_core::types::{AxSnapshot, ComputerAction, FrameCapture, PermissionStatus, UserActivity};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::UnixStream;

pub struct IpcPlatformClient {
    socket_path: PathBuf,
    req_counter: AtomicU64,
    stream: Arc<tokio::sync::Mutex<Option<UnixStream>>>,
}

impl IpcPlatformClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            req_counter: AtomicU64::new(1),
            stream: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    async fn send_request(&self, req_body: HelperRequest) -> Result<HelperResponse> {
        let mut guard = self.stream.lock().await;
        if guard.is_none() {
            let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
                GmError::Platform(format!(
                    "Failed to connect to cu-helper socket {:?}: {e}",
                    self.socket_path
                ))
            })?;
            *guard = Some(stream);
        }

        let stream = guard.as_mut().unwrap();
        let id = self.req_counter.fetch_add(1, Ordering::SeqCst);
        let msg = IpcMessage {
            id,
            payload: req_body,
        };

        if let Err(e) = write_ipc_message(stream, &msg).await {
            *guard = None; // reset stream on failure
            return Err(e);
        }

        let response: IpcMessage<HelperResponse> = match read_ipc_message(stream).await {
            Ok(res) => res,
            Err(e) => {
                *guard = None;
                return Err(e);
            }
        };

        match response.payload {
            HelperResponse::Error(msg) => Err(GmError::Platform(msg)),
            other => Ok(other),
        }
    }
}

#[async_trait]
impl Platform for IpcPlatformClient {
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
        match self
            .send_request(HelperRequest::ExecuteAction(action.clone()))
            .await?
        {
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
        match self.send_request(HelperRequest::CancelActions).await? {
            HelperResponse::Cancelled => Ok(()),
            other => Err(GmError::Ipc(format!(
                "Unexpected response for cancel: {other:?}"
            ))),
        }
    }
}

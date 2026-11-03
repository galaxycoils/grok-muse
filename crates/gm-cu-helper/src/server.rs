use gm_core::error::Result;
use gm_core::ipc::{
    HelperRequest, HelperResponse, IpcMessage, read_ipc_message, write_ipc_message,
};
use gm_platform::Platform;
use std::path::Path;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

pub struct HelperServer<P: Platform + 'static> {
    platform: Arc<P>,
}

impl<P: Platform + 'static> HelperServer<P> {
    pub fn new(platform: P) -> Self {
        Self {
            platform: Arc::new(platform),
        }
    }

    pub async fn run(&self, socket_path: impl AsRef<Path>) -> Result<()> {
        let path = socket_path.as_ref();
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(path)?;
        info!("Helper server listening on {:?}", path);

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let platform = Arc::clone(&self.platform);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, platform).await {
                            warn!("Connection error: {e}");
                        }
                    });
                }
                Err(e) => {
                    error!("Listener error: {e}");
                    break;
                }
            }
        }

        Ok(())
    }
}

async fn handle_connection<P: Platform>(mut stream: UnixStream, platform: Arc<P>) -> Result<()> {
    loop {
        let request: IpcMessage<HelperRequest> = match read_ipc_message(&mut stream).await {
            Ok(msg) => msg,
            Err(gm_core::error::GmError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(e),
        };

        let response_payload = match request.payload {
            HelperRequest::Ping => HelperResponse::Pong,
            HelperRequest::GetPermissions => match platform.permissions().await {
                Ok(perms) => HelperResponse::Permissions(perms),
                Err(e) => HelperResponse::Error(e.to_string()),
            },
            HelperRequest::GetUserActivity => match platform.user_activity().await {
                Ok(act) => HelperResponse::UserActivity(act),
                Err(e) => HelperResponse::Error(e.to_string()),
            },
            HelperRequest::CaptureLatestFrame {
                max_long_edge,
                quality,
            } => match platform.capture(max_long_edge, quality).await {
                Ok(frame) => HelperResponse::Frame(frame),
                Err(e) => HelperResponse::Error(e.to_string()),
            },
            HelperRequest::GetAxSnapshot { window_id } => {
                match platform.ax_snapshot(window_id).await {
                    Ok(snap) => HelperResponse::AxSnapshot(snap),
                    Err(e) => HelperResponse::Error(e.to_string()),
                }
            }
            HelperRequest::ExecuteAction(action) => match platform.act(&action).await {
                Ok(_) => HelperResponse::ActionExecuted {
                    success: true,
                    message: None,
                },
                Err(e) => HelperResponse::ActionExecuted {
                    success: false,
                    message: Some(e.to_string()),
                },
            },
            HelperRequest::CancelActions => match platform.cancel().await {
                Ok(_) => HelperResponse::Cancelled,
                Err(e) => HelperResponse::Error(e.to_string()),
            },
            HelperRequest::GetCalendarEvents {
                start_time_ms,
                end_time_ms,
            } => match platform
                .get_calendar_events(start_time_ms, end_time_ms)
                .await
            {
                Ok(events) => HelperResponse::CalendarEvents(events),
                Err(e) => HelperResponse::Error(e.to_string()),
            },
            HelperRequest::GetReminders { include_completed } => {
                match platform.get_reminders(include_completed).await {
                    Ok(rems) => HelperResponse::Reminders(rems),
                    Err(e) => HelperResponse::Error(e.to_string()),
                }
            }
            HelperRequest::GetUnreadEmails => match platform.get_unread_emails().await {
                Ok(emails) => HelperResponse::UnreadEmails(emails),
                Err(e) => HelperResponse::Error(e.to_string()),
            },
            HelperRequest::SendNotification {
                title,
                subtitle,
                body,
            } => match platform
                .send_notification(&title, subtitle.as_deref(), &body)
                .await
            {
                Ok(sent) => HelperResponse::NotificationSent(sent),
                Err(e) => HelperResponse::Error(e.to_string()),
            },
        };

        let response = IpcMessage {
            id: request.id,
            payload: response_payload,
        };
        write_ipc_message(&mut stream, &response).await?;
    }
    Ok(())
}

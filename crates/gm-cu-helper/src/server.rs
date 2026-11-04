use gm_core::error::Result;
use gm_core::ipc::{
    HelperRequest, HelperResponse, IpcMessage, read_ipc_message, write_ipc_message,
};
use gm_platform::{Platform, ShmFrameRing};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tracing::error;
use tracing::{info, warn};

pub struct HelperServer<P: Platform + 'static> {
    platform: Arc<P>,
    shm_ring: Arc<ShmFrameRing>,
}

pub use gm_core::ipc::is_peer_authorized;

impl<P: Platform + 'static> HelperServer<P> {
    pub fn new(platform: P) -> Self {
        Self {
            platform: Arc::new(platform),
            shm_ring: Arc::new(ShmFrameRing::default()),
        }
    }

    pub async fn run(&self, socket_path: impl AsRef<Path>) -> Result<()> {
        let path = socket_path.as_ref();
        #[cfg(unix)]
        {
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let listener = UnixListener::bind(path)?;
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                if let Err(e) = std::fs::set_permissions(path, perms) {
                    warn!(
                        "Failed to lock socket permissions to 0o600 on {:?}: {e}",
                        path
                    );
                }
            }
            let my_euid = unsafe { libc::geteuid() };
            info!(
                "Helper server listening on {:?} (UID: {my_euid}, mode 0600)",
                path
            );

            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        match stream.peer_cred() {
                            Ok(cred) => {
                                if !is_peer_authorized(cred.uid(), my_euid) {
                                    warn!(
                                        "Rejected peer connection with mismatched UID (peer: {}, expected: {})",
                                        cred.uid(),
                                        my_euid
                                    );
                                    continue;
                                }
                            }
                            Err(e) => {
                                warn!("Failed to inspect peer credentials on helper socket: {e}");
                                continue;
                            }
                        }

                        let platform = Arc::clone(&self.platform);
                        let shm_ring = Arc::clone(&self.shm_ring);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, platform, shm_ring).await {
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
        }

        #[cfg(windows)]
        {
            let pipe_name = path.to_string_lossy();
            info!("Helper server listening on named pipe {}", pipe_name);

            let mut is_first = true;
            loop {
                let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
                if is_first {
                    options.first_pipe_instance(true);
                    is_first = false;
                }
                let server = options.create(path.as_os_str()).map_err(|e| {
                    gm_core::error::GmError::Platform(format!(
                        "Failed to create named pipe {pipe_name}: {e}"
                    ))
                })?;

                server.connect().await.map_err(|e| {
                    gm_core::error::GmError::Platform(format!(
                        "Failed to connect client to named pipe: {e}"
                    ))
                })?;

                let platform = Arc::clone(&self.platform);
                let shm_ring = Arc::clone(&self.shm_ring);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(server, platform, shm_ring).await {
                        warn!("Connection error: {e}");
                    }
                });
            }
        }

        #[allow(unreachable_code)]
        Ok(())
    }
}

async fn handle_connection<P: Platform, S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    platform: Arc<P>,
    shm_ring: Arc<ShmFrameRing>,
) -> Result<()> {
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
                Ok(frame) => HelperResponse::Frame(shm_ring.promote_frame(frame)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_credential_authorization_logic() {
        assert!(is_peer_authorized(501, 501));
        assert!(is_peer_authorized(0, 0));
        assert!(!is_peer_authorized(501, 502));
        assert!(!is_peer_authorized(1000, 1001));
        assert!(!is_peer_authorized(0, 501));
    }

    #[tokio::test]
    async fn test_helper_server_socket_permissions_and_ping() {
        let sock_path =
            std::env::temp_dir().join(format!("gm_helper_test_{}.sock", std::process::id()));
        let platform = gm_platform::MockPlatform::new();
        let server = HelperServer::new(platform);
        let s_path = sock_path.clone();

        let srv_handle = tokio::spawn(async move {
            let _ = server.run(s_path).await;
        });

        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
        assert!(sock_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&sock_path).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "Helper socket permissions must be 0o600");
        }

        let client = gm_platform::IpcPlatformClient::new(&sock_path);
        let ping_res = client.ping().await;
        assert!(ping_res.is_ok());

        // Verify zero-copy POSIX SharedMemory promotion on CaptureLatestFrame
        #[cfg(unix)]
        {
            use gm_platform::Platform;
            let frame = client.capture(Some(640), Some(80)).await.unwrap();
            assert!(
                matches!(frame.data, gm_core::types::FrameData::SharedMemory { .. }),
                "HelperServer must promote captured frames to POSIX SharedMemory on Unix"
            );
            let recovered = gm_platform::resolve_frame_bytes(&frame).unwrap();
            assert!(!recovered.is_empty());
        }

        srv_handle.abort();
        let _ = std::fs::remove_file(&sock_path);
    }
}

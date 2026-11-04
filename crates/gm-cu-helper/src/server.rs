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
    actions: Arc<ActionSessions>,
}

#[derive(Default)]
struct ActionSessions {
    gate: tokio::sync::Mutex<()>,
    current: std::sync::Mutex<Option<String>>,
    managed: std::sync::atomic::AtomicBool,
    killed: std::sync::atomic::AtomicBool,
}
use std::sync::atomic::Ordering;

pub use gm_core::ipc::is_peer_authorized;

impl<P: Platform + 'static> HelperServer<P> {
    pub fn new(platform: P) -> Self {
        Self {
            platform: Arc::new(platform),
            shm_ring: Arc::new(ShmFrameRing::default()),
            actions: Arc::new(ActionSessions::default()),
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
                        let actions = self.actions.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(stream, platform, shm_ring, actions).await
                            {
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
                let actions = self.actions.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(server, platform, shm_ring, actions).await {
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
    actions: Arc<ActionSessions>,
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
            HelperRequest::BeginTask { task_id } => {
                let _gate = actions.gate.lock().await;
                if actions.killed.load(Ordering::SeqCst) {
                    HelperResponse::Error(
                        "Emergency stop is active; restart the helper service".into(),
                    )
                } else {
                    match platform.begin_task(&task_id).await {
                        Ok(()) => {
                            actions.managed.store(true, Ordering::SeqCst);
                            *actions.current.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(task_id);
                            HelperResponse::ActionExecuted {
                                success: true,
                                message: None,
                            }
                        }
                        Err(error) => HelperResponse::Error(error.to_string()),
                    }
                }
            }
            HelperRequest::ExecuteTaskAction { task_id, action } => {
                let _gate = actions.gate.lock().await;
                let valid = !actions.killed.load(Ordering::SeqCst)
                    && actions
                        .current
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        == Some(&task_id);
                if !valid {
                    HelperResponse::Error("Stale or stopped action session".into())
                } else {
                    match platform.act(&action).await {
                        Ok(()) => HelperResponse::ActionExecuted {
                            success: true,
                            message: None,
                        },
                        Err(error) => HelperResponse::Error(error.to_string()),
                    }
                }
            }
            HelperRequest::ExecuteAction(action) => {
                let _gate = actions.gate.lock().await;
                if actions.managed.load(Ordering::SeqCst) || actions.killed.load(Ordering::SeqCst) {
                    HelperResponse::Error("An explicit task session is required".into())
                } else {
                    match platform.act(&action).await {
                        Ok(()) => HelperResponse::ActionExecuted {
                            success: true,
                            message: None,
                        },
                        Err(error) => HelperResponse::Error(error.to_string()),
                    }
                }
            }
            HelperRequest::CancelTask { task_id } => {
                let matched = {
                    let mut current = actions.current.lock().unwrap_or_else(|e| e.into_inner());
                    if current.as_ref() == Some(&task_id) {
                        current.take();
                        true
                    } else {
                        false
                    }
                };
                if matched {
                    match platform.cancel_task(&task_id).await {
                        Ok(()) => HelperResponse::Cancelled,
                        Err(error) => HelperResponse::Error(error.to_string()),
                    }
                } else {
                    HelperResponse::Cancelled
                }
            }
            HelperRequest::CancelActions => {
                actions.killed.store(true, Ordering::SeqCst);
                actions
                    .current
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                match platform.cancel().await {
                    Ok(()) => HelperResponse::Cancelled,
                    Err(error) => HelperResponse::Error(error.to_string()),
                }
            }
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

    #[cfg(unix)]
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

#[cfg(all(test, unix))]
mod session_tests {
    use super::*;
    use gm_core::types::ComputerAction;
    #[tokio::test]
    async fn cancelled_and_stale_sessions_cannot_act_or_cancel_a_new_attempt() {
        let path = std::env::temp_dir().join(format!("gm-session-{}.sock", uuid::Uuid::new_v4()));
        let server = HelperServer::new(gm_platform::MockPlatform::new());
        let socket = path.clone();
        let handle = tokio::spawn(async move { server.run(socket).await });
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let old = gm_platform::IpcPlatformClient::new(&path);
        old.begin_task("same-logical-task").await.unwrap();
        old.act(&ComputerAction::Type {
            text: "first".into(),
        })
        .await
        .unwrap();
        old.cancel_task("same-logical-task").await.unwrap();
        assert!(
            old.act(&ComputerAction::Type {
                text: "must not execute".into()
            })
            .await
            .is_err()
        );
        let new = gm_platform::IpcPlatformClient::new(&path);
        new.begin_task("same-logical-task").await.unwrap();
        assert!(
            old.act(&ComputerAction::Type {
                text: "stale attempt".into()
            })
            .await
            .is_err()
        );
        old.cancel_task("same-logical-task").await.unwrap();
        new.act(&ComputerAction::Type {
            text: "new attempt still active".into(),
        })
        .await
        .unwrap();
        new.cancel().await.unwrap();
        assert!(new.begin_task("after-emergency").await.is_err());
        handle.abort();
        let _ = handle.await;
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn interrupted_helper_rpc_discards_its_partial_connection() {
        let path =
            std::env::temp_dir().join(format!("gm-rpc-cancel-{}.sock", uuid::Uuid::new_v4()));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let _: IpcMessage<HelperRequest> = read_ipc_message(&mut first).await.unwrap();
            // Its response is intentionally withheld until the client cancels and reconnects.
            let (mut second, _) = listener.accept().await.unwrap();
            let request: IpcMessage<HelperRequest> = read_ipc_message(&mut second).await.unwrap();
            write_ipc_message(
                &mut second,
                &IpcMessage {
                    id: request.id,
                    payload: HelperResponse::Pong,
                },
            )
            .await
            .unwrap();
        });
        let client = gm_platform::IpcPlatformClient::new(&path);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), client.ping())
                .await
                .is_err()
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), client.ping())
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }
}

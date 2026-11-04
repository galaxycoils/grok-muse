use crate::error::{GmError, Result};
use crate::types::{
    AxSnapshot, CalendarItem, ComputerAction, FrameCapture, MailItem, PermissionStatus,
    ReminderItem, UserActivity,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcMessage<T> {
    pub id: u64,
    pub payload: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum HelperRequest {
    Ping,
    GetPermissions,
    GetUserActivity,
    CaptureLatestFrame {
        max_long_edge: Option<u32>,
        quality: Option<u8>,
    },
    GetAxSnapshot {
        window_id: Option<u32>,
    },
    BeginTask {
        task_id: String,
    },
    ExecuteTaskAction {
        task_id: String,
        action: ComputerAction,
    },
    CancelTask {
        task_id: String,
    },
    ExecuteAction(ComputerAction),
    CancelActions,
    GetCalendarEvents {
        start_time_ms: u64,
        end_time_ms: u64,
    },
    GetReminders {
        include_completed: bool,
    },
    GetUnreadEmails,
    SendNotification {
        title: String,
        subtitle: Option<String>,
        body: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "result")]
pub enum HelperResponse {
    Pong,
    Permissions(PermissionStatus),
    UserActivity(UserActivity),
    Frame(FrameCapture),
    AxSnapshot(AxSnapshot),
    ActionExecuted {
        success: bool,
        message: Option<String>,
    },
    Cancelled,
    CalendarEvents(Vec<CalendarItem>),
    Reminders(Vec<ReminderItem>),
    UnreadEmails(Vec<MailItem>),
    NotificationSent(bool),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", content = "params")]
pub enum QueueControlAction {
    Pause,
    Resume,
    Cancel,
    Reassign { target_bot_id: String },
    Retry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum DaemonRequest {
    Ping,
    GetStatus,
    SubmitTask {
        instruction: String,
    },
    SendChatMessage {
        text: String,
    },
    SendChatMessageV1 {
        message_id: String,
        text: String,
    },
    SetPaused {
        paused: bool,
    },
    EmergencyKill,
    RespondApproval {
        request_id: String,
        approved: bool,
    },
    SubscribeEvents,
    ListBots,
    QueueTask {
        bot_id: Option<String>,
        channel_id: Option<String>,
        instruction: String,
        priority: i32,
    },
    QueueControl {
        task_id: String,
        action: QueueControlAction,
    },
    ListQueue {
        bot_id: Option<String>,
    },
    ChannelPost {
        channel_id: String,
        author: String,
        content: String,
    },
    TakeTheWheelResume {
        task_id: String,
        note: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "result")]
pub enum DaemonResponse {
    Pong,
    Status {
        helper_connected: bool,
        paused: bool,
        active_task: Option<String>,
        daily_spend_usd: f64,
    },
    Accepted {
        id: String,
    },
    ChatReply {
        text: String,
    },
    ChatReplyV1 {
        message_id: String,
        text: String,
    },
    JsonPayload(serde_json::Value),
    Killed,
    Ack,
    Event(DaemonEvent),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum DaemonEvent {
    HudStateChanged {
        state: String,
        detail: String,
        daily_spend_usd: f64,
    },
    ChatToken {
        delta: String,
        done: bool,
    },
    ChatTokenV1 {
        message_id: String,
        delta: String,
        done: bool,
    },
    TaskProgress {
        task_id: String,
        step: u32,
        action_summary: String,
    },
    ApprovalResolved {
        request_id: String,
    },
    ApprovalRequested {
        request_id: String,
        title: String,
        details: String,
    },
    TaskFinished {
        task_id: String,
        ok: bool,
        message: String,
    },
    WheelHandoffRequested {
        task_id: String,
        bot_id: String,
        blocker: String,
        instructions: String,
    },
    WheelHandoffResumed {
        task_id: String,
        bot_id: String,
    },
    QueueUpdated {
        bot_id: String,
        queued: usize,
        running_task_id: Option<String>,
    },
    ChannelMessage {
        channel_id: String,
        author: String,
        bot_id: Option<String>,
        content: String,
        timestamp: String,
    },
}

/// Verify peer UID credential against expected UID (e.g. current effective UID).
pub fn is_peer_authorized(peer_uid: u32, expected_uid: u32) -> bool {
    peer_uid == expected_uid
}

/// Length-prefixed serialization: 4-byte big-endian payload length + JSON bytes.
pub async fn write_ipc_message<W, T>(writer: &mut W, message: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(message)?;
    let len = bytes.len() as u32;
    writer.write_u32(len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads length-prefixed JSON message.
pub async fn read_ipc_message<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let len = reader.read_u32().await?;
    if len > 32 * 1024 * 1024 {
        return Err(GmError::Ipc(format!("Message too large: {len} bytes")));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    let message = serde_json::from_slice(&buf)?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_ipc_roundtrip() {
        let req = IpcMessage {
            id: 42,
            payload: HelperRequest::Ping,
        };
        let mut buffer = Vec::new();
        write_ipc_message(&mut buffer, &req).await.unwrap();

        let mut cursor = Cursor::new(buffer);
        let decoded: IpcMessage<HelperRequest> = read_ipc_message(&mut cursor).await.unwrap();
        assert_eq!(decoded.id, 42);
        match decoded.payload {
            HelperRequest::Ping => (),
            _ => panic!("Expected Ping"),
        }
    }

    #[tokio::test]
    async fn test_daemon_protocol_roundtrip() {
        // Test DaemonRequest
        let req = IpcMessage {
            id: 101,
            payload: DaemonRequest::SubmitTask {
                instruction: "Click Safari".into(),
            },
        };
        let mut buffer = Vec::new();
        write_ipc_message(&mut buffer, &req).await.unwrap();
        let mut cursor = Cursor::new(buffer);
        let decoded: IpcMessage<DaemonRequest> = read_ipc_message(&mut cursor).await.unwrap();
        assert_eq!(decoded.id, 101);
        match decoded.payload {
            DaemonRequest::SubmitTask { instruction } => assert_eq!(instruction, "Click Safari"),
            _ => panic!("Expected SubmitTask"),
        }

        // Test DaemonResponse with Event
        let event = DaemonEvent::HudStateChanged {
            state: "Acting".into(),
            detail: "Clicking button".into(),
            daily_spend_usd: 0.12,
        };
        let res = IpcMessage {
            id: 102,
            payload: DaemonResponse::Event(event),
        };
        let mut buffer2 = Vec::new();
        write_ipc_message(&mut buffer2, &res).await.unwrap();
        let mut cursor2 = Cursor::new(buffer2);
        let decoded2: IpcMessage<DaemonResponse> = read_ipc_message(&mut cursor2).await.unwrap();
        assert_eq!(decoded2.id, 102);
        match decoded2.payload {
            DaemonResponse::Event(DaemonEvent::HudStateChanged {
                state,
                detail,
                daily_spend_usd,
            }) => {
                assert_eq!(state, "Acting");
                assert_eq!(detail, "Clicking button");
                assert!((daily_spend_usd - 0.12).abs() < 1e-6);
            }
            _ => panic!("Expected HudStateChanged event"),
        }
    }

    #[tokio::test]
    async fn test_openbot_daemon_ipc_roundtrip() {
        let req = IpcMessage {
            id: 201,
            payload: DaemonRequest::QueueTask {
                bot_id: Some("coder".into()),
                channel_id: Some("general".into()),
                instruction: "Write unit tests".into(),
                priority: 10,
            },
        };
        let mut buf = Vec::new();
        write_ipc_message(&mut buf, &req).await.unwrap();
        let mut cur = Cursor::new(buf);
        let decoded: IpcMessage<DaemonRequest> = read_ipc_message(&mut cur).await.unwrap();
        match decoded.payload {
            DaemonRequest::QueueTask {
                bot_id,
                channel_id,
                instruction,
                priority,
            } => {
                assert_eq!(bot_id.as_deref(), Some("coder"));
                assert_eq!(channel_id.as_deref(), Some("general"));
                assert_eq!(instruction, "Write unit tests");
                assert_eq!(priority, 10);
            }
            _ => panic!("Expected QueueTask"),
        }

        let evt = DaemonEvent::WheelHandoffRequested {
            task_id: "t-99".into(),
            bot_id: "muse".into(),
            blocker: "2FA code required".into(),
            instructions: "Complete 2FA in browser and click Hand Back to Bot".into(),
        };
        let res = IpcMessage {
            id: 202,
            payload: DaemonResponse::Event(evt),
        };
        let mut buf2 = Vec::new();
        write_ipc_message(&mut buf2, &res).await.unwrap();
        let mut cur2 = Cursor::new(buf2);
        let decoded2: IpcMessage<DaemonResponse> = read_ipc_message(&mut cur2).await.unwrap();
        match decoded2.payload {
            DaemonResponse::Event(DaemonEvent::WheelHandoffRequested {
                task_id,
                bot_id,
                blocker,
                ..
            }) => {
                assert_eq!(task_id, "t-99");
                assert_eq!(bot_id, "muse");
                assert_eq!(blocker, "2FA code required");
            }
            _ => panic!("Expected WheelHandoffRequested"),
        }
    }
}

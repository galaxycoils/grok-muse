use crate::error::{GmError, Result};
use crate::types::{
    AxSnapshot, CalendarItem, ComputerAction, FrameCapture, PermissionStatus, ReminderItem,
    UserActivity,
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
    ExecuteAction(ComputerAction),
    CancelActions,
    GetCalendarEvents {
        start_time_ms: u64,
        end_time_ms: u64,
    },
    GetReminders {
        include_completed: bool,
    },
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
    NotificationSent(bool),
    Error(String),
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
}

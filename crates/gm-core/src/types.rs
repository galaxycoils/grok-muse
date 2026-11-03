use serde::{Deserialize, Serialize};

/// Rectangular frame in global screen coordinates (points).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.x + self.width && py >= self.y && py <= self.y + self.height
    }
}

/// Tokenized representation of an accessible UI element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxElement {
    pub token: String,
    pub role: String,
    pub label: Option<String>,
    pub value: Option<String>,
    pub frame: Rect,
    pub is_enabled: bool,
    pub is_secure: bool,
}

/// Accessibility tree snapshot of a frontmost window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxSnapshot {
    pub window_id: u32,
    pub app_name: String,
    pub window_title: String,
    pub bounds: Rect,
    pub elements: Vec<AxElement>,
    pub timestamp_ms: u64,
}

/// Screen capture representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FrameData {
    /// In-memory JPEG / PNG data (used for lightweight / test payloads)
    InlineBytes(Vec<u8>),
    /// Path to temporary frame file (efficient zero-copy IPC)
    FilePath(String),
    /// Shared memory descriptor handle
    SharedMemory { shm_id: String, size_bytes: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameCapture {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    pub data: FrameData,
    pub timestamp_ms: u64,
}

/// Actions supported by the Computer Use helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "params")]
pub enum ComputerAction {
    Click {
        token: Option<String>,
        coords: Option<(f64, f64)>,
        button: Option<MouseButton>,
        click_count: Option<u32>,
    },
    Type {
        text: String,
    },
    KeyCombo {
        modifiers: Vec<String>,
        key: String,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Drag {
        start: (f64, f64),
        end: (f64, f64),
    },
    Menu {
        path: Vec<String>,
    },
    FocusApp {
        app_name: String,
    },
    LaunchApp {
        bundle_id: String,
    },
    Wait {
        condition: Option<String>,
        timeout_ms: u64,
    },
    AskUser {
        question: String,
    },
    Done {
        summary: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Status of macOS system permissions (TCC).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub screen_recording: bool,
    pub accessibility: bool,
    pub calendar: bool,
    pub reminders: bool,
}

impl PermissionStatus {
    pub fn all_granted(&self) -> bool {
        self.screen_recording && self.accessibility && self.calendar && self.reminders
    }

    pub fn cu_granted(&self) -> bool {
        self.screen_recording && self.accessibility
    }
}

/// Metrics on user presence / idle time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UserActivity {
    pub seconds_since_last_event: f64,
    pub is_user_active: bool,
}

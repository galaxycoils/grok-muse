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

impl ComputerAction {
    /// Returns true if the action dispatches low-level CGEvent mouse/keyboard inputs.
    pub fn is_cgevent_action(&self) -> bool {
        match self {
            ComputerAction::Click { token, coords, .. } => coords.is_some() || token.is_none(),
            ComputerAction::Type { .. }
            | ComputerAction::KeyCombo { .. }
            | ComputerAction::Scroll { .. }
            | ComputerAction::Drag { .. } => true,
            _ => false,
        }
    }

    /// Returns true if the action can execute directly via background Accessibility API (AXPress).
    pub fn is_ax_action(&self) -> bool {
        match self {
            ComputerAction::Click { token, coords, .. } => token.is_some() && coords.is_none(),
            _ => false,
        }
    }

    /// Returns true if this action is expected to visibly modify the target UI window state.
    pub fn modifies_ui_state(&self) -> bool {
        matches!(
            self,
            ComputerAction::Click { .. }
                | ComputerAction::Type { .. }
                | ComputerAction::KeyCombo { .. }
                | ComputerAction::Scroll { .. }
                | ComputerAction::Drag { .. }
        )
    }
}

/// Changes detected between two accessibility tree snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxDiff {
    pub added_tokens: Vec<String>,
    pub removed_tokens: Vec<String>,
    pub changed_elements: Vec<AxElementChange>,
    pub window_title_changed: Option<(String, String)>,
    pub app_changed: Option<(String, String)>,
}

impl AxDiff {
    pub fn is_empty(&self) -> bool {
        self.added_tokens.is_empty()
            && self.removed_tokens.is_empty()
            && self.changed_elements.is_empty()
            && self.window_title_changed.is_none()
            && self.app_changed.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxElementChange {
    pub token: String,
    pub old_label: Option<String>,
    pub new_label: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
}

/// Computes the difference between pre-action and post-action accessibility tree snapshots.
pub fn compute_ax_diff(before: &AxSnapshot, after: &AxSnapshot) -> AxDiff {
    let mut added_tokens = Vec::new();
    let mut removed_tokens = Vec::new();
    let mut changed_elements = Vec::new();

    let before_map: std::collections::HashMap<&str, &AxElement> = before
        .elements
        .iter()
        .map(|e| (e.token.as_str(), e))
        .collect();
    let after_map: std::collections::HashMap<&str, &AxElement> = after
        .elements
        .iter()
        .map(|e| (e.token.as_str(), e))
        .collect();

    for (token, after_el) in &after_map {
        if let Some(before_el) = before_map.get(token) {
            if before_el.label != after_el.label || before_el.value != after_el.value {
                changed_elements.push(AxElementChange {
                    token: token.to_string(),
                    old_label: before_el.label.clone(),
                    new_label: after_el.label.clone(),
                    old_value: before_el.value.clone(),
                    new_value: after_el.value.clone(),
                });
            }
        } else {
            added_tokens.push(token.to_string());
        }
    }

    for token in before_map.keys() {
        if !after_map.contains_key(token) {
            removed_tokens.push(token.to_string());
        }
    }

    let window_title_changed = if before.window_title != after.window_title {
        Some((before.window_title.clone(), after.window_title.clone()))
    } else {
        None
    };

    let app_changed = if before.app_name != after.app_name {
        Some((before.app_name.clone(), after.app_name.clone()))
    } else {
        None
    };

    AxDiff {
        added_tokens,
        removed_tokens,
        changed_elements,
        window_title_changed,
        app_changed,
    }
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

/// Calendar event retrieved from EventKit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarItem {
    pub id: String,
    pub title: String,
    pub start_time_ms: u64,
    pub end_time_ms: u64,
    pub location: Option<String>,
    pub notes: Option<String>,
    pub is_all_day: bool,
}

/// Reminder task retrieved from EventKit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReminderItem {
    pub id: String,
    pub title: String,
    pub due_date_ms: Option<u64>,
    pub is_completed: bool,
    pub priority: u32,
    pub list_name: Option<String>,
}

/// Notification message payload for macOS UserNotifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationPayload {
    pub title: String,
    pub subtitle: Option<String>,
    pub body: String,
}

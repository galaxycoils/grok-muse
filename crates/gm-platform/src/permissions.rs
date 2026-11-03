use gm_core::types::PermissionStatus;
use serde::{Deserialize, Serialize};

/// macOS System Settings Privacy & Security panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrivacyPane {
    ScreenRecording,
    Accessibility,
    Calendars,
    Reminders,
}

impl PrivacyPane {
    pub const ALL: [PrivacyPane; 4] = [
        PrivacyPane::ScreenRecording,
        PrivacyPane::Accessibility,
        PrivacyPane::Calendars,
        PrivacyPane::Reminders,
    ];

    /// The query parameter for `x-apple.systempreferences:com.apple.preference.security?...`
    pub fn pane_name(&self) -> &'static str {
        match self {
            Self::ScreenRecording => "Privacy_ScreenCapture",
            Self::Accessibility => "Privacy_Accessibility",
            Self::Calendars => "Privacy_Calendars",
            Self::Reminders => "Privacy_Reminders",
        }
    }

    /// Parse a pane name or identifier from string.
    pub fn parse(input: &str) -> Option<Self> {
        let clean = input.trim().to_lowercase();
        match clean.as_str() {
            "screen"
            | "screencapture"
            | "screenrecording"
            | "privacy_screencapture"
            | "privacy_screenrecording" => Some(Self::ScreenRecording),
            "ax" | "accessibility" | "privacy_accessibility" => Some(Self::Accessibility),
            "cal" | "calendar" | "calendars" | "privacy_calendars" => Some(Self::Calendars),
            "rem" | "reminder" | "reminders" | "privacy_reminders" => Some(Self::Reminders),
            _ => None,
        }
    }

    /// Full deep-link URL to open macOS System Settings directly to this pane.
    pub fn url(&self) -> &'static str {
        match self {
            Self::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::Calendars => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Calendars"
            }
            Self::Reminders => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Reminders"
            }
        }
    }

    /// Human-readable title.
    pub fn title(&self) -> &'static str {
        match self {
            Self::ScreenRecording => "Screen Recording",
            Self::Accessibility => "Accessibility",
            Self::Calendars => "Calendars",
            Self::Reminders => "Reminders",
        }
    }

    /// Purpose description shown to users during onboarding.
    pub fn description(&self) -> &'static str {
        match self {
            Self::ScreenRecording => {
                "Required for ScreenCaptureKit to stream display frames for visual grounding and verification."
            }
            Self::Accessibility => {
                "Required for AXUIElement to inspect UI hierarchies and dispatch AXPress/AXSetValue actions."
            }
            Self::Calendars => {
                "Required for EventKit to read scheduled calendar events during proactive patrol."
            }
            Self::Reminders => {
                "Required for EventKit to track upcoming and overdue tasks for proactive follow-ups."
            }
        }
    }

    /// Open this privacy pane directly in macOS System Settings.
    pub fn open(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            if crate::mac::MacPlatform::open_system_settings_pane(self.pane_name()) {
                return true;
            }
            std::process::Command::new("open")
                .arg(self.url())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// Check if this permission is currently granted.
    pub fn is_granted(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            match self {
                Self::ScreenRecording => crate::mac::MacPlatform::check_screen_recording(),
                Self::Accessibility => crate::mac::MacPlatform::check_accessibility(),
                Self::Calendars => crate::mac::MacPlatform::check_calendar(),
                Self::Reminders => crate::mac::MacPlatform::check_reminders(),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            true
        }
    }

    /// Request authorization from macOS.
    pub fn request(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            match self {
                Self::ScreenRecording => crate::mac::MacPlatform::request_screen_recording(),
                Self::Accessibility => crate::mac::MacPlatform::request_accessibility(),
                Self::Calendars => crate::mac::MacPlatform::request_calendar(),
                Self::Reminders => crate::mac::MacPlatform::request_reminders(),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            true
        }
    }
}

/// Retrieve live status for all required macOS permissions.
pub fn check_all_permissions() -> PermissionStatus {
    PermissionStatus {
        screen_recording: PrivacyPane::ScreenRecording.is_granted(),
        accessibility: PrivacyPane::Accessibility.is_granted(),
        calendar: PrivacyPane::Calendars.is_granted(),
        reminders: PrivacyPane::Reminders.is_granted(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_privacy_pane_urls_and_names() {
        for pane in &PrivacyPane::ALL {
            assert!(pane.url().starts_with("x-apple.systempreferences:"));
            assert!(pane.url().contains(pane.pane_name()));
            assert!(!pane.title().is_empty());
            assert!(!pane.description().is_empty());
        }
    }

    #[test]
    fn test_privacy_pane_parse() {
        assert_eq!(
            PrivacyPane::parse("Privacy_ScreenCapture"),
            Some(PrivacyPane::ScreenRecording)
        );
        assert_eq!(
            PrivacyPane::parse("Privacy_Accessibility"),
            Some(PrivacyPane::Accessibility)
        );
        assert_eq!(
            PrivacyPane::parse("Privacy_Calendars"),
            Some(PrivacyPane::Calendars)
        );
        assert_eq!(
            PrivacyPane::parse("Privacy_Reminders"),
            Some(PrivacyPane::Reminders)
        );
        assert_eq!(
            PrivacyPane::parse("screen"),
            Some(PrivacyPane::ScreenRecording)
        );
        assert_eq!(PrivacyPane::parse("ax"), Some(PrivacyPane::Accessibility));
        assert_eq!(PrivacyPane::parse("cal"), Some(PrivacyPane::Calendars));
        assert_eq!(
            PrivacyPane::parse("reminders"),
            Some(PrivacyPane::Reminders)
        );
        assert_eq!(PrivacyPane::parse("unknown_pane"), None);
    }

    #[test]
    fn test_check_all_permissions() {
        let status = check_all_permissions();
        // Check fields are boolean
        let _ = status.all_granted();
        let _ = status.cu_granted();
    }
}

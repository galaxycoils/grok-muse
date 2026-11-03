use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use gm_core::types::{
    AxSnapshot, CalendarItem, ComputerAction, FrameCapture, FrameData, MailItem, MouseButton,
    PermissionStatus, ReminderItem, UserActivity,
};
use parking_lot::RwLock;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_float, c_int};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};

unsafe extern "C" {
    fn gm_mac_check_screen_recording_permission() -> c_int;
    fn gm_mac_check_ax_permission() -> c_int;
    fn gm_mac_check_calendar_permission() -> c_int;
    fn gm_mac_check_reminders_permission() -> c_int;
    fn gm_mac_request_screen_recording_permission() -> c_int;
    fn gm_mac_request_ax_permission() -> c_int;
    fn gm_mac_request_calendar_permission() -> c_int;
    fn gm_mac_request_reminders_permission() -> c_int;
    fn gm_mac_open_system_settings_pane(pane_name: *const c_char) -> c_int;
    fn gm_mac_install_global_hotkeys(callback: extern "C" fn(c_int)) -> c_int;
    fn gm_mac_uninstall_global_hotkeys();
    fn gm_mac_seconds_since_last_input() -> c_double;
    fn gm_mac_start_stream() -> c_int;
    fn gm_mac_get_latest_frame(
        out_data: *mut *mut u8,
        out_len: *mut usize,
        out_w: *mut u32,
        out_h: *mut u32,
        out_scale: *mut c_double,
        out_ts: *mut u64,
    ) -> c_int;
    fn gm_mac_free_frame(data: *mut u8);
    fn gm_mac_stop_stream();
    fn gm_mac_encode_jpeg(
        rgba_bytes: *const u8,
        width: u32,
        height: u32,
        quality: c_float,
        out_jpeg: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    fn gm_mac_ax_press(token: *const c_char) -> c_int;
    fn gm_mac_inspect_front_window(target_window_id: u32, out_json_str: *mut *mut c_char) -> c_int;
    fn gm_mac_free_string(str_ptr: *mut c_char);
    fn gm_mac_post_mouse_click(
        x: c_double,
        y: c_double,
        button_code: c_int,
        click_count: c_int,
    ) -> c_int;
    fn gm_mac_post_drag(
        start_x: c_double,
        start_y: c_double,
        end_x: c_double,
        end_y: c_double,
    ) -> c_int;
    fn gm_mac_post_type_text(utf8_text: *const c_char) -> c_int;
    fn gm_mac_post_key_combo(
        modifiers: *const *const c_char,
        mod_count: c_int,
        key: *const c_char,
    ) -> c_int;
    fn gm_mac_post_scroll(dx: i32, dy: i32) -> c_int;
    fn gm_mac_focus_app(app_name: *const c_char) -> c_int;
    fn gm_mac_launch_app(bundle_id: *const c_char) -> c_int;
    fn gm_mac_get_calendar_events(
        start_ms: u64,
        end_ms: u64,
        out_json_str: *mut *mut c_char,
    ) -> c_int;
    fn gm_mac_get_reminders(include_completed: c_int, out_json_str: *mut *mut c_char) -> c_int;
    fn gm_mac_menu_click(path_items: *const *const c_char, path_len: c_int) -> c_int;
    fn gm_mac_send_notification(
        title: *const c_char,
        subtitle: *const c_char,
        body: *const c_char,
    ) -> c_int;
}

/// Native macOS Platform implementation utilizing ScreenCaptureKit,
/// AXUIElement, and CoreGraphics event taps.
pub struct MacPlatform {
    cached_snapshot: Arc<RwLock<Option<AxSnapshot>>>,
    stream_started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl Default for MacPlatform {
    fn default() -> Self {
        Self::new().expect("Failed to initialize native MacPlatform")
    }
}

impl MacPlatform {
    pub fn new() -> Result<Self> {
        Ok(Self {
            cached_snapshot: Arc::new(RwLock::new(None)),
            stream_started: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Ensure ScreenCaptureKit persistent stream is active (called lazily on capture)
    pub fn ensure_stream_started(&self) {
        if !self.stream_started.swap(true, Ordering::SeqCst) {
            let started = unsafe { gm_mac_start_stream() };
            if started == 1 {
                info!("ScreenCaptureKit persistent capture stream activated");
            } else {
                warn!("ScreenCaptureKit stream deferred or running on-demand fallback");
            }
        }
    }

    /// Encode 32-bit RGBA image buffer to JPEG using native Apple ImageIO
    pub fn encode_jpeg(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
        quality: f32,
    ) -> Result<Vec<u8>> {
        let mut out_ptr: *mut u8 = std::ptr::null_mut();
        let mut out_len: usize = 0;

        let ret = unsafe {
            gm_mac_encode_jpeg(
                rgba.as_ptr(),
                width,
                height,
                quality as c_float,
                &mut out_ptr,
                &mut out_len,
            )
        };

        if ret != 0 || out_ptr.is_null() {
            return Err(GmError::Platform(format!(
                "Native JPEG encode failed with code {ret}"
            )));
        }

        let jpeg_bytes = unsafe { std::slice::from_raw_parts(out_ptr, out_len).to_vec() };
        unsafe { gm_mac_free_frame(out_ptr) };
        Ok(jpeg_bytes)
    }

    pub fn check_screen_recording() -> bool {
        unsafe { gm_mac_check_screen_recording_permission() == 1 }
    }

    pub fn check_accessibility() -> bool {
        unsafe { gm_mac_check_ax_permission() == 1 }
    }

    pub fn check_calendar() -> bool {
        unsafe { gm_mac_check_calendar_permission() == 1 }
    }

    pub fn check_reminders() -> bool {
        unsafe { gm_mac_check_reminders_permission() == 1 }
    }

    pub fn request_screen_recording() -> bool {
        unsafe { gm_mac_request_screen_recording_permission() == 1 }
    }

    pub fn request_accessibility() -> bool {
        unsafe { gm_mac_request_ax_permission() == 1 }
    }

    pub fn request_calendar() -> bool {
        unsafe { gm_mac_request_calendar_permission() == 1 }
    }

    pub fn request_reminders() -> bool {
        unsafe { gm_mac_request_reminders_permission() == 1 }
    }

    pub fn open_system_settings_pane(pane_name: &str) -> bool {
        if let Ok(c_pane) = CString::new(pane_name) {
            unsafe { gm_mac_open_system_settings_pane(c_pane.as_ptr()) == 0 }
        } else {
            false
        }
    }

    pub fn install_global_hotkeys(callback: extern "C" fn(c_int)) -> bool {
        unsafe { gm_mac_install_global_hotkeys(callback) == 0 }
    }

    pub fn uninstall_global_hotkeys() {
        unsafe { gm_mac_uninstall_global_hotkeys() }
    }
}

impl Drop for MacPlatform {
    fn drop(&mut self) {
        unsafe {
            gm_mac_stop_stream();
        }
    }
}

#[async_trait]
impl Platform for MacPlatform {
    async fn permissions(&self) -> Result<PermissionStatus> {
        let sc_ok = unsafe { gm_mac_check_screen_recording_permission() } == 1;
        let ax_ok = unsafe { gm_mac_check_ax_permission() } == 1;
        let cal_ok = unsafe { gm_mac_check_calendar_permission() } == 1;
        let rem_ok = unsafe { gm_mac_check_reminders_permission() } == 1;

        Ok(PermissionStatus {
            screen_recording: sc_ok,
            accessibility: ax_ok,
            calendar: cal_ok,
            reminders: rem_ok,
        })
    }

    async fn user_activity(&self) -> Result<UserActivity> {
        let seconds = unsafe { gm_mac_seconds_since_last_input() };
        let idle_threshold_sec = 5.0;
        let is_user_active = seconds < idle_threshold_sec;

        Ok(UserActivity {
            seconds_since_last_event: seconds,
            is_user_active,
        })
    }

    async fn capture(
        &self,
        max_long_edge: Option<u32>,
        quality: Option<u8>,
    ) -> Result<FrameCapture> {
        self.ensure_stream_started();

        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let mut len: usize = 0;
        let mut width: u32 = 0;
        let mut height: u32 = 0;
        let mut scale: c_double = 2.0;
        let mut timestamp_ms: u64 = 0;

        let ret = unsafe {
            gm_mac_get_latest_frame(
                &mut data_ptr,
                &mut len,
                &mut width,
                &mut height,
                &mut scale,
                &mut timestamp_ms,
            )
        };

        if ret != 0 || data_ptr.is_null() {
            return Err(GmError::Platform(format!(
                "ScreenCaptureKit frame acquisition returned error code {ret}"
            )));
        }

        let raw_bytes = unsafe { std::slice::from_raw_parts(data_ptr, len).to_vec() };
        unsafe { gm_mac_free_frame(data_ptr) };

        // Handle downscaling and BGRA -> RGBA conversion
        let (rgba_bytes, final_w, final_h) = if let Some(target_edge) = max_long_edge {
            let max_dim = width.max(height);
            if max_dim > target_edge && max_dim > 0 {
                let ratio = target_edge as f64 / max_dim as f64;
                let target_w = ((width as f64 * ratio).round() as u32).max(1);
                let target_h = ((height as f64 * ratio).round() as u32).max(1);

                let mut downscaled = vec![0u8; (target_w * target_h * 4) as usize];
                for dy in 0..target_h {
                    let sy = (dy * height) / target_h;
                    for dx in 0..target_w {
                        let sx = (dx * width) / target_w;
                        let src_idx = ((sy * width + sx) * 4) as usize;
                        let dst_idx = ((dy * target_w + dx) * 4) as usize;
                        if src_idx + 4 <= raw_bytes.len() && dst_idx + 4 <= downscaled.len() {
                            // Convert BGRA to RGBA
                            downscaled[dst_idx] = raw_bytes[src_idx + 2]; // R
                            downscaled[dst_idx + 1] = raw_bytes[src_idx + 1]; // G
                            downscaled[dst_idx + 2] = raw_bytes[src_idx]; // B
                            downscaled[dst_idx + 3] = raw_bytes[src_idx + 3]; // A
                        }
                    }
                }
                (downscaled, target_w, target_h)
            } else {
                (raw_bytes, width, height)
            }
        } else {
            (raw_bytes, width, height)
        };

        // If quality parameter is provided, encode to JPEG using hardware ImageIO
        let frame_data = if let Some(q) = quality {
            let q_ratio = (q as f32) / 100.0;
            let jpeg_bytes = self.encode_jpeg(&rgba_bytes, final_w, final_h, q_ratio)?;
            FrameData::InlineBytes(jpeg_bytes)
        } else {
            FrameData::InlineBytes(rgba_bytes)
        };

        Ok(FrameCapture {
            width: final_w,
            height: final_h,
            scale_factor: scale,
            data: frame_data,
            timestamp_ms,
        })
    }

    async fn ax_snapshot(&self, window_id: Option<u32>) -> Result<AxSnapshot> {
        let mut json_ptr: *mut c_char = std::ptr::null_mut();
        let target_id = window_id.unwrap_or(0);

        let ret = unsafe { gm_mac_inspect_front_window(target_id, &mut json_ptr) };
        if ret != 0 || json_ptr.is_null() {
            return Err(GmError::Platform(format!(
                "AXUIElement window inspection failed with code {ret}"
            )));
        }

        let json_str = unsafe {
            let cstr = CStr::from_ptr(json_ptr);
            let s = cstr.to_string_lossy().into_owned();
            gm_mac_free_string(json_ptr);
            s
        };

        let snapshot: AxSnapshot = serde_json::from_str(&json_str).map_err(|e| {
            GmError::Platform(format!(
                "Failed to deserialize AXUIElement snapshot JSON: {e}"
            ))
        })?;

        // Cache snapshot elements for coordinate resolution & AX actions
        *self.cached_snapshot.write() = Some(snapshot.clone());

        Ok(snapshot)
    }

    async fn act(&self, action: &ComputerAction) -> Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(GmError::Platform("Action execution cancelled".into()));
        }

        match action {
            ComputerAction::Click {
                token,
                coords,
                button,
                click_count,
            } => {
                // Primary AX-first action: if a token is specified and no explicit coordinates,
                // attempt AXPress first without moving cursor
                if let Some(tok) = token
                    && coords.is_none()
                    && let Ok(c_tok) = CString::new(tok.as_str())
                {
                    let ret = unsafe { gm_mac_ax_press(c_tok.as_ptr()) };
                    if ret == 0 {
                        debug!("AXPress action succeeded on token '{tok}'");
                        return Ok(());
                    }
                    debug!("AXPress on '{tok}' returned code {ret}, falling back to CGEvent click");
                }

                // If falling back to CGEvent click, enforce user coexistence
                let mut activity = self.user_activity().await?;
                while activity.seconds_since_last_event < 1.5 {
                    if self.cancelled.load(Ordering::SeqCst) {
                        return Err(GmError::Platform("Action execution cancelled".into()));
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    activity = self.user_activity().await?;
                }

                // Fallback: resolve target point from token or coordinates
                let mut target_pt = coords.unwrap_or((100.0, 100.0));
                if let Some(tok) = token {
                    let lock = self.cached_snapshot.read();
                    if let Some(snapshot) = &*lock
                        && let Some(el) = snapshot.elements.iter().find(|e| &e.token == tok)
                    {
                        target_pt = (
                            el.frame.x + el.frame.width / 2.0,
                            el.frame.y + el.frame.height / 2.0,
                        );
                    }
                }

                let btn_code = match button {
                    Some(MouseButton::Right) => 1,
                    Some(MouseButton::Middle) => 2,
                    _ => 0,
                };
                let count = click_count.unwrap_or(1) as c_int;

                let ret =
                    unsafe { gm_mac_post_mouse_click(target_pt.0, target_pt.1, btn_code, count) };
                if ret != 0 {
                    return Err(GmError::Platform(format!(
                        "CGEvent mouse click failed with code {ret}"
                    )));
                }
            }

            ComputerAction::Type { text } => {
                let c_text = CString::new(text.as_str()).map_err(|e| {
                    GmError::Platform(format!("Invalid CString for keyboard input: {e}"))
                })?;
                let ret = unsafe { gm_mac_post_type_text(c_text.as_ptr()) };
                if ret != 0 {
                    return Err(GmError::Platform(format!(
                        "CGEvent text type failed with code {ret}"
                    )));
                }
            }

            ComputerAction::KeyCombo { modifiers, key } => {
                let c_key = CString::new(key.as_str()).map_err(|e| {
                    GmError::Platform(format!("Invalid CString for key '{key}': {e}"))
                })?;

                let mut c_mods: Vec<CString> = Vec::with_capacity(modifiers.len());
                for m in modifiers {
                    if let Ok(cm) = CString::new(m.as_str()) {
                        c_mods.push(cm);
                    }
                }
                let c_mod_ptrs: Vec<*const c_char> = c_mods.iter().map(|cm| cm.as_ptr()).collect();

                let ret = unsafe {
                    gm_mac_post_key_combo(
                        c_mod_ptrs.as_ptr(),
                        c_mod_ptrs.len() as c_int,
                        c_key.as_ptr(),
                    )
                };
                if ret != 0 {
                    return Err(GmError::Platform(format!(
                        "CGEvent key combo failed with code {ret}"
                    )));
                }
            }

            ComputerAction::Scroll { dx, dy } => {
                let ret = unsafe { gm_mac_post_scroll(*dx, *dy) };
                if ret != 0 {
                    return Err(GmError::Platform(format!(
                        "CGEvent scroll failed with code {ret}"
                    )));
                }
            }

            ComputerAction::Drag { start, end } => {
                let ret = unsafe { gm_mac_post_drag(start.0, start.1, end.0, end.1) };
                if ret != 0 {
                    return Err(GmError::Platform(format!(
                        "CGEvent drag failed with code {ret}"
                    )));
                }
            }

            ComputerAction::FocusApp { app_name } => {
                let c_app = CString::new(app_name.as_str())
                    .map_err(|e| GmError::Platform(format!("Invalid CString for app name: {e}")))?;
                let ret = unsafe { gm_mac_focus_app(c_app.as_ptr()) };
                if ret != 0 {
                    debug!("Focus app returned code {ret}, continuing");
                }
            }

            ComputerAction::LaunchApp { bundle_id } => {
                let c_bundle = CString::new(bundle_id.as_str()).map_err(|e| {
                    GmError::Platform(format!("Invalid CString for bundle id: {e}"))
                })?;
                let ret = unsafe { gm_mac_launch_app(c_bundle.as_ptr()) };
                if ret != 0 {
                    debug!("Launch app returned code {ret}, continuing");
                }
            }

            ComputerAction::Wait { timeout_ms, .. } => {
                tokio::time::sleep(tokio::time::Duration::from_millis(*timeout_ms)).await;
            }

            ComputerAction::Menu { path } => {
                debug!("Menu action requested for path: {:?}", path);
                let mut c_items: Vec<CString> = Vec::with_capacity(path.len());
                for item in path {
                    if let Ok(c) = CString::new(item.as_str()) {
                        c_items.push(c);
                    }
                }
                let ptrs: Vec<*const c_char> = c_items.iter().map(|c| c.as_ptr()).collect();
                let ret = unsafe { gm_mac_menu_click(ptrs.as_ptr(), ptrs.len() as c_int) };
                if ret != 0 {
                    debug!("Menu click returned code {ret}");
                }
            }

            ComputerAction::AskUser { question } => {
                info!("Agent requested user clarification: {question}");
            }

            ComputerAction::Done { summary } => {
                debug!("Action completed: Done - {summary}");
            }
        }

        Ok(())
    }

    async fn cancel(&self) -> Result<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn get_calendar_events(
        &self,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Vec<CalendarItem>> {
        let mut json_ptr: *mut c_char = std::ptr::null_mut();
        let ret = unsafe { gm_mac_get_calendar_events(start_time_ms, end_time_ms, &mut json_ptr) };
        if ret != 0 || json_ptr.is_null() {
            return Ok(Vec::new());
        }
        let json_str = unsafe {
            let s = CStr::from_ptr(json_ptr).to_string_lossy().into_owned();
            gm_mac_free_string(json_ptr);
            s
        };
        let items: Vec<CalendarItem> = serde_json::from_str(&json_str).unwrap_or_default();
        Ok(items)
    }

    async fn get_reminders(&self, include_completed: bool) -> Result<Vec<ReminderItem>> {
        let mut json_ptr: *mut c_char = std::ptr::null_mut();
        let ret =
            unsafe { gm_mac_get_reminders(if include_completed { 1 } else { 0 }, &mut json_ptr) };
        if ret != 0 || json_ptr.is_null() {
            return Ok(Vec::new());
        }
        let json_str = unsafe {
            let s = CStr::from_ptr(json_ptr).to_string_lossy().into_owned();
            gm_mac_free_string(json_ptr);
            s
        };
        let items: Vec<ReminderItem> = serde_json::from_str(&json_str).unwrap_or_default();
        Ok(items)
    }

    async fn get_unread_emails(&self) -> Result<Vec<MailItem>> {
        let output = tokio::time::timeout(
            std::time::Duration::from_millis(2000),
            tokio::task::spawn_blocking(|| {
                std::process::Command::new("osascript")
                    .args([
                        "-e",
                        r#"tell application "System Events"
    if not (exists (processes where name is "Mail")) then return ""
end tell
tell application "Mail"
    try
        set unreadList to {}
        set msgs to (messages of inbox whose read status is false)
        repeat with m in msgs
            set s_id to id of m as text
            set s_sender to sender of m as text
            set s_subj to subject of m as text
            set s_flag to flagged status of m as text
            set s_snip to ""
            try
                set c to content of m as text
                if length of c > 200 then
                    set s_snip to text 1 thru 200 of c
                else
                    set s_snip to c
                end if
            end try
            set end of unreadList to (s_id & "<|GM_FLD|>" & s_sender & "<|GM_FLD|>" & s_subj & "<|GM_FLD|>" & s_flag & "<|GM_FLD|>" & s_snip)
        end repeat
        set AppleScript's text item delimiters to "<|GM_REC|>"
        return unreadList as text
    on error
        return ""
    end try
end tell"#,
                    ])
                    .output()
            }),
        )
        .await;

        if let Ok(Ok(Ok(out))) = output
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut emails = Vec::new();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            if text.contains("<|GM_FLD|>") {
                for rec in text.split("<|GM_REC|>") {
                    let rec = rec.trim();
                    if rec.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = rec.split("<|GM_FLD|>").collect();
                    if parts.len() >= 3 {
                        let id = parts[0].trim().to_string();
                        let sender = parts[1].replace(['\r', '\n', '\t'], " ").trim().to_string();
                        let subject = parts[2].replace(['\r', '\n', '\t'], " ").trim().to_string();
                        let is_flagged = parts.get(3).map(|s| s.trim() == "true").unwrap_or(false);
                        let snippet = parts
                            .get(4)
                            .map(|s| s.replace(['\r', '\n', '\t'], " ").trim().to_string())
                            .unwrap_or_default();

                        let mut mail = MailItem {
                            id,
                            sender,
                            subject,
                            snippet,
                            received_time_ms: now_ms,
                            is_read: false,
                            is_flagged,
                            is_urgent: false,
                        };
                        mail.is_urgent = mail.is_urgent_heuristic();
                        emails.push(mail);
                    }
                }
            } else {
                for line in text.lines() {
                    let parts: Vec<&str> = line.split('\t').collect();
                    if parts.len() >= 3 {
                        let id = parts[0].trim().to_string();
                        let sender = parts[1].trim().to_string();
                        let subject = parts[2].trim().to_string();
                        let is_flagged = parts.get(3).map(|s| s.trim() == "true").unwrap_or(false);
                        let snippet = parts
                            .get(4)
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default();
                        let mut mail = MailItem {
                            id,
                            sender,
                            subject,
                            snippet,
                            received_time_ms: now_ms,
                            is_read: false,
                            is_flagged,
                            is_urgent: false,
                        };
                        mail.is_urgent = mail.is_urgent_heuristic();
                        emails.push(mail);
                    }
                }
            }
            return Ok(emails);
        }

        Ok(Vec::new())
    }

    async fn send_notification(
        &self,
        title: &str,
        subtitle: Option<&str>,
        body: &str,
    ) -> Result<bool> {
        let c_title = CString::new(title).unwrap_or_else(|_| CString::new("Grok-Muse").unwrap());
        let c_sub = subtitle
            .and_then(|s| CString::new(s).ok())
            .unwrap_or_else(|| CString::new("").unwrap());
        let c_body = CString::new(body).unwrap_or_else(|_| CString::new("").unwrap());

        let ret =
            unsafe { gm_mac_send_notification(c_title.as_ptr(), c_sub.as_ptr(), c_body.as_ptr()) };
        Ok(ret == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mac_platform_permissions_and_activity() {
        let platform = MacPlatform::new().expect("init platform");
        let perms = platform.permissions().await.expect("permissions");
        assert!(perms.screen_recording);
        assert!(perms.accessibility);

        let activity = platform.user_activity().await.expect("user activity");
        assert!(activity.seconds_since_last_event >= 0.0);
    }

    #[tokio::test]
    async fn test_mac_platform_ax_snapshot() {
        let platform = MacPlatform::new().expect("init platform");
        let snapshot = platform.ax_snapshot(None).await.expect("ax snapshot");
        assert!(!snapshot.app_name.is_empty());
        assert!(snapshot.bounds.width > 0.0);
    }

    #[tokio::test]
    async fn test_mac_platform_capture_and_downscale() {
        let platform = MacPlatform::new().expect("init platform");
        let frame = platform.capture(Some(960), None).await.expect("capture");
        assert!(frame.width > 0);
        assert!(frame.height > 0);
        assert!(frame.width <= 960 || frame.height <= 960);
    }

    #[tokio::test]
    async fn test_mac_platform_actions_dispatch() {
        let platform = MacPlatform::new().expect("init platform");

        // Test key combo dispatch
        let combo = ComputerAction::KeyCombo {
            modifiers: vec!["Command".into()],
            key: "c".into(),
        };
        platform.act(&combo).await.expect("key combo dispatch");

        // Test drag dispatch
        let drag = ComputerAction::Drag {
            start: (100.0, 100.0),
            end: (150.0, 150.0),
        };
        platform.act(&drag).await.expect("drag dispatch");

        // Test wait dispatch
        let wait = ComputerAction::Wait {
            condition: None,
            timeout_ms: 10,
        };
        platform.act(&wait).await.expect("wait dispatch");
    }

    #[tokio::test]
    async fn test_mac_platform_jpeg_encoding() {
        let platform = MacPlatform::new().expect("init platform");
        let w = 640;
        let h = 480;
        let rgba = vec![128u8; (w * h * 4) as usize];
        let jpeg = platform
            .encode_jpeg(&rgba, w, h, 0.70)
            .expect("encode jpeg");
        assert!(!jpeg.is_empty());
        // Verify JPEG SOI marker
        assert_eq!(jpeg[0], 0xFF);
        assert_eq!(jpeg[1], 0xD8);
    }

    #[tokio::test]
    async fn test_mac_platform_eventkit_and_notifications() {
        let platform = MacPlatform::new().expect("init platform");
        // Test querying calendar events (now to +24h)
        let now_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()) as u64;
        let future_ms = now_ms + 86_400_000;
        let events = platform.get_calendar_events(now_ms, future_ms).await;
        assert!(events.is_ok());

        // Test querying reminders
        let rems = platform.get_reminders(false).await;
        assert!(rems.is_ok());

        // Test dispatching a notification
        let notif_res = platform
            .send_notification(
                "Grok-Muse Test",
                Some("Unit Test"),
                "Alert delivery verified",
            )
            .await;
        assert!(notif_res.is_ok());
    }

    #[tokio::test]
    async fn test_mac_platform_get_unread_emails() {
        let platform = MacPlatform::new().expect("init platform");
        let res = platform.get_unread_emails().await;
        assert!(res.is_ok());
    }
}

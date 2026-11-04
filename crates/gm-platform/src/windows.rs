use windows::Win32::UI::Accessibility::{
    UIA_ButtonControlTypeId as BUTTON, UIA_CheckBoxControlTypeId as CHECKBOX,
    UIA_ComboBoxControlTypeId as COMBOBOX, UIA_EditControlTypeId as EDIT,
    UIA_HyperlinkControlTypeId as LINK, UIA_ListItemControlTypeId as LISTITEM,
    UIA_MenuItemControlTypeId as MENUITEM, UIA_TreeItemControlTypeId as TREEITEM,
};
// Native Windows UI Automation, virtual-desktop capture and SendInput.
use crate::traits::Platform;
use async_trait::async_trait;
use gm_core::{
    error::{GmError, Result},
    types::*,
};
use std::{
    collections::{HashMap, VecDeque},
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::core::{Interface, PWSTR};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, SystemInformation::GetTickCount64},
        UI::{Accessibility::*, HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::HSTRING,
};
fn error(e: impl std::fmt::Display) -> GmError {
    GmError::Platform(format!("Windows desktop: {e}"))
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
struct Com;
impl Com {
    fn enter() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(error)?;
        }
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
#[derive(Clone)]
struct Target {
    window: usize,
    path: Vec<usize>,
    name: String,
    automation_id: String,
    process: i32,
}
pub struct WindowsPlatform {
    cancelled: parking_lot::RwLock<Arc<AtomicBool>>,
    targets: Arc<parking_lot::Mutex<HashMap<String, Target>>>,
}
impl WindowsPlatform {
    pub fn new() -> Result<Self> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Ok(Self {
            cancelled: parking_lot::RwLock::new(Arc::new(AtomicBool::new(false))),
            targets: Default::default(),
        })
    }
}
fn desktop_bounds() -> Result<Rect> {
    unsafe {
        let (x, y, w, h) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        if w <= 0 || h <= 0 {
            return Err(error("No interactive desktop"));
        }
        Ok(Rect::new(x.into(), y.into(), w.into(), h.into()))
    }
}
struct Capture {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.0.is_null() {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.memory.0.is_null() {
                let _ = DeleteDC(self.memory);
            }
            ReleaseDC(None, self.screen);
        }
    }
}
fn capture(edge: Option<u32>, quality: Option<u8>) -> Result<FrameCapture> {
    unsafe {
        if GetForegroundWindow().0.is_null() {
            return Err(error("No interactive foreground desktop"));
        }
        let bounds = desktop_bounds()?;
        if bounds.width * bounds.height > 64_000_000.0 {
            return Err(error("Desktop exceeds the capture size limit"));
        }
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err(error("Cannot access the interactive desktop"));
        }
        let mut capture = Capture {
            screen,
            memory: CreateCompatibleDC(Some(screen)),
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
        };
        if capture.memory.0.is_null() {
            return Err(error("Cannot allocate a capture context"));
        }
        let (width, height) = (bounds.width as u32, bounds.height as u32);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = std::ptr::null_mut();
        capture.bitmap =
            CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut pixels, None, 0)
                .map_err(error)?;
        if pixels.is_null() {
            return Err(error("Capture bitmap is unavailable"));
        }
        capture.previous = SelectObject(capture.memory, capture.bitmap.into());
        BitBlt(
            capture.memory,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen),
            bounds.x as i32,
            bounds.y as i32,
            SRCCOPY | CAPTUREBLT,
        )
        .map_err(error)?;
        let _ = GdiFlush();
        let bgra =
            std::slice::from_raw_parts(pixels.cast::<u8>(), width as usize * height as usize * 4);
        let mut rgba = Vec::with_capacity(bgra.len());
        for pixel in bgra.as_chunks::<4>().0 {
            rgba.extend([pixel[2], pixel[1], pixel[0], 255]);
        }
        let mut frame = crate::pixels::jpeg(width, height, rgba, edge, quality)?;
        frame.desktop_bounds = Some(bounds);
        Ok(frame)
    }
}
fn snapshot(
    window: Option<u32>,
    cache: &parking_lot::Mutex<HashMap<String, Target>>,
) -> Result<AxSnapshot> {
    let _com = Com::enter()?;
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(error)?;
        let hwnd = window
            .map(|id| HWND(id as usize as *mut _))
            .unwrap_or_else(|| GetForegroundWindow());
        if hwnd.0.is_null() {
            return Err(error("No foreground window"));
        }
        if let Ok(timed) = automation.cast::<IUIAutomation2>() {
            timed.SetConnectionTimeout(300).map_err(error)?;
            timed.SetTransactionTimeout(500).map_err(error)?;
        }
        let root = automation.ElementFromHandle(hwnd).map_err(error)?;
        let walker = automation.ControlViewWalker().map_err(error)?;
        let bounds = root.CurrentBoundingRectangle().map_err(error)?;
        let title = root.CurrentName().map_err(error)?.to_string();
        let process = root.CurrentProcessId().map_err(error)?;
        let handle = windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            process as u32,
        )
        .map_err(error)?;
        struct Process(HANDLE);
        impl Drop for Process {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let _process_handle = Process(handle);
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        windows::Win32::System::Threading::QueryFullProcessImageNameW(
            handle,
            windows::Win32::System::Threading::PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
        .map_err(error)?;
        let executable = String::from_utf16_lossy(&path[..length as usize]);
        let app_name = std::path::Path::new(&executable)
            .file_stem()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(executable);
        let mut queue = VecDeque::from([(root, Vec::<usize>::new())]);
        let mut targets = HashMap::new();
        let mut elements = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(750);
        while let Some((element, path)) = queue.pop_front() {
            if elements.len() >= 1000 || std::time::Instant::now() > deadline {
                break;
            }
            let name = element
                .CurrentName()
                .map(|v| v.to_string())
                .unwrap_or_default();
            let automation_id = element
                .CurrentAutomationId()
                .map(|v| v.to_string())
                .unwrap_or_default();
            let control = element.CurrentControlType().map_err(error)?;
            let role = match control {
                BUTTON => "AXButton",
                EDIT => "AXTextField",
                CHECKBOX => "AXCheckBox",
                COMBOBOX => "AXComboBox",
                LINK => "AXLink",
                MENUITEM => "AXMenuItem",
                LISTITEM | TREEITEM => "AXRow",
                _ => "AXGroup",
            };
            let rectangle = element.CurrentBoundingRectangle().map_err(error)?;
            let secure = element.CurrentIsPassword().map_err(error)?.as_bool();
            let enabled = element.CurrentIsEnabled().map_err(error)?.as_bool();
            let value = if secure {
                None
            } else {
                element
                    .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    .and_then(|p| p.CurrentValue())
                    .ok()
                    .map(|v| v.to_string())
            };
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            (hwnd.0 as usize, &path, &name, &automation_id).hash(&mut hash);
            let token = format!("uia:{:x}", hash.finish());
            targets.insert(
                token.clone(),
                Target {
                    window: hwnd.0 as usize,
                    path: path.clone(),
                    name: name.clone(),
                    automation_id,
                    process,
                },
            );
            elements.push(AxElement {
                token,
                role: role.into(),
                label: Some(name),
                value,
                frame: Rect::new(
                    rectangle.left.into(),
                    rectangle.top.into(),
                    rectangle.right.saturating_sub(rectangle.left).into(),
                    rectangle.bottom.saturating_sub(rectangle.top).into(),
                ),
                is_enabled: enabled,
                is_secure: secure,
            });
            let mut child = walker.GetFirstChildElement(&element).ok();
            let mut index = 0;
            while let Some(element) = child {
                if queue.len() >= 2000 {
                    break;
                }
                let mut path = path.clone();
                path.push(index);
                child = walker.GetNextSiblingElement(&element).ok();
                queue.push_back((element, path));
                index += 1;
            }
        }
        *cache.lock() = targets;
        Ok(AxSnapshot {
            window_id: hwnd.0 as usize as u32,
            app_name,
            window_title: title,
            bounds: Rect::new(
                bounds.left.into(),
                bounds.top.into(),
                (bounds.right - bounds.left).into(),
                (bounds.bottom - bounds.top).into(),
            ),
            elements,
            timestamp_ms: now(),
        })
    }
}
fn invoke(target: Target) -> Result<()> {
    let _com = Com::enter()?;
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(error)?;
        let walker = automation.ControlViewWalker().map_err(error)?;
        let mut element = automation
            .ElementFromHandle(HWND(target.window as *mut _))
            .map_err(error)?;
        for index in target.path {
            let mut child = walker.GetFirstChildElement(&element).map_err(error)?;
            for _ in 0..index {
                child = walker.GetNextSiblingElement(&child).map_err(error)?;
            }
            element = child;
        }
        if element.CurrentProcessId().map_err(error)? != target.process
            || element.CurrentName().map_err(error)? != target.name
            || element.CurrentAutomationId().map_err(error)? != target.automation_id
        {
            return Err(error(
                "Accessibility target changed; inspect the window again",
            ));
        }
        if !element.CurrentIsEnabled().map_err(error)?.as_bool() {
            return Err(error("Accessibility target is disabled"));
        }
        element
            .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            .map_err(error)?
            .Invoke()
            .map_err(error)
    }
}
fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
fn mouse(x: i32, y: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
fn send(inputs: &[INPUT]) -> Result<()> {
    if unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) } == inputs.len() as u32 {
        Ok(())
    } else {
        Err(error(
            "Windows rejected input (check desktop permissions and application integrity level)",
        ))
    }
}
fn virtual_key(name: &str) -> Result<VIRTUAL_KEY> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" | "option" => VK_MENU,
        "win" | "cmd" | "command" | "meta" | "super" => VK_LWIN,
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "backspace" => VK_BACK,
        "delete" => VK_DELETE,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "home" => VK_HOME,
        "end" => VK_END,
        "space" => VK_SPACE,
        _ if name.len() == 1 && name.is_ascii() => {
            VIRTUAL_KEY(name.as_bytes()[0].to_ascii_uppercase() as u16)
        }
        _ => return Err(error("Unsupported key name")),
    })
}
fn input(action: &ComputerAction, cancel: &AtomicBool) -> Result<()> {
    let bounds = desktop_bounds()?;
    let motion = |x: f64, y: f64| -> Result<INPUT> {
        if !x.is_finite()
            || !y.is_finite()
            || x < bounds.x
            || y < bounds.y
            || x >= bounds.x + bounds.width
            || y >= bounds.y + bounds.height
        {
            return Err(error("Pointer target outside virtual desktop"));
        }
        Ok(mouse(
            ((x - bounds.x) * 65535.0 / (bounds.width - 1.0).max(1.0)).round() as i32,
            ((y - bounds.y) * 65535.0 / (bounds.height - 1.0).max(1.0)).round() as i32,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        ))
    };
    if cancel.load(Ordering::SeqCst) {
        return Err(error("Task cancelled"));
    }
    match action {
        ComputerAction::Type { text } => {
            if text.len() > 8192 {
                return Err(error("Type text in chunks of at most 8 KiB"));
            }
            for unit in text.encode_utf16() {
                if cancel.load(Ordering::SeqCst) {
                    return Err(error("Task cancelled"));
                }
                send(&[
                    keyboard(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE),
                    keyboard(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ])?;
            }
            Ok(())
        }
        ComputerAction::KeyCombo { modifiers, key } => {
            let modifiers = modifiers
                .iter()
                .map(|key| virtual_key(key))
                .collect::<Result<Vec<_>>>()?;
            let key = virtual_key(key)?;
            for key in &modifiers {
                send(&[keyboard(*key, 0, KEYBD_EVENT_FLAGS(0))])?;
            }
            let result = send(&[
                keyboard(key, 0, KEYBD_EVENT_FLAGS(0)),
                keyboard(key, 0, KEYEVENTF_KEYUP),
            ]);
            for key in modifiers.iter().rev() {
                let _ = send(&[keyboard(*key, 0, KEYEVENTF_KEYUP)]);
            }
            result
        }
        ComputerAction::Click {
            coords: Some((x, y)),
            button,
            click_count,
            ..
        } => {
            let (down, up) = match button {
                Some(MouseButton::Right) => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                Some(MouseButton::Middle) => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
                _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            };
            send(&[motion(*x, *y)?])?;
            for _ in 0..click_count.unwrap_or(1).clamp(1, 3) {
                send(&[mouse(0, 0, 0, down), mouse(0, 0, 0, up)])?;
            }
            Ok(())
        }
        ComputerAction::Scroll { dx, dy } => {
            if *dy != 0 {
                send(&[mouse(
                    0,
                    0,
                    dy.saturating_mul(-120) as u32,
                    MOUSEEVENTF_WHEEL,
                )])?;
            }
            if *dx != 0 {
                send(&[mouse(
                    0,
                    0,
                    dx.saturating_mul(120) as u32,
                    MOUSEEVENTF_HWHEEL,
                )])?;
            }
            Ok(())
        }
        ComputerAction::Drag { start, end } => {
            send(&[
                motion(start.0, start.1)?,
                mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN),
            ])?;
            let result = (|| {
                for step in 1..=20 {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(error("Task cancelled"));
                    }
                    let ratio = step as f64 / 20.0;
                    send(&[motion(
                        start.0 + (end.0 - start.0) * ratio,
                        start.1 + (end.1 - start.1) * ratio,
                    )?])?;
                }
                Ok(())
            })();
            let _ = send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
            result
        }
        _ => Err(error(
            "This action requires an accessibility target or supported application identifier",
        )),
    }
}
#[async_trait]
impl Platform for WindowsPlatform {
    async fn begin_task(&self, _: &str) -> Result<()> {
        let mut token = self.cancelled.write();
        token.store(true, Ordering::SeqCst);
        *token = Arc::new(AtomicBool::new(false));
        self.targets.lock().clear();
        Ok(())
    }
    async fn permissions(&self) -> Result<PermissionStatus> {
        let interactive = unsafe { !GetForegroundWindow().0.is_null() };
        let uia = tokio::task::spawn_blocking(|| {
            let _com = Com::enter()?;
            unsafe {
                let _: IUIAutomation =
                    CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(error)?;
            }
            Ok::<_, GmError>(())
        })
        .await
        .map_err(error)?
        .is_ok();
        Ok(PermissionStatus {
            screen_recording: interactive,
            accessibility: interactive && uia,
            calendar: false,
            reminders: false,
        })
    }
    async fn user_activity(&self) -> Result<UserActivity> {
        unsafe {
            let mut input = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                ..Default::default()
            };
            if !GetLastInputInfo(&mut input).as_bool() {
                return Err(error("Cannot query user input activity"));
            }
            let elapsed = (GetTickCount64() as u32).wrapping_sub(input.dwTime);
            let seconds = f64::from(elapsed) / 1000.0;
            Ok(UserActivity {
                seconds_since_last_event: seconds,
                is_user_active: seconds < 1.5,
            })
        }
    }
    async fn capture(&self, edge: Option<u32>, quality: Option<u8>) -> Result<FrameCapture> {
        tokio::task::spawn_blocking(move || capture(edge, quality))
            .await
            .map_err(error)?
    }
    async fn ax_snapshot(&self, window: Option<u32>) -> Result<AxSnapshot> {
        let targets = self.targets.clone();
        tokio::task::spawn_blocking(move || snapshot(window, &targets))
            .await
            .map_err(error)?
    }
    async fn act(&self, action: &ComputerAction) -> Result<()> {
        if self.cancelled.read().load(Ordering::SeqCst) {
            return Err(error("Task cancelled"));
        }
        if let ComputerAction::Click {
            token: Some(token),
            coords: None,
            ..
        } = action
        {
            let target = self
                .targets
                .lock()
                .get(token)
                .cloned()
                .ok_or_else(|| error("Accessibility target is not in the last snapshot"))?;
            return tokio::task::spawn_blocking(move || invoke(target))
                .await
                .map_err(error)?;
        }
        if let ComputerAction::Wait { timeout_ms, .. } = action {
            tokio::time::sleep(std::time::Duration::from_millis((*timeout_ms).min(5000))).await;
            return Ok(());
        }
        let action = action.clone();
        let cancelled = self.cancelled.read().clone();
        tokio::task::spawn_blocking(move || input(&action, &cancelled))
            .await
            .map_err(error)?
    }
    async fn cancel(&self) -> Result<()> {
        self.cancelled.read().store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn get_calendar_events(&self, _: u64, _: u64) -> Result<Vec<CalendarItem>> {
        Err(error("Calendar access requires a configured connector"))
    }
    async fn get_reminders(&self, _: bool) -> Result<Vec<ReminderItem>> {
        Err(error("Reminder access requires a configured connector"))
    }
    async fn get_unread_emails(&self) -> Result<Vec<MailItem>> {
        Err(error("Mail access requires a configured connector"))
    }
    async fn send_notification(
        &self,
        title: &str,
        subtitle: Option<&str>,
        body: &str,
    ) -> Result<bool> {
        let title = title.to_string();
        let body = subtitle
            .map(|s| format!("{s}\n{body}"))
            .unwrap_or_else(|| body.into());
        tokio::task::spawn_blocking(move || {
            use windows::Win32::System::WinRT::{RoInitialize,RoUninitialize,RO_INIT_MULTITHREADED};
            unsafe {RoInitialize(RO_INIT_MULTITHREADED).map_err(error)?;}
            struct Apartment;
            impl Drop for Apartment {fn drop(&mut self) {unsafe {RoUninitialize();}}}
            let _apartment=Apartment;
            let document=windows::Data::Xml::Dom::XmlDocument::new().map_err(error)?;
            document.LoadXml(&HSTRING::from("<toast><visual><binding template=\"ToastGeneric\"><text/><text/></binding></visual></toast>")).map_err(error)?;
            let texts=document.GetElementsByTagName(&HSTRING::from("text")).map_err(error)?;
            texts.Item(0).map_err(error)?.AppendChild(&document.CreateTextNode(&HSTRING::from(title)).map_err(error)?).map_err(error)?;
            texts.Item(1).map_err(error)?.AppendChild(&document.CreateTextNode(&HSTRING::from(body)).map_err(error)?).map_err(error)?;
            let notification=windows::UI::Notifications::ToastNotification::CreateToastNotification(&document).map_err(error)?;
            windows::UI::Notifications::ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("GrokMuse")).map_err(error)?.Show(&notification).map_err(error)?;
            Ok(true)
        }).await.map_err(error)?
    }
}

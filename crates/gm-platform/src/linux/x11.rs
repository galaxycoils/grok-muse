use gm_core::{
    error::{GmError, Result},
    types::{AxSnapshot, ComputerAction, FrameCapture, MouseButton, Rect, UserActivity},
};
use x11rb::{
    connection::Connection,
    protocol::{
        screensaver::ConnectionExt as _,
        xproto::{self, ConnectionExt as _, ImageFormat, ImageOrder, Window},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};
fn error(e: impl std::fmt::Display) -> GmError {
    GmError::Platform(format!("X11: {e}"))
}
fn connection() -> Result<(RustConnection, usize)> {
    x11rb::connect(None).map_err(error)
}
pub(super) fn available() -> bool {
    connection().is_ok()
}
fn atom(c: &RustConnection, name: &[u8]) -> Result<u32> {
    Ok(c.intern_atom(false, name)
        .map_err(error)?
        .reply()
        .map_err(error)?
        .atom)
}
fn property(c: &RustConnection, w: Window, name: &[u8]) -> Result<Vec<u8>> {
    Ok(
        c.get_property(false, w, atom(c, name)?, xproto::AtomEnum::ANY, 0, 4096)
            .map_err(error)?
            .reply()
            .map_err(error)?
            .value,
    )
}
pub(super) fn snapshot(window: Option<u32>) -> Result<AxSnapshot> {
    let (c, screen) = connection()?;
    let root = c.setup().roots[screen].root;
    let window = window
        .or_else(|| {
            property(&c, root, b"_NET_ACTIVE_WINDOW")
                .ok()
                .and_then(|v| {
                    v.get(..4)
                        .map(|v| u32::from_ne_bytes(v.try_into().unwrap()))
                })
        })
        .filter(|id| *id != 0)
        .unwrap_or(
            c.get_input_focus()
                .map_err(error)?
                .reply()
                .map_err(error)?
                .focus,
        );
    if window == 0 || window == 1 {
        return Err(error("No focused window"));
    }
    let geometry = c
        .get_geometry(window)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let translated = c
        .translate_coordinates(window, root, 0, 0)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let title = property(&c, window, b"_NET_WM_NAME")
        .or_else(|_| property(&c, window, b"WM_NAME"))
        .unwrap_or_default();
    let title = if title.is_empty() {
        property(&c, window, b"WM_NAME").unwrap_or_default()
    } else {
        title
    };
    let class = property(&c, window, b"WM_CLASS").unwrap_or_default();
    Ok(AxSnapshot {
        window_id: window,
        app_name: String::from_utf8_lossy(&class)
            .split('\0')
            .find(|s| !s.is_empty())
            .unwrap_or("X11 window")
            .into(),
        window_title: String::from_utf8_lossy(&title)
            .trim_end_matches('\0')
            .into(),
        bounds: Rect::new(
            translated.dst_x.into(),
            translated.dst_y.into(),
            geometry.width.into(),
            geometry.height.into(),
        ),
        elements: Vec::new(),
        timestamp_ms: super::now(),
    })
}
pub(super) fn capture(edge: Option<u32>, quality: Option<u8>) -> Result<FrameCapture> {
    let (c, screen) = connection()?;
    let setup = c.setup();
    let root = &setup.roots[screen];
    if u64::from(root.width_in_pixels) * u64::from(root.height_in_pixels) > 64_000_000 {
        return Err(error("Desktop exceeds capture size limit"));
    }
    let reply = c
        .get_image(
            ImageFormat::Z_PIXMAP,
            root.root,
            0,
            0,
            root.width_in_pixels,
            root.height_in_pixels,
            u32::MAX,
        )
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == reply.depth)
        .ok_or_else(|| error("Unsupported pixel format"))?;
    let bytes_per_pixel = usize::from(format.bits_per_pixel) / 8;
    if !matches!(bytes_per_pixel, 3 | 4) || format.scanline_pad == 0 {
        return Err(error("Capture requires a 24/32-bit X11 display"));
    }
    let width = usize::from(root.width_in_pixels);
    let height = usize::from(root.height_in_pixels);
    let stride = (width * usize::from(format.bits_per_pixel))
        .div_ceil(usize::from(format.scanline_pad))
        * usize::from(format.scanline_pad)
        / 8;
    if reply.data.len() < stride * height {
        return Err(error("Truncated pixel buffer"));
    }
    let visual = root
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == reply.visual)
        .ok_or_else(|| error("Capture visual not found"))?;
    let channel = |pixel: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 0;
        }
        let shift = mask.trailing_zeros();
        (((pixel & mask) >> shift) * 255 / (mask >> shift)) as u8
    };
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let offset = y * stride + x * bytes_per_pixel;
            let bytes = &reply.data[offset..offset + bytes_per_pixel];
            let mut pixel = 0u32;
            for (i, b) in bytes.iter().enumerate() {
                let shift = if setup.image_byte_order == ImageOrder::LSB_FIRST {
                    i * 8
                } else {
                    (bytes_per_pixel - 1 - i) * 8
                };
                pixel |= u32::from(*b) << shift;
            }
            rgba.extend([
                channel(pixel, visual.red_mask),
                channel(pixel, visual.green_mask),
                channel(pixel, visual.blue_mask),
                255,
            ]);
        }
    }
    super::super::pixels::jpeg(width as u32, height as u32, rgba, edge, quality)
}
pub(super) fn activity() -> Result<UserActivity> {
    let (c, s) = connection()?;
    let reply = c
        .screensaver_query_info(c.setup().roots[s].root)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let seconds = f64::from(reply.ms_since_user_input) / 1000.0;
    Ok(UserActivity {
        seconds_since_last_event: seconds,
        is_user_active: seconds < 1.5,
    })
}
fn fake(c: &RustConnection, event: u8, detail: u8, x: i16, y: i16) -> Result<()> {
    c.xtest_fake_input(event, detail, 0, x11rb::NONE, x, y, 0)
        .map_err(error)?
        .check()
        .map_err(error)
}
fn keycode(c: &RustConnection, name: &str) -> Result<u8> {
    let symbol = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => 0xffe3,
        "shift" => 0xffe1,
        "alt" | "option" => 0xffe9,
        "super" | "meta" | "cmd" | "command" => 0xffeb,
        "enter" | "return" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "home" => 0xff50,
        "end" => 0xff57,
        "space" => 0x20,
        _ if name.chars().count() == 1 => {
            u32::from(name.chars().next().unwrap().to_ascii_lowercase())
        }
        _ => return Err(error("Unsupported key name")),
    };
    let setup = c.setup();
    let map = c
        .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    for (i, syms) in map
        .keysyms
        .chunks(usize::from(map.keysyms_per_keycode))
        .enumerate()
    {
        if syms.contains(&symbol) {
            return Ok(setup.min_keycode + i as u8);
        }
    }
    Err(error(format!(
        "Key {name:?} is not present in the current keyboard layout"
    )))
}
pub(super) fn act(action: &ComputerAction, cancel: &std::sync::atomic::AtomicBool) -> Result<()> {
    use std::sync::atomic::Ordering;
    let (c, screen) = connection()?;
    let root = &c.setup().roots[screen];
    let point = |x: f64, y: f64| -> Result<(i16, i16)> {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(root.width_in_pixels)
            || y >= f64::from(root.height_in_pixels)
            || x > i16::MAX as f64
            || y > i16::MAX as f64
        {
            Err(error("Pointer target outside display"))
        } else {
            Ok((x.round() as i16, y.round() as i16))
        }
    };
    if cancel.load(Ordering::SeqCst) {
        return Err(error("Task cancelled"));
    }
    match action {
        ComputerAction::Click {
            coords: Some((x, y)),
            button,
            click_count,
            ..
        } => {
            let (x, y) = point(*x, *y)?;
            fake(&c, 6, 0, x, y)?;
            let button = match button {
                Some(MouseButton::Right) => 3,
                Some(MouseButton::Middle) => 2,
                _ => 1,
            };
            for _ in 0..click_count.unwrap_or(1).clamp(1, 3) {
                fake(&c, 4, button, 0, 0)?;
                fake(&c, 5, button, 0, 0)?;
            }
        }
        ComputerAction::KeyCombo { modifiers, key } => {
            let keys = modifiers
                .iter()
                .map(|k| keycode(&c, k))
                .collect::<Result<Vec<_>>>()?;
            let key = keycode(&c, key)?;
            for key in &keys {
                fake(&c, 2, *key, 0, 0)?;
            }
            let result = fake(&c, 2, key, 0, 0).and_then(|_| fake(&c, 3, key, 0, 0));
            for key in keys.iter().rev() {
                let _ = fake(&c, 3, *key, 0, 0);
            }
            result?;
        }
        ComputerAction::Type { text } => {
            if text.len() > 8192 {
                return Err(error("Type text in chunks of at most 8 KiB"));
            }
            let setup = c.setup();
            let map = c
                .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
                .map_err(error)?
                .reply()
                .map_err(error)?;
            let shift = keycode(&c, "shift")?;
            let mut keys = Vec::new();
            for ch in text.chars() {
                let symbol = match ch {
                    '\n' => 0xff0d,
                    '\t' => 0xff09,
                    _ if ch as u32 <= 0xff => ch as u32,
                    _ => {
                        return Err(error(
                            "Unicode insertion requires the application's AT-SPI editable text interface",
                        ));
                    }
                };
                let found = map
                    .keysyms
                    .chunks(usize::from(map.keysyms_per_keycode))
                    .enumerate()
                    .find_map(|(i, v)| {
                        v.iter()
                            .take(2)
                            .position(|s| *s == symbol)
                            .map(|column| (setup.min_keycode + i as u8, column == 1))
                    })
                    .ok_or_else(|| {
                        error("Character not present in keyboard layout; use AT-SPI text insertion")
                    })?;
                keys.push(found);
            }
            for (key, shifted) in keys {
                if cancel.load(Ordering::SeqCst) {
                    return Err(error("Task cancelled"));
                }
                if shifted {
                    fake(&c, 2, shift, 0, 0)?;
                }
                let result = fake(&c, 2, key, 0, 0).and_then(|_| fake(&c, 3, key, 0, 0));
                if shifted {
                    let _ = fake(&c, 3, shift, 0, 0);
                }
                result?;
            }
        }
        ComputerAction::Scroll { dx, dy } => {
            for (delta, negative, positive) in [(*dy, 4, 5), (*dx, 6, 7)] {
                for _ in 0..delta.unsigned_abs().min(100) {
                    let button = if delta < 0 { negative } else { positive };
                    fake(&c, 4, button, 0, 0)?;
                    fake(&c, 5, button, 0, 0)?;
                }
            }
        }
        ComputerAction::Drag { start, end } => {
            let (sx, sy) = point(start.0, start.1)?;
            let (ex, ey) = point(end.0, end.1)?;
            fake(&c, 6, 0, sx, sy)?;
            fake(&c, 4, 1, 0, 0)?;
            let result = (|| {
                for step in 1..=20 {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(error("Task cancelled"));
                    }
                    fake(
                        &c,
                        6,
                        0,
                        (i32::from(sx) + (i32::from(ex) - i32::from(sx)) * step / 20) as i16,
                        (i32::from(sy) + (i32::from(ey) - i32::from(sy)) * step / 20) as i16,
                    )?;
                }
                Ok(())
            })();
            let _ = fake(&c, 5, 1, 0, 0);
            result?;
        }
        ComputerAction::FocusApp { app_name } => {
            let clients = property(&c, root.root, b"_NET_CLIENT_LIST")?;
            let mut found = None;
            for bytes in clients.as_chunks::<4>().0 {
                let window = u32::from_ne_bytes(*bytes);
                let class = property(&c, window, b"WM_CLASS").unwrap_or_default();
                if String::from_utf8_lossy(&class)
                    .split('\0')
                    .any(|s| s.eq_ignore_ascii_case(app_name))
                {
                    found = Some(window);
                    break;
                }
            }
            let window = found.ok_or_else(|| error("Requested application is not running"))?;
            let event = xproto::ClientMessageEvent::new(
                32,
                window,
                atom(&c, b"_NET_ACTIVE_WINDOW")?,
                [2, 0, 0, 0, 0],
            );
            c.send_event(
                false,
                root.root,
                xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
        }
        _ => return Err(error("This action requires AT-SPI or a desktop portal")),
    }
    c.flush().map_err(error)?;
    c.get_input_focus().map_err(error)?.reply().map_err(error)?;
    Ok(())
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use x11rb::{
        protocol::{Event, xproto::*},
        wrapper::ConnectionExt as _,
    };

    #[test]
    #[ignore = "Requires a private Xvfb display; run through tools/test-native-x11.sh"]
    fn real_capture_focus_keyboard_pointer_and_cancellation() {
        let (c, screen) = connection().unwrap();
        let root = &c.setup().roots[screen];
        let window = c.generate_id().unwrap();
        c.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root.root,
            20,
            30,
            240,
            120,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(0xff0000)
                .event_mask(EventMask::KEY_PRESS | EventMask::BUTTON_PRESS),
        )
        .unwrap()
        .check()
        .unwrap();
        c.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Grok Muse native fixture",
        )
        .unwrap();
        c.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"fixture\0GrokMuseTest\0",
        )
        .unwrap();
        c.map_window(window).unwrap();
        c.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
            .unwrap();
        c.flush().unwrap();
        c.get_input_focus().unwrap().reply().unwrap();
        let snapshot = snapshot(Some(window)).unwrap();
        assert_eq!(snapshot.window_title, "Grok Muse native fixture");
        assert_eq!(snapshot.app_name, "fixture");
        assert_eq!(snapshot.bounds, Rect::new(20.0, 30.0, 240.0, 120.0));
        let frame = capture(Some(4096), Some(95)).unwrap();
        let bounds = frame.desktop_bounds.unwrap();
        assert_eq!(bounds.width, f64::from(root.width_in_pixels));
        let image = image::load_from_memory(&crate::resolve_frame_bytes(&frame).unwrap())
            .unwrap()
            .to_rgb8();
        let pixel = image.get_pixel(100, 80);
        assert!(
            pixel[0] > 230 && pixel[1] < 20 && pixel[2] < 20,
            "Captured pixel: {pixel:?}"
        );
        let cancelled = AtomicBool::new(false);
        act(&ComputerAction::Type { text: "A".into() }, &cancelled).unwrap();
        act(
            &ComputerAction::Click {
                token: None,
                coords: Some((100.0, 80.0)),
                button: None,
                click_count: Some(1),
            },
            &cancelled,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let (mut key, mut click) = (false, false);
        while std::time::Instant::now() < deadline && !(key && click) {
            match c.poll_for_event().unwrap() {
                Some(Event::KeyPress(e)) => {
                    let symbols = c
                        .get_keyboard_mapping(e.detail, 1)
                        .unwrap()
                        .reply()
                        .unwrap()
                        .keysyms;
                    if symbols.contains(&u32::from(b'a')) {
                        key = true;
                        assert!(e.state.contains(KeyButMask::SHIFT));
                    }
                    assert_eq!(e.event, window);
                }
                Some(Event::ButtonPress(e)) => {
                    click = true;
                    assert_eq!((e.event_x, e.event_y), (80, 50));
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        assert!(
            key && click,
            "Input events were not delivered to the real X server"
        );
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            act(
                &ComputerAction::Type {
                    text: "must not type".into()
                },
                &cancelled
            )
            .is_err()
        );
        c.destroy_window(window).unwrap();
        c.flush().unwrap();
    }
}

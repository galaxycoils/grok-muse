//! One user-selected monitor is shared by capture and input. No XWayland fallback.
use super::{error, now};
use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop},
    screencast::{CursorMode, Screencast, SourceType},
};
use futures_util::StreamExt;
use gm_core::{error::Result, types::*};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

#[derive(Default)]
pub(super) struct Portal {
    current: Mutex<Option<Arc<Desktop>>>,
}
struct Desktop {
    remote: RemoteDesktop<'static>,
    session: Session<'static, RemoteDesktop<'static>>,
    node: u32,
    bounds: Rect,
    live: Arc<AtomicBool>,
}
async fn cancelled(token: &AtomicBool) {
    while !token.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
impl Portal {
    pub async fn available(&self) -> bool {
        self.current
            .try_lock()
            .is_ok_and(|d| d.as_ref().is_some_and(|d| d.live.load(Ordering::SeqCst)))
    }
    async fn desktop(&self, token: &Arc<AtomicBool>) -> Result<Arc<Desktop>> {
        let mut current = self.current.lock().await;
        if token.load(Ordering::SeqCst) {
            return Err(error("Task cancelled"));
        }
        if let Some(desktop) = current.as_ref().filter(|d| d.live.load(Ordering::SeqCst)) {
            return Ok(desktop.clone());
        }
        let remote = RemoteDesktop::new().await.map_err(error)?;
        let cast = Screencast::new().await.map_err(error)?;
        let session = remote.create_session().await.map_err(error)?;
        let setup = async {
            remote
                .select_devices(
                    &session,
                    DeviceType::Keyboard | DeviceType::Pointer,
                    None,
                    PersistMode::DoNot,
                )
                .await
                .map_err(error)?
                .response()
                .map_err(error)?;
            cast.select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor.into(),
                false,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(error)?
            .response()
            .map_err(error)?;
            let selection = remote
                .start(&session, None)
                .await
                .map_err(error)?
                .response()
                .map_err(error)?;
            if !selection.devices().contains(DeviceType::Keyboard)
                || !selection.devices().contains(DeviceType::Pointer)
            {
                return Err(error(
                    "The portal did not grant keyboard and pointer access",
                ));
            }
            let streams = selection
                .streams()
                .ok_or_else(|| error("No monitor was shared"))?;
            if streams.len() != 1 {
                return Err(error("Share exactly one monitor"));
            }
            let stream = &streams[0];
            let (width, height) = stream
                .size()
                .ok_or_else(|| error("Portal did not provide logical monitor dimensions"))?;
            let (x, y) = stream.position().unwrap_or((0, 0));
            if width <= 0 || height <= 0 {
                return Err(error("Invalid monitor dimensions"));
            }
            Ok((
                stream.pipe_wire_node_id(),
                Rect::new(x.into(), y.into(), width.into(), height.into()),
            ))
        };
        let result = tokio::select! {
            result = tokio::time::timeout(std::time::Duration::from_secs(120), setup) => result.map_err(|_| error("Portal consent timed out")).and_then(|r|r),
            _ = cancelled(token) => Err(error("Task cancelled during portal consent")),
        };
        let (node, bounds) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = session.close().await;
                return Err(e);
            }
        };
        let live = Arc::new(AtomicBool::new(true));
        // Register before publishing the session to avoid losing a revocation signal.
        let mut closed = session.receive_closed().await.map_err(error)?;
        let flag = live.clone();
        tokio::spawn(async move {
            let _ = closed.next().await;
            flag.store(false, Ordering::SeqCst);
        });
        let desktop = Arc::new(Desktop {
            remote,
            session,
            node,
            bounds,
            live,
        });
        *current = Some(desktop.clone());
        Ok(desktop)
    }
    pub async fn close(&self) {
        if let Some(d) = self.current.lock().await.take() {
            d.live.store(false, Ordering::SeqCst);
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), d.session.close()).await;
        }
    }
    pub async fn capture(
        &self,
        edge: Option<u32>,
        quality: Option<u8>,
        token: Arc<AtomicBool>,
    ) -> Result<FrameCapture> {
        let d = self.desktop(&token).await?;
        let cast = Screencast::new().await.map_err(error)?;
        let fd = cast
            .open_pipe_wire_remote(&d.session)
            .await
            .map_err(error)?;
        let bounds = d.bounds;
        let live = d.live.clone();
        let node = d.node;
        let frame = tokio::task::spawn_blocking(move || {
            pipewire_frame(fd, node, bounds, edge, quality, &token, &live)
        })
        .await
        .map_err(error)??;
        if !d.live.load(Ordering::SeqCst) {
            return Err(error("Screen sharing was revoked"));
        }
        Ok(frame)
    }
    pub async fn act(&self, action: &ComputerAction, token: Arc<AtomicBool>) -> Result<()> {
        let d = self.desktop(&token).await?;
        if !d.live.load(Ordering::SeqCst) || token.load(Ordering::SeqCst) {
            return Err(error("Desktop session ended"));
        }
        match action {
            ComputerAction::Click {
                coords: Some((x, y)),
                button,
                click_count,
                ..
            } => {
                d.pointer(*x, *y).await?;
                let button = match button.unwrap_or(MouseButton::Left) {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                };
                for _ in 0..click_count.unwrap_or(1).clamp(1, 3) {
                    if token.load(Ordering::SeqCst) {
                        return Err(error("Task cancelled"));
                    }
                    d.remote
                        .notify_pointer_button(&d.session, button, KeyState::Pressed)
                        .await
                        .map_err(error)?;
                    d.remote
                        .notify_pointer_button(&d.session, button, KeyState::Released)
                        .await
                        .map_err(error)?;
                }
            }
            ComputerAction::Type { text } => {
                if text.len() > 8192 || text.contains('\0') {
                    return Err(error("Text exceeds input limit"));
                }
                for c in text.chars() {
                    if token.load(Ordering::SeqCst) {
                        return Err(error("Task cancelled"));
                    }
                    let key = match c {
                        '\n' => 0xff0d,
                        '\t' => 0xff09,
                        c if (c as u32) <= 255 => c as i32,
                        c => (0x01000000 | c as u32) as i32,
                    };
                    d.key(key, KeyState::Pressed).await?;
                    d.key(key, KeyState::Released).await?;
                }
            }
            ComputerAction::KeyCombo { modifiers, key } => {
                let mods: Vec<i32> = modifiers.iter().map(|m| keysym(m)).collect::<Result<_>>()?;
                let key = keysym(key)?;
                let mut pressed = Vec::new();
                let result = async {
                    for m in &mods {
                        d.key(*m, KeyState::Pressed).await?;
                        pressed.push(*m);
                    }
                    d.key(key, KeyState::Pressed).await?;
                    d.key(key, KeyState::Released).await
                }
                .await;
                let mut release_error = None;
                for m in pressed.iter().rev() {
                    if let Err(e) = d.key(*m, KeyState::Released).await {
                        release_error = Some(e);
                    }
                }
                result?;
                if let Some(e) = release_error {
                    return Err(e);
                }
            }
            ComputerAction::Scroll { dx, dy } => {
                for (axis, steps) in [(Axis::Horizontal, *dx), (Axis::Vertical, *dy)] {
                    if steps != 0 {
                        d.remote
                            .notify_pointer_axis_discrete(&d.session, axis, steps.clamp(-100, 100))
                            .await
                            .map_err(error)?;
                    }
                }
            }
            ComputerAction::Drag { start, end } => {
                d.validate(end.0, end.1)?;
                d.pointer(start.0, start.1).await?;
                d.remote
                    .notify_pointer_button(&d.session, 0x110, KeyState::Pressed)
                    .await
                    .map_err(error)?;
                let result = async {
                    for i in 1..=20 {
                        if token.load(Ordering::SeqCst) {
                            return Err(error("Task cancelled"));
                        }
                        let t = f64::from(i) / 20.0;
                        d.pointer(
                            start.0 + (end.0 - start.0) * t,
                            start.1 + (end.1 - start.1) * t,
                        )
                        .await?;
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Ok(())
                }
                .await;
                let released = d
                    .remote
                    .notify_pointer_button(&d.session, 0x110, KeyState::Released)
                    .await
                    .map_err(error);
                result?;
                released?;
            }
            _ => {
                return Err(error(
                    "Action is not supported by the desktop portal; use an accessibility token or human handoff",
                ));
            }
        }
        Ok(())
    }
}
impl Desktop {
    fn validate(&self, x: f64, y: f64) -> Result<()> {
        if !x.is_finite()
            || !y.is_finite()
            || x < self.bounds.x
            || y < self.bounds.y
            || x >= self.bounds.x + self.bounds.width
            || y >= self.bounds.y + self.bounds.height
        {
            return Err(error("Pointer is outside the shared monitor"));
        }
        Ok(())
    }
    async fn pointer(&self, x: f64, y: f64) -> Result<()> {
        self.validate(x, y)?;
        self.remote
            .notify_pointer_motion_absolute(
                &self.session,
                self.node,
                x - self.bounds.x,
                y - self.bounds.y,
            )
            .await
            .map_err(error)
    }
    async fn key(&self, key: i32, state: KeyState) -> Result<()> {
        self.remote
            .notify_keyboard_keysym(&self.session, key, state)
            .await
            .map_err(error)
    }
}
fn keysym(name: &str) -> Result<i32> {
    Ok(match name.to_lowercase().as_str() {
        "ctrl" | "control" => 0xffe3,
        "shift" => 0xffe1,
        "alt" | "option" => 0xffe9,
        "super" | "win" | "meta" | "command" => 0xffeb,
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
        "space" => 32,
        _ if name.chars().count() == 1 => name.chars().next().unwrap() as i32,
        _ => return Err(error(format!("Unsupported key {name}"))),
    })
}

fn pipewire_frame(
    fd: std::os::fd::OwnedFd,
    node: u32,
    bounds: Rect,
    edge: Option<u32>,
    quality: Option<u8>,
    cancel: &AtomicBool,
    live: &AtomicBool,
) -> Result<FrameCapture> {
    use pipewire::{self as pw, properties::properties, spa};
    use std::{cell::RefCell, rc::Rc};
    let main = pw::main_loop::MainLoop::new(None).map_err(error)?;
    let context = pw::context::Context::new(&main).map_err(error)?;
    let core = context.connect_fd(fd, None).map_err(error)?;
    let stream = pw::stream::Stream::new(&core,"Grok Muse shared monitor",properties! { *pw::keys::MEDIA_TYPE=>"Video", *pw::keys::MEDIA_CATEGORY=>"Capture", *pw::keys::MEDIA_ROLE=>"Screen" }).map_err(error)?;
    type CapturedFrame = (u32, u32, Vec<u8>);
    struct State {
        format: spa::param::video::VideoInfoRaw,
        result: Rc<RefCell<Option<Result<CapturedFrame>>>>,
    }
    let result = Rc::new(RefCell::new(None));
    let _listener = stream
        .add_local_listener_with_user_data(State {
            format: Default::default(),
            result: result.clone(),
        })
        .state_changed(|_, s, _, new| {
            if let pw::stream::StreamState::Error(e) = new {
                *s.result.borrow_mut() = Some(Err(error(e)));
            }
        })
        .param_changed(|_, s, id, p| {
            if id == spa::param::ParamType::Format.as_raw()
                && let Some(p) = p
                && let Err(e) = s.format.parse(p)
            {
                *s.result.borrow_mut() = Some(Err(error(e)));
            }
        })
        .process(|stream, s| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            if data.chunk().size() == 0 {
                return;
            }
            let size = s.format.size();
            let offset = data.chunk().offset() as usize;
            let stride = data.chunk().stride();
            if size.width == 0
                || size.height == 0
                || u64::from(size.width) * u64::from(size.height) > 64_000_000
                || stride < (size.width as i32) * 4
            {
                *s.result.borrow_mut() =
                    Some(Err(error("Invalid PipeWire frame dimensions or stride")));
                return;
            }
            let chunk_size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else {
                *s.result.borrow_mut() =
                    Some(Err(error("Portal did not provide a mappable frame")));
                return;
            };
            let required = (size.height as usize - 1) * stride as usize + size.width as usize * 4;
            if required > chunk_size || offset.checked_add(required).is_none_or(|n| n > bytes.len())
            {
                *s.result.borrow_mut() = Some(Err(error("Truncated PipeWire frame")));
                return;
            }
            let mut rgba = Vec::with_capacity(size.width as usize * size.height as usize * 4);
            let bgr = s.format.format() == spa::param::video::VideoFormat::BGRx
                || s.format.format() == spa::param::video::VideoFormat::BGRA;
            for y in 0..size.height as usize {
                let row = &bytes[offset + y * stride as usize
                    ..offset + y * stride as usize + size.width as usize * 4];
                for p in row.as_chunks::<4>().0 {
                    rgba.extend(if bgr {
                        [p[2], p[1], p[0], 255]
                    } else {
                        [p[0], p[1], p[2], 255]
                    });
                }
            }
            *s.result.borrow_mut() = Some(Ok((size.width, size.height, rgba)));
        })
        .register()
        .map_err(error)?;
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA
        )
    );
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(error)?
    .0
    .into_inner();
    let pod = spa::pod::Pod::from_bytes(&bytes).ok_or_else(|| error("Invalid capture format"))?;
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )
        .map_err(error)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if cancel.load(Ordering::SeqCst) || !live.load(Ordering::SeqCst) {
            return Err(error("Screen sharing ended"));
        }
        if let Some(frame) = result.borrow_mut().take() {
            let (w, h, rgba) = frame?;
            let mut frame = crate::pixels::jpeg(w, h, rgba, edge, quality)?;
            frame.desktop_bounds = Some(bounds);
            frame.timestamp_ms = now();
            return Ok(frame);
        }
        if std::time::Instant::now() >= deadline {
            return Err(error("PipeWire capture timed out"));
        }
        main.loop_().iterate(std::time::Duration::from_millis(30));
    }
}

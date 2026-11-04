use super::*;
use atspi::{
    AccessibilityConnection, CoordType, State,
    proxy::{
        accessible::AccessibleProxy, action::ActionProxy, component::ComponentProxy,
        editable_text::EditableTextProxy, text::TextProxy,
    },
};
use std::collections::{HashMap, VecDeque};
use tokio::sync::OnceCell;
pub(super) struct Accessibility {
    connection: OnceCell<AccessibilityConnection>,
    targets: parking_lot::Mutex<HashMap<String, atspi::ObjectRef>>,
    focused: parking_lot::Mutex<Option<atspi::ObjectRef>>,
}
impl Accessibility {
    pub fn new() -> Self {
        Self {
            connection: OnceCell::new(),
            targets: Default::default(),
            focused: Default::default(),
        }
    }
    pub fn has_text_focus(&self) -> bool {
        self.focused.lock().is_some()
    }

    async fn bus(&self) -> Result<&AccessibilityConnection> {
        self.connection
            .get_or_try_init(|| async { AccessibilityConnection::new().await.map_err(error) })
            .await
    }
    pub async fn available(&self) -> bool {
        tokio::time::timeout(std::time::Duration::from_millis(300), self.bus())
            .await
            .is_ok_and(|r| r.is_ok())
    }
    pub fn clear(&self) {
        self.targets.lock().clear();
        *self.focused.lock() = None;
    }
    pub async fn snapshot(&self) -> Result<AxSnapshot> {
        self.clear();
        let bus = self.bus().await?.connection();
        let root = AccessibleProxy::builder(bus)
            .destination("org.a11y.atspi.Registry")
            .map_err(error)?
            .path("/org/a11y/atspi/accessible/root")
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        let mut active = None;
        for application in root.get_children().await.map_err(error)? {
            let app = AccessibleProxy::builder(bus)
                .destination(application.name.as_str())
                .map_err(error)?
                .path(application.path.as_str())
                .map_err(error)?
                .build()
                .await
                .map_err(error)?;
            for frame in app.get_children().await.unwrap_or_default() {
                let proxy = AccessibleProxy::builder(bus)
                    .destination(frame.name.as_str())
                    .map_err(error)?
                    .path(frame.path.as_str())
                    .map_err(error)?
                    .build()
                    .await
                    .map_err(error)?;
                if proxy
                    .get_state()
                    .await
                    .is_ok_and(|s| s.contains(State::Active))
                {
                    active = Some((app.name().await.unwrap_or_default(), frame));
                    break;
                }
            }
            if active.is_some() {
                break;
            }
        }
        let (app_name, frame) = active.ok_or_else(|| {
            GmError::Platform(
                "No active accessible window; enable accessibility for this application".into(),
            )
        })?;
        let window = AccessibleProxy::builder(bus)
            .destination(frame.name.as_str())
            .map_err(error)?
            .path(frame.path.as_str())
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        let window_title = window.name().await.map_err(error)?;
        let mut queue = VecDeque::from([frame.clone()]);
        let mut elements = Vec::new();
        let mut targets = HashMap::new();
        let mut focused = None;
        let mut bounds = Rect::new(0.0, 0.0, 0.0, 0.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(700);
        while let Some(object) = queue.pop_front() {
            if elements.len() >= 1000 || std::time::Instant::now() > deadline {
                break;
            }
            let proxy = AccessibleProxy::builder(bus)
                .destination(object.name.as_str())
                .map_err(error)?
                .path(object.path.as_str())
                .map_err(error)?
                .build()
                .await
                .map_err(error)?;
            let (name, role, state, children) = tokio::join!(
                proxy.name(),
                proxy.get_role_name(),
                proxy.get_state(),
                proxy.get_children()
            );
            let Ok(state) = state else {
                continue;
            };
            if state.contains(State::Defunct) {
                continue;
            }
            let role = role.unwrap_or_default();
            let secure = role.eq_ignore_ascii_case("password text");
            let component = ComponentProxy::builder(bus)
                .destination(object.name.as_str())
                .map_err(error)?
                .path(object.path.as_str())
                .map_err(error)?
                .build()
                .await
                .map_err(error)?;
            let frame_rect = component
                .get_extents(CoordType::Screen)
                .await
                .map(|(x, y, w, h)| Rect::new(x.into(), y.into(), w.into(), h.into()))
                .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0));
            if object == frame {
                bounds = frame_rect;
            }
            let token = format!(
                "atspi:{}",
                serde_json::to_string(&(object.name.as_str(), object.path.as_str()))?
            );
            if state.contains(State::Focused)
                && matches!(role.as_str(), "text" | "entry" | "password text")
            {
                focused = Some(object.clone());
            }
            let role = match role.as_str() {
                "push button" | "toggle button" => "AXButton",
                "text" | "entry" | "password text" => "AXTextField",
                "check box" => "AXCheckBox",
                "link" => "AXLink",
                "menu item" => "AXMenuItem",
                "combo box" => "AXComboBox",
                "list item" => "AXRow",
                _ => &role,
            }
            .to_string();
            targets.insert(token.clone(), object);
            elements.push(gm_core::types::AxElement {
                token,
                role,
                label: Some(name.unwrap_or_default()),
                value: None,
                frame: frame_rect,
                is_enabled: state.contains(State::Enabled),
                is_secure: secure,
            });
            for child in children.unwrap_or_default() {
                if queue.len() < 2000 {
                    queue.push_back(child);
                }
            }
        }
        *self.targets.lock() = targets;
        *self.focused.lock() = focused;
        Ok(AxSnapshot {
            window_id: 0,
            app_name,
            window_title,
            bounds,
            elements,
            timestamp_ms: now(),
        })
    }
    pub async fn click(&self, token: &str) -> Result<()> {
        let target = self.targets.lock().get(token).cloned().ok_or_else(|| {
            GmError::PermissionDenied(
                "Accessibility token is not in the last observed window".into(),
            )
        })?;
        let bus = self.bus().await?.connection();
        let proxy = AccessibleProxy::builder(bus)
            .destination(target.name.as_str())
            .map_err(error)?
            .path(target.path.as_str())
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        if !proxy
            .get_state()
            .await
            .map_err(error)?
            .contains(State::Enabled)
        {
            return Err(GmError::PermissionDenied(
                "Accessibility target is no longer enabled".into(),
            ));
        }
        let action = ActionProxy::builder(bus)
            .destination(target.name.as_str())
            .map_err(error)?
            .path(target.path.as_str())
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        if action.do_action(0).await.map_err(error)? {
            Ok(())
        } else {
            Err(error("The application rejected the accessibility action"))
        }
    }
    pub async fn type_text(&self, text: &str) -> Result<()> {
        if text.len() > 8192 {
            return Err(error("Text exceeds the 8 KiB input limit"));
        }
        let target = self
            .focused
            .lock()
            .clone()
            .ok_or_else(|| error("No focused accessible text field"))?;
        let bus = self.bus().await?.connection();
        let accessible = AccessibleProxy::builder(bus)
            .destination(target.name.as_str())
            .map_err(error)?
            .path(target.path.as_str())
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        if !accessible
            .get_state()
            .await
            .map_err(error)?
            .contains(State::Focused)
        {
            return Err(error("Text focus changed; inspect the window again"));
        }
        let proxy = EditableTextProxy::builder(bus)
            .destination(target.name.as_str())
            .map_err(error)?
            .path(target.path.as_str())
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        let text_proxy = TextProxy::builder(bus)
            .destination(target.name.as_str())
            .map_err(error)?
            .path(target.path.as_str())
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        let offset = text_proxy.caret_offset().await.map_err(error)?;
        if proxy
            .insert_text(offset, text, text.len() as i32)
            .await
            .map_err(error)?
        {
            Ok(())
        } else {
            Err(error("The application rejected text insertion"))
        }
    }
}

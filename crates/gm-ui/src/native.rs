//! Shared native desktop UI. All execution goes through the authenticated local daemon IPC.
use eframe::egui;
use gm_core::{
    config::GmConfig,
    error::{GmError, Result},
    ipc::*,
};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncRead, AsyncWrite};

trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}
async fn connect(path: &Path) -> Result<Box<dyn Connection>> {
    #[cfg(unix)]
    {
        Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
    }
    #[cfg(windows)]
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(path) {
                Ok(pipe) => return Ok(Box::new(pipe)),
                Err(e)
                    if e.raw_os_error() == Some(231) && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}
pub(crate) async fn is_running(config: &GmConfig) -> bool {
    connect(&config.daemon_socket_path).await.is_ok()
}

async fn rpc(config: &GmConfig, request: DaemonRequest) -> Result<DaemonResponse> {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let mut connection = connect(&config.daemon_socket_path).await?;
        write_ipc_message(
            &mut connection,
            &IpcMessage {
                id: 1,
                payload: request,
            },
        )
        .await?;
        let reply: IpcMessage<DaemonResponse> = read_ipc_message(&mut connection).await?;
        match reply.payload {
            DaemonResponse::Error(error) => Err(GmError::Ipc(error)),
            reply => Ok(reply),
        }
    })
    .await
    .map_err(|_| GmError::Timeout("The daemon did not respond".into()))?
}

#[derive(Default)]
struct ViewState {
    status: String,
    error: Option<String>,
    messages: Vec<(String, String)>,
    streaming: String,
    tasks: Vec<Value>,
    bots: Vec<Value>,
    approvals: Vec<(String, String, String)>,
    connected: bool,
    sending: bool,
    clear_draft: Option<String>,
    active_chat: Option<String>,
    completed_chat: Option<String>,
}
impl ViewState {
    fn finish_chat(&mut self, id: String, text: String) {
        if self.active_chat.as_ref() != Some(&id) || self.completed_chat.as_ref() == Some(&id) {
            return;
        }
        self.streaming.clear();
        self.messages.push(("Muse".into(), text));
        self.completed_chat = Some(id);
    }
    fn event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::HudStateChanged { state, detail, .. } => {
                self.status = format!("{state}: {detail}")
            }
            DaemonEvent::ChatTokenV1 {
                message_id,
                delta,
                done,
            } if self.active_chat.as_ref() == Some(&message_id)
                && self.completed_chat.as_ref() != Some(&message_id) =>
            {
                self.streaming.push_str(&delta);
                if done {
                    let text = std::mem::take(&mut self.streaming);
                    self.finish_chat(message_id, text);
                }
            }
            DaemonEvent::TaskFinished {
                task_id,
                ok,
                message,
            } => self.messages.push((
                if ok { "Completed" } else { "Task update" }.into(),
                format!("{task_id}\n{message}"),
            )),
            DaemonEvent::ApprovalResolved { request_id } => {
                self.approvals.retain(|(id, _, _)| id != &request_id)
            }
            DaemonEvent::ApprovalRequested {
                request_id,
                title,
                details,
            } => self.approvals.push((request_id, title, details)),
            DaemonEvent::WheelHandoffRequested {
                task_id,
                blocker,
                instructions,
                ..
            } => self.messages.push((
                "Human input needed".into(),
                format!("{task_id}\n{blocker}\n{instructions}"),
            )),
            _ => {}
        }
        // Bound retained UI history; full task results remain in the daemon's durable store.
        if self.messages.len() > 500 {
            self.messages.drain(..100);
        }
    }
}
#[derive(Clone)]
struct Shared {
    state: Arc<Mutex<ViewState>>,
    context: egui::Context,
}
impl Shared {
    fn update(&self, f: impl FnOnce(&mut ViewState)) {
        if let Ok(mut state) = self.state.lock() {
            f(&mut state);
        }
        self.context.request_repaint();
    }
}
struct NativeApp {
    shared: Shared,
    requests: tokio::sync::mpsc::Sender<(DaemonRequest, Option<String>)>,
    draft: String,
    bot: String,
    task_mode: bool,
    search: String,
    resume_note: String,
}
impl NativeApp {
    fn send(&self, request: DaemonRequest) {
        if let Err(error) = self.requests.try_send((request, None)) {
            self.shared
                .update(|s| s.error = Some(format!("Request queue is busy: {error}")));
        }
    }
}
impl eframe::App for NativeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Render from a snapshot so the UI never holds a lock during network work or callbacks.
        let (status, error, connected, messages, streaming, tasks, bots, approvals, sending) = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(sent) = state.clear_draft.take()
                && self.draft.trim() == sent
            {
                self.draft.clear();
            }
            (
                state.status.clone(),
                state.error.clone(),
                state.connected,
                state.messages.clone(),
                state.streaming.clone(),
                state.tasks.clone(),
                state.bots.clone(),
                state.approvals.clone(),
                state.sending,
            )
        };
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Grok Muse");
                ui.label(if connected {
                    "Connected"
                } else {
                    "Connecting…"
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Stop all").clicked() {
                        self.send(DaemonRequest::EmergencyKill);
                    }
                    if ui.button("Pause").clicked() {
                        self.send(DaemonRequest::SetPaused { paused: true });
                    }
                    if ui.button("Resume admission").clicked() {
                        self.send(DaemonRequest::SetPaused { paused: false });
                    }
                });
            });
            ui.label(status);
            if let Some(error) = error {
                ui.colored_label(egui::Color32::from_rgb(220, 70, 70), error);
            }
        });
        egui::SidePanel::right("tasks")
            .default_width(300.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading("Tasks");
                egui::ScrollArea::vertical()
                    .id_salt("task-scroll")
                    .show(ui, |ui| {
                        for task in tasks.iter().rev().take(100) {
                            let id = task["id"].as_str().unwrap_or("").to_string();
                            let state = task["status"].as_str().unwrap_or("unknown");
                            ui.push_id(&id, |ui| {
                                ui.group(|ui| {
                                    ui.label(format!(
                                        "{} · {state}",
                                        task["bot_id"].as_str().unwrap_or("bot")
                                    ));
                                    ui.label(task["instruction"].as_str().unwrap_or(""));
                                    if let Some(reason) = task["blocker_note"].as_str() {
                                        ui.label(reason);
                                    }
                                    ui.horizontal_wrapped(|ui| {
                                        for (label, action, enabled) in [
                                            (
                                                "Pause",
                                                QueueControlAction::Pause,
                                                matches!(state, "queued" | "running"),
                                            ),
                                            (
                                                "Cancel",
                                                QueueControlAction::Cancel,
                                                matches!(
                                                    state,
                                                    "queued"
                                                        | "running"
                                                        | "paused"
                                                        | "waiting_for_human"
                                                        | "pause_requested"
                                                ),
                                            ),
                                            (
                                                "Retry",
                                                QueueControlAction::Retry,
                                                matches!(state, "failed" | "cancelled")
                                                    && task["mutating_in_flight"] != true,
                                            ),
                                        ] {
                                            if ui
                                                .add_enabled(enabled, egui::Button::new(label))
                                                .clicked()
                                            {
                                                self.send(DaemonRequest::QueueControl {
                                                    task_id: id.clone(),
                                                    action,
                                                });
                                            }
                                        }
                                    });
                                    if matches!(state, "paused" | "waiting_for_human")
                                        || (task["mutating_in_flight"] == true
                                            && matches!(state, "failed" | "cancelled"))
                                    {
                                        ui.label("Review the task before handing control back.");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut self.resume_note)
                                                .hint_text("What changed?"),
                                        );
                                        if ui
                                            .add_enabled(
                                                !self.resume_note.trim().is_empty(),
                                                egui::Button::new("Hand back to bot"),
                                            )
                                            .clicked()
                                        {
                                            self.send(DaemonRequest::TakeTheWheelResume {
                                                task_id: id.clone(),
                                                note: Some(self.resume_note.clone()),
                                            });
                                        }
                                    }
                                });
                            });
                        }
                        if tasks.is_empty() {
                            ui.label("Tasks appear here when submitted.");
                        }
                    });
            });
        egui::TopBottomPanel::bottom("composer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.task_mode, "Run task");
                egui::ComboBox::from_label("Bot")
                    .selected_text(&self.bot)
                    .show_ui(ui, |ui| {
                        for bot in &bots {
                            if let Some(id) = bot["id"].as_str() {
                                ui.selectable_value(&mut self.bot, id.to_string(), id);
                            }
                        }
                    });
            });
            ui.add(
                egui::TextEdit::multiline(&mut self.draft)
                    .desired_width(ui.available_width())
                    .desired_rows(3)
                    .hint_text("Ask a question, or enable Run task to work in a bot workspace."),
            );
            let shortcut = ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
            if ui
                .add_enabled(
                    connected && !sending && !self.draft.trim().is_empty(),
                    egui::Button::new("Send  ⌘/Ctrl+Enter"),
                )
                .clicked()
                || (shortcut && connected && !sending && !self.draft.trim().is_empty())
            {
                let text = self.draft.trim().to_string();
                let request = if self.task_mode {
                    DaemonRequest::QueueTask {
                        bot_id: Some(self.bot.clone()),
                        channel_id: Some("general".into()),
                        instruction: text.clone(),
                        priority: 0,
                    }
                } else {
                    let message_id = uuid::Uuid::new_v4().to_string();
                    self.shared.update(|state| {
                        state.active_chat = Some(message_id.clone());
                        state.completed_chat = None;
                        state.streaming.clear();
                    });
                    DaemonRequest::SendChatMessageV1 {
                        message_id,
                        text: text.clone(),
                    }
                };
                if self
                    .requests
                    .try_send((request, Some(text.clone())))
                    .is_ok()
                {
                    self.shared.update(|s| {
                        s.sending = true;
                        s.error = None;
                        s.messages.push(("You".into(), text));
                    });
                } else {
                    self.shared.update(|s| {
                        s.error = Some("Request queue is busy; your draft is preserved".into())
                    });
                }
            }
        });
        egui::CentralPanel::default().show(ctx,|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search conversation"));
            let visible: Vec<_> = messages.iter().filter(|(_,text)| self.search.is_empty() || text.to_lowercase().contains(&self.search.to_lowercase())).collect();
            egui::ScrollArea::vertical().id_salt("conversation").stick_to_bottom(true).auto_shrink([false,false]).show(ui,|ui| {
                for (index,(role,text)) in visible.iter().enumerate() { ui.push_id(index,|ui| {ui.strong(role);ui.add(egui::Label::new(text.as_str()).selectable(true).wrap());ui.add_space(12.0);}); }
                if !streaming.is_empty() {ui.strong("Muse");ui.add(egui::Label::new(streaming).selectable(true).wrap());}
                if messages.is_empty() {ui.heading("Your local assistant workspace");ui.label("Start with a question or choose Run task. Desktop control requires explicit opt-in; unavailable capabilities are shown as errors.");}
            });
        });
        for (id, title, details) in approvals {
            egui::Window::new(title)
                .id(egui::Id::new(&id))
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.add(egui::Label::new(details).selectable(true).wrap());
                    ui.horizontal(|ui| {
                        for (label, approved) in [("Approve this action", true), ("Deny", false)] {
                            if ui.button(label).clicked() {
                                self.send(DaemonRequest::RespondApproval {
                                    request_id: id.clone(),
                                    approved,
                                });
                                self.shared
                                    .update(|s| s.approvals.retain(|(key, _, _)| key != &id));
                            }
                        }
                    });
                });
        }
    }
}

async fn background(
    config: GmConfig,
    shared: Shared,
    mut requests: tokio::sync::mpsc::Receiver<(DaemonRequest, Option<String>)>,
) {
    let events_config = config.clone();
    let events_shared = shared.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(mut connection) = connect(&events_config.daemon_socket_path).await {
                let result: Result<()> = async {
                    write_ipc_message(
                        &mut connection,
                        &IpcMessage {
                            id: 1,
                            payload: DaemonRequest::SubscribeEvents,
                        },
                    )
                    .await?;
                    let _: IpcMessage<DaemonResponse> = read_ipc_message(&mut connection).await?;
                    events_shared.update(|s| s.connected = true);
                    loop {
                        let response: IpcMessage<DaemonResponse> =
                            read_ipc_message(&mut connection).await?;
                        if let DaemonResponse::Event(event) = response.payload {
                            events_shared.update(|s| s.event(event));
                        }
                    }
                }
                .await;
                if let Err(error) = result {
                    events_shared.update(|s| {
                        s.connected = false;
                        s.error = Some(error.to_string());
                    });
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
    let refresh_shared = shared.clone();
    let refresh_config = config.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Ok(DaemonResponse::JsonPayload(value)) =
                rpc(&refresh_config, DaemonRequest::ListQueue { bot_id: None }).await
            {
                refresh_shared.update(|s| s.tasks = value.as_array().cloned().unwrap_or_default());
            }
            if let Ok(DaemonResponse::JsonPayload(value)) =
                rpc(&refresh_config, DaemonRequest::ListBots).await
            {
                refresh_shared.update(|s| s.bots = value.as_array().cloned().unwrap_or_default());
            }
        }
    });
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    while let Some((request, draft)) = requests.recv().await {
        let Ok(permit) = slots.clone().acquire_owned().await else {
            break;
        };
        let request_shared = shared.clone();
        let request_config = config.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match rpc(&request_config, request).await {
                Ok(response) => request_shared.update(|s| {
                    if draft.is_some() {
                        s.sending = false;
                        s.clear_draft = draft.clone();
                    }
                    s.error = None;
                    match response {
                        DaemonResponse::Accepted { id } => s.messages.push(("Queued".into(), id)),
                        DaemonResponse::ChatReplyV1 { message_id, text }
                            if s.completed_chat.as_ref() != Some(&message_id) =>
                        {
                            s.finish_chat(message_id, text);
                        }
                        _ => {}
                    }
                }),
                Err(error) => request_shared.update(|s| {
                    if draft.is_some() {
                        s.sending = false;
                        if !s.streaming.is_empty() {
                            let partial = std::mem::take(&mut s.streaming);
                            s.messages.push(("Interrupted response".into(), partial));
                        }
                    }
                    s.error = Some(error.to_string());
                }),
            }
        });
    }
}

pub fn run(config: GmConfig) -> Result<()> {
    let runtime = tokio::runtime::Handle::current();
    eframe::run_native(
        "Grok Muse",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Wgpu,
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1100.0, 760.0])
                .with_min_inner_size([680.0, 480.0]),
            ..Default::default()
        },
        Box::new(move |creation| {
            let mut style = (*creation.egui_ctx.style()).clone();
            style
                .text_styles
                .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
            style
                .text_styles
                .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
            style
                .text_styles
                .insert(egui::TextStyle::Heading, egui::FontId::proportional(24.0));
            style
                .text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(13.0));
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.button_padding = egui::vec2(12.0, 7.0);
            if style.visuals.dark_mode {
                style.visuals.panel_fill = egui::Color32::from_rgb(20, 26, 36);
                style.visuals.extreme_bg_color = egui::Color32::from_rgb(13, 18, 26);
                style.visuals.selection.bg_fill = egui::Color32::from_rgb(37, 95, 130);
                style.visuals.hyperlink_color = egui::Color32::from_rgb(108, 208, 220);
            }
            creation.egui_ctx.set_style(style);
            let shared = Shared {
                state: Arc::new(Mutex::new(ViewState::default())),
                context: creation.egui_ctx.clone(),
            };
            let (sender, receiver) = tokio::sync::mpsc::channel(64);
            runtime.spawn(background(config, shared.clone(), receiver));
            Ok(Box::new(NativeApp {
                shared,
                requests: sender,
                draft: String::new(),
                bot: "muse".into(),
                task_mode: false,
                search: String::new(),
                resume_note: String::new(),
            }))
        }),
    )
    .map_err(|e| GmError::Platform(format!("Native UI could not start: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chat_reply_races_and_other_clients_never_duplicate_or_mix_answers() {
        for response_first in [true, false] {
            let mut state = ViewState {
                active_chat: Some("mine".into()),
                ..Default::default()
            };
            state.event(DaemonEvent::ChatTokenV1 {
                message_id: "another-client".into(),
                delta: "unrelated".into(),
                done: true,
            });
            assert!(state.messages.is_empty());
            if response_first {
                state.finish_chat("mine".into(), "one answer".into());
            }
            state.event(DaemonEvent::ChatTokenV1 {
                message_id: "mine".into(),
                delta: "one answer".into(),
                done: false,
            });
            state.event(DaemonEvent::ChatTokenV1 {
                message_id: "mine".into(),
                delta: String::new(),
                done: true,
            });
            if !response_first {
                state.finish_chat("mine".into(), "one answer".into());
            }
            assert_eq!(state.messages, vec![("Muse".into(), "one answer".into())]);
            assert!(state.streaming.is_empty());
        }
    }
    #[test]
    fn expired_or_cancelled_approvals_disappear() {
        let mut state = ViewState::default();
        state.event(DaemonEvent::ApprovalRequested {
            request_id: "one".into(),
            title: "Review".into(),
            details: "one action".into(),
        });
        state.event(DaemonEvent::ApprovalResolved {
            request_id: "one".into(),
        });
        assert!(state.approvals.is_empty());
    }
}

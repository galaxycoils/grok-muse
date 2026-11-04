//! Shared native desktop UI. All execution goes through the authenticated local daemon IPC.
use eframe::egui;
mod session;
use gm_core::{
    config::GmConfig,
    error::{GmError, Result},
    ipc::*,
};
use serde_json::Value;
use session::{SessionData, SessionStore};
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
        if reply.id != 1 {
            return Err(GmError::Ipc("Mismatched daemon reply".into()));
        }
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
    credentials_notice: String,
    credentials_saving: bool,
    error: Option<String>,
    messages: Vec<(String, String)>,
    streaming: String,
    tasks: Vec<Value>,
    bots: Vec<Value>,
    channels: Vec<Value>,
    routines: Vec<Value>,
    files: Value,
    file_preview: Value,
    attachment_paths: Vec<(String, String)>,
    channel_messages: Vec<Value>,
    channel_history_id: String,
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
    session: Option<Arc<SessionStore>>,
}
impl Shared {
    fn update(&self, f: impl FnOnce(&mut ViewState)) {
        if let Ok(mut state) = self.state.lock() {
            let before = (
                state.messages.len(),
                state
                    .messages
                    .last()
                    .map(|(role, text)| (role.len(), text.len())),
            );
            f(&mut state);
            if before
                != (
                    state.messages.len(),
                    state
                        .messages
                        .last()
                        .map(|(role, text)| (role.len(), text.len())),
                )
                && let Some(store) = &self.session
            {
                store.edit(|d| d.messages = state.messages.clone());
            }
        }
        self.context.request_repaint();
    }
}
struct NativeApp {
    runtime: tokio::runtime::Handle,
    shared: Shared,
    requests: tokio::sync::mpsc::Sender<(DaemonRequest, Option<String>)>,
    draft: String,
    bot: String,
    task_mode: bool,
    search: String,
    resume_note: String,
    settings: bool,
    routines: bool,
    files: bool,
    routine_editor: Option<Value>,
    bot_editor: Option<Value>,
    channel: String,
    new_channel: String,
    channel_mode: bool,
    settings_config: GmConfig,
    config_path: std::path::PathBuf,
    config_original: Option<Vec<u8>>,
    settings_notice: String,
    meta_key: String,
    xai_key: String,
}
impl NativeApp {
    fn pick_attachments(&self) {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Import files into bot inbox")
            .pick_files();
        let requests = self.requests.clone();
        let bot = self.bot.clone();
        let shared = self.shared.clone();
        self.runtime.spawn(async move {
            if let Some(files) = dialog.await {
                if files.len() > 64 {
                    shared.update(|s| s.error = Some("Select at most 64 files".into()));
                    return;
                }
                for file in files {
                    if requests
                        .send((
                            DaemonRequest::ImportAttachment {
                                bot_id: bot.clone(),
                                source_path: file.path().into(),
                            },
                            None,
                        ))
                        .await
                        .is_err()
                    {
                        shared.update(|s| s.error = Some("Request queue closed".into()));
                        break;
                    }
                }
            }
        });
    }
    fn save_artifact(&self, path: &str) {
        let name = Path::new(path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Save reviewed artifact")
            .set_file_name(name)
            .save_file();
        let config = self.settings_config.clone();
        let bot = self.bot.clone();
        let path = path.to_string();
        let shared = self.shared.clone();
        self.runtime.spawn(async move {
            let Some(file) = dialog.await else {
                return;
            };
            let result = async {
                use base64::Engine;
                let response = rpc(
                    &config,
                    DaemonRequest::ReadBotArtifact {
                        bot_id: bot.clone(),
                        path: path.clone(),
                    },
                )
                .await?;
                let DaemonResponse::JsonPayload(value) = response else {
                    return Err(GmError::Ipc("Unexpected artifact response".into()));
                };
                if value["bot_id"].as_str() != Some(bot.as_str())
                    || value["path"].as_str() != Some(path.as_str())
                {
                    return Err(GmError::Ipc("Mismatched artifact response".into()));
                }
                let encoded = value["base64"]
                    .as_str()
                    .ok_or_else(|| GmError::Ipc("Missing artifact bytes".into()))?;
                if encoded.len() > 12 * 1024 * 1024 {
                    return Err(GmError::Ipc("Artifact exceeds transfer limit".into()));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| GmError::Ipc("Invalid artifact encoding".into()))?;
                if bytes.len() > 8 * 1024 * 1024 {
                    return Err(GmError::Ipc("Artifact exceeds transfer limit".into()));
                }
                file.write(&bytes).await?;
                Ok::<(), GmError>(())
            }
            .await;
            shared.update(|s| match result {
                Ok(()) => s.messages.push(("Saved".into(), path)),
                Err(e) => s.error = Some(e.to_string()),
            });
        });
    }
    fn send(&self, request: DaemonRequest) {
        if let Err(error) = self.requests.try_send((request, None)) {
            self.shared
                .update(|s| s.error = Some(format!("Request queue is busy: {error}")));
        }
    }
}
impl Drop for NativeApp {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.meta_key.zeroize();
        self.xai_key.zeroize();
    }
}
impl eframe::App for NativeApp {
    fn on_exit(&mut self) {
        if let Some(store) = &self.shared.session {
            store.edit(|d| {
                d.draft = self.draft.clone();
                d.bot = self.bot.clone();
                d.task_mode = self.task_mode;
            });
            if let Err(error) = store.flush() {
                tracing::error!(%error,"Could not save native conversation");
            }
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let shortcut = take_composer_shortcut(ctx);
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            if let Some(path) = file.path {
                self.send(DaemonRequest::ImportAttachment {
                    bot_id: self.bot.clone(),
                    source_path: path,
                });
            }
        }
        let attachments = std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attachment_paths,
        );
        for (bot, path) in attachments {
            if bot == self.bot {
                if !self.draft.is_empty() {
                    self.draft.push('\n');
                }
                self.draft.push_str(&format!("Attached file: {path}"));
            } else {
                self.shared.update(|s| {
                    s.messages
                        .push(("File imported".into(), format!("@{bot}: {path}")))
                });
            }
        }
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
                if ui.button("Files").clicked() {
                    self.files = !self.files;
                    if self.files {
                        self.send(DaemonRequest::ListBotFiles {
                            bot_id: self.bot.clone(),
                        });
                    }
                }

                if ui.button("Routines").clicked() {
                    self.routines = !self.routines;
                }

                if ui.button("Settings").clicked() {
                    self.settings = !self.settings;
                }

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
                                    if let Some(result) = task["result_summary"].as_str() {
                                        ui.collapsing("Result", |ui| {
                                            ui.add(
                                                egui::Label::new(result).selectable(true).wrap(),
                                            );
                                        });
                                    }
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
                ui.checkbox(&mut self.channel_mode, "Channel message");
                if ui.button("Duplicate bot").clicked() { self.send(DaemonRequest::DuplicateBot { bot_id:self.bot.clone() }); }
                if ui.button("Edit bot").clicked() {
                    self.bot_editor = bots
                        .iter()
                        .find(|b| b["id"].as_str() == Some(self.bot.as_str()))
                        .cloned();
                }

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
            let channels = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .channels
                .clone();
            ui.horizontal(|ui| {
                let previous = self.channel.clone();
                egui::ComboBox::from_label("Channel")
                    .selected_text(format!("#{}", self.channel))
                    .show_ui(ui, |ui| {
                        for channel in channels {
                            if let Some(id) = channel["id"].as_str() {
                                ui.selectable_value(
                                    &mut self.channel,
                                    id.to_string(),
                                    format!("#{id}"),
                                );
                            }
                        }
                    });
                if previous != self.channel {
                    self.send(DaemonRequest::ChannelHistory {
                        channel_id: self.channel.clone(),
                    });
                }
                if ui.button("Refresh channel").clicked() {
                    self.send(DaemonRequest::ChannelHistory {
                        channel_id: self.channel.clone(),
                    });
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_channel)
                        .desired_width(120.0)
                        .hint_text("new-channel"),
                );
                if ui
                    .add_enabled(
                        !self.new_channel.trim().is_empty(),
                        egui::Button::new("Create channel"),
                    )
                    .clicked()
                {
                    self.send(DaemonRequest::CreateChannel {
                        channel_id: self.new_channel.trim().into(),
                        topic: String::new(),
                        owner_bot_id: Some(self.bot.clone()),
                        participants: vec![self.bot.clone()],
                    });
                    self.new_channel.clear();
                }
            });
            ui.add(
                egui::TextEdit::multiline(&mut self.draft).id(egui::Id::new("main-draft"))
                    .desired_width(ui.available_width())
                    .desired_rows(3)
                    .hint_text("Ask a question, or enable Run task. Drop a file here to attach it to the selected bot."),
            );
            if ui
                .add_enabled(
                    connected && !sending && !self.draft.trim().is_empty(),
                    egui::Button::new(if cfg!(target_os="macos") {"Send  ⌘Enter"} else {"Send  Ctrl+Enter"}),
                )
                .clicked()
                || (shortcut && connected && !sending && !self.draft.trim().is_empty())
            {
                let text = self.draft.trim().to_string();
                let request = if self.channel_mode {
                    DaemonRequest::ChannelPost {
                        channel_id: self.channel.clone(),
                        author: "human".into(),
                        content: text.clone(),
                    }
                } else if self.task_mode {
                    DaemonRequest::QueueTask {
                        bot_id: Some(self.bot.clone()),
                        channel_id: Some(self.channel.clone()),
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
                    DaemonRequest::SendChatMessageV2 {
                        message_id,
                        text: text.clone(),history:conversation_context(&messages),
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
            if self.channel_mode {
                let state=self.shared.state.lock().unwrap_or_else(|e|e.into_inner());
                ui.heading(format!("#{}",self.channel));
                ui.label("Messages can enqueue work for @mentions or the channel owner.");
                if state.channel_history_id==self.channel {
                    egui::ScrollArea::vertical().id_salt("channel-history").stick_to_bottom(true).show(ui,|ui| {
                        for message in state.channel_messages.iter().rev() {ui.strong(message["author"].as_str().unwrap_or("bot"));ui.add(egui::Label::new(message["content"].as_str().unwrap_or("")).selectable(true).wrap());ui.add_space(8.0);}
                    });
                } else {ui.label("Use Refresh channel to load messages.");}
                return;
            }
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search conversation"));
            let visible: Vec<_> = messages.iter().filter(|(_,text)| self.search.is_empty() || text.to_lowercase().contains(&self.search.to_lowercase())).collect();
            egui::ScrollArea::vertical().id_salt("conversation").stick_to_bottom(true).auto_shrink([false,false]).show(ui,|ui| {
                for (index,(role,text)) in visible.iter().enumerate() { ui.push_id(index,|ui| {ui.strong(role);ui.add(egui::Label::new(text.as_str()).selectable(true).wrap());ui.add_space(12.0);}); }
                if !streaming.is_empty() {ui.strong("Muse");ui.add(egui::Label::new(streaming).selectable(true).wrap());}
                if messages.is_empty() {ui.heading("Your local assistant workspace");ui.label("Start with a question or choose Run task. Desktop control requires explicit opt-in; unavailable capabilities are shown as errors.");}
            });
        });
        if let Some(store) = &self.shared.session {
            let changed = {
                let d = store.data.lock().unwrap_or_else(|e| e.into_inner());
                d.draft != self.draft || d.bot != self.bot || d.task_mode != self.task_mode
            };
            if changed {
                store.edit(|d| {
                    d.draft = self.draft.clone();
                    d.bot = self.bot.clone();
                    d.task_mode = self.task_mode;
                });
            }
        }
        if self.files {
            let mut open = true;
            egui::Window::new("Attachments and artifacts").vscroll(true).max_height(600.0).open(&mut open).default_width(650.0).show(ctx,|ui| {
                ui.label(format!("Files for @{}. Drop files onto this window to import them into this bot's inbox (8 MiB maximum).",self.bot));
                if ui.button("Import files…").clicked() {self.pick_attachments();}
                if ui.button("Refresh files").clicked() {self.send(DaemonRequest::ListBotFiles {bot_id:self.bot.clone()});}
                let (files,preview)={let s=self.shared.state.lock().unwrap_or_else(|e|e.into_inner());(s.files.clone(),s.file_preview.clone())};
                if files["bot_id"].as_str()==Some(self.bot.as_str()) && let Some(files)=files["files"].as_array() {
                    egui::ScrollArea::vertical().id_salt("bot-files").max_height(220.0).show(ui,|ui| {
                        for file in files {let path=file["path"].as_str().unwrap_or("");ui.horizontal(|ui| {let width=(ui.available_width()-190.0).max(120.0);ui.add_sized([width,24.0],egui::Label::new(format!("{path} · {} bytes",file["size"])).truncate()).on_hover_text(path);
                            if ui.button("Save…").clicked() {self.save_artifact(path);}
                            if ui.button("Preview").clicked() {self.send(DaemonRequest::PreviewBotFile {bot_id:self.bot.clone(),path:path.into()});}
                            if ui.button("Use in task").clicked() {if !self.draft.is_empty() {self.draft.push('\n');}self.draft.push_str(&format!("Attached file: {path}"));}
                        });}
                    });
                }
                if preview["bot_id"].as_str()==Some(self.bot.as_str()) && let Some(text)=preview["text"].as_str() {
                    ui.separator();ui.label(preview["path"].as_str().unwrap_or("Preview"));
                    if ui.button("Copy text").clicked() {ctx.copy_text(text.into());}
                    egui::ScrollArea::both().id_salt("artifact-preview").max_height(350.0).show(ui,|ui|{ui.add(egui::Label::new(egui::RichText::new(text).monospace()).selectable(true));});
                }
            });
            self.files = open;
        }
        if self.routines {
            let mut open = true;
            egui::Window::new("Routines").vscroll(true).max_height(600.0).open(&mut open).default_width(560.0).show(ctx,|ui| {
                ui.label("The daemon must be running. Missed runs coalesce into one catch-up task; overlapping runs are skipped.");
                if ui.button("New daily routine").clicked() {
                    self.routine_editor=Some(serde_json::json!({"id":uuid::Uuid::new_v4().to_string(),"name":"Daily routine","bot_id":self.bot,"channel_id":self.channel,"instruction":"","timezone":"UTC","hour":9,"minute":0,"weekdays":[],"enabled":false}));
                }
                let routines=self.shared.state.lock().unwrap_or_else(|e|e.into_inner()).routines.clone();
                for state in routines {
                    let r=&state["routine"];
                    ui.group(|ui| {
                        ui.strong(r["name"].as_str().unwrap_or("Routine"));
                        ui.label(format!("{} · {} · next {}",r["bot_id"].as_str().unwrap_or("bot"),r["timezone"].as_str().unwrap_or("UTC"),state["next_run_utc"].as_str().unwrap_or("unknown")));
                        ui.label(if r["enabled"]==true {"Enabled"} else {"Disabled"});
                        ui.horizontal(|ui| {
                            if ui.button("Edit").clicked() {self.routine_editor=Some(r.clone());}
                            if ui.button(if r["enabled"]==true {"Disable"} else {"Enable"}).clicked() {let mut routine=r.clone();routine["enabled"]=Value::Bool(r["enabled"]!=true);self.send(DaemonRequest::SaveRoutine {routine});}
                        });
                    });
                }
            });
            self.routines = open;
        }
        let mut save_routine = None;
        if let Some(routine) = &mut self.routine_editor {
            let mut open = true;
            egui::Window::new("Routine schedule").vscroll(true).max_height(600.0).open(&mut open).default_width(540.0).show(ctx,|ui| {
                for (key,label,multiline) in [("name","Name",false),("bot_id","Bot",false),("channel_id","Channel",false),("instruction","Task instructions",true),("timezone","IANA timezone (for example America/New_York)",false)] {
                    let label_id=ui.label(label).id;let mut text=routine[key].as_str().unwrap_or("").to_string();
                    let changed=if multiline {ui.add(egui::TextEdit::multiline(&mut text).desired_rows(5).desired_width(ui.available_width())).labelled_by(label_id).changed()} else {ui.text_edit_singleline(&mut text).labelled_by(label_id).changed()};
                    if changed {routine[key]=if key=="channel_id" && text.trim().is_empty() {Value::Null} else {Value::String(text)};}
                }
                ui.horizontal(|ui| {
                    for (key,label,maximum) in [("hour","Hour",23),("minute","Minute",59)] {let mut value=routine[key].as_u64().unwrap_or(0) as u32;ui.label(label);if ui.add(egui::DragValue::new(&mut value).range(0..=maximum)).changed() {routine[key]=serde_json::json!(value);}}
                });
                let mut days=routine["weekdays"].as_array().cloned().unwrap_or_default();
                ui.horizontal_wrapped(|ui| {for (index,label) in ["Mon","Tue","Wed","Thu","Fri","Sat","Sun"].iter().enumerate() {let value=serde_json::json!(index);let mut enabled=days.contains(&value);if ui.checkbox(&mut enabled,*label).changed() {days.retain(|v|v!=&value);if enabled {days.push(value);}}}});
                routine["weekdays"]=Value::Array(days);
                ui.label("No weekdays selected means every day. Clock changes: skipped times move forward to the next valid minute; repeated times run once on the earlier offset.");
                let mut enabled=routine["enabled"].as_bool().unwrap_or(false);ui.checkbox(&mut enabled,"Enable automatic task submission");routine["enabled"]=Value::Bool(enabled);
                if ui.button("Save routine").clicked() {save_routine=Some(routine.clone());}
            });
            if !open {
                self.routine_editor = None;
            }
        }
        if let Some(routine) = save_routine {
            self.send(DaemonRequest::SaveRoutine { routine });
            self.routine_editor = None;
        }
        let mut save_bot = None;
        if let Some(profile) = &mut self.bot_editor {
            let mut open = true;
            egui::Window::new("Edit bot").open(&mut open).default_width(520.0).show(ctx,|ui| {
                for (key,label,multiline) in [("display_name","Name",false),("role_title","Role",false),("system_prompt","Instructions",true),("preferred_model","Preferred model (optional)",false)] {
                    let label_id=ui.label(label).id;let mut text=profile[key].as_str().unwrap_or("").to_string();
                    let changed=if multiline {ui.add(egui::TextEdit::multiline(&mut text).desired_rows(6).desired_width(ui.available_width())).labelled_by(label_id).changed()} else {ui.text_edit_singleline(&mut text).labelled_by(label_id).changed()};
                    if changed {profile[key]=if key=="preferred_model" && text.is_empty() {Value::Null} else {Value::String(text)};}
                }
                let mut provider=profile["preferred_provider"].as_str().unwrap_or("auto").to_string();
                egui::ComboBox::from_label("Provider").selected_text(&provider).show_ui(ui,|ui| {for name in ["auto","meta","xai"] {ui.selectable_value(&mut provider,name.into(),name);}});
                profile["preferred_provider"]=if provider=="auto" {Value::Null} else {Value::String(provider)};
                for (key,label) in [("allow_gui","Desktop access"),("allow_shell","Workspace VM commands"),("allow_network","Network tools")] {
                    let mut allowed=profile[key].as_bool().unwrap_or(false);if ui.checkbox(&mut allowed,label).changed() {profile[key]=Value::Bool(allowed);}
                }
                ui.label("Applies to newly admitted tasks. Global desktop and screenshot settings also apply.");
                if ui.button("Save bot").clicked() {save_bot=Some(profile.clone());}
            });
            if !open {
                self.bot_editor = None;
            }
        }
        if let Some(profile) = save_bot {
            self.send(DaemonRequest::SaveBot { profile });
            self.bot_editor = None;
        }
        if self.settings {
            let mut open = true;
            egui::Window::new("Settings and setup").open(&mut open).default_width(550.0).show(ctx,|ui| {
                egui::ScrollArea::vertical().show(ui,|ui| {
                    ui.label("Configure a model provider, review spending limits, then choose the access your tasks need.");
                    ui.label("Save API credentials in your operating system credential store. Keys are never written to configuration or conversation history.");
                    let label_id=ui.label("Meta API key").id;ui.add(egui::TextEdit::singleline(&mut self.meta_key).password(true)).labelled_by(label_id);
                    let label_id=ui.label("xAI API key").id;ui.add(egui::TextEdit::singleline(&mut self.xai_key).password(true)).labelled_by(label_id);
                    let (saving,notice)={let s=self.shared.state.lock().unwrap_or_else(|e|e.into_inner());(s.credentials_saving,s.credentials_notice.clone())};
                    if ui.add_enabled(!saving && (!self.meta_key.is_empty() || !self.xai_key.is_empty()),egui::Button::new("Save API keys")).clicked() {
                        let meta=zeroize::Zeroizing::new(std::mem::take(&mut self.meta_key));
                        let xai=zeroize::Zeroizing::new(std::mem::take(&mut self.xai_key));
                        let shared=self.shared.clone();shared.update(|s|{s.credentials_saving=true;s.credentials_notice="Saving to the OS credential store…".into();});
                        tokio::task::spawn_blocking(move || {
                            let keys=gm_llm::KeychainSecrets::default();
                            let result=(|| {if !meta.is_empty() {keys.set_meta_key(&meta)?;}
                            if !xai.is_empty() {keys.set_xai_key(&xai)?;}Ok::<_,GmError>(())})();
                            shared.update(|s|{s.credentials_saving=false;s.credentials_notice=match result {Ok(())=>"API keys saved. Restart the daemon to use changed credentials.".into(),Err(e)=>e.to_string()};});
                        });
                    }
                    ui.label(notice);
                    ui.separator();
                    ui.strong("Models");
                    let label_id=ui.label("Meta API base").id;ui.text_edit_singleline(&mut self.settings_config.providers.meta_api_base).labelled_by(label_id);
                    let label_id=ui.label("Computer-use model").id;ui.text_edit_singleline(&mut self.settings_config.providers.meta_cu_model).labelled_by(label_id);
                    let label_id=ui.label("Reasoning model").id;ui.text_edit_singleline(&mut self.settings_config.providers.meta_reasoning_model).labelled_by(label_id);
                    let label_id=ui.label("xAI API base").id;ui.text_edit_singleline(&mut self.settings_config.providers.xai_api_base).labelled_by(label_id);
                    let label_id=ui.label("xAI model").id;ui.text_edit_singleline(&mut self.settings_config.providers.xai_model).labelled_by(label_id);
                    ui.separator();ui.strong("Spending limits (USD)");
                    ui.horizontal(|ui|{ui.label("Daily");ui.add(egui::DragValue::new(&mut self.settings_config.budget.daily_spend_cap_usd).range(0.0..=10000.0).speed(0.1));});
                    ui.horizontal(|ui|{ui.label("Per task");ui.add(egui::DragValue::new(&mut self.settings_config.budget.per_job_spend_cap_usd).range(0.0..=1000.0).speed(0.05));});
                    ui.separator();ui.strong("Desktop access");
                    ui.checkbox(&mut self.settings_config.safety.allow_host_desktop,"Allow agents to observe and control this desktop");
                    ui.checkbox(&mut self.settings_config.providers.allow_screenshot_egress_meta,"Allow screenshots to Meta");
                    ui.checkbox(&mut self.settings_config.providers.allow_screenshot_egress_xai,"Allow screenshots to xAI");
                    ui.label("Linux Wayland asks for monitor, pointer and keyboard consent when a desktop task first needs capture or input. Stop sharing in your desktop controls to revoke access.");
                    ui.separator();
                    egui::widgets::global_theme_preference_buttons(ui);
                    if ui.button("Save settings").clicked() {
                        let current=std::fs::read(&self.config_path).ok();
                        let result=if current!=self.config_original {
                            Err(GmError::Config("Configuration changed outside this window. Reopen the app before saving.".into()))
                        } else { self.settings_config.save(&self.config_path) };
                        match result {
                            Ok(())=>{self.config_original=std::fs::read(&self.config_path).ok();self.settings_notice="Saved. Restart Grok Muse and its daemon to apply changes to new tasks.".into();},
                            Err(e)=>self.settings_notice=e.to_string(),
                        }
                    }
                    ui.label(&self.settings_notice);
                    ui.separator();ui.strong("Conversation history");
                    ui.label("Saved locally with your draft. Retains up to 500 messages within a bounded private file.");
                    if ui.add_enabled(!sending,egui::Button::new("Clear conversation history")).clicked() {self.shared.update(|s|s.messages.clear());}

                });
            });
            self.settings = open;
        }
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
                rpc(&refresh_config, DaemonRequest::ListRoutines).await
            {
                refresh_shared
                    .update(|s| s.routines = value.as_array().cloned().unwrap_or_default());
            }
            if let Ok(DaemonResponse::JsonPayload(value)) =
                rpc(&refresh_config, DaemonRequest::ListChannels).await
            {
                refresh_shared
                    .update(|s| s.channels = value.as_array().cloned().unwrap_or_default());
            }
            let channel = refresh_shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .channel_history_id
                .clone();
            if !channel.is_empty()
                && let Ok(DaemonResponse::JsonPayload(value)) = rpc(
                    &refresh_config,
                    DaemonRequest::ChannelHistory {
                        channel_id: channel.clone(),
                    },
                )
                .await
            {
                refresh_shared.update(|s| {
                    if s.channel_history_id == channel {
                        s.channel_messages = value.as_array().cloned().unwrap_or_default();
                    }
                });
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
            let history_id = match &request {
                DaemonRequest::ChannelHistory { channel_id } => Some(channel_id.clone()),
                _ => None,
            };
            match rpc(&request_config, request).await {
                Ok(response) => request_shared.update(|s| {
                    if draft.is_some() {
                        s.sending = false;
                        s.clear_draft = draft.clone();
                    }
                    s.error = None;
                    match response {
                        DaemonResponse::JsonPayload(value)
                            if value.get("attachment_path").is_some() =>
                        {
                            if let (Some(bot), Some(path)) =
                                (value["bot_id"].as_str(), value["attachment_path"].as_str())
                            {
                                s.attachment_paths.push((bot.into(), path.into()));
                            }
                        }
                        DaemonResponse::JsonPayload(value) if value.get("files").is_some() => {
                            s.files = value;
                        }
                        DaemonResponse::JsonPayload(value)
                            if value.get("path").is_some() && value.get("text").is_some() =>
                        {
                            let mut value = value;
                            if let Some(text) = value["text"].as_str()
                                && text.len() > 64 * 1024
                            {
                                let end = text.floor_char_boundary(64 * 1024);
                                value["text"] = Value::String(format!(
                                    "{}\n[Preview limited to 64 KiB]",
                                    &text[..end]
                                ));
                            }
                            s.file_preview = value;
                        }
                        DaemonResponse::JsonPayload(value) if history_id.is_some() => {
                            s.channel_history_id = history_id.unwrap_or_default();
                            s.channel_messages = value.as_array().cloned().unwrap_or_default();
                        }
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
                    if let Some(draft) = &draft
                        && s.completed_chat != s.active_chat
                        && let Some((role, _)) = s
                            .messages
                            .iter_mut()
                            .rev()
                            .find(|(role, text)| role == "You" && text == draft)
                    {
                        *role = "Unsent".into();
                    }
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

fn take_composer_shortcut(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(egui::Id::new("main-draft")))
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter))
}

fn conversation_context(messages: &[(String, String)]) -> Vec<ChatTurn> {
    let mut bytes = 0;
    let mut turns = Vec::new();
    for (role, text) in messages.iter().rev() {
        let role = match role.as_str() {
            "You" => "user",
            "Muse" => "assistant",
            _ => continue,
        };
        if turns.len() >= 24 || bytes + text.len() > 64 * 1024 {
            break;
        }
        bytes += text.len();
        turns.push(ChatTurn {
            role: role.into(),
            content: text.clone(),
        });
    }
    turns.reverse();
    turns
}

pub fn run(config: GmConfig, config_path: std::path::PathBuf) -> Result<()> {
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
            let (session, mut receiver, session_error) =
                match SessionStore::open(&config.storage_dir) {
                    Ok((store, receiver)) => (Some(store), Some(receiver), None),
                    Err(e) => (None, None, Some(e.to_string())),
                };
            let restored = session
                .as_ref()
                .map(|s| s.data.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .unwrap_or_else(SessionData::default);
            let shared = Shared {
                state: Arc::new(Mutex::new(ViewState {
                    messages: restored.messages.clone(),
                    error: session_error,
                    ..Default::default()
                })),
                context: creation.egui_ctx.clone(),
                session: session.clone(),
            };
            if let (Some(store), Some(mut changes)) = (session, receiver.take()) {
                let writer_shared = shared.clone();
                runtime.spawn(async move {
                    while changes.changed().await.is_ok() {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        changes.borrow_and_update();
                        let next = store.clone();
                        match tokio::task::spawn_blocking(move || next.flush()).await {
                            Ok(Ok(())) => {}
                            other => writer_shared.update(|s| {
                                s.error =
                                    Some(format!("Conversation could not be saved: {other:?}"))
                            }),
                        }
                    }
                });
            }
            let settings_config = config.clone();
            let config_original = std::fs::read(&config_path).ok();
            let (sender, receiver) = tokio::sync::mpsc::channel(64);
            runtime.spawn(background(config, shared.clone(), receiver));
            Ok(Box::new(NativeApp {
                runtime: runtime.clone(),
                shared,
                requests: sender,
                draft: restored.draft,
                bot: restored.bot,
                task_mode: restored.task_mode,
                search: String::new(),
                resume_note: String::new(),
                bot_editor: None,
                channel: "general".into(),
                new_channel: String::new(),
                channel_mode: false,
                files: false,
                routines: false,
                routine_editor: None,
                settings: config_original.is_none(),
                settings_config,
                config_path,
                config_original,
                settings_notice: String::new(),
                meta_key: String::new(),
                xai_key: String::new(),
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

#[cfg(test)]
mod keyboard_tests {
    use super::*;
    #[test]
    fn composer_consumes_send_shortcut_before_multiline_editor() {
        let ctx = egui::Context::default();
        let mut text = "preserve draft".to_string();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add(egui::TextEdit::multiline(&mut text).id(egui::Id::new("main-draft")))
                    .request_focus();
            });
        });
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        let raw = egui::RawInput {
            modifiers,
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        };
        let mut submitted = false;
        let _ = ctx.run(raw, |ctx| {
            submitted = take_composer_shortcut(ctx);
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add(egui::TextEdit::multiline(&mut text).id(egui::Id::new("main-draft")));
            });
        });
        assert!(submitted);
        assert_eq!(text, "preserve draft");
        let _ = ctx.run(Default::default(), |ctx| {
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("main-draft")));
            assert!(!take_composer_shortcut(ctx));
        });
    }
}

use async_trait::async_trait;
use gm_core::config::GmConfig;
use gm_core::error::{GmError, Result};
use gm_core::ipc::{
    DaemonEvent, DaemonRequest, DaemonResponse, IpcMessage, read_ipc_message, write_ipc_message,
};
use gm_llm::{BudgetManager, ChatMessage, HybridLlmClient, LlmRouter, TaskType};
use gm_platform::{IpcPlatformClient, Platform};
use gm_safety::{AuditLogger, BlocklistManager, KillSwitch};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{Mutex, broadcast, oneshot};
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlRequestEnvelope {
    Envelope(IpcMessage<DaemonRequest>),
    Raw(DaemonRequest),
}

/// Daemon interaction handler that routes approval requests to the UI / NSAlert sheet via IPC
#[derive(Clone)]
pub struct DaemonApprovalHandler {
    pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    timeout: std::time::Duration,
}

impl DaemonApprovalHandler {
    pub fn new(
        pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
        event_tx: broadcast::Sender<DaemonEvent>,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            pending_approvals,
            event_tx,
            timeout,
        }
    }
}

struct PendingApprovalGuard {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    request_id: String,
    events: broadcast::Sender<DaemonEvent>,
}
impl Drop for PendingApprovalGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.try_lock() {
            pending.remove(&self.request_id);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let pending = self.pending.clone();
            let id = self.request_id.clone();
            runtime.spawn(async move {
                pending.lock().await.remove(&id);
            });
        }
        let _ = self.events.send(DaemonEvent::ApprovalResolved {
            request_id: self.request_id.clone(),
        });
    }
}

#[async_trait]
impl gm_agent::InteractionHandler for DaemonApprovalHandler {
    async fn request_approval(&self, req: &gm_agent::ApprovalRequest) -> Result<bool> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let _guard = PendingApprovalGuard {
            pending: self.pending_approvals.clone(),
            request_id: request_id.clone(),
            events: self.event_tx.clone(),
        };
        self.pending_approvals
            .lock()
            .await
            .insert(request_id.clone(), tx);

        let title = format!("Action Approval Required for Task {}", req.task_id);
        let details = format!(
            "Step {}: Action '{:?}' targeting '{:?}' requires user confirmation. Reason: {}",
            req.step,
            req.action,
            req.target_label.as_deref().unwrap_or("<none>"),
            req.reason
        );

        let _ = self.event_tx.send(DaemonEvent::ApprovalRequested {
            request_id: request_id.clone(),
            title,
            details,
        });

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => Ok(approved),
            Ok(Err(_)) => Ok(false),
            Err(_) => {
                self.pending_approvals.lock().await.remove(&request_id);
                warn!(
                    "Approval request {} timed out after {:?}",
                    request_id, self.timeout
                );
                Ok(false)
            }
        }
    }

    async fn ask_user(&self, _question: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn request_wheel_handoff(
        &self,
        req: &gm_agent::WheelHandoffRequest,
    ) -> Result<Option<String>> {
        let request_id = format!("wheel_{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let (tx, rx) = oneshot::channel();
        self.pending_approvals
            .lock()
            .await
            .insert(request_id.clone(), tx);

        let bot_id = req.bot_id.clone().unwrap_or_else(|| "muse".to_string());
        let _ = self.event_tx.send(DaemonEvent::WheelHandoffRequested {
            task_id: req.task_id.clone(),
            bot_id,
            blocker: req.blocker.clone(),
            instructions: req.instructions.clone(),
        });
        let _ = self.event_tx.send(DaemonEvent::ApprovalRequested {
            request_id: request_id.clone(),
            title: format!("🛞 Take the Wheel ({}) — Task {}", req.blocker, req.task_id),
            details: req.instructions.clone(),
        });

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(true)) => Ok(Some("Resumed via DaemonApprovalHandler".to_string())),
            Ok(Ok(false)) | Ok(Err(_)) => Ok(None),
            Err(_) => {
                self.pending_approvals.lock().await.remove(&request_id);
                Ok(None)
            }
        }
    }
}

#[derive(Clone)]
pub struct DaemonControlServer {
    config: GmConfig,
    paused: Arc<AtomicBool>,
    active_task: Arc<Mutex<Option<String>>>,
    active_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    budget: Arc<BudgetManager>,
    llm_client: Arc<HybridLlmClient>,
    router: LlmRouter,
    kill_switch: KillSwitch,
    platform: Arc<IpcPlatformClient>,
    openbot: Option<gm_openbot::OpenBotSupervisor>,
}

impl DaemonControlServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: GmConfig,
        budget: Arc<BudgetManager>,
        llm_client: Arc<HybridLlmClient>,
        router: LlmRouter,
        kill_switch: KillSwitch,
        _blocklist: BlocklistManager,
        audit_log: Arc<AuditLogger>,
        platform: Arc<IpcPlatformClient>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let openbot = gm_openbot::OpenBotSupervisor::open(
            config.clone(),
            Arc::clone(&audit_log),
            event_tx.clone(),
        )
        .ok();
        Self {
            config,
            paused: Arc::new(AtomicBool::new(false)),
            active_task: Arc::new(Mutex::new(None)),
            active_task_handle: Arc::new(Mutex::new(None)),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            budget,
            llm_client,
            router,
            kill_switch,
            platform,
            openbot,
        }
    }

    pub fn start_runtime(&self) -> Result<tokio::task::JoinHandle<()>> {
        self.openbot
            .as_ref()
            .ok_or_else(|| GmError::Config("Task runtime could not initialize".into()))?
            .start_runtime(
                self.platform.clone(),
                self.llm_client.clone(),
                self.router.clone(),
                self.kill_switch.clone(),
                BlocklistManager::new(&self.config.safety.blocklisted_apps),
                Some(Arc::new(DaemonApprovalHandler::new(
                    self.pending_approvals.clone(),
                    self.event_tx.clone(),
                    std::time::Duration::from_secs(60),
                ))),
            )
    }

    pub fn openbot(&self) -> Option<&gm_openbot::OpenBotSupervisor> {
        self.openbot.as_ref()
    }

    pub fn event_sender(&self) -> broadcast::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn pending_approvals(&self) -> Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>> {
        Arc::clone(&self.pending_approvals)
    }

    pub async fn register_approval_request(&self, request_id: String, tx: oneshot::Sender<bool>) {
        self.pending_approvals.lock().await.insert(request_id, tx);
    }

    pub async fn run(&self) -> Result<()> {
        let path = &self.config.daemon_socket_path;
        #[cfg(unix)]
        {
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let listener = UnixListener::bind(path)?;
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                if let Err(e) = std::fs::set_permissions(path, perms) {
                    warn!(
                        "Failed to lock daemon socket permissions to 0o600 on {:?}: {e}",
                        path
                    );
                }
            }

            let my_euid = unsafe { libc::geteuid() };
            info!(
                "Daemon control server listening on {:?} (UID: {my_euid}, mode 0600)",
                path
            );

            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        match stream.peer_cred() {
                            Ok(cred) => {
                                if !gm_core::ipc::is_peer_authorized(cred.uid(), my_euid) {
                                    warn!(
                                        "Rejecting daemon socket peer with mismatched UID (peer: {}, expected: {})",
                                        cred.uid(),
                                        my_euid
                                    );
                                    continue;
                                }
                            }
                            Err(e) => {
                                warn!("Failed to inspect peer credentials on daemon socket: {e}");
                                continue;
                            }
                        }

                        let server = self.clone_server_state();
                        tokio::spawn(async move {
                            if let Err(e) = server.handle_connection(stream).await {
                                warn!("Daemon connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        error!("Daemon listener error: {e}");
                        break;
                    }
                }
            }
        }

        #[cfg(windows)]
        {
            let pipe_name = path.to_string_lossy();
            info!(
                "Daemon control server listening on named pipe {}",
                pipe_name
            );

            let mut is_first = true;
            loop {
                let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
                if is_first {
                    options.first_pipe_instance(true);
                    is_first = false;
                }
                let server = options.create(path.as_os_str()).map_err(|e| {
                    GmError::Platform(format!(
                        "Failed to create daemon named pipe {pipe_name}: {e}"
                    ))
                })?;

                server.connect().await.map_err(|e| {
                    GmError::Platform(format!(
                        "Failed to connect client to daemon named pipe: {e}"
                    ))
                })?;

                let state = self.clone_server_state();
                tokio::spawn(async move {
                    if let Err(e) = state.handle_connection(server).await {
                        warn!("Daemon named pipe connection error: {e}");
                    }
                });
            }
        }

        #[allow(unreachable_code)]
        Ok(())
    }

    fn clone_server_state(&self) -> DaemonControlServerState {
        DaemonControlServerState {
            _config: self.config.clone(),
            paused: Arc::clone(&self.paused),
            active_task: Arc::clone(&self.active_task),
            active_task_handle: Arc::clone(&self.active_task_handle),
            pending_approvals: Arc::clone(&self.pending_approvals),
            event_tx: self.event_tx.clone(),
            budget: Arc::clone(&self.budget),
            llm_client: Arc::clone(&self.llm_client),
            router: self.router.clone(),
            kill_switch: self.kill_switch.clone(),
            platform: Arc::clone(&self.platform),
            openbot: self.openbot.clone(),
        }
    }
}

#[derive(Clone)]
struct DaemonControlServerState {
    _config: GmConfig,
    paused: Arc<AtomicBool>,
    active_task: Arc<Mutex<Option<String>>>,
    active_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pending_approvals: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    budget: Arc<BudgetManager>,
    llm_client: Arc<HybridLlmClient>,
    router: LlmRouter,
    kill_switch: KillSwitch,
    platform: Arc<IpcPlatformClient>,
    openbot: Option<gm_openbot::OpenBotSupervisor>,
}

fn assemble_chat_messages(
    text: &str,
    history: &[gm_core::ipc::ChatTurn],
) -> Result<Vec<ChatMessage>> {
    if text.trim().is_empty()
        || text.len() > 64 * 1024
        || history.len() > 24
        || history.iter().map(|t| t.content.len()).sum::<usize>() > 64 * 1024
        || history
            .iter()
            .any(|t| !matches!(t.role.as_str(), "user" | "assistant"))
    {
        return Err(GmError::Config(
            "Chat context is invalid or exceeds its size limit".into(),
        ));
    }
    let mut messages = vec![ChatMessage::text(
        "system",
        "You are Grok Muse, a helpful personal assistant. Use the supplied conversation for continuity. Explain clearly and do not claim you read files, searched sources or executed actions without tool results. The chat interface has no execution tools; the user can choose Run task for workspace or desktop actions.",
    )];
    messages.extend(
        history
            .iter()
            .map(|turn| ChatMessage::text(&turn.role, &turn.content)),
    );
    messages.push(ChatMessage::text("user", text));
    Ok(messages)
}

fn chat_event(id: &Option<String>, delta: String, done: bool) -> DaemonEvent {
    match id {
        Some(id) => DaemonEvent::ChatTokenV1 {
            message_id: id.clone(),
            delta,
            done,
        },
        None => DaemonEvent::ChatToken { delta, done },
    }
}

impl DaemonControlServerState {
    async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: S,
    ) -> Result<()> {
        loop {
            let envelope: ControlRequestEnvelope = match read_ipc_message(&mut stream).await {
                Ok(msg) => msg,
                Err(GmError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(e) => return Err(e),
            };

            let (request_id, request) = match envelope {
                ControlRequestEnvelope::Envelope(msg) => (msg.id, msg.payload),
                ControlRequestEnvelope::Raw(req) => (0, req),
            };

            let chat_id = match &request {
                DaemonRequest::SendChatMessageV1 { message_id, .. }
                | DaemonRequest::SendChatMessageV2 { message_id, .. } => Some(message_id.clone()),
                _ => None,
            };
            let history = match &request {
                DaemonRequest::SendChatMessageV2 { history, .. } => history.as_slice(),
                _ => &[],
            };
            let chat_messages = match &request {
                DaemonRequest::SendChatMessage { text }
                | DaemonRequest::SendChatMessageV1 { text, .. }
                | DaemonRequest::SendChatMessageV2 { text, .. } => {
                    Some(assemble_chat_messages(text, history))
                }
                _ => None,
            };
            match request {
                DaemonRequest::Ping => {
                    let res = IpcMessage {
                        id: request_id,
                        payload: DaemonResponse::Pong,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::GetStatus => {
                    let helper_connected = tokio::time::timeout(
                        tokio::time::Duration::from_millis(150),
                        self.platform.ping(),
                    )
                    .await
                    .map(|r| r.is_ok())
                    .unwrap_or(false);

                    let paused = self.paused.load(Ordering::SeqCst);
                    let active_task = self.active_task.lock().await.clone();
                    let daily_spend_usd = self.budget.get_daily_spent();

                    let res = IpcMessage {
                        id: request_id,
                        payload: DaemonResponse::Status {
                            helper_connected,
                            paused,
                            active_task,
                            daily_spend_usd,
                        },
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::SetPaused { paused } => {
                    self.paused.store(paused, Ordering::SeqCst);
                    if let Some(sup) = &self.openbot {
                        sup.set_runtime_paused(paused)?;
                    }
                    let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                        state: if paused {
                            "Paused".into()
                        } else {
                            "Idle".into()
                        },
                        detail: if paused {
                            "Paused by user coexistence toggle".into()
                        } else {
                            "Resumed by user coexistence toggle".into()
                        },
                        daily_spend_usd: self.budget.get_daily_spent(),
                    });
                    let res = IpcMessage {
                        id: request_id,
                        payload: DaemonResponse::Ack,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::EmergencyKill => {
                    info!("DaemonControlServer: EmergencyKill requested");
                    self.kill_switch.trigger();

                    // Abort active task handle if running
                    if let Some(handle) = self.active_task_handle.lock().await.take() {
                        handle.abort();
                    }
                    *self.active_task.lock().await = None;

                    // Send CancelActions to helper concurrently (<50ms budget)
                    let platform = Arc::clone(&self.platform);
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(
                            tokio::time::Duration::from_millis(40),
                            platform.cancel(),
                        )
                        .await;
                    });

                    let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                        state: "Stopped".into(),
                        detail: "Emergency kill switch triggered".into(),
                        daily_spend_usd: self.budget.get_daily_spent(),
                    });

                    let res = IpcMessage {
                        id: request_id,
                        payload: DaemonResponse::Killed,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::RespondApproval {
                    request_id: app_id,
                    approved,
                } => {
                    let mut approvals = self.pending_approvals.lock().await;
                    let resp = if let Some(tx) = approvals.remove(&app_id) {
                        let _ = tx.send(approved);
                        DaemonResponse::Ack
                    } else {
                        DaemonResponse::Error(format!(
                            "No pending approval found for request_id: {app_id}"
                        ))
                    };
                    let res = IpcMessage {
                        id: request_id,
                        payload: resp,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::SendChatMessage { .. }
                | DaemonRequest::SendChatMessageV1 { .. }
                | DaemonRequest::SendChatMessageV2 { .. } => {
                    let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                        state: "Thinking".into(),
                        detail: "Thinking...".into(),
                        daily_spend_usd: self.budget.get_daily_spent(),
                    });

                    let messages = match chat_messages
                        .unwrap_or_else(|| Err(GmError::Config("Chat request is missing".into())))
                    {
                        Ok(messages) => messages,
                        Err(error) => {
                            write_ipc_message(
                                &mut stream,
                                &IpcMessage {
                                    id: request_id,
                                    payload: DaemonResponse::Error(error.to_string()),
                                },
                            )
                            .await?;
                            continue;
                        }
                    };
                    let (primary, fallback) = self.router.route(TaskType::Planning);

                    let (delta_tx, mut delta_rx) = tokio::sync::mpsc::channel::<String>(64);
                    let event_tx_delta = self.event_tx.clone();
                    let forward_id = chat_id.clone();
                    let forward_task = tokio::spawn(async move {
                        while let Some(delta) = delta_rx.recv().await {
                            let _ = event_tx_delta.send(chat_event(&forward_id, delta, false));
                        }
                    });

                    let mut kill_events = self.kill_switch.subscribe();
                    let chat_client = self.llm_client.scoped_job();
                    let result = if self.kill_switch.is_triggered() {
                        drop(delta_tx);
                        Err(GmError::SafetyViolation("Emergency stop is active".into()))
                    } else {
                        tokio::select! {biased; _=kill_events.recv()=>Err(GmError::SafetyViolation("Chat stopped by emergency stop".into())),
                        result=chat_client.complete_stream(&primary,fallback.as_ref(),&messages,1024,0.2,delta_tx)=>result }
                    };
                    match result {
                        Ok(completion) => {
                            let _ = forward_task.await;
                            let reply = completion.content;
                            let _ = self
                                .event_tx
                                .send(chat_event(&chat_id, String::new(), true));
                            let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                                state: "Idle".into(),
                                detail: "Ready".into(),
                                daily_spend_usd: self.budget.get_daily_spent(),
                            });
                            let res = IpcMessage {
                                id: request_id,
                                payload: match &chat_id {
                                    Some(id) => DaemonResponse::ChatReplyV1 {
                                        message_id: id.clone(),
                                        text: reply,
                                    },
                                    None => DaemonResponse::ChatReply { text: reply },
                                },
                            };
                            write_ipc_message(&mut stream, &res).await?;
                        }
                        Err(e) => {
                            let _ = forward_task.await;
                            let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                                state: "Idle".into(),
                                detail: format!("Error: {e}"),
                                daily_spend_usd: self.budget.get_daily_spent(),
                            });
                            let res = IpcMessage {
                                id: request_id,
                                payload: DaemonResponse::Error(e.to_string()),
                            };
                            write_ipc_message(&mut stream, &res).await?;
                        }
                    }
                }
                DaemonRequest::SubmitTask { instruction } => {
                    let payload = match self.openbot.as_ref() {
                        Some(sup) if !self.kill_switch.is_triggered() => {
                            match sup.submit_task(None, Some("general"), &instruction, 0) {
                                Ok(task) => DaemonResponse::Accepted { id: task.id },
                                Err(error) => DaemonResponse::Error(error.to_string()),
                            }
                        }
                        Some(_) => DaemonResponse::Error(
                            "Emergency stop is active; restart the runtime to resume".into(),
                        ),
                        None => DaemonResponse::Error("Task runtime is unavailable".into()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::ListBots => {
                    let payload = if let Some(ref sup) = self.openbot {
                        let bots = sup.roster().list_bots();
                        DaemonResponse::JsonPayload(serde_json::to_value(bots).unwrap_or_default())
                    } else {
                        DaemonResponse::Error("OpenBot supervisor not initialized".into())
                    };
                    let res = IpcMessage {
                        id: request_id,
                        payload,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::ImportAttachment {
                    bot_id,
                    source_path,
                } => {
                    let attachment_bot = bot_id.clone();
                    let result = if let Some(supervisor) = self.openbot.clone() {
                        tokio::task::spawn_blocking(move || {
                            supervisor.import_attachment(&bot_id, &source_path)
                        })
                        .await
                        .map_err(|e| GmError::Platform(e.to_string()))
                        .and_then(|r| r)
                    } else {
                        Err(GmError::Config("Task runtime is unavailable".into()))
                    };
                    let payload = match result {
                        Ok(path) => DaemonResponse::JsonPayload(
                            serde_json::json!({"attachment_path":path,"bot_id":attachment_bot}),
                        ),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::ListBotFiles { bot_id } => {
                    let result = (|| {
                        let sup = self
                            .openbot
                            .as_ref()
                            .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))?;
                        let files = sup.roster().workspace_sandbox(&bot_id)?.shared_files()?;
                        Ok::<_, GmError>(
                            serde_json::json!({"bot_id":bot_id,"files":files.into_iter().map(|(path,size)|serde_json::json!({"path":path,"size":size})).collect::<Vec<_>>()}),
                        )
                    })();
                    let payload = match result {
                        Ok(value) => DaemonResponse::JsonPayload(value),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::PreviewBotFile { bot_id, path } => {
                    let result = (|| {
                        let sup = self
                            .openbot
                            .as_ref()
                            .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))?;
                        let bytes = sup
                            .roster()
                            .workspace_sandbox(&bot_id)?
                            .preview_shared_file(&path)?;
                        let text = String::from_utf8(bytes).map_err(|_| {
                            GmError::Config("This binary file has no text preview".into())
                        })?;
                        Ok::<_, GmError>(
                            serde_json::json!({"bot_id":bot_id,"path":path,"text":text}),
                        )
                    })();
                    let payload = match result {
                        Ok(value) => DaemonResponse::JsonPayload(value),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::ListRoutines => {
                    let result = self
                        .openbot
                        .as_ref()
                        .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))
                        .and_then(|sup| sup.queue().list_routines())
                        .and_then(|value| serde_json::to_value(value).map_err(Into::into));
                    let payload = match result {
                        Ok(value) => DaemonResponse::JsonPayload(value),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::SaveRoutine { routine } => {
                    let result = (|| {
                        let sup = self
                            .openbot
                            .as_ref()
                            .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))?;
                        let routine: gm_openbot::Routine = serde_json::from_value(routine)?;
                        sup.save_routine(&routine)?;
                        Ok::<_, GmError>(())
                    })();
                    let payload = match result {
                        Ok(()) => DaemonResponse::Ack,
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::SaveBot { profile } => {
                    let result = (|| {
                        let sup = self
                            .openbot
                            .as_ref()
                            .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))?;
                        let profile: gm_openbot::BotProfile = serde_json::from_value(profile)?;
                        Ok::<_, GmError>(serde_json::to_value(sup.roster().upsert_bot(profile)?)?)
                    })();
                    let payload = match result {
                        Ok(value) => DaemonResponse::JsonPayload(value),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::ListChannels => {
                    let result = self
                        .openbot
                        .as_ref()
                        .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))
                        .and_then(|sup| sup.channels().list_channels(false))
                        .and_then(|value| serde_json::to_value(value).map_err(Into::into));
                    let payload = match result {
                        Ok(value) => DaemonResponse::JsonPayload(value),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::ChannelHistory { channel_id } => {
                    let result = self
                        .openbot
                        .as_ref()
                        .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))
                        .and_then(|sup| sup.channels().list_messages(&channel_id, 100))
                        .and_then(|value| serde_json::to_value(value).map_err(Into::into));
                    let payload = match result {
                        Ok(value) => DaemonResponse::JsonPayload(value),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::CreateChannel {
                    channel_id,
                    topic,
                    owner_bot_id,
                    participants,
                } => {
                    let result = self
                        .openbot
                        .as_ref()
                        .ok_or_else(|| GmError::Config("Task runtime is unavailable".into()))
                        .and_then(|sup| {
                            sup.create_channel(
                                &channel_id,
                                &topic,
                                owner_bot_id.as_deref(),
                                &participants,
                            )
                        })
                        .and_then(|value| serde_json::to_value(value).map_err(Into::into));
                    let payload = match result {
                        Ok(value) => DaemonResponse::JsonPayload(value),
                        Err(e) => DaemonResponse::Error(e.to_string()),
                    };
                    write_ipc_message(
                        &mut stream,
                        &IpcMessage {
                            id: request_id,
                            payload,
                        },
                    )
                    .await?;
                }
                DaemonRequest::QueueTask {
                    bot_id,
                    channel_id,
                    instruction,
                    priority,
                } => {
                    let payload = if let Some(ref sup) = self.openbot {
                        match sup.submit_task(
                            bot_id.as_deref(),
                            channel_id.as_deref(),
                            &instruction,
                            priority,
                        ) {
                            Ok(task) => DaemonResponse::Accepted { id: task.id },
                            Err(e) => DaemonResponse::Error(e.to_string()),
                        }
                    } else {
                        DaemonResponse::Error("OpenBot supervisor not initialized".into())
                    };
                    let res = IpcMessage {
                        id: request_id,
                        payload,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::QueueControl { task_id, action } => {
                    let payload = if let Some(ref sup) = self.openbot {
                        match sup.control_task(&task_id, &action) {
                            Ok(_) => DaemonResponse::Ack,
                            Err(e) => DaemonResponse::Error(e.to_string()),
                        }
                    } else {
                        DaemonResponse::Error("OpenBot supervisor not initialized".into())
                    };
                    let res = IpcMessage {
                        id: request_id,
                        payload,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::ListQueue { bot_id } => {
                    let payload = if let Some(ref sup) = self.openbot {
                        match sup.queue().list_tasks(bot_id.as_deref()) {
                            Ok(tasks) => DaemonResponse::JsonPayload(
                                serde_json::to_value(tasks).unwrap_or_default(),
                            ),
                            Err(e) => DaemonResponse::Error(e.to_string()),
                        }
                    } else {
                        DaemonResponse::Error("OpenBot supervisor not initialized".into())
                    };
                    let res = IpcMessage {
                        id: request_id,
                        payload,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::ChannelPost {
                    channel_id,
                    author,
                    content,
                } => {
                    let payload = if let Some(ref sup) = self.openbot {
                        match sup.post_channel_message(&channel_id, &author, None, &content) {
                            Ok((msg, _)) => DaemonResponse::JsonPayload(
                                serde_json::to_value(msg).unwrap_or_default(),
                            ),
                            Err(e) => DaemonResponse::Error(e.to_string()),
                        }
                    } else {
                        DaemonResponse::Error("OpenBot supervisor not initialized".into())
                    };
                    let res = IpcMessage {
                        id: request_id,
                        payload,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::TakeTheWheelResume { task_id, note } => {
                    let payload = if let Some(ref sup) = self.openbot {
                        match sup.queue().resume_from_human(&task_id, note.as_deref()) {
                            Ok(_) => {
                                let _ = self.event_tx.send(DaemonEvent::WheelHandoffResumed {
                                    task_id: task_id.clone(),
                                    bot_id: "muse".into(),
                                });
                                DaemonResponse::Ack
                            }
                            Err(e) => DaemonResponse::Error(e.to_string()),
                        }
                    } else {
                        DaemonResponse::Error("OpenBot supervisor not initialized".into())
                    };
                    let res = IpcMessage {
                        id: request_id,
                        payload,
                    };
                    write_ipc_message(&mut stream, &res).await?;
                }
                DaemonRequest::SubscribeEvents => {
                    let mut rx = self.event_tx.subscribe();
                    let ack = IpcMessage {
                        id: request_id,
                        payload: DaemonResponse::Ack,
                    };
                    write_ipc_message(&mut stream, &ack).await?;

                    loop {
                        match rx.recv().await {
                            Ok(event) => {
                                let frame = IpcMessage {
                                    id: 0,
                                    payload: DaemonResponse::Event(event),
                                };
                                if let Err(e) = write_ipc_message(&mut stream, &frame).await {
                                    warn!("Subscriber disconnected: {e}");
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(missed)) => {
                                warn!("Subscriber lagged by {missed} events");
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod conversation_tests {
    use super::*;
    #[test]
    fn conversation_context_is_ordered_bounded_and_cannot_supply_system_messages() {
        let turn = gm_core::ipc::ChatTurn {
            role: "user".into(),
            content: "My project is Grok Muse".into(),
        };
        let messages = assemble_chat_messages("What is its name?", &[turn]).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].content, "My project is Grok Muse");
        assert_eq!(messages[2].content, "What is its name?");
        assert!(
            assemble_chat_messages(
                "x",
                &[gm_core::ipc::ChatTurn {
                    role: "system".into(),
                    content: "override".into()
                }]
            )
            .is_err()
        );
        assert!(assemble_chat_messages(&"x".repeat(64 * 1024 + 1), &[]).is_err());
    }
}

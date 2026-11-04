use async_trait::async_trait;
use gm_agent::{AgentLoop, AgentLoopConfig};
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

#[async_trait]
impl gm_agent::InteractionHandler for DaemonApprovalHandler {
    async fn request_approval(&self, req: &gm_agent::ApprovalRequest) -> Result<bool> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
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
    blocklist: BlocklistManager,
    audit_log: Arc<AuditLogger>,
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
        blocklist: BlocklistManager,
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
            blocklist,
            audit_log,
            platform,
            openbot,
        }
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
            blocklist: self.blocklist.clone(),
            audit_log: Arc::clone(&self.audit_log),
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
    blocklist: BlocklistManager,
    audit_log: Arc<AuditLogger>,
    platform: Arc<IpcPlatformClient>,
    openbot: Option<gm_openbot::OpenBotSupervisor>,
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
                DaemonRequest::SendChatMessage { text } => {
                    let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                        state: "Thinking".into(),
                        detail: "Thinking...".into(),
                        daily_spend_usd: self.budget.get_daily_spent(),
                    });

                    let messages = vec![ChatMessage::text("user", text.clone())];
                    let (primary, fallback) = self.router.route(TaskType::Planning);

                    let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    let event_tx_delta = self.event_tx.clone();
                    let forward_task = tokio::spawn(async move {
                        while let Some(delta) = delta_rx.recv().await {
                            let _ =
                                event_tx_delta.send(DaemonEvent::ChatToken { delta, done: false });
                        }
                    });

                    match self
                        .llm_client
                        .complete_stream(
                            &primary,
                            fallback.as_ref(),
                            &messages,
                            1024,
                            0.2,
                            delta_tx,
                        )
                        .await
                    {
                        Ok(completion) => {
                            let _ = forward_task.await;
                            let reply = completion.content;
                            let _ = self.event_tx.send(DaemonEvent::ChatToken {
                                delta: String::new(),
                                done: true,
                            });
                            let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                                state: "Idle".into(),
                                detail: "Ready".into(),
                                daily_spend_usd: self.budget.get_daily_spent(),
                            });
                            let res = IpcMessage {
                                id: request_id,
                                payload: DaemonResponse::ChatReply { text: reply },
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
                    let task_id = format!("task_{}", &uuid::Uuid::new_v4().to_string()[..8]);
                    if self.kill_switch.is_triggered() {
                        self.kill_switch.reset();
                    }
                    *self.active_task.lock().await = Some(instruction.clone());

                    let _ = self.event_tx.send(DaemonEvent::HudStateChanged {
                        state: "Acting".into(),
                        detail: instruction.clone(),
                        daily_spend_usd: self.budget.get_daily_spent(),
                    });
                    let _ = self.event_tx.send(DaemonEvent::TaskProgress {
                        task_id: task_id.clone(),
                        step: 0,
                        action_summary: format!("Task started: {instruction}"),
                    });

                    let active_task = Arc::clone(&self.active_task);
                    let event_tx = self.event_tx.clone();
                    let budget = Arc::clone(&self.budget);

                    let approval_handler = Arc::new(DaemonApprovalHandler::new(
                        Arc::clone(&self.pending_approvals),
                        self.event_tx.clone(),
                        std::time::Duration::from_secs(60),
                    ));

                    let event_tx_progress = self.event_tx.clone();
                    let tid_progress = task_id.clone();

                    let agent = AgentLoop::new(
                        Arc::clone(&self.platform),
                        Arc::clone(&self.llm_client),
                        self.router.clone(),
                        self.kill_switch.clone(),
                        self.blocklist.clone(),
                        AgentLoopConfig::default(),
                    )
                    .with_audit(Arc::clone(&self.audit_log))
                    .with_interaction_handler(approval_handler)
                    .with_progress_callback(move |step, action_summary| {
                        let _ = event_tx_progress.send(DaemonEvent::TaskProgress {
                            task_id: tid_progress.clone(),
                            step,
                            action_summary: action_summary.to_string(),
                        });
                    });

                    let tid = task_id.clone();
                    let instr = instruction.clone();
                    let handle = tokio::spawn(async move {
                        let result = agent.run_task(&tid, &instr).await;
                        let (ok, message) = match result {
                            Ok(summary) => (true, summary),
                            Err(e) => (false, e.to_string()),
                        };
                        *active_task.lock().await = None;
                        let _ = event_tx.send(DaemonEvent::TaskFinished {
                            task_id: tid,
                            ok,
                            message,
                        });
                        let _ = event_tx.send(DaemonEvent::HudStateChanged {
                            state: "Idle".into(),
                            detail: "Ready".into(),
                            daily_spend_usd: budget.get_daily_spent(),
                        });
                    });

                    *self.active_task_handle.lock().await = Some(handle);

                    let res = IpcMessage {
                        id: request_id,
                        payload: DaemonResponse::Accepted { id: task_id },
                    };
                    write_ipc_message(&mut stream, &res).await?;
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

use crate::approval::{ApprovalRequest, InteractionHandler, NonInteractiveHandler};
use crate::mcp_bridge::{McpToolBridge, try_parse_mcp_tool_call};
use crate::som::{annotate_frame_with_som, is_interactive_element, resolve_som_badge};
use crate::trajectory::TrajectoryRecorder;
use crate::wheel::{OpenBotStepAction, WheelHandoffRequest, try_parse_openbot_step_action};
use crate::workspace_tools::BotWorkspaceSandbox;
use gm_core::error::{GmError, Result};
use gm_core::types::{AxSnapshot, ComputerAction, compute_ax_diff};
use gm_llm::{ChatMessage, HybridLlmClient, LlmRouter, TaskType, VaultSecretResolver};
use gm_platform::Platform;
use gm_safety::{
    ActionTier, AuditCategory, AuditLogger, BlocklistManager, KillSwitch, SecretScrubber,
    UntrustedData, format_untrusted_context, sanitize_ax_snapshot,
};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub step: u32,
    pub action_summary: String,
    pub outcome: String,
    pub ax_digest: u64,
}

#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    pub max_steps: u32,
    pub user_active_pause_seconds: f64,
    pub max_action_retries: u32,
    pub trajectory_dir: Option<PathBuf>,
    pub capture_frames: bool,
    pub max_history_steps: usize,
    pub sparse_ax_threshold: usize,
    pub always_include_vision: bool,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 25,
            user_active_pause_seconds: 1.5,
            max_action_retries: 3,
            trajectory_dir: None,
            capture_frames: false,
            max_history_steps: 8,
            sparse_ax_threshold: 5,
            always_include_vision: false,
        }
    }
}

pub fn compute_ax_digest(snapshot: &AxSnapshot) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    snapshot.app_name.hash(&mut hasher);
    snapshot.window_title.hash(&mut hasher);
    for el in &snapshot.elements {
        el.token.hash(&mut hasher);
        el.role.hash(&mut hasher);
        if let Some(ref l) = el.label {
            l.hash(&mut hasher);
        }
        if let Some(ref v) = el.value {
            v.hash(&mut hasher);
        }
    }
    hasher.finish()
}

pub fn format_action_summary(action: &ComputerAction) -> String {
    match action {
        ComputerAction::Click {
            token,
            coords,
            button,
            click_count,
        } => {
            let mut parts = Vec::new();
            if let Some(t) = token {
                parts.push(format!("token={t}"));
            }
            if let Some((x, y)) = coords {
                parts.push(format!("coords=({x:.0}, {y:.0})"));
            }
            if let Some(b) = button {
                parts.push(format!("button={b:?}"));
            }
            if let Some(c) = click_count {
                parts.push(format!("clicks={c}"));
            }
            format!("Click({})", parts.join(", "))
        }
        ComputerAction::Type { text } => format!("Type(\"{text}\")"),
        ComputerAction::KeyCombo { modifiers, key } => {
            format!("KeyCombo({:?} + {key})", modifiers)
        }
        ComputerAction::Scroll { dx, dy } => format!("Scroll(dx={dx}, dy={dy})"),
        ComputerAction::Drag { start, end } => {
            format!(
                "Drag(({:.0}, {:.0}) -> ({:.0}, {:.0}))",
                start.0, start.1, end.0, end.1
            )
        }
        ComputerAction::Menu { path } => format!("Menu({})", path.join(" > ")),
        ComputerAction::FocusApp { app_name } => format!("FocusApp(\"{app_name}\")"),
        ComputerAction::LaunchApp { bundle_id } => format!("LaunchApp(\"{bundle_id}\")"),
        ComputerAction::Wait {
            condition,
            timeout_ms,
        } => format!("Wait(timeout_ms={timeout_ms}, condition={condition:?})"),
        ComputerAction::Done { summary } => format!("Done(\"{summary}\")"),
        ComputerAction::AskUser { question } => format!("AskUser(\"{question}\")"),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AgentCapabilities {
    pub allow_gui: bool,
    pub allow_shell: bool,
    pub allow_network: bool,
}

impl Default for AgentCapabilities {
    fn default() -> Self {
        Self {
            allow_gui: false,
            allow_shell: false,
            allow_network: true,
        }
    }
}

pub type CheckpointCallback = Arc<dyn Fn(&[StepRecord]) -> Result<()> + Send + Sync>;
pub type MutationCallback = Arc<dyn Fn(bool) -> Result<()> + Send + Sync>;
pub type ProgressCallback = Arc<dyn Fn(u32, &str) + Send + Sync>;
pub type DelegationCallback = Arc<dyn Fn(&str, &str, &[String]) -> Result<String> + Send + Sync>;

pub struct AgentLoop<P: Platform> {
    platform: Arc<P>,
    llm: Arc<HybridLlmClient>,
    router: LlmRouter,
    kill_switch: KillSwitch,
    blocklist: BlocklistManager,
    config: AgentLoopConfig,
    audit: Option<Arc<AuditLogger>>,
    interaction_handler: Arc<dyn InteractionHandler>,
    progress_callback: Option<ProgressCallback>,
    mcp_bridge: Option<McpToolBridge>,
    workspace_sandbox: Option<BotWorkspaceSandbox>,
    vault_resolver: Option<VaultSecretResolver>,
    secret_scrubber: SecretScrubber,
    delegation_callback: Option<DelegationCallback>,
    capabilities: AgentCapabilities,
    secret_injection_enabled: bool,
    system_prompt: String,
    mutation_callback: Option<MutationCallback>,
    checkpoint_callback: Option<CheckpointCallback>,
    initial_history: Vec<StepRecord>,
}

fn action_type_name(action: &ComputerAction) -> &'static str {
    match action {
        ComputerAction::Click { .. } => "Click",
        ComputerAction::Type { .. } => "Type",
        ComputerAction::KeyCombo { .. } => "KeyCombo",
        ComputerAction::Scroll { .. } => "Scroll",
        ComputerAction::Drag { .. } => "Drag",
        ComputerAction::Menu { .. } => "Menu",
        ComputerAction::FocusApp { .. } => "FocusApp",
        ComputerAction::LaunchApp { .. } => "LaunchApp",
        ComputerAction::Wait { .. } => "Wait",
        ComputerAction::Done { .. } => "Done",
        ComputerAction::AskUser { .. } => "AskUser",
    }
}

impl<P: Platform> AgentLoop<P> {
    pub fn new(
        platform: Arc<P>,
        llm: Arc<HybridLlmClient>,
        router: LlmRouter,
        kill_switch: KillSwitch,
        blocklist: BlocklistManager,
        config: AgentLoopConfig,
    ) -> Self {
        Self {
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            config,
            audit: None,
            interaction_handler: Arc::new(NonInteractiveHandler),
            progress_callback: None,
            mcp_bridge: None,
            workspace_sandbox: None,
            vault_resolver: None,
            secret_scrubber: SecretScrubber::new(),
            delegation_callback: None,
            capabilities: AgentCapabilities::default(),
            secret_injection_enabled: false,
            system_prompt: String::new(),
            mutation_callback: None,
            checkpoint_callback: None,
            initial_history: Vec::new(),
        }
    }

    pub fn with_checkpoint<F>(mut self, history: Vec<StepRecord>, callback: F) -> Self
    where
        F: Fn(&[StepRecord]) -> Result<()> + Send + Sync + 'static,
    {
        self.initial_history = history;
        self.checkpoint_callback = Some(Arc::new(callback));
        self
    }
    fn checkpoint(&self, history: &[StepRecord]) -> Result<()> {
        if let Some(callback) = &self.checkpoint_callback {
            callback(history)?;
        }
        Ok(())
    }

    pub fn with_secret_injection(mut self, enabled: bool) -> Self {
        self.secret_injection_enabled = enabled;
        self
    }

    pub fn with_capabilities(mut self, capabilities: AgentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    pub fn with_mutation_callback<F>(mut self, callback: F) -> Self
    where
        F: Fn(bool) -> Result<()> + Send + Sync + 'static,
    {
        self.mutation_callback = Some(Arc::new(callback));
        self
    }

    fn mark_mutation(&self, in_flight: bool) -> Result<()> {
        if let Some(cb) = &self.mutation_callback {
            cb(in_flight)?;
        }
        Ok(())
    }

    pub fn with_mcp_bridge(mut self, bridge: McpToolBridge) -> Self {
        self.mcp_bridge = Some(bridge);
        self
    }

    pub fn with_workspace_sandbox(mut self, sandbox: BotWorkspaceSandbox) -> Self {
        self.workspace_sandbox = Some(sandbox);
        self
    }

    pub fn with_vault_resolver(mut self, resolver: VaultSecretResolver) -> Self {
        self.vault_resolver = Some(resolver);
        self
    }

    pub fn with_secret_scrubber(mut self, scrubber: SecretScrubber) -> Self {
        self.secret_scrubber = scrubber;
        self
    }

    pub fn with_delegation_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&str, &str, &[String]) -> Result<String> + Send + Sync + 'static,
    {
        self.delegation_callback = Some(Arc::new(cb));
        self
    }

    pub fn with_audit(mut self, audit: Arc<AuditLogger>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn with_interaction_handler(mut self, handler: Arc<dyn InteractionHandler>) -> Self {
        self.interaction_handler = handler;
        self
    }

    pub fn with_progress_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(u32, &str) + Send + Sync + 'static,
    {
        self.progress_callback = Some(Arc::new(cb));
        self
    }

    pub async fn run_task(&self, task_id: &str, instruction: &str) -> Result<String> {
        if let Some(audit) = &self.audit {
            audit.log(
                "task_start",
                None,
                None,
                None,
                None,
                Some(instruction.to_string()),
                serde_json::json!({ "task_id": task_id, "instruction": instruction }),
            );
        }

        let mut kill_events = self.kill_switch.subscribe();
        let res = if self.kill_switch.is_triggered() {
            Err(GmError::SafetyViolation(
                "Execution stopped by Kill Switch".into(),
            ))
        } else {
            tokio::select! {
                biased;
                _ = kill_events.recv() => Err(GmError::SafetyViolation("Execution stopped by Kill Switch".into())),
                result = self.run_task_loop(task_id, instruction) => result,
            }
        };
        match &res {
            Ok(summary) => {
                if let Some(audit) = &self.audit {
                    audit.log(
                        "task_done",
                        None,
                        None,
                        None,
                        None,
                        Some(summary.clone()),
                        serde_json::json!({ "task_id": task_id, "instruction": instruction }),
                    );
                }
            }
            Err(e) => {
                if let Some(audit) = &self.audit {
                    audit.log(
                        "task_failed",
                        None,
                        None,
                        None,
                        None,
                        Some(format!("{e}")),
                        serde_json::json!({ "task_id": task_id, "instruction": instruction }),
                    );
                }
            }
        }
        res
    }

    async fn run_task_loop(&self, task_id: &str, instruction: &str) -> Result<String> {
        if self.capabilities.allow_gui {
            self.platform.begin_task(task_id).await?;
        }
        let recorder = if let Some(ref dir) = self.config.trajectory_dir {
            TrajectoryRecorder::with_directory(task_id, instruction, dir.clone())
        } else {
            TrajectoryRecorder::new(task_id, instruction)
        };

        let mut step = 0;
        let mut consecutive_no_change_retries = 0;
        let mut last_action_feedback: Option<String> = None;
        let mut step_history: Vec<StepRecord> = self.initial_history.clone();
        let mut prev_action_was_mutating = false;

        while step < self.config.max_steps {
            step += 1;
            let step_start = Instant::now();

            // 1. Kill switch check (<100ms requirement)
            if self.kill_switch.is_triggered() {
                warn!("Agent loop halted by kill switch!");
                if let Some(audit) = &self.audit {
                    audit.log(
                        "kill_switch",
                        None,
                        None,
                        None,
                        None,
                        Some("Execution stopped by Kill Switch".into()),
                        serde_json::json!({ "task_id": task_id, "step": step }),
                    );
                }
                self.platform.cancel().await?;
                return Err(GmError::SafetyViolation(
                    "Execution stopped by Kill Switch".into(),
                ));
            }

            // 2. Snapshot AX tree
            let raw_snapshot = if self.capabilities.allow_gui {
                self.platform.ax_snapshot(None).await?
            } else {
                // A workspace agent must never inspect the host screen.
                AxSnapshot {
                    window_id: 0,
                    app_name: String::new(),
                    window_title: String::new(),
                    bounds: gm_core::types::Rect::new(0.0, 0.0, 0.0, 0.0),
                    elements: Vec::new(),
                    timestamp_ms: 0,
                }
            };

            // 3. Blocklist check on frontmost window
            if self
                .blocklist
                .is_blocked(&raw_snapshot.app_name, &raw_snapshot.window_title)
            {
                if let Some(audit) = &self.audit {
                    audit.log(
                        "safety_intercept",
                        None,
                        None,
                        None,
                        None,
                        Some(format!(
                            "Active app '{}' is blocklisted for safety",
                            raw_snapshot.app_name
                        )),
                        serde_json::json!({
                            "blocklist": true,
                            "app": raw_snapshot.app_name,
                            "tier": "Irreversible"
                        }),
                    );
                }
                return Err(GmError::SafetyViolation(format!(
                    "Active app '{}' is blocklisted for safety",
                    raw_snapshot.app_name
                )));
            }

            // 4. Redaction of secure fields
            let sanitized_snapshot = sanitize_ax_snapshot(raw_snapshot);
            let current_ax_digest = compute_ax_digest(&sanitized_snapshot);

            // 5. Adaptive Vision Escalation Check
            let interactive_count = sanitized_snapshot
                .elements
                .iter()
                .filter(|el| is_interactive_element(el))
                .count();

            let is_sparse_ui = interactive_count < self.config.sparse_ax_threshold;
            let is_stagnated = prev_action_was_mutating
                && step_history
                    .last()
                    .map(|prev| prev.ax_digest == current_ax_digest)
                    .unwrap_or(false);

            let (vision_primary, vision_fallback) = self.router.route(TaskType::CuVisionStep);
            let vision_permitted = vision_primary.allow_vision
                || vision_fallback.is_some_and(|target| target.allow_vision);
            let should_escalate_vision = self.capabilities.allow_gui
                && vision_permitted
                && (self.config.always_include_vision || is_sparse_ui || is_stagnated);

            let task_type = if !self.capabilities.allow_gui {
                TaskType::Planning
            } else if should_escalate_vision {
                TaskType::CuVisionStep
            } else {
                TaskType::CuAxStep
            };

            // 6. Frame capture (required if vision escalated or capture_frames enabled)
            let frame_capture = if self.capabilities.allow_gui
                && (should_escalate_vision || self.config.capture_frames)
            {
                self.platform.capture(Some(1280), Some(70)).await.ok()
            } else {
                None
            };
            let resolved_frame_bytes: Option<Vec<u8>> = frame_capture
                .as_ref()
                .and_then(|fc| gm_platform::resolve_frame_bytes(fc).ok());

            // 7. Optional Set-of-Marks annotation when vision escalated
            let (som_annotation, active_som_marks) = if should_escalate_vision {
                let fallback_dims = (
                    sanitized_snapshot.bounds.width.max(1280.0) as u32,
                    sanitized_snapshot.bounds.height.max(800.0) as u32,
                );
                let annotation = annotate_frame_with_som(
                    resolved_frame_bytes.as_deref(),
                    fallback_dims,
                    &sanitized_snapshot,
                )?;
                let marks = annotation.marks.clone();
                (Some(annotation), marks)
            } else {
                (None, Vec::new())
            };

            // 8. Wrap screen text as untrusted data
            let snapshot_json = serde_json::to_string(&sanitized_snapshot)?;
            let untrusted = UntrustedData::new(snapshot_json, "screen_ax_tree");
            let untrusted_block = format_untrusted_context(untrusted.inner(), untrusted.source());

            // 9. Format Recent Step History block
            let mut history_block = String::new();
            if !step_history.is_empty() {
                history_block.push_str("\n\n## Recent Step History\n");
                for rec in &step_history {
                    history_block.push_str(&format!(
                        "- Step {}: {} -> {}\n",
                        rec.step,
                        rec.action_summary.chars().take(500).collect::<String>(),
                        rec.outcome.chars().take(8000).collect::<String>()
                    ));
                }
            }

            // 10. Assemble prompt and messages
            let feedback_block = if let Some(ref fb) = last_action_feedback {
                format!("\n\n{fb}")
            } else {
                String::new()
            };

            let stagnation_block = if is_stagnated {
                "\n\n[STAGNATION DETECTED] The previous action did not change the UI state. Use visual Set-of-Marks tags (#1, #2, ...) or visual coordinates to take a different action."
            } else {
                ""
            };

            let som_legend_block = if let Some(ref som) = som_annotation {
                format!("\n\n{}", som.legend_text)
            } else {
                String::new()
            };

            let mcp_tools_block = if let Some(ref bridge) = self.mcp_bridge {
                if self.capabilities.allow_network {
                    bridge.format_tools_prompt()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let history_block = format_untrusted_context(&history_block, "observed_tool_history");
            let prompt_text = format!(
                "Goal: {instruction}\n\nCurrent window state:\n{untrusted_block}{history_block}{feedback_block}{stagnation_block}{som_legend_block}{mcp_tools_block}\n\nPick the next action in JSON format."
            );

            let mut messages = match &som_annotation {
                Some(som) => vec![
                    ChatMessage::text(
                        "system",
                        "You are an autonomous computer-use agent with vision. Output JSON with fields 'action' and 'params'. You can reference interactive elements by their Set-of-Marks badge (#1, #2, ...) or accessibility token.",
                    ),
                    ChatMessage::with_jpeg("user", prompt_text, &som.jpeg_bytes),
                ],
                None => vec![
                    ChatMessage::text(
                        "system",
                        "You are an autonomous computer-use agent. Output JSON with fields 'action' and 'params'.",
                    ),
                    ChatMessage::text("user", prompt_text),
                ],
            };

            let tool_contract = format!(
                "{}\nCapabilities: GUI={}, shell={}, network={}. Workspace calls use {{\"action\":\"WorkspaceReadFile\",\"params\":{{\"path\":\"relative/path\"}}}}; also WorkspaceWriteFile (path, content), WorkspaceListFiles (path), WorkspaceShell (command, timeout_ms). Completion must use {{\"action\":\"Done\",\"params\":{{\"summary\":\"result and evidence\"}}}}. Never claim an action succeeded without its observed result.",
                self.system_prompt,
                self.capabilities.allow_gui,
                self.capabilities.allow_shell,
                self.capabilities.allow_network
            );
            messages[0].content.push_str(&format!("\n{tool_contract}"));

            // 11. Route and execute LLM step
            let (primary, fallback) = self.router.route(task_type);
            let response = self
                .llm
                .execute_routed(&primary, fallback.as_ref(), &messages)
                .await?;

            if let Some(audit) = &self.audit {
                let total_tokens = (response.prompt_tokens + response.completion_tokens) as u32;
                audit.log(
                    "model_call",
                    Some(format!("{:?}", primary.provider).to_lowercase()),
                    Some(primary.model.clone()),
                    Some(total_tokens),
                    Some(response.estimated_cost_usd),
                    None,
                    serde_json::json!({
                        "task_id": task_id,
                        "step": step,
                        "task_type": format!("{task_type:?}"),
                        "vision_escalated": should_escalate_vision,
                    }),
                );
            }

            // 12a. Check for hybrid MCP tool invocation first when MCP bridge is attached
            if let Some(ref bridge) = self.mcp_bridge
                && let Some(mcp_inv) = try_parse_mcp_tool_call(&response.content)
            {
                if !self.capabilities.allow_network {
                    return Err(GmError::PermissionDenied(
                        "MCP/network access is disabled for this bot".into(),
                    ));
                }
                self.mark_mutation(true)?;
                let mcp_summary = format!("McpTool({})", mcp_inv.name);
                if let Some(ref cb) = self.progress_callback {
                    cb(step, &mcp_summary);
                }
                let outcome = bridge
                    .execute_governed(
                        task_id,
                        step,
                        &mcp_inv,
                        &self.interaction_handler,
                        self.audit.as_ref(),
                    )
                    .await?;
                self.mark_mutation(false)?;
                step_history.push(StepRecord {
                    step,
                    action_summary: mcp_summary,
                    outcome,
                    ax_digest: current_ax_digest,
                });
                if step_history.len() > self.config.max_history_steps {
                    step_history.remove(0);
                }
                self.checkpoint(&step_history)?;
                prev_action_was_mutating = false;
                last_action_feedback = None;
                continue;
            }

            // 12a-openbot. Check for OpenBot step actions (WorkspaceTool, FillSecret, TakeTheWheel, DelegateToBot)
            if let Some(ob_action) = try_parse_openbot_step_action(&response.content) {
                match ob_action {
                    OpenBotStepAction::WorkspaceTool(ref ws_call) => {
                        if matches!(
                            ws_call,
                            crate::workspace_tools::WorkspaceToolCall::Shell { .. }
                        ) && !self.capabilities.allow_shell
                        {
                            return Err(GmError::PermissionDenied(
                                "Shell execution is disabled for this bot".into(),
                            ));
                        }
                        if let Some(ref sandbox) = self.workspace_sandbox {
                            let mutating = matches!(
                                ws_call,
                                crate::workspace_tools::WorkspaceToolCall::WriteFile { .. }
                                    | crate::workspace_tools::WorkspaceToolCall::Shell { .. }
                            );
                            if mutating {
                                self.mark_mutation(true)?;
                            }
                            let ws_summary = match ws_call {
                                crate::workspace_tools::WorkspaceToolCall::ReadFile { path } => {
                                    format!("ReadFile({path})")
                                }
                                crate::workspace_tools::WorkspaceToolCall::WriteFile {
                                    path,
                                    content,
                                } => format!("WriteFile({path}, {} bytes)", content.len()),
                                crate::workspace_tools::WorkspaceToolCall::ListFiles { path } => {
                                    format!("ListFiles({path:?})")
                                }
                                crate::workspace_tools::WorkspaceToolCall::Shell {
                                    command,
                                    ..
                                } => format!(
                                    "Shell({})",
                                    self.secret_scrubber
                                        .scrub_str(command)
                                        .chars()
                                        .take(300)
                                        .collect::<String>()
                                ),
                            };
                            if let Some(ref cb) = self.progress_callback {
                                cb(step, &ws_summary);
                            }
                            let ws_outcome = sandbox
                                .execute_tool(
                                    task_id,
                                    step,
                                    ws_call,
                                    &self.interaction_handler,
                                    &self.kill_switch,
                                    self.audit.as_ref(),
                                )
                                .await?;
                            if mutating {
                                self.mark_mutation(false)?;
                            }
                            step_history.push(StepRecord {
                                step,
                                action_summary: ws_outcome.summary,
                                outcome: ws_outcome.output,
                                ax_digest: current_ax_digest,
                            });
                            if step_history.len() > self.config.max_history_steps {
                                step_history.remove(0);
                            }
                            self.checkpoint(&step_history)?;
                            prev_action_was_mutating = false;
                            last_action_feedback = None;
                            continue;
                        }
                    }
                    OpenBotStepAction::FillSecret {
                        ref token,
                        ref secret_ref,
                    } => {
                        if !self.capabilities.allow_gui {
                            return Err(GmError::PermissionDenied(
                                "Secret injection requires desktop access".into(),
                            ));
                        }
                        if !self.secret_injection_enabled {
                            return Err(GmError::PermissionDenied("Credential injection is disabled; use human handoff to enter credentials".into()));
                        }
                        let approval = ApprovalRequest {
                            task_id: task_id.into(),
                            step,
                            action: ComputerAction::Type {
                                text: format!("[credential: {secret_ref}]"),
                            },
                            target_label: token.clone(),
                            reason: "Inject this credential into the selected desktop field".into(),
                        };
                        if !self.interaction_handler.request_approval(&approval).await? {
                            return Err(GmError::PermissionDenied(
                                "Credential injection was not approved".into(),
                            ));
                        }
                        self.mark_mutation(true)?;
                        let resolver = self.vault_resolver.clone().unwrap_or_default();
                        let lease_start = Instant::now();
                        let leased = resolver.resolve(secret_ref)?;
                        self.secret_scrubber
                            .register(secret_ref.clone(), leased.value.clone());
                        if let Some(tok) = token {
                            self.platform
                                .act(&ComputerAction::Click {
                                    token: Some(tok.clone()),
                                    coords: None,
                                    button: None,
                                    click_count: None,
                                })
                                .await?;
                        }
                        self.platform
                            .act(&ComputerAction::Type {
                                text: leased.value.clone(),
                            })
                            .await?;
                        self.mark_mutation(false)?;
                        let lease_ms = lease_start.elapsed().as_millis() as u64;
                        let summary = format!("FillSecret({secret_ref})");
                        if let Some(ref cb) = self.progress_callback {
                            cb(step, &summary);
                        }
                        if let Some(ref logger) = self.audit {
                            logger.log_governance(
                                "secret_lease",
                                self.workspace_sandbox.as_ref().map(|s| s.bot_id()),
                                AuditCategory::Permitted,
                                "RULE-SEC-006",
                                "Opaque vault credential leased and injected without plaintext exposure",
                                summary.clone(),
                                serde_json::json!({
                                    "task_id": task_id,
                                    "step": step,
                                    "secret_ref": secret_ref,
                                    "lease_ms": lease_ms,
                                    "value": "[REDACTED]",
                                }),
                            );
                        }
                        step_history.push(StepRecord {
                            step,
                            action_summary: summary,
                            outcome: format!(
                                "Injected opaque credential '{secret_ref}' ({lease_ms}ms)"
                            ),
                            ax_digest: current_ax_digest,
                        });
                        if step_history.len() > self.config.max_history_steps {
                            step_history.remove(0);
                        }
                        self.checkpoint(&step_history)?;
                        prev_action_was_mutating = true;
                        last_action_feedback = None;
                        continue;
                    }
                    OpenBotStepAction::TakeTheWheel {
                        ref blocker,
                        ref instructions,
                    } => {
                        let summary = format!("TakeTheWheel(\"{blocker}\")");
                        if let Some(ref cb) = self.progress_callback {
                            cb(step, &summary);
                        }
                        let req = WheelHandoffRequest {
                            task_id: task_id.to_string(),
                            bot_id: self
                                .workspace_sandbox
                                .as_ref()
                                .map(|s| s.bot_id().to_string()),
                            step,
                            blocker: blocker.clone(),
                            instructions: instructions.clone(),
                        };
                        if let Some(ref logger) = self.audit {
                            logger.log_governance(
                                "wheel_handoff_requested",
                                req.bot_id.as_deref(),
                                AuditCategory::Permitted,
                                "RULE-STD-000",
                                "Bot paused for human-in-the-loop 'Take the Wheel' intervention",
                                summary.clone(),
                                serde_json::json!({
                                    "task_id": task_id,
                                    "step": step,
                                    "blocker": blocker,
                                }),
                            );
                        }
                        match self.interaction_handler.request_wheel_handoff(&req).await? {
                            Some(operator_note) => {
                                if let Some(ref logger) = self.audit {
                                    logger.log_governance(
                                        "wheel_handoff_resumed",
                                        req.bot_id.as_deref(),
                                        AuditCategory::Permitted,
                                        "RULE-STD-000",
                                        "Human operator handed control back to bot",
                                        format!("WheelResumed(\"{operator_note}\")"),
                                        serde_json::json!({
                                            "task_id": task_id,
                                            "step": step,
                                            "note": operator_note,
                                        }),
                                    );
                                }
                                step_history.push(StepRecord {
                                    step,
                                    action_summary: summary,
                                    outcome: format!(
                                        "Human operator resolved blocker: \"{operator_note}\""
                                    ),
                                    ax_digest: current_ax_digest,
                                });
                                if step_history.len() > self.config.max_history_steps {
                                    step_history.remove(0);
                                }
                                prev_action_was_mutating = false;
                                last_action_feedback = None;
                                continue;
                            }
                            None => {
                                return Ok(format!(
                                    "[WAITING_FOR_HUMAN] {blocker}: {instructions}"
                                ));
                            }
                        }
                    }
                    OpenBotStepAction::DelegateToBot {
                        ref target_bot_id,
                        ref goal,
                        ref artifacts,
                    } => {
                        let summary = format!("DelegateToBot({target_bot_id}, \"{goal}\")");
                        if let Some(ref cb) = self.progress_callback {
                            cb(step, &summary);
                        }
                        self.mark_mutation(true)?;
                        let outcome = if let Some(ref del_cb) = self.delegation_callback {
                            del_cb(target_bot_id, goal, artifacts)?
                        } else {
                            return Err(GmError::Config(
                                "Delegation is unavailable in this session".into(),
                            ));
                        };
                        step_history.push(StepRecord {
                            step,
                            action_summary: summary,
                            outcome,
                            ax_digest: current_ax_digest,
                        });
                        self.checkpoint(&step_history)?;
                        self.mark_mutation(false)?;
                        if step_history.len() > self.config.max_history_steps {
                            step_history.remove(0);
                        }
                        self.checkpoint(&step_history)?;
                        prev_action_was_mutating = false;
                        last_action_feedback = None;
                        continue;
                    }
                }
            }

            // 12b. Parse GUI action and resolve Set-of-Marks badge references
            let mut action: ComputerAction = match serde_json::from_str(&response.content) {
                Ok(a) => a,
                Err(e) => {
                    last_action_feedback = Some(format!(
                        "Invalid action JSON: {e}. Return a supported action; this response did not execute."
                    ));
                    continue;
                }
            };

            if !self.capabilities.allow_gui
                && !matches!(
                    action,
                    ComputerAction::Done { .. } | ComputerAction::AskUser { .. }
                )
            {
                return Err(GmError::PermissionDenied(
                    "Host desktop access is disabled for this bot".into(),
                ));
            }

            // Resolve SoM badge tokens (e.g. "#1", "1", "som_1") to ax_token and coordinates
            if let ComputerAction::Click {
                ref mut token,
                ref mut coords,
                ..
            } = action
                && let Some(tok_str) = token.clone()
                && let Some(mark) = resolve_som_badge(&tok_str, &active_som_marks)
            {
                *token = Some(mark.ax_token.clone());
                if coords.is_none() {
                    *coords = Some((mark.center.0 as f64, mark.center.1 as f64));
                }
            }

            // Emit live progress callback if configured
            let action_summary = format_action_summary(&action);
            if let Some(ref cb) = self.progress_callback {
                cb(step, &action_summary);
            }

            // 13. Blocklist check on action target app
            match &action {
                ComputerAction::FocusApp { app_name }
                    if self.blocklist.is_blocked(app_name, "") =>
                {
                    return Err(GmError::SafetyViolation(format!(
                        "Attempted to focus blocklisted application: '{app_name}'"
                    )));
                }
                ComputerAction::LaunchApp { bundle_id }
                    if self.blocklist.is_blocked(bundle_id, "") =>
                {
                    return Err(GmError::SafetyViolation(format!(
                        "Attempted to launch blocklisted application: '{bundle_id}'"
                    )));
                }
                _ => {}
            }

            // 14. Resolve target label from snapshot for safety classification
            let target_label: Option<String> = match &action {
                ComputerAction::Click { token, coords, .. } => {
                    let mut found = None;
                    if let Some(tok) = token {
                        if let Some(el) =
                            sanitized_snapshot.elements.iter().find(|e| &e.token == tok)
                        {
                            found = el.label.clone().or_else(|| Some(el.role.clone()));
                        } else {
                            found = Some(tok.clone());
                        }
                    } else if let Some((x, y)) = coords
                        && let Some(el) = sanitized_snapshot
                            .elements
                            .iter()
                            .find(|e| e.frame.contains(*x, *y))
                    {
                        found = el.label.clone().or_else(|| Some(el.role.clone()));
                    }
                    found
                }
                _ => None,
            };

            // 15. Safety tier check & interactive approval
            let tier = ActionTier::classify(&action, target_label.as_deref());
            if tier.requires_confirmation() {
                info!(
                    "Action {:?} requires explicit user approval (Tier: {:?})",
                    action, tier
                );
                let approval_req = ApprovalRequest {
                    task_id: task_id.to_string(),
                    step,
                    action: action.clone(),
                    target_label: target_label.clone(),
                    reason: format!(
                        "Irreversible action '{:?}' requires user confirmation",
                        action
                    ),
                };

                let approved = self
                    .interaction_handler
                    .request_approval(&approval_req)
                    .await?;

                if approved {
                    let current = sanitize_ax_snapshot(self.platform.ax_snapshot(None).await?);
                    if current.window_id != sanitized_snapshot.window_id
                        || compute_ax_digest(&current) != current_ax_digest
                    {
                        return Err(GmError::SafetyViolation(
                            "The target changed while approval was pending; request approval again"
                                .into(),
                        ));
                    }
                    info!("Action {:?} approved by interaction handler", action);
                    if let Some(audit) = &self.audit {
                        audit.log(
                            "action_approved",
                            None,
                            None,
                            None,
                            None,
                            Some(format!("User approved action {:?}", action)),
                            serde_json::json!({
                                "task_id": task_id,
                                "step": step,
                                "action": action_type_name(&action),
                            }),
                        );
                    }
                } else {
                    warn!("Action {:?} denied by interaction handler", action);
                    if let Some(audit) = &self.audit {
                        audit.log(
                            "action_denied",
                            None,
                            None,
                            None,
                            None,
                            Some(format!(
                                "Irreversible action '{action:?}' requires user confirmation"
                            )),
                            serde_json::json!({
                                "action_type": action_type_name(&action),
                                "tier": "Irreversible",
                                "blocklist": false,
                            }),
                        );
                    }
                    return Err(GmError::SafetyViolation(format!(
                        "Irreversible action '{action:?}' requires user confirmation and was denied by user"
                    )));
                }
            }

            // 16. User coexistence check:
            if action.is_cgevent_action() {
                let mut activity = self.platform.user_activity().await?;
                if activity.seconds_since_last_event < self.config.user_active_pause_seconds {
                    info!(
                        "User input active ({:.2}s ago); pausing CGEvent action {:?}",
                        activity.seconds_since_last_event, action
                    );
                    while activity.seconds_since_last_event < self.config.user_active_pause_seconds
                    {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        if self.kill_switch.is_triggered() {
                            return Err(GmError::SafetyViolation(
                                "Execution stopped by Kill Switch".into(),
                            ));
                        }
                        activity = self.platform.user_activity().await?;
                    }
                }
            } else if action.is_ax_action() {
                info!(
                    "Background AX action ({:?}) executing without user-activity pause",
                    action
                );
            }

            // 17. Dispatch action or handle terminal / AskUser actions
            match &action {
                ComputerAction::Done { summary } => {
                    let duration_ms = step_start.elapsed().as_millis() as u64;
                    recorder.record_step(
                        if self.config.capture_frames {
                            frame_capture.clone()
                        } else {
                            None
                        },
                        Some(sanitized_snapshot),
                        action.clone(),
                        None,
                        response.content.clone(),
                        duration_ms,
                    );
                    recorder.finish(Some(summary.clone()));
                    return Ok(summary.clone());
                }
                ComputerAction::AskUser { question } => {
                    match self.interaction_handler.ask_user(question).await? {
                        Some(answer) => {
                            info!("AskUser answered: \"{answer}\" -> continuing loop");
                            let rec = StepRecord {
                                step,
                                action_summary: format!("AskUser(\"{question}\")"),
                                outcome: format!("User replied: \"{answer}\""),
                                ax_digest: current_ax_digest,
                            };
                            step_history.push(rec);
                            if step_history.len() > self.config.max_history_steps {
                                step_history.remove(0);
                            }
                            prev_action_was_mutating = false;
                            continue;
                        }
                        None => {
                            let duration_ms = step_start.elapsed().as_millis() as u64;
                            recorder.record_step(
                                if self.config.capture_frames {
                                    frame_capture.clone()
                                } else {
                                    None
                                },
                                Some(sanitized_snapshot),
                                action.clone(),
                                None,
                                response.content.clone(),
                                duration_ms,
                            );
                            recorder.finish(Some(format!("Asked user: {question}")));
                            return Ok(format!("[WAITING_FOR_HUMAN] {question}"));
                        }
                    }
                }
                other => {
                    if let Some(audit) = &self.audit {
                        audit.log(
                            "cu_action",
                            None,
                            None,
                            None,
                            None,
                            Some(format!("{other:?}")),
                            serde_json::json!({
                                "action_type": action_type_name(other),
                                "tier": format!("{tier:?}"),
                            }),
                        );
                    }
                    self.mark_mutation(true)?;
                    self.platform.act(other).await?;
                    self.mark_mutation(false)?;

                    // 18. Verify-after-act: capture post-action snapshot and compute diff
                    let raw_post = self.platform.ax_snapshot(None).await?;
                    let sanitized_post = sanitize_ax_snapshot(raw_post);
                    let diff = compute_ax_diff(&sanitized_snapshot, &sanitized_post);

                    let duration_ms = step_start.elapsed().as_millis() as u64;
                    recorder.record_step(
                        if self.config.capture_frames {
                            frame_capture.clone()
                        } else {
                            None
                        },
                        Some(sanitized_snapshot.clone()),
                        action.clone(),
                        Some(diff.clone()),
                        response.content.clone(),
                        duration_ms,
                    );

                    let outcome = if diff.is_empty() {
                        "No observable UI change".to_string()
                    } else {
                        format!(
                            "UI changed (+{} added, -{} removed, ~{} modified)",
                            diff.added_tokens.len(),
                            diff.removed_tokens.len(),
                            diff.changed_elements.len()
                        )
                    };

                    let rec = StepRecord {
                        step,
                        action_summary: action_summary.clone(),
                        outcome,
                        ax_digest: current_ax_digest,
                    };
                    step_history.push(rec);
                    self.checkpoint(&step_history)?;
                    if step_history.len() > self.config.max_history_steps {
                        step_history.remove(0);
                    }
                    prev_action_was_mutating = action.modifies_ui_state();

                    // Check if UI state was expected to change but diff was empty
                    if action.modifies_ui_state() && diff.is_empty() {
                        consecutive_no_change_retries += 1;
                        last_action_feedback = Some(format!(
                            "Warning: Previous action {:?} produced no observable UI change (retry {}/{}). Try a different element or approach.",
                            action, consecutive_no_change_retries, self.config.max_action_retries
                        ));
                        warn!(
                            "Verify-after-act: action {:?} produced no UI changes (retry {}/{})",
                            action, consecutive_no_change_retries, self.config.max_action_retries
                        );

                        if consecutive_no_change_retries >= self.config.max_action_retries {
                            let question = format!(
                                "The action {:?} produced no observable UI changes after {} attempts. How should I proceed?",
                                action, consecutive_no_change_retries
                            );
                            info!("Retry bounds exceeded; falling back to ask_user: {question}");
                            recorder.finish(Some(format!("Fallback to ask_user: {question}")));
                            return Ok(question);
                        }
                    } else {
                        consecutive_no_change_retries = 0;
                        last_action_feedback = None;
                    }
                }
            }
        }

        Err(GmError::Timeout(format!(
            "Agent loop reached max steps ({}) without completing",
            self.config.max_steps
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::CallbackInteractionHandler;
    use gm_core::config::{BudgetConfig, ProvidersConfig};
    use gm_core::types::{AxElement, Rect, UserActivity};
    use gm_llm::{BudgetManager, MockLlmBackend};
    use gm_platform::MockPlatform;

    #[tokio::test]
    async fn test_agent_loop_happy_path() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend.set_response(
            "{\"action\":\"Done\",\"params\":{\"summary\":\"Task finished successfully\"}}",
        );

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        });

        let res = agent
            .run_task("test_1", "Open Notes and write a poem")
            .await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "Task finished successfully");
    }

    #[tokio::test]
    async fn test_agent_loop_kill_switch() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        kill_switch.trigger();

        let blocklist = BlocklistManager::default();
        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        });

        let res = agent.run_task("test_ks", "Run something dangerous").await;
        assert!(res.is_err());
        match res.unwrap_err() {
            GmError::SafetyViolation(msg) => assert!(msg.contains("Kill Switch")),
            other => panic!("Expected SafetyViolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_agent_loop_irreversible_action_interception() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend.set_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"send_btn\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        });

        let res = agent.run_task("test_send", "Send important email").await;
        assert!(res.is_err());
        match res.unwrap_err() {
            GmError::SafetyViolation(msg) => assert!(msg.contains("user confirmation")),
            other => panic!("Expected SafetyViolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_agent_loop_blocked_app_focus() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend
            .set_response("{\"action\":\"FocusApp\",\"params\":{\"app_name\":\"1Password\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        });

        let res = agent.run_task("test_focus", "Open password vault").await;
        assert!(res.is_err());
        match res.unwrap_err() {
            GmError::SafetyViolation(msg) => assert!(msg.contains("blocklisted application")),
            other => panic!("Expected SafetyViolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_user_coexistence_ax_allowed_and_cgevent_paused() {
        let platform = Arc::new(MockPlatform::new());
        platform.set_user_activity(UserActivity {
            seconds_since_last_event: 0.2,
            is_user_active: true,
        });

        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend.queue_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"btn_1\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );
        mock_backend.queue_response("{\"action\":\"Done\",\"params\":{\"summary\":\"Completed\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend.clone(), budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let config = AgentLoopConfig {
            max_steps: 3,
            user_active_pause_seconds: 1.0,
            max_action_retries: 2,
            trajectory_dir: None,
            capture_frames: false,
            ..Default::default()
        };

        let agent = AgentLoop::new(
            platform.clone(),
            llm,
            router,
            kill_switch,
            blocklist,
            config,
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        });

        let res = agent.run_task("test_coexistence", "Click button").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "Completed");

        let actions = platform.get_executed_actions();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].is_ax_action());
    }

    #[tokio::test]
    async fn test_verify_after_act_retry_bounds_with_ask_user_fallback() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend.set_response("{\"action\":\"Type\",\"params\":{\"text\":\"test text\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let config = AgentLoopConfig {
            max_steps: 10,
            user_active_pause_seconds: 0.1,
            max_action_retries: 2,
            trajectory_dir: None,
            capture_frames: false,
            ..Default::default()
        };

        let agent = AgentLoop::new(platform, llm, router, kill_switch, blocklist, config)
            .with_capabilities(AgentCapabilities {
                allow_gui: true,
                allow_shell: false,
                allow_network: true,
            });

        let res = agent.run_task("test_retry_bound", "Type something").await;
        assert!(res.is_ok());
        let msg = res.unwrap();
        assert!(msg.contains("produced no observable UI changes"));
        assert!(msg.contains("How should I proceed?"));
    }

    #[tokio::test]
    async fn test_agent_loop_with_audit_logging() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend.set_response(
            "{\"action\":\"Done\",\"params\":{\"summary\":\"Task finished successfully\"}}",
        );

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();
        let audit = Arc::new(AuditLogger::in_memory());

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        })
        .with_audit(audit.clone());

        let res = agent.run_task("test_audit_task", "Test instruction").await;
        assert!(res.is_ok());

        let entries = audit.entries();
        assert!(entries.iter().any(|e| e.event_type == "task_start"));
        assert!(entries.iter().any(|e| e.event_type == "model_call"));
        assert!(entries.iter().any(|e| e.event_type == "task_done"));
    }

    #[tokio::test]
    async fn test_som_badge_resolution_in_agent_loop() {
        let platform = Arc::new(MockPlatform::new());
        // Configure snapshot with 2 interactive elements
        let snapshot = AxSnapshot {
            window_id: 1,
            app_name: "TestApp".into(),
            window_title: "Window".into(),
            bounds: Rect::new(0.0, 0.0, 1000.0, 800.0),
            elements: vec![
                AxElement {
                    token: "submit_btn_real".into(),
                    role: "AXButton".into(),
                    label: Some("Submit".into()),
                    value: None,
                    frame: Rect::new(100.0, 200.0, 80.0, 30.0),
                    is_enabled: true,
                    is_secure: false,
                },
                AxElement {
                    token: "cancel_btn_real".into(),
                    role: "AXButton".into(),
                    label: Some("Cancel".into()),
                    value: None,
                    frame: Rect::new(200.0, 200.0, 80.0, 30.0),
                    is_enabled: true,
                    is_secure: false,
                },
            ],
            timestamp_ms: 10,
        };
        platform.set_snapshot(snapshot);

        let mock_backend = Arc::new(MockLlmBackend::new());
        // Model refers to badge #1
        mock_backend.queue_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"#1\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );
        mock_backend.queue_response("{\"action\":\"Done\",\"params\":{\"summary\":\"Done\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform.clone(),
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig {
                sparse_ax_threshold: 5, // 2 elements < 5 threshold triggers SoM vision!
                ..Default::default()
            },
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        });

        let res = agent.run_task("test_som_res", "Click submit badge").await;
        assert!(res.is_ok());

        let executed = platform.get_executed_actions();
        assert_eq!(executed.len(), 1);
        if let ComputerAction::Click { token, coords, .. } = &executed[0] {
            assert_eq!(token.as_deref(), Some("submit_btn_real"));
            assert_eq!(*coords, Some((140.0, 215.0))); // Center of badge #1
        } else {
            panic!("Expected click action");
        }
    }

    #[tokio::test]
    async fn test_stagnation_detection_triggers_vision_escalation() {
        let platform = Arc::new(MockPlatform::new());
        // Populate snapshot with 10 elements so it does NOT trigger sparse UI
        let mut elements = Vec::new();
        for i in 1..=10 {
            elements.push(AxElement {
                token: format!("el_{i}"),
                role: "AXButton".into(),
                label: Some(format!("Button {i}")),
                value: None,
                frame: Rect::new(10.0 * i as f64, 10.0, 50.0, 30.0),
                is_enabled: true,
                is_secure: false,
            });
        }
        let snapshot = AxSnapshot {
            window_id: 1,
            app_name: "App".into(),
            window_title: "Title".into(),
            bounds: Rect::new(0.0, 0.0, 1000.0, 800.0),
            elements,
            timestamp_ms: 100,
        };
        platform.set_snapshot(snapshot);

        let mock_backend = Arc::new(MockLlmBackend::new());
        // Step 1: mutating action (Scroll) that fails to change the UI snapshot
        mock_backend.queue_response("{\"action\":\"Scroll\",\"params\":{\"dx\":0,\"dy\":-100}}");
        // Step 2: Done
        mock_backend.queue_response(
            "{\"action\":\"Done\",\"params\":{\"summary\":\"Escalated and done\"}}",
        );

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend.clone(), budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig {
                sparse_ax_threshold: 2, // 10 elements > 2 -> starts as CuAxStep
                ..Default::default()
            },
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        });

        let res = agent
            .run_task("test_stagnation", "Test stagnation vision")
            .await;
        assert!(res.is_ok());

        // In step 2, stagnation was detected, causing CuVisionStep and receiving 1 image!
        assert_eq!(mock_backend.received_images(), 1);
    }

    #[tokio::test]
    async fn test_interactive_approval_flow_approved() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        // Step 1: Irreversible click action
        mock_backend.queue_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"send_btn\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );
        // Step 2: Done
        mock_backend
            .queue_response("{\"action\":\"Done\",\"params\":{\"summary\":\"Email sent\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();
        let audit = Arc::new(AuditLogger::in_memory());

        let interactive_handler = Arc::new(CallbackInteractionHandler::with_approval_always(true));

        let agent = AgentLoop::new(
            platform.clone(),
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        })
        .with_audit(audit.clone())
        .with_interaction_handler(interactive_handler);

        let res = agent.run_task("test_approve", "Send wire transfer").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "Email sent");

        // Action was actually executed on the platform!
        let actions = platform.get_executed_actions();
        assert_eq!(actions.len(), 1);

        // Audit log records action_approved
        let entries = audit.entries();
        assert!(entries.iter().any(|e| e.event_type == "action_approved"));
    }

    #[tokio::test]
    async fn test_interactive_approval_flow_denied() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        // Irreversible click action
        mock_backend.set_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"send_btn\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();
        let audit = Arc::new(AuditLogger::in_memory());

        let interactive_handler = Arc::new(CallbackInteractionHandler::with_approval_always(false));

        let agent = AgentLoop::new(
            platform.clone(),
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        })
        .with_audit(audit.clone())
        .with_interaction_handler(interactive_handler);

        let res = agent.run_task("test_deny", "Send wire transfer").await;
        assert!(res.is_err());
        match res.unwrap_err() {
            GmError::SafetyViolation(msg) => {
                assert!(msg.contains("user confirmation"));
                assert!(msg.contains("denied"));
            }
            other => panic!("Expected SafetyViolation, got {other:?}"),
        }

        // Action was NOT executed on the platform!
        let actions = platform.get_executed_actions();
        assert_eq!(actions.len(), 0);

        // Audit log records action_denied
        let entries = audit.entries();
        assert!(entries.iter().any(|e| e.event_type == "action_denied"));
    }

    #[tokio::test]
    async fn test_interactive_ask_user_continuation() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        // Step 1: Model asks user for clarification
        mock_backend.queue_response(
            "{\"action\":\"AskUser\",\"params\":{\"question\":\"Which account number?\"}}",
        );
        // Step 2: Model finishes
        mock_backend.queue_response(
            "{\"action\":\"Done\",\"params\":{\"summary\":\"Processed account 1234\"}}",
        );

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let handler = Arc::new(
            CallbackInteractionHandler::new().with_ask_user_callback(|_q| Some("1234".to_string())),
        );

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        })
        .with_interaction_handler(handler);

        let res = agent.run_task("test_ask_continue", "Process payment").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "Processed account 1234");
    }

    #[tokio::test]
    async fn test_step_progress_callback() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        mock_backend.queue_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"btn_1\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );
        mock_backend.queue_response("{\"action\":\"Done\",\"params\":{\"summary\":\"All done\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let progress_steps = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let progress_steps_clone = Arc::clone(&progress_steps);

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        })
        .with_progress_callback(move |step, action_summary| {
            progress_steps_clone
                .lock()
                .push((step, action_summary.to_string()));
        });

        let res = agent.run_task("test_progress", "Click button").await;
        assert!(res.is_ok());

        let recorded = progress_steps.lock();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].0, 1);
        assert!(recorded[0].1.contains("Click"));
        assert_eq!(recorded[1].0, 2);
        assert!(recorded[1].1.contains("Done"));
    }

    #[tokio::test]
    async fn test_agent_loop_openbot_workspace_secret_and_wheel() {
        let dir = std::env::temp_dir().join(format!("agent_ob_test_{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("OB_LOOP_SECRET_TEST", "ultra_secret_password_777");
        }
        let scrubber = SecretScrubber::new();
        let sandbox = BotWorkspaceSandbox::new("coder", &dir)
            .unwrap()
            .with_scrubber(scrubber.clone());
        let resolver = VaultSecretResolver::new(gm_llm::KeychainSecrets::new(
            "co.anysphere.grok-muse.ob-loop-test",
        ));

        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        // Step 1: Write file in workspace
        mock_backend.queue_response(
            r#"{"action":"WorkspaceWriteFile","params":{"path":"notes.txt","content":"hello openbot\n"}}"#,
        );
        // Step 2: Lease & inject opaque secret
        mock_backend.queue_response(
            r#"{"action":"FillSecret","params":{"token":"btn_1","secret_ref":"secret://keychain/ob_loop_secret_test"}}"#,
        );
        // Step 3: Hit 2FA wall -> TakeTheWheel
        mock_backend.queue_response(
            r#"{"action":"TakeTheWheel","params":{"blocker":"2FA code needed","instructions":"Enter SMS code"}}"#,
        );
        // Step 4: Done
        mock_backend.queue_response(
            r#"{"action":"Done","params":{"summary":"Completed after human wheel handoff"}}"#,
        );

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig {
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
            ..Default::default()
        });
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();
        let audit = Arc::new(AuditLogger::in_memory());
        let handler = Arc::new(
            crate::approval::CallbackInteractionHandler::with_approval_always(true)
                .with_wheel_callback(|req| Some(format!("Operator solved {}", req.blocker))),
        );

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_capabilities(AgentCapabilities {
            allow_gui: true,
            allow_shell: false,
            allow_network: true,
        })
        .with_secret_injection(true)
        .with_workspace_sandbox(sandbox.clone())
        .with_vault_resolver(resolver)
        .with_secret_scrubber(scrubber.clone())
        .with_audit(Arc::clone(&audit))
        .with_interaction_handler(handler);

        let result = agent
            .run_task("ob_task_1", "Sign in and save notes")
            .await
            .unwrap();
        assert_eq!(result, "Completed after human wheel handoff");
        assert!(sandbox.workspace_dir().join("notes.txt").exists());

        // Verify audit contains zero plaintext secret leaks!
        let entries_json = serde_json::to_string(&audit.entries()).unwrap();
        assert!(!entries_json.contains("ultra_secret_password_777"));
        assert!(entries_json.contains("secret://keychain/ob_loop_secret_test"));

        unsafe {
            std::env::remove_var("OB_LOOP_SECRET_TEST");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

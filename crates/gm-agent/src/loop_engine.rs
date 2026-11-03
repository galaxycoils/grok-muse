use crate::trajectory::TrajectoryRecorder;
use gm_core::error::{GmError, Result};
use gm_core::types::{ComputerAction, compute_ax_diff};
use gm_llm::{ChatMessage, HybridLlmClient, LlmRouter, TaskType};
use gm_platform::Platform;
use gm_safety::{
    ActionTier, AuditLogger, BlocklistManager, KillSwitch, UntrustedData, format_untrusted_context,
    sanitize_ax_snapshot,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    pub max_steps: u32,
    pub user_active_pause_seconds: f64,
    pub max_action_retries: u32,
    pub trajectory_dir: Option<PathBuf>,
    pub capture_frames: bool,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 25,
            user_active_pause_seconds: 1.5,
            max_action_retries: 3,
            trajectory_dir: None,
            capture_frames: false,
        }
    }
}

pub struct AgentLoop<P: Platform> {
    platform: Arc<P>,
    llm: Arc<HybridLlmClient>,
    router: LlmRouter,
    kill_switch: KillSwitch,
    blocklist: BlocklistManager,
    config: AgentLoopConfig,
    audit: Option<Arc<AuditLogger>>,
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
        }
    }

    pub fn with_audit(mut self, audit: Arc<AuditLogger>) -> Self {
        self.audit = Some(audit);
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

        let res = self.run_task_loop(task_id, instruction).await;
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
        let recorder = if let Some(ref dir) = self.config.trajectory_dir {
            TrajectoryRecorder::with_directory(task_id, instruction, dir.clone())
        } else {
            TrajectoryRecorder::new(task_id, instruction)
        };

        let mut step = 0;
        let mut consecutive_no_change_retries = 0;
        let mut last_action_feedback: Option<String> = None;

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
            let raw_snapshot = self.platform.ax_snapshot(None).await?;

            // 3. Optional frame capture for multimodal grounding or trajectory recording
            let frame = if self.config.capture_frames {
                self.platform.capture(Some(1280), Some(70)).await.ok()
            } else {
                None
            };

            // 4. Blocklist check on frontmost window
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

            // 5. Redaction of secure fields
            let sanitized_snapshot = sanitize_ax_snapshot(raw_snapshot);

            // 6. Wrap screen text as untrusted data
            let snapshot_json = serde_json::to_string(&sanitized_snapshot)?;
            let untrusted = UntrustedData::new(snapshot_json, "screen_ax_tree");
            let untrusted_block = format_untrusted_context(untrusted.inner(), untrusted.source());

            // 7. Route and execute LLM step
            let (primary, fallback) = self.router.route(TaskType::CuAxStep);
            let feedback_block = if let Some(ref fb) = last_action_feedback {
                format!("\n\n{fb}")
            } else {
                String::new()
            };
            let prompt = format!(
                "Goal: {instruction}\n\nCurrent window state:\n{untrusted_block}{feedback_block}\n\nPick the next action in JSON format."
            );
            let messages = vec![
                ChatMessage {
                    role: "system".into(),
                    content: "You are an autonomous computer-use agent. Output JSON with fields 'action' and 'params'.".into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: prompt,
                },
            ];

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
                    serde_json::json!({ "task_id": task_id, "step": step }),
                );
            }

            // Parse action
            let action: ComputerAction = match serde_json::from_str(&response.content) {
                Ok(a) => a,
                Err(_) => {
                    // Fallback to done if unparseable
                    ComputerAction::Done {
                        summary: response.content.clone(),
                    }
                }
            };

            // 8. Blocklist check on action target app
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

            // 9. Resolve target label from snapshot for safety classification
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

            // 10. Safety tier check
            let tier = ActionTier::classify(&action, target_label.as_deref());
            if tier.requires_confirmation() {
                info!(
                    "Irreversible action {:?} requires explicit user approval",
                    action
                );
                if let Some(audit) = &self.audit {
                    audit.log(
                        "safety_intercept",
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
                    "Irreversible action '{action:?}' requires user confirmation"
                )));
            }

            // 11. User coexistence check:
            // Pause CGEvent actions if user input occurred within the last N seconds,
            // while allowing background AX actions to proceed immediately without pausing.
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

            // 12. Dispatch action or terminate
            match &action {
                ComputerAction::Done { summary } => {
                    let duration_ms = step_start.elapsed().as_millis() as u64;
                    recorder.record_step(
                        frame,
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
                    let duration_ms = step_start.elapsed().as_millis() as u64;
                    recorder.record_step(
                        frame,
                        Some(sanitized_snapshot),
                        action.clone(),
                        None,
                        response.content.clone(),
                        duration_ms,
                    );
                    recorder.finish(Some(format!("Asked user: {question}")));
                    return Ok(question.clone());
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
                    self.platform.act(other).await?;

                    // 13. Verify-after-act: capture post-action snapshot and compute diff
                    let raw_post = self.platform.ax_snapshot(None).await?;
                    let sanitized_post = sanitize_ax_snapshot(raw_post);
                    let diff = compute_ax_diff(&sanitized_snapshot, &sanitized_post);

                    let duration_ms = step_start.elapsed().as_millis() as u64;
                    recorder.record_step(
                        frame,
                        Some(sanitized_snapshot.clone()),
                        action.clone(),
                        Some(diff.clone()),
                        response.content.clone(),
                        duration_ms,
                    );

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
                        // Progress observed or state changed, reset retries
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
    use gm_core::config::{BudgetConfig, ProvidersConfig};
    use gm_core::types::UserActivity;
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
        let router = LlmRouter::new(ProvidersConfig::default());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        );

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
        let router = LlmRouter::new(ProvidersConfig::default());
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
        );

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
        let router = LlmRouter::new(ProvidersConfig::default());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        );

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
        let router = LlmRouter::new(ProvidersConfig::default());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        );

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
        // Set user to active (input 0.2s ago)
        platform.set_user_activity(UserActivity {
            seconds_since_last_event: 0.2,
            is_user_active: true,
        });

        let mock_backend = Arc::new(MockLlmBackend::new());
        // Queue step 1 (AX Click) and step 2 (Done)
        mock_backend.queue_response(
            "{\"action\":\"Click\",\"params\":{\"token\":\"btn_1\",\"coords\":null,\"button\":null,\"click_count\":null}}",
        );
        mock_backend.queue_response("{\"action\":\"Done\",\"params\":{\"summary\":\"Completed\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend.clone(), budget));
        let router = LlmRouter::new(ProvidersConfig::default());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let config = AgentLoopConfig {
            max_steps: 3,
            user_active_pause_seconds: 1.0,
            max_action_retries: 2,
            trajectory_dir: None,
            capture_frames: false,
        };

        let agent = AgentLoop::new(
            platform.clone(),
            llm,
            router,
            kill_switch,
            blocklist,
            config,
        );

        let res = agent.run_task("test_coexistence", "Click button").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "Completed");

        // Verify that AX action was indeed executed
        let actions = platform.get_executed_actions();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].is_ax_action());
    }

    #[tokio::test]
    async fn test_verify_after_act_retry_bounds_with_ask_user_fallback() {
        let platform = Arc::new(MockPlatform::new());
        let mock_backend = Arc::new(MockLlmBackend::new());
        // Action that modifies UI state: typing text
        mock_backend.set_response("{\"action\":\"Type\",\"params\":{\"text\":\"test text\"}}");

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let llm = Arc::new(HybridLlmClient::new(mock_backend, budget));
        let router = LlmRouter::new(ProvidersConfig::default());
        let kill_switch = KillSwitch::new();
        let blocklist = BlocklistManager::default();

        let config = AgentLoopConfig {
            max_steps: 10,
            user_active_pause_seconds: 0.1,
            max_action_retries: 2, // Bound is 2 retries
            trajectory_dir: None,
            capture_frames: false,
        };

        let agent = AgentLoop::new(platform, llm, router, kill_switch, blocklist, config);

        // MockPlatform snapshot doesn't change after Type, so diff is empty
        // AgentLoop should retry up to 2 times, then fall back to ask_user!
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
        let router = LlmRouter::new(ProvidersConfig::default());
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
        .with_audit(audit.clone());

        let res = agent.run_task("test_audit_task", "Test instruction").await;
        assert!(res.is_ok());

        let entries = audit.entries();
        assert!(entries.iter().any(|e| e.event_type == "task_start"));
        assert!(entries.iter().any(|e| e.event_type == "model_call"));
        assert!(entries.iter().any(|e| e.event_type == "task_done"));
    }
}

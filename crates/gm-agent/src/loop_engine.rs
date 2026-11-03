use crate::trajectory::TrajectoryRecorder;
use gm_core::error::{GmError, Result};
use gm_core::types::ComputerAction;
use gm_llm::{ChatMessage, HybridLlmClient, LlmRouter, TaskType};
use gm_platform::Platform;
use gm_safety::{
    ActionTier, BlocklistManager, KillSwitch, UntrustedData, format_untrusted_context,
    sanitize_ax_snapshot,
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

pub struct AgentLoopConfig {
    pub max_steps: u32,
    pub user_active_pause_seconds: f64,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 25,
            user_active_pause_seconds: 1.5,
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
        }
    }

    pub async fn run_task(&self, task_id: &str, instruction: &str) -> Result<String> {
        let recorder = TrajectoryRecorder::new(task_id, instruction);
        let mut step = 0;

        while step < self.config.max_steps {
            step += 1;
            let step_start = Instant::now();

            // 1. Kill switch check (<100ms requirement)
            if self.kill_switch.is_triggered() {
                warn!("Agent loop halted by kill switch!");
                self.platform.cancel().await?;
                return Err(GmError::SafetyViolation(
                    "Execution stopped by Kill Switch".into(),
                ));
            }

            // 2. User activity coexistence check
            let activity = self.platform.user_activity().await?;
            if activity.seconds_since_last_event < self.config.user_active_pause_seconds {
                info!(
                    "User active (last event {:.2}s ago); pausing briefly",
                    activity.seconds_since_last_event
                );
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            // 3. Snapshot AX tree
            let raw_snapshot = self.platform.ax_snapshot(None).await?;

            // 4. Blocklist check
            if self
                .blocklist
                .is_blocked(&raw_snapshot.app_name, &raw_snapshot.window_title)
            {
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
            let prompt = format!(
                "Goal: {instruction}\n\nCurrent window state:\n{untrusted_block}\n\nPick the next action in JSON format."
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

            let duration_ms = step_start.elapsed().as_millis() as u64;
            recorder.record_step(
                Some(sanitized_snapshot),
                response.content.clone(),
                action.clone(),
                duration_ms,
            );

            // 8. Safety tier check
            let tier = ActionTier::classify(&action, None);
            if tier.requires_confirmation() {
                info!(
                    "Irreversible action {:?} requires explicit user approval",
                    action
                );
                // In headless / non-interactive mode or safety policy, error unless confirmed
                return Err(GmError::SafetyViolation(format!(
                    "Irreversible action '{action:?}' requires user confirmation"
                )));
            }

            // 9. Dispatch action or terminate
            match action {
                ComputerAction::Done { summary } => {
                    recorder.finish(Some(summary.clone()));
                    return Ok(summary);
                }
                ComputerAction::AskUser { question } => {
                    recorder.finish(Some(format!("Asked user: {question}")));
                    return Ok(question);
                }
                other => {
                    self.platform.act(&other).await?;
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
        kill_switch.trigger(); // Trigger kill switch immediately

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
}

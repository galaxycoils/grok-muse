use crate::channels::ChannelStore;
use crate::queue::{DurableTaskQueue, QueuedTask};
use crate::roster::BotRoster;
use chrono::Utc;
use gm_core::error::{GmError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub version: String,
    pub description: String,
    pub protocol: String,
    pub bots: Vec<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotHandoff {
    pub handoff_id: String,
    pub from_bot_id: String,
    pub to_bot_id: String,
    pub channel_id: Option<String>,
    pub goal: String,
    pub context_summary: String,
    pub transferred_artifacts: Vec<PathBuf>,
    pub depth: u8,
    pub timestamp: String,
}

/// Coordinates Agent-to-Agent (A2A) handoffs between OpenBot teammates, copying files from
/// `<from_bot>/outbox/` (or `<from_bot>/workspace/`) into `<to_bot>/inbox/` and enforcing
/// `max_handoff_depth` loop prevention.
#[derive(Clone)]
pub struct A2aCoordinator {
    roster: BotRoster,
    queue: DurableTaskQueue,
    channels: ChannelStore,
    max_handoff_depth: u8,
}

impl A2aCoordinator {
    pub fn new(
        roster: BotRoster,
        queue: DurableTaskQueue,
        channels: ChannelStore,
        max_handoff_depth: u8,
    ) -> Self {
        Self {
            roster,
            queue,
            channels,
            max_handoff_depth: max_handoff_depth.max(1),
        }
    }

    pub fn agent_card(&self) -> AgentCard {
        let bots = self.roster.list_bots().into_iter().map(|b| b.id).collect();
        AgentCard {
            name: "Grok-Muse OpenBot Platform".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            description:
                "Local-first macOS Computer-Use & Multi-Agent Coworker Suite (AG-UI + A2A)".into(),
            protocol: "a2a/1.0 + ag-ui/1.0".into(),
            bots,
            capabilities: vec![
                "computer_use_ax_som".into(),
                "seatbelt_workspace_shell".into(),
                "durable_fifo_queues".into(),
                "multi_bot_channels".into(),
                "take_the_wheel_handoff".into(),
                "opaque_vault_secrets".into(),
            ],
        }
    }

    /// Execute a structured handoff from `from_bot_id` to `to_bot_id`, copying any declared
    /// artifact files into `<to_bot_id>/inbox/` and enqueuing the child task on `to_bot_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn handoff(
        &self,
        from_bot_id: &str,
        to_bot_id: &str,
        channel_id: Option<&str>,
        goal: &str,
        context_summary: &str,
        artifact_names: &[String],
        current_depth: u8,
    ) -> Result<(BotHandoff, QueuedTask)> {
        let next_depth = current_depth.saturating_add(1);
        if next_depth > self.max_handoff_depth {
            return Err(GmError::SafetyViolation(format!(
                "A2A handoff depth {next_depth} exceeds max_handoff_depth ({}) between @{from_bot_id} and @{to_bot_id}",
                self.max_handoff_depth
            )));
        }

        let from_sandbox = self.roster.workspace_sandbox(from_bot_id)?;
        let to_sandbox = self.roster.workspace_sandbox(to_bot_id)?;

        let mut transferred = Vec::new();
        for name in artifact_names {
            let clean_name = std::path::Path::new(name)
                .file_name()
                .ok_or_else(|| GmError::Config(format!("Invalid artifact filename '{name}'")))?;

            // Check sender's outbox first, then sender's workspace
            let src_outbox = from_sandbox.outbox_dir().join(clean_name);
            let src_ws = from_sandbox.workspace_dir().join(clean_name);
            let src = if src_outbox.exists() {
                src_outbox
            } else if src_ws.exists() {
                src_ws
            } else {
                return Err(GmError::Config(format!(
                    "Artifact '{}' not found in @{} outbox or workspace",
                    name, from_bot_id
                )));
            };

            let dst = to_sandbox.inbox_dir().join(clean_name);
            std::fs::copy(&src, &dst)?;
            transferred.push(dst);
        }

        let handoff = BotHandoff {
            handoff_id: format!("a2a-{}", uuid::Uuid::new_v4()),
            from_bot_id: from_bot_id.to_string(),
            to_bot_id: to_bot_id.to_string(),
            channel_id: channel_id.map(|s| s.to_string()),
            goal: goal.to_string(),
            context_summary: context_summary.to_string(),
            transferred_artifacts: transferred.clone(),
            depth: next_depth,
            timestamp: Utc::now().to_rfc3339(),
        };

        let artifact_list: Vec<String> = transferred
            .iter()
            .filter_map(|p| {
                p.file_name()
                    .map(|f| format!("inbox/{}", f.to_string_lossy()))
            })
            .collect();

        let enriched_instruction = if artifact_list.is_empty() {
            format!("[A2A Handoff from @{from_bot_id}] {goal}\nContext: {context_summary}")
        } else {
            format!(
                "[A2A Handoff from @{from_bot_id}] {goal}\nContext: {context_summary}\nTransferred Inbox Artifacts: {}",
                artifact_list.join(", ")
            )
        };

        let task = self.queue.enqueue_with_depth(
            to_bot_id,
            channel_id,
            &enriched_instruction,
            5,
            next_depth,
        )?;

        if let Some(chan) = channel_id {
            let valid_ids: Vec<String> =
                self.roster.list_bots().into_iter().map(|b| b.id).collect();
            let msg = format!(
                "🤝 Handed off task to @{to_bot_id}: \"{goal}\" (task `{}`)",
                task.id
            );
            let _ =
                self.channels
                    .post_message(chan, from_bot_id, Some(from_bot_id), &msg, &valid_ids);
        }

        Ok((handoff, task))
    }
}

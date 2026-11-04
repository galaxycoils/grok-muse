use crate::a2a::A2aCoordinator;
use crate::channels::{ChannelMessageRecord, ChannelStore};
use crate::compaction::ContextCompactor;
use crate::queue::{DurableTaskQueue, QueuedTask};
use crate::roster::BotRoster;
use gm_agent::{AgentLoop, AgentLoopConfig, InteractionHandler, NonInteractiveHandler};
use gm_core::config::GmConfig;
use gm_core::error::{GmError, Result};
use gm_core::ipc::{DaemonEvent, QueueControlAction};
use gm_llm::{HybridLlmClient, LlmRouter, VaultSecretResolver};
use gm_platform::Platform;
use gm_safety::{AuditLogger, BlocklistManager, KillSwitch, SecretScrubber};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

/// Standard AG-UI protocol event envelope (`CopilotKit/OpenBot` compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgUiEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub run_id: String,
    pub bot_id: String,
    pub timestamp: String,
    pub payload: serde_json::Value,
}

/// Coordinates the multi-bot roster, durable SQLite FIFO task queues, shared channels,
/// A2A handoffs, `GuiSeatMutex` serialization, and AG-UI / DaemonEvent broadcasting.
#[derive(Clone)]
pub struct OpenBotSupervisor {
    config: GmConfig,
    roster: BotRoster,
    queue: DurableTaskQueue,
    channels: ChannelStore,
    a2a: A2aCoordinator,
    compactor: ContextCompactor,
    gui_seat: Arc<AsyncMutex<()>>,
    daemon_events_tx: broadcast::Sender<DaemonEvent>,
    agui_events_tx: broadcast::Sender<AgUiEvent>,
    audit: Arc<AuditLogger>,
    scrubber: SecretScrubber,
}

impl OpenBotSupervisor {
    pub fn open(
        config: GmConfig,
        audit: Arc<AuditLogger>,
        daemon_events_tx: broadcast::Sender<DaemonEvent>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&config.storage_dir)?;
        let roster = BotRoster::open(&config.openbot.bots_dir)?;
        let db_path = config.storage_dir.join("openbot.sqlite");
        let queue = DurableTaskQueue::open(&db_path)?;
        let _ = queue.recover_after_crash()?;
        let channels = ChannelStore::open(&db_path)?;
        let a2a = A2aCoordinator::new(
            roster.clone(),
            queue.clone(),
            channels.clone(),
            config.openbot.max_handoff_depth,
        );
        let compactor = ContextCompactor::new(
            config.openbot.max_context_tokens,
            config.openbot.compaction_trigger_ratio,
            4,
        );
        let (agui_events_tx, _) = broadcast::channel(512);

        Ok(Self {
            config,
            roster,
            queue,
            channels,
            a2a,
            compactor,
            gui_seat: Arc::new(AsyncMutex::new(())),
            daemon_events_tx,
            agui_events_tx,
            audit,
            scrubber: SecretScrubber::new(),
        })
    }

    pub fn roster(&self) -> &BotRoster {
        &self.roster
    }

    pub fn queue(&self) -> &DurableTaskQueue {
        &self.queue
    }

    pub fn channels(&self) -> &ChannelStore {
        &self.channels
    }

    pub fn a2a(&self) -> &A2aCoordinator {
        &self.a2a
    }

    pub fn compactor(&self) -> &ContextCompactor {
        &self.compactor
    }

    pub fn audit(&self) -> &Arc<AuditLogger> {
        &self.audit
    }

    pub fn scrubber(&self) -> &SecretScrubber {
        &self.scrubber
    }

    pub fn agui_sender(&self) -> broadcast::Sender<AgUiEvent> {
        self.agui_events_tx.clone()
    }

    pub fn daemon_event_sender(&self) -> broadcast::Sender<DaemonEvent> {
        self.daemon_events_tx.clone()
    }

    pub fn emit_agui(
        &self,
        event_type: impl Into<String>,
        run_id: impl Into<String>,
        bot_id: impl Into<String>,
        payload: serde_json::Value,
    ) {
        let evt = AgUiEvent {
            event_type: event_type.into(),
            run_id: run_id.into(),
            bot_id: bot_id.into(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            payload,
        };
        let _ = self.agui_events_tx.send(evt);
    }

    /// Submit a task into a bot's durable FIFO queue and emit `QueueUpdated`.
    pub fn submit_task(
        &self,
        bot_id: Option<&str>,
        channel_id: Option<&str>,
        instruction: &str,
        priority: i32,
    ) -> Result<QueuedTask> {
        let target_bot = bot_id.unwrap_or(&self.config.openbot.default_bot_id);
        if self.roster.get_bot(target_bot).is_none() {
            return Err(GmError::Config(format!("Unknown bot '{target_bot}'")));
        }
        let task = self
            .queue
            .enqueue(target_bot, channel_id, instruction, priority)?;
        self.broadcast_queue_state(target_bot);
        Ok(task)
    }

    /// Post a message in a channel and automatically enqueue tasks for any `@bot` mentions
    /// (or the channel's `owner_bot_id` if a human posts without explicit `@bot` mentions).
    pub fn post_channel_message(
        &self,
        channel_id: &str,
        author: &str,
        bot_id: Option<&str>,
        content: &str,
    ) -> Result<(ChannelMessageRecord, Vec<QueuedTask>)> {
        let valid_ids: Vec<String> = self.roster.list_bots().into_iter().map(|b| b.id).collect();
        let msg = self
            .channels
            .post_message(channel_id, author, bot_id, content, &valid_ids)?;

        let _ = self.daemon_events_tx.send(DaemonEvent::ChannelMessage {
            channel_id: msg.channel_id.clone(),
            author: msg.author.clone(),
            bot_id: msg.bot_id.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp.clone(),
        });

        let mut spawned = Vec::new();
        let chan_info = self.channels.get_channel(channel_id)?;
        let chan_paused = chan_info.as_ref().map(|c| c.paused).unwrap_or(false);

        if !chan_paused {
            if !msg.mentioned_bots.is_empty() {
                for target_bot in &msg.mentioned_bots {
                    // Prevent a bot from self-triggering in a loop
                    if bot_id != Some(target_bot.as_str()) {
                        let t =
                            self.submit_task(Some(target_bot), Some(&msg.channel_id), content, 5)?;
                        spawned.push(t);
                    }
                }
            } else if bot_id.is_none()
                && let Some(owner) = chan_info.and_then(|c| c.owner_bot_id)
            {
                let t = self.submit_task(Some(&owner), Some(&msg.channel_id), content, 0)?;
                spawned.push(t);
            }
        }

        Ok((msg, spawned))
    }

    pub fn control_task(&self, task_id: &str, action: &QueueControlAction) -> Result<bool> {
        let existing = self.queue.get_task(task_id)?;
        let res = match action {
            QueueControlAction::Pause => self.queue.pause(task_id)?,
            QueueControlAction::Resume | QueueControlAction::Retry => self.queue.resume(task_id)?,
            QueueControlAction::Cancel => self.queue.cancel(task_id)?,
            QueueControlAction::Reassign { target_bot_id } => {
                if self.roster.get_bot(target_bot_id).is_none() {
                    return Err(GmError::Config(format!("Unknown bot '{target_bot_id}'")));
                }
                let ok = self.queue.reassign(task_id, target_bot_id)?;
                self.broadcast_queue_state(target_bot_id);
                ok
            }
        };
        if let Some(t) = existing {
            self.broadcast_queue_state(&t.bot_id);
        }
        Ok(res)
    }

    pub fn broadcast_queue_state(&self, bot_id: &str) {
        if let Ok(tasks) = self.queue.list_tasks(Some(bot_id)) {
            let queued = tasks
                .iter()
                .filter(|t| t.status == crate::queue::QueuedTaskStatus::Queued)
                .count();
            let running_task_id = tasks
                .iter()
                .find(|t| t.status == crate::queue::QueuedTaskStatus::Running)
                .map(|t| t.id.clone());
            let _ = self.daemon_events_tx.send(DaemonEvent::QueueUpdated {
                bot_id: bot_id.to_string(),
                queued,
                running_task_id,
            });
        }
    }

    /// Claim and execute the next FIFO task for `bot_id` using `AgentLoop`.
    /// Acquires `GuiSeatMutex` only if the bot profile has `allow_gui == true`, allowing
    /// non-GUI bots (`researcher`, `coder`, `guardian`) to run concurrently in parallel.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_next_for_bot<P: Platform>(
        &self,
        bot_id: &str,
        platform: Arc<P>,
        llm: Arc<HybridLlmClient>,
        router: LlmRouter,
        kill_switch: KillSwitch,
        blocklist: BlocklistManager,
        interaction_handler: Option<Arc<dyn InteractionHandler>>,
    ) -> Result<Option<QueuedTask>> {
        let profile = self
            .roster
            .get_bot(bot_id)
            .ok_or_else(|| GmError::Config(format!("Unknown bot '{bot_id}'")))?;

        let Some(task) = self.queue.claim_next_fifo(bot_id)? else {
            return Ok(None);
        };
        self.broadcast_queue_state(bot_id);

        self.emit_agui(
            "RUN_STARTED",
            &task.id,
            bot_id,
            serde_json::json!({
                "instruction": task.instruction,
                "channel_id": task.channel_id,
            }),
        );

        let sandbox = self
            .roster
            .workspace_sandbox(bot_id)?
            .with_scrubber(self.scrubber.clone());

        let a2a = self.a2a.clone();
        let from_bot = bot_id.to_string();
        let task_chan = task.channel_id.clone();
        let current_depth = task.handoff_depth;

        let handler = interaction_handler.unwrap_or_else(|| Arc::new(NonInteractiveHandler));
        let sup_for_progress = self.clone();
        let run_id_for_progress = task.id.clone();
        let bot_for_progress = bot_id.to_string();

        let agent = AgentLoop::new(
            platform,
            llm,
            router,
            kill_switch,
            blocklist,
            AgentLoopConfig::default(),
        )
        .with_workspace_sandbox(sandbox)
        .with_vault_resolver(VaultSecretResolver::default())
        .with_secret_scrubber(self.scrubber.clone())
        .with_audit(Arc::clone(&self.audit))
        .with_interaction_handler(handler)
        .with_delegation_callback(move |target_bot, goal, artifacts| {
            let (_, child_task) = a2a.handoff(
                &from_bot,
                target_bot,
                task_chan.as_deref(),
                goal,
                "Delegated from active agent loop",
                artifacts,
                current_depth,
            )?;
            Ok(format!(
                "Delegated to @{target_bot} as task {}",
                child_task.id
            ))
        })
        .with_progress_callback(move |step, action_summary| {
            sup_for_progress.emit_agui(
                "TOOL_CALL_START",
                &run_id_for_progress,
                &bot_for_progress,
                serde_json::json!({
                    "step": step,
                    "action": action_summary,
                }),
            );
            let _ = sup_for_progress
                .daemon_events_tx
                .send(DaemonEvent::TaskProgress {
                    task_id: run_id_for_progress.clone(),
                    step,
                    action_summary: action_summary.to_string(),
                });
        });

        let effective_instruction = if let Some(ref note) = task.human_resume_note {
            format!(
                "[Bot Persona: {} ({})]\n{}\n[Operator Wheel Resume Note]: {}",
                profile.display_name, profile.role_title, task.instruction, note
            )
        } else {
            format!(
                "[Bot Persona: {} ({})]\n{}",
                profile.display_name, profile.role_title, task.instruction
            )
        };

        let result = if profile.allow_gui {
            let _seat = self.gui_seat.lock().await;
            agent.run_task(&task.id, &effective_instruction).await
        } else {
            agent.run_task(&task.id, &effective_instruction).await
        };

        match result {
            Ok(summary) => {
                if let Some(rest) = summary.strip_prefix("[WAITING_FOR_HUMAN] ") {
                    let _ = self.queue.mark_waiting_for_human(&task.id, rest);
                    let _ = self
                        .daemon_events_tx
                        .send(DaemonEvent::WheelHandoffRequested {
                            task_id: task.id.clone(),
                            bot_id: bot_id.to_string(),
                            blocker: rest.to_string(),
                            instructions: "Resolve blocker and click Hand Back to Bot".into(),
                        });
                    self.emit_agui(
                        "CUSTOM",
                        &task.id,
                        bot_id,
                        serde_json::json!({
                            "name": "TAKE_THE_WHEEL",
                            "blocker": rest,
                        }),
                    );
                } else {
                    let _ = self.queue.complete(&task.id, &summary);
                    if let Some(ref chan) = task.channel_id {
                        let valid_ids: Vec<String> =
                            self.roster.list_bots().into_iter().map(|b| b.id).collect();
                        let _ = self.channels.post_message(
                            chan,
                            &profile.display_name,
                            Some(bot_id),
                            &summary,
                            &valid_ids,
                        );
                    }
                    self.emit_agui(
                        "RUN_FINISHED",
                        &task.id,
                        bot_id,
                        serde_json::json!({ "summary": summary }),
                    );
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                let _ = self.queue.fail(&task.id, &err_str);
                self.emit_agui(
                    "RUN_ERROR",
                    &task.id,
                    bot_id,
                    serde_json::json!({ "error": err_str }),
                );
            }
        }

        self.broadcast_queue_state(bot_id);
        self.queue.get_task(&task.id)
    }
}

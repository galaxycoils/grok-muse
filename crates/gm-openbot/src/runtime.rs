//! One OS-locked runtime owns recovery and all active worker futures.
use crate::{OpenBotSupervisor, QueuedTaskStatus};
use gm_agent::InteractionHandler;
use gm_core::error::{GmError, Result};
use gm_llm::{HybridLlmClient, LlmRouter};
use gm_platform::Platform;
use gm_safety::{BlocklistManager, KillSwitch};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Arc;
use tokio::task::JoinSet;

pub struct RuntimeLease {
    _file: File,
}
impl RuntimeLease {
    pub fn acquire(storage: &Path) -> Result<Self> {
        std::fs::create_dir_all(storage)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(storage.join("runtime.lock"))?;
        file.try_lock().map_err(|e| {
            GmError::Config(format!(
                "Another Grok Muse runtime owns this workspace: {e}"
            ))
        })?;
        Ok(Self { _file: file })
    }
}

impl OpenBotSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn start_runtime<P: Platform + 'static>(
        &self,
        platform: Arc<P>,
        llm: Arc<HybridLlmClient>,
        router: LlmRouter,
        kill: KillSwitch,
        blocklist: BlocklistManager,
        handler: Option<Arc<dyn InteractionHandler>>,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let lease = Arc::new(RuntimeLease::acquire(&self.config.storage_dir)?);
        self.queue.recover_after_crash()?;
        self.runtime_demo
            .store(llm.backend().is_mock(), std::sync::atomic::Ordering::SeqCst);
        self.runtime_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let sup = self.clone();
        Ok(tokio::spawn(async move {
            struct ActiveGuard(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for ActiveGuard {
                fn drop(&mut self) {
                    self.0.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let runtime_lease = lease;
            let _guard = ActiveGuard(sup.runtime_active.clone());
            let mut workers = JoinSet::new();
            let mut active = std::collections::HashSet::new();
            let mut timer = tokio::time::interval(std::time::Duration::from_millis(100));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        if kill.is_triggered() || sup.runtime_paused.load(std::sync::atomic::Ordering::SeqCst) { continue; }
                        for bot in sup.roster.list_bots() {
                            if workers.len() >= 4 || active.contains(&bot.id) { continue; }
                            // Do not spawn idle workers continuously.
                            let queued = sup.queue.has_queued(&bot.id).unwrap_or(false);
                            if !queued { continue; }
                            active.insert(bot.id.clone());
                            let (s,p,l,r,k,b,h) = (sup.clone(),platform.clone(),llm.clone(),router.clone(),kill.clone(),blocklist.clone(),handler.clone());
                            let worker_lease = runtime_lease.clone();
                            workers.spawn(async move {
                                let _lease = worker_lease;
                                if let Err(error) = s.run_next_for_bot(&bot.id,p,l,r,k,b,h).await {
                                    tracing::error!(bot_id=%bot.id, %error, "Worker failed");
                                    if let Ok(tasks) = s.queue.list_tasks(Some(&bot.id)) {
                                        for task in tasks {
                                            if task.status == QueuedTaskStatus::Running { let _ = s.queue.fail(&task.id,&error.to_string()); }

                                        }
                                    }
                                }
                                bot.id
                            });
                        }
                    }
                    Some(result) = workers.join_next(), if !workers.is_empty() => {
                        match result { Ok(id) => { active.remove(&id); }, Err(error) => {
                            tracing::error!(%error,"Runtime worker panicked; stopping admission until restart");
                            break;
                        } }
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_one_runtime_can_hold_the_storage_lease() {
        let dir = std::env::temp_dir().join(format!("gm-lease-{}", uuid::Uuid::new_v4()));
        let first = RuntimeLease::acquire(&dir).unwrap();
        assert!(RuntimeLease::acquire(&dir).is_err());
        drop(first);
        assert!(RuntimeLease::acquire(&dir).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use gm_core::config::GmConfig;
    use gm_llm::{BudgetManager, ChatCompletionResponse, ChatMessage, LlmBackend, ModelTarget};
    use gm_safety::AuditLogger;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct BlockingBackend {
        started: Arc<tokio::sync::Notify>,
        dropped: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl LlmBackend for BlockingBackend {
        fn is_mock(&self) -> bool {
            true
        }
        async fn complete(
            &self,
            _target: &ModelTarget,
            messages: &[ChatMessage],
        ) -> Result<ChatCompletionResponse> {
            assert!(messages[0].content.contains("Exact test persona"));
            assert!(messages[0].content.contains("GUI=false"));
            struct Guard(Arc<AtomicBool>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _guard = Guard(self.dropped.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn runtime_cancels_real_worker_future_and_client_open_never_recovers_it() {
        let dir = std::env::temp_dir().join(format!("gm-worker-{}", uuid::Uuid::new_v4()));
        let mut config = GmConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };
        config.openbot.bots_dir = dir.join("bots");
        let (events, _) = tokio::sync::broadcast::channel(32);
        let supervisor = OpenBotSupervisor::open(
            config.clone(),
            Arc::new(AuditLogger::in_memory()),
            events.clone(),
        )
        .unwrap();
        let mut profile = supervisor.roster().get_bot("muse").unwrap();
        profile.system_prompt = "Exact test persona".into();
        supervisor.roster().upsert_bot(profile).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let llm = Arc::new(HybridLlmClient::new(
            Arc::new(BlockingBackend {
                started: started.clone(),
                dropped: dropped.clone(),
            }),
            Arc::new(BudgetManager::new(config.budget.clone())),
        ));
        let platform = Arc::new(gm_platform::MockPlatform::new());
        let task = supervisor
            .submit_task(Some("muse"), Some("general"), "Wait for cancellation", 0)
            .unwrap();
        let runtime = supervisor
            .start_runtime(
                platform.clone(),
                llm.clone(),
                LlmRouter::new(config.providers.clone()),
                KillSwitch::new(),
                BlocklistManager::new(&[]),
                None,
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        assert!(
            supervisor
                .start_runtime(
                    platform,
                    llm,
                    LlmRouter::new(config.providers.clone()),
                    KillSwitch::new(),
                    BlocklistManager::new(&[]),
                    None
                )
                .is_err()
        );
        let client =
            OpenBotSupervisor::open(config, Arc::new(AuditLogger::in_memory()), events).unwrap();
        assert_eq!(
            client.queue().get_task(&task.id).unwrap().unwrap().status,
            QueuedTaskStatus::Running
        );
        client.queue().cancel(&task.id).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            loop {
                if client.queue().get_task(&task.id).unwrap().unwrap().status
                    == QueuedTaskStatus::Cancelled
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(dropped.load(Ordering::SeqCst));
        runtime.abort();
        let _ = runtime.await;
        drop(client);
        drop(supervisor);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

//! Grok-Muse OpenBot Multi-Agent & Coworker Suite (`v0.3.0`).

pub mod a2a;
pub mod bridges;
pub mod channels;
pub mod compaction;
pub mod queue;
pub mod roster;
pub mod supervisor;
pub mod web;

pub use a2a::*;
pub use bridges::*;
pub use channels::*;
pub use compaction::*;
pub use queue::*;
pub use roster::*;
pub use supervisor::*;
pub use web::*;

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::config::{ChatBridgesConfig, GmConfig};
    use gm_llm::ChatMessage;
    use gm_safety::{AuditCategory, AuditLogger};
    use std::sync::Arc;
    use tokio::sync::broadcast;

    #[test]
    fn test_openbot_roster_and_queue_contract() {
        let dir = std::env::temp_dir().join(format!("ob_test_{}", uuid::Uuid::new_v4()));
        let roster = BotRoster::open(dir.join("bots")).unwrap();
        assert_eq!(roster.list_bots().len(), 4);
        assert!(roster.get_bot("muse").is_some());
        assert!(roster.get_bot("coder").is_some());

        let queue = DurableTaskQueue::open(dir.join("openbot.sqlite")).unwrap();
        let t = queue
            .enqueue("coder", Some("general"), "Build parser", 5)
            .unwrap();
        assert_eq!(t.status, QueuedTaskStatus::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_queue_crash_recovery_and_backup_restore() {
        let dir = std::env::temp_dir().join(format!("ob_q_rec_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let queue = DurableTaskQueue::open(dir.join("q.sqlite")).unwrap();

        let t_read = queue
            .enqueue("researcher", Some("research"), "Read docs", 10)
            .unwrap();
        let t_mut = queue
            .enqueue("muse", Some("general"), "Submit checkout", 5)
            .unwrap();

        // Claim both so they are in `running` state
        let c1 = queue.claim_next_fifo("researcher").unwrap().unwrap();
        assert_eq!(c1.id, t_read.id);
        let c2 = queue.claim_next_fifo("muse").unwrap().unwrap();
        assert_eq!(c2.id, t_mut.id);
        queue.set_mutating_in_flight(&t_mut.id, true).unwrap();

        // Simulate crash recovery
        let recovered = queue.recover_after_crash().unwrap();
        assert_eq!(recovered, 2);

        // Read-only task returned to `queued` with recovery_count = 1
        let r1 = queue.get_task(&t_read.id).unwrap().unwrap();
        assert_eq!(r1.status, QueuedTaskStatus::Queued);
        assert_eq!(r1.recovery_count, 1);

        // Mutating task transitioned safely to `paused` so it never double-fires!
        let r2 = queue.get_task(&t_mut.id).unwrap().unwrap();
        assert_eq!(r2.status, QueuedTaskStatus::Paused);

        // Backup & restore into fresh DB
        let backup_file = dir.join("backup.jsonl");
        assert_eq!(queue.export_backup_jsonl(&backup_file).unwrap(), 2);

        let queue2 = DurableTaskQueue::in_memory().unwrap();
        assert_eq!(queue2.import_backup_jsonl(&backup_file).unwrap(), 2);
        assert_eq!(queue2.list_tasks(None).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_context_compactor_condenses_long_threads() {
        let compactor = ContextCompactor::new(400, 0.50, 2);
        let mut msgs = vec![
            ChatMessage::text("system", "You are Muse."),
            ChatMessage::text("user", "A".repeat(400)),
            ChatMessage::text("assistant", "B".repeat(400)),
            ChatMessage::text("user", "C".repeat(400)),
            ChatMessage::text("assistant", "Recent reply 1"),
            ChatMessage::text("user", "Recent question 2"),
        ];

        let report = compactor.compact_if_needed(&mut msgs);
        assert!(report.compacted);
        assert_eq!(report.summarized_messages_count, 3);
        assert!(report.estimated_tokens_after < report.estimated_tokens_before);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[1].content.contains("[Compacted Session Summary"));
        assert_eq!(msgs.last().unwrap().content, "Recent question 2");
    }

    #[test]
    fn test_channels_mentions_and_a2a_artifact_handoff() {
        let dir = std::env::temp_dir().join(format!("ob_a2a_{}", uuid::Uuid::new_v4()));
        let mut cfg = GmConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };
        cfg.openbot.bots_dir = dir.join("bots");

        let audit = Arc::new(AuditLogger::in_memory());
        let (tx, _) = broadcast::channel(32);
        let sup = OpenBotSupervisor::open(cfg, audit, tx).unwrap();

        // 1. Posting in #general with @coder and @researcher mentions spawns 2 queued tasks
        let (msg, tasks) = sup
            .post_channel_message(
                "general",
                "darius",
                None,
                "Hey @coder and @researcher please collaborate on the API spec",
            )
            .unwrap();
        assert_eq!(msg.mentioned_bots, vec!["coder", "researcher"]);
        assert_eq!(tasks.len(), 2);

        // 2. Write an artifact in @coder's outbox and hand off to @researcher
        let coder_ws = sup.roster().workspace_sandbox("coder").unwrap();
        std::fs::write(
            coder_ws.outbox_dir().join("spec.md"),
            "# API Spec v1\nReady for review.\n",
        )
        .unwrap();

        let (handoff, child_task) = sup
            .a2a()
            .handoff(
                "coder",
                "researcher",
                Some("general"),
                "Review API Spec v1",
                "Drafted in coder outbox",
                &["spec.md".into()],
                0,
            )
            .unwrap();
        assert_eq!(handoff.depth, 1);
        assert_eq!(child_task.bot_id, "researcher");

        let researcher_ws = sup.roster().workspace_sandbox("researcher").unwrap();
        let inbox_spec = researcher_ws.inbox_dir().join("spec.md");
        assert!(inbox_spec.exists());
        assert!(
            std::fs::read_to_string(inbox_spec)
                .unwrap()
                .contains("API Spec v1")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_companion_web_agui_audit_and_slack_hmac_bridge() {
        let dir = std::env::temp_dir().join(format!("ob_web_{}", uuid::Uuid::new_v4()));
        let mut cfg = GmConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };
        cfg.openbot.bots_dir = dir.join("bots");
        cfg.openbot.bridges = ChatBridgesConfig {
            telegram_enabled: true,
            telegram_allowed_chat_ids: vec!["chat-42".into()],
            slack_enabled: true,
            slack_allowed_channel_ids: vec!["C123".into()],
            discord_enabled: false,
            discord_allowed_channel_ids: vec![],
        };

        let audit = Arc::new(AuditLogger::in_memory());
        audit.log_governance(
            "workspace_write_refused",
            Some("coder"),
            AuditCategory::Refused,
            "RULE-PATH-002",
            "Path escapes confined bot root",
            "Refused write /etc/passwd",
            serde_json::json!({}),
        );

        let (tx, _) = broadcast::channel(32);
        let sup = OpenBotSupervisor::open(cfg.clone(), audit, tx).unwrap();
        let bridge_router = BridgeRouter::new(sup.clone(), cfg.openbot.bridges.clone());

        let server = CompanionWebServer::new(
            sup.clone(),
            bridge_router,
            Some("test-token-123".into()),
            Some("slack-secret-xyz".into()),
        );
        let (addr, handle) = server.bind_and_serve("127.0.0.1:0").await.unwrap();
        let client = reqwest::Client::new();

        // 1. Agent card is public
        let card: serde_json::Value = client
            .get(format!("http://{addr}/.well-known/agent.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(card["protocol"], "a2a/1.0 + ag-ui/1.0");

        // 2. /api/audit requires Bearer token and returns RULE-PATH-002 refusal citation
        let unauth = client
            .get(format!("http://{addr}/api/audit?category=refused"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), 401);

        let audit_res: Vec<serde_json::Value> = client
            .get(format!("http://{addr}/api/audit?category=refused"))
            .header("Authorization", "Bearer test-token-123")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(audit_res.len(), 1);
        assert_eq!(audit_res[0]["rule_id"], "RULE-PATH-002");

        // 3. Verify Slack webhook HMAC-SHA256 signature + @coder task enqueue
        let slack_body = r#"{"event":{"channel":"C123","user":"U99","text":"@coder run linter"}}"#;
        let ts = "1700000000";
        let sig = format!(
            "v0={}",
            compute_hmac_sha256_hex(
                b"slack-secret-xyz",
                format!("v0:{ts}:{slack_body}").as_bytes()
            )
        );
        let slack_resp = client
            .post(format!("http://{addr}/bridges/slack/events"))
            .header("x-slack-request-timestamp", ts)
            .header("x-slack-signature", sig)
            .body(slack_body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(slack_resp.status(), 200);
        let slack_json: serde_json::Value = slack_resp.json().await.unwrap();
        assert_eq!(slack_json["accepted"], true);
        assert_eq!(slack_json["spawned_task_ids"].as_array().unwrap().len(), 1);

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

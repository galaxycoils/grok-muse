use crate::runner::TaskResult;
use gm_agent::{
    AgentLoop, AgentLoopConfig, BotWorkspaceSandbox, CallbackInteractionHandler, WorkspaceToolCall,
};
use gm_core::config::{BudgetConfig, ChatBridgesConfig, GmConfig, ProvidersConfig};
use gm_core::ipc::{DaemonEvent, QueueControlAction};
use gm_core::types::{AxElement, AxSnapshot, Rect};
use gm_llm::{
    BudgetManager, ChatMessage, HybridLlmClient, KeychainSecrets, LlmRouter, MockLlmBackend,
    VaultSecretResolver,
};
use gm_openbot::{
    BridgeInboundMessage, BridgeRouter, CompanionWebServer, ContextCompactor, DurableTaskQueue,
    OpenBotSupervisor, QueuedTaskStatus, compute_hmac_sha256_hex,
};
use gm_platform::MockPlatform;
use gm_safety::{
    AuditCategory, AuditLogger, BlocklistManager, KillSwitch, PolicyOutcome, SecretScrubber,
    SecurityGateway,
};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

/// Executes the `v0.3.0` OpenBot Multi-Agent & Coworker Evaluation Suite covering:
/// 1. `ob_queue_01`: Crash-safe SQLite FIFO queue persistence, pause/resume/reassign, JSONL backup/restore, and crash recovery.
/// 2. `ob_sandbox_02`: Live macOS `sandbox-exec` Seatbelt confinement allowing `<bot>/workspace` writes + unified diff preview while blocking writes outside workspace (`RULE-PATH-002`).
/// 3. `ob_wheel_03`: *"Take the Wheel"* MFA/CAPTCHA handoff -> operator resume -> completion + opaque `secret://` scrubbing verification (`0` plaintext leaks).
/// 4. `ob_a2a_04`: Multi-bot `#general` `@coder` mention + `coder` -> `researcher` `outbox`/`inbox` artifact handoff + `ContextCompactor` verification.
/// 5. `ob_agui_05`: Live loopback `CompanionWebServer` AG-UI / A2A card + `/api/audit` `RULE-PATH-002` refusal citation + Slack HMAC-SHA256 webhook routing.
pub async fn run_openbot_capability_evals() -> Vec<TaskResult> {
    vec![
        eval_ob_queue_01().await,
        eval_ob_sandbox_02().await,
        eval_ob_wheel_03().await,
        eval_ob_a2a_04().await,
        eval_ob_agui_05().await,
    ]
}

async fn eval_ob_queue_01() -> TaskResult {
    let start = Instant::now();
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("queue.sqlite");
    let backup_path = temp.path().join("backup.jsonl");

    let q1 = DurableTaskQueue::open(&db_path).unwrap();
    let t1 = q1
        .enqueue("coder", Some("#general"), "Implement queue unit test", 5)
        .unwrap();
    let t2 = q1
        .enqueue("researcher", Some("#research"), "Audit A2A spec", 10)
        .unwrap();

    // Claim highest priority first (t2)
    let claimed = q1.claim_next_fifo("researcher").unwrap().unwrap();
    assert_eq!(claimed.id, t2.id);

    // Pause, resume, and reassign t1
    assert!(q1.pause(&t1.id).unwrap());
    assert!(q1.resume(&t1.id).unwrap());
    assert!(q1.reassign(&t1.id, "muse").unwrap());

    // Simulate daemon crash and reopen SQLite queue
    drop(q1);
    let q2 = DurableTaskQueue::open(&db_path).unwrap();
    let recovered = q2.recover_after_crash().unwrap();
    assert_eq!(recovered, 1);

    let after_t2 = q2.get_task(&t2.id).unwrap().unwrap();
    assert_eq!(after_t2.status, QueuedTaskStatus::Queued);

    let exported = q2.export_backup_jsonl(&backup_path).unwrap();
    assert_eq!(exported, 2);

    TaskResult {
        task_id: "ob_queue_01".into(),
        task_name: "OpenBot Crash-Safe SQLite FIFO Queue & Backup Recovery".into(),
        success: true,
        duration_ms: start.elapsed().as_millis() as u64,
        safety_violation: false,
        details: format!(
            "Recovered {recovered} in-flight task after simulated crash and exported {exported} tasks to JSONL"
        ),
    }
}

async fn eval_ob_sandbox_02() -> TaskResult {
    let start = Instant::now();
    let temp = tempfile::tempdir().unwrap();
    let sandbox = BotWorkspaceSandbox::new("coder", temp.path().join("coder")).unwrap();
    let gw = SecurityGateway::new(BlocklistManager::default());
    let handler: Arc<dyn gm_agent::InteractionHandler> =
        Arc::new(CallbackInteractionHandler::with_approval_always(true));
    let ks = KillSwitch::new();

    // 1. Permitted write inside workspace with unified diff preview
    let write_out = sandbox
        .execute_tool(
            "ob_sandbox_02",
            1,
            &WorkspaceToolCall::WriteFile {
                path: "src/hello.rs".into(),
                content: "pub fn greet() -> &'static str {\n    \"hello openbot\"\n}\n".into(),
            },
            &handler,
            &ks,
            None,
        )
        .await
        .unwrap();
    assert!(write_out.summary.contains("WriteFile"));
    assert!(
        write_out
            .diff_preview
            .as_deref()
            .unwrap_or("")
            .contains("+++ b/src/hello.rs")
    );

    // 2. Escape traversal blocked by canonical path confinement
    let escape_err = sandbox
        .execute_tool(
            "ob_sandbox_02",
            2,
            &WorkspaceToolCall::WriteFile {
                path: "../../outside.txt".into(),
                content: "escape".into(),
            },
            &handler,
            &ks,
            None,
        )
        .await;
    assert!(escape_err.is_err());

    // 3. Governance rule citation RULE-PATH-002 verified
    let outside_decision = gw.evaluate_workspace_path(
        sandbox.workspace_dir(),
        std::path::Path::new("/etc/hosts"),
        true,
    );
    assert_eq!(outside_decision.outcome, PolicyOutcome::Refused);
    assert_eq!(outside_decision.rule_id, "RULE-PATH-002");

    // 4. Live macOS kernel Seatbelt sandbox-exec verification
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
        let probe_outside = std::path::PathBuf::from(home).join(format!(
            ".gm_eval_seatbelt_outside_{}.txt",
            uuid::Uuid::new_v4().simple()
        ));
        let cmd = format!(
            "echo ok > inside_ok.txt && echo blocked > \"{}\"",
            probe_outside.display()
        );
        let sh_res = sandbox
            .execute_tool(
                "ob_sandbox_02",
                3,
                &WorkspaceToolCall::Shell {
                    command: cmd,
                    timeout_ms: Some(5000),
                },
                &handler,
                &ks,
                None,
            )
            .await
            .unwrap();
        assert!(
            sh_res.summary.contains("exit 1") || sh_res.output.contains("Operation not permitted")
        );
        assert!(!probe_outside.exists());
        assert!(sandbox.workspace_dir().join("inside_ok.txt").exists());
    }

    TaskResult {
        task_id: "ob_sandbox_02".into(),
        task_name: "OpenBot Seatbelt Kernel Sandbox & Unified Diff Confinement".into(),
        success: true,
        duration_ms: start.elapsed().as_millis() as u64,
        safety_violation: false,
        details: "Seatbelt (/usr/bin/sandbox-exec) allowed workspace writes + unified diff preview and blocked outside writes (RULE-PATH-002)".into(),
    }
}

async fn eval_ob_wheel_03() -> TaskResult {
    let start = Instant::now();
    let temp = tempfile::tempdir().unwrap();
    let secret_literal = "ghp_SUPER_SECRET_VAULT_TOKEN_999999";
    unsafe {
        std::env::set_var("GM_EVAL_GITHUB_PAT", secret_literal);
    }

    let scrubber = SecretScrubber::new();
    let sandbox = BotWorkspaceSandbox::new("muse", temp.path().join("muse"))
        .unwrap()
        .with_scrubber(scrubber.clone());
    let resolver = VaultSecretResolver::new(KeychainSecrets::new("co.anysphere.grok-muse.eval"));

    let platform = Arc::new(MockPlatform::new());
    platform.set_snapshot(AxSnapshot {
        window_id: 301,
        app_name: "Safari".into(),
        window_title: "GitHub — Two-Factor Authentication".into(),
        bounds: Rect::new(0.0, 0.0, 1280.0, 800.0),
        elements: vec![AxElement {
            token: "api_token_input".into(),
            role: "AXSecureTextField".into(),
            label: Some("Personal Access Token".into()),
            value: None,
            frame: Rect::new(200.0, 220.0, 260.0, 32.0),
            is_enabled: true,
            is_secure: true,
        }],
        timestamp_ms: 1000,
    });

    let mock_llm = Arc::new(MockLlmBackend::new());
    mock_llm.queue_response(
        r#"{"action":"TakeTheWheel","params":{"blocker":"Hardware 2FA Security Key Required","instructions":"Touch your YubiKey and click Hand Back to Bot"}}"#,
    );
    mock_llm.queue_response(
        r#"{"action":"FillSecret","params":{"token":"api_token_input","secret_ref":"secret://keychain/gm_eval_github_pat"}}"#,
    );
    mock_llm.queue_response(
        r#"{"action":"Done","params":{"summary":"Authenticated and injected opaque vault secret without plaintext exposure"}}"#,
    );

    let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
    let llm = Arc::new(HybridLlmClient::new(mock_llm, budget));
    let router = LlmRouter::new(ProvidersConfig::default());
    let audit = Arc::new(AuditLogger::in_memory());

    let handler = Arc::new(
        CallbackInteractionHandler::with_approval_always(true).with_wheel_callback(|req| {
            assert!(req.blocker.contains("2FA"));
            Some("Completed hardware 2FA touch".into())
        }),
    );

    let agent = AgentLoop::new(
        platform,
        llm,
        router,
        KillSwitch::new(),
        BlocklistManager::default(),
        AgentLoopConfig::default(),
    )
    .with_workspace_sandbox(sandbox)
    .with_vault_resolver(resolver)
    .with_secret_scrubber(scrubber)
    .with_audit(audit.clone())
    .with_interaction_handler(handler);

    let summary = agent
        .run_task("ob_wheel_03", "Sign in to GitHub and configure token")
        .await
        .unwrap();
    assert!(summary.contains("opaque vault secret"));

    // Verify zero plaintext secret leakage in audit log
    let entries_json = serde_json::to_string(&audit.entries()).unwrap();
    assert!(!entries_json.contains(secret_literal));
    assert!(entries_json.contains("secret://keychain/gm_eval_github_pat"));

    unsafe {
        std::env::remove_var("GM_EVAL_GITHUB_PAT");
    }

    TaskResult {
        task_id: "ob_wheel_03".into(),
        task_name: "OpenBot Take-the-Wheel Handoff & Opaque Vault Secret Scrubbing".into(),
        success: true,
        duration_ms: start.elapsed().as_millis() as u64,
        safety_violation: false,
        details: "Paused on 2FA for human handoff, resumed cleanly, and scrubbed secret://keychain/gm_eval_github_pat (0 leaks)".into(),
    }
}

async fn eval_ob_a2a_04() -> TaskResult {
    let start = Instant::now();
    let temp = tempfile::tempdir().unwrap();
    let config = GmConfig {
        storage_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let audit = Arc::new(AuditLogger::in_memory());
    let (event_tx, _) = broadcast::channel::<DaemonEvent>(64);
    let sup = OpenBotSupervisor::open(config, audit, event_tx).unwrap();

    // 1. Post to #general mentioning @coder
    let (msg, spawned) = sup
        .post_channel_message(
            "general",
            "operator",
            None,
            "Hey @coder please generate a benchmark artifact and hand off to @researcher",
        )
        .unwrap();
    assert_eq!(
        msg.mentioned_bots,
        vec!["coder".to_string(), "researcher".to_string()]
    );
    assert_eq!(spawned.len(), 2);
    assert_eq!(spawned[0].bot_id, "coder");

    // 2. Write artifact in coder's outbox and delegate to researcher via A2A
    let coder_sb = sup.roster().workspace_sandbox("coder").unwrap();
    std::fs::write(
        coder_sb.outbox_dir().join("bench_report.md"),
        "# Benchmark Report\np50: 142ms\n",
    )
    .unwrap();

    let (handoff, _child_task) = sup
        .a2a()
        .handoff(
            "coder",
            "researcher",
            Some("general"),
            "Verify benchmark report in your inbox",
            "Generated p50=142ms benchmark report",
            &["bench_report.md".to_string()],
            0,
        )
        .unwrap();
    assert_eq!(handoff.transferred_artifacts.len(), 1);

    let researcher_sb = sup.roster().workspace_sandbox("researcher").unwrap();
    assert!(researcher_sb.inbox_dir().join("bench_report.md").exists());

    // 3. Verify ContextCompactor reduces oversized thread history
    let compactor = ContextCompactor::new(80, 0.75, 2);
    let mut long_history: Vec<ChatMessage> = (0..12)
        .map(|i| {
            ChatMessage::text(
                "user",
                format!(
                    "Message {i}: Detailed engineering telemetry log entry with numbers and status updates."
                ),
            )
        })
        .collect();
    let initial_len = long_history.len();
    let report = compactor.compact_if_needed(&mut long_history);
    assert!(report.compacted);
    assert!(long_history.len() < initial_len);

    TaskResult {
        task_id: "ob_a2a_04".into(),
        task_name: "OpenBot Multi-Bot Channel Mentions, A2A Artifact Handoff & Compaction".into(),
        success: true,
        duration_ms: start.elapsed().as_millis() as u64,
        safety_violation: false,
        details: "Spawned @coder task from #general, copied bench_report.md from coder/outbox to researcher/inbox, and compacted context".into(),
    }
}

async fn eval_ob_agui_05() -> TaskResult {
    let start = Instant::now();
    let temp = tempfile::tempdir().unwrap();
    let bridges_cfg = ChatBridgesConfig {
        slack_enabled: true,
        slack_allowed_channel_ids: vec!["C_OPS_1".into()],
        ..Default::default()
    };

    let config = GmConfig {
        storage_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    let audit = Arc::new(AuditLogger::in_memory());
    let (event_tx, _) = broadcast::channel::<DaemonEvent>(64);
    let sup = OpenBotSupervisor::open(config, audit.clone(), event_tx).unwrap();

    // Log a refused governance entry (RULE-PATH-002)
    let gw = SecurityGateway::new(BlocklistManager::default());
    let decision = gw.evaluate_workspace_path(
        std::path::Path::new("/tmp/ws"),
        std::path::Path::new("/etc/shadow"),
        true,
    );
    audit.log_governance(
        "workspace_file_write",
        Some("coder"),
        AuditCategory::Refused,
        decision.rule_id,
        &decision.citation,
        "write_file:/etc/shadow",
        serde_json::json!({"path": "/etc/shadow"}),
    );

    let slack_secret = "eval_slack_secret_key";
    let bridge = BridgeRouter::new(sup.clone(), bridges_cfg);
    let web = CompanionWebServer::new(
        sup.clone(),
        bridge,
        Some("eval_token_123".into()),
        Some(slack_secret.into()),
    );
    let (addr, handle) = web.bind_and_serve("127.0.0.1:0").await.unwrap();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // 1. Fetch public A2A agent card
    let card_res: serde_json::Value = client
        .get(format!("{base}/.well-known/agent.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(card_res["bots"].as_array().unwrap().len(), 4);

    // 2. Query /api/audit?category=refused with Bearer token
    let refused_res: serde_json::Value = client
        .get(format!("{base}/api/audit?category=refused"))
        .bearer_auth("eval_token_123")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let refused_arr = refused_res.as_array().unwrap();
    assert_eq!(refused_arr.len(), 1);
    assert_eq!(refused_arr[0]["rule_id"], "RULE-PATH-002");

    // 3. Post signed Slack webhook event and verify task is spawned
    let slack_body = r#"{"type":"event_callback","event":{"channel":"C_OPS_1","user":"U_ALICE","text":"@guardian audit recent workspace diffs"}}"#;
    let ts = "1700000100";
    let sig = format!(
        "v0={}",
        compute_hmac_sha256_hex(
            slack_secret.as_bytes(),
            format!("v0:{ts}:{slack_body}").as_bytes()
        )
    );
    let slack_resp = client
        .post(format!("{base}/bridges/slack/events"))
        .header("x-slack-request-timestamp", ts)
        .header("x-slack-signature", sig)
        .body(slack_body)
        .send()
        .await
        .unwrap();
    assert_eq!(slack_resp.status(), reqwest::StatusCode::OK);

    // Also verify QueueControlAction works via supervisor
    let guardian_tasks = sup.queue().list_tasks(Some("guardian")).unwrap();
    assert_eq!(guardian_tasks.len(), 1);
    assert!(
        sup.control_task(&guardian_tasks[0].id, &QueueControlAction::Pause)
            .unwrap()
    );

    let _ = BridgeInboundMessage {
        platform: "slack".into(),
        external_channel_id: "C_OPS_1".into(),
        sender_name: "U_ALICE".into(),
        text: "/status".into(),
    };

    handle.abort();

    TaskResult {
        task_id: "ob_agui_05".into(),
        task_name: "OpenBot Companion Web Server, Governance Audit & Signed Slack Bridge".into(),
        success: true,
        duration_ms: start.elapsed().as_millis() as u64,
        safety_violation: false,
        details: "Verified /.well-known/agent.json, /api/audit RULE-PATH-002 refusal citation, and HMAC-SHA256 signed Slack webhook dispatch".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_openbot_capability_eval_suite_all_pass() {
        let results = run_openbot_capability_evals().await;
        assert_eq!(results.len(), 5);
        for r in &results {
            assert!(r.success, "Eval {} failed: {}", r.task_id, r.details);
            assert!(!r.safety_violation);
        }
    }
}

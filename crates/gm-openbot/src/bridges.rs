use crate::supervisor::OpenBotSupervisor;
use gm_core::config::ChatBridgesConfig;
use gm_core::error::{GmError, Result};
use gm_core::ipc::QueueControlAction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeInboundMessage {
    pub platform: String,
    pub external_channel_id: String,
    pub sender_name: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeDispatchReply {
    pub accepted: bool,
    pub response_text: String,
    pub spawned_task_ids: Vec<String>,
}

/// Verifies Slack webhook HMAC-SHA256 signature (`v0=<hex>`) over `v0:<timestamp>:<raw_body>`.
pub fn verify_slack_signature(
    signing_secret: &str,
    timestamp: &str,
    raw_body: &str,
    signature_header: &str,
) -> bool {
    let expected = compute_hmac_sha256_hex(
        signing_secret.as_bytes(),
        format!("v0:{timestamp}:{raw_body}").as_bytes(),
    );
    let expected_header = format!("v0={expected}");
    constant_time_eq(
        expected_header.as_bytes(),
        signature_header.trim().as_bytes(),
    )
}

/// RFC 2104 HMAC-SHA256 using `sha2::Sha256` and `hex`.
pub fn compute_hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = Sha256::digest(key);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    hex::encode(outer.finalize())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Routes incoming messages from Telegram, Slack, and Discord into OpenBot channels and queues,
/// enforcing per-platform channel allowlists and supporting remote `/wheel`, `/pause`, `/resume`,
/// and `@bot` commands.
#[derive(Clone)]
pub struct BridgeRouter {
    supervisor: OpenBotSupervisor,
    config: ChatBridgesConfig,
}

impl BridgeRouter {
    pub fn new(supervisor: OpenBotSupervisor, config: ChatBridgesConfig) -> Self {
        Self { supervisor, config }
    }

    pub fn is_channel_allowed(&self, platform: &str, external_channel_id: &str) -> bool {
        match platform.to_lowercase().as_str() {
            "telegram" => {
                self.config.telegram_enabled
                    && (self.config.telegram_allowed_chat_ids.is_empty()
                        || self
                            .config
                            .telegram_allowed_chat_ids
                            .iter()
                            .any(|id| id == external_channel_id))
            }
            "slack" => {
                self.config.slack_enabled
                    && (self.config.slack_allowed_channel_ids.is_empty()
                        || self
                            .config
                            .slack_allowed_channel_ids
                            .iter()
                            .any(|id| id == external_channel_id))
            }
            "discord" => {
                self.config.discord_enabled
                    && (self.config.discord_allowed_channel_ids.is_empty()
                        || self
                            .config
                            .discord_allowed_channel_ids
                            .iter()
                            .any(|id| id == external_channel_id))
            }
            _ => false,
        }
    }

    pub fn handle_inbound(&self, msg: &BridgeInboundMessage) -> Result<BridgeDispatchReply> {
        if !self.is_channel_allowed(&msg.platform, &msg.external_channel_id) {
            return Err(GmError::SafetyViolation(format!(
                "External bridge '{}' channel '{}' is not enabled or not in allowlist",
                msg.platform, msg.external_channel_id
            )));
        }

        let trimmed = msg.text.trim();

        // 1. Check slash commands: `/wheel <task_id> [note]`, `/pause <task_id>`, `/resume <task_id>`
        if let Some(rest) = trimmed.strip_prefix("/wheel ") {
            let mut parts = rest.trim().splitn(2, ' ');
            let task_id = parts.next().unwrap_or("").trim();
            let note = parts.next().map(|s| s.trim());
            if task_id.is_empty() {
                return Ok(BridgeDispatchReply {
                    accepted: false,
                    response_text: "Usage: /wheel <task_id> [note]".into(),
                    spawned_task_ids: vec![],
                });
            }
            let ok = self.supervisor.queue().resume_from_human(task_id, note)?;
            return Ok(BridgeDispatchReply {
                accepted: ok,
                response_text: if ok {
                    format!("🛞 Handed task `{task_id}` back to bot.")
                } else {
                    format!("Task `{task_id}` not found.")
                },
                spawned_task_ids: vec![],
            });
        }

        if let Some(task_id) = trimmed.strip_prefix("/pause ") {
            let clean = task_id.trim();
            let ok = self
                .supervisor
                .control_task(clean, &QueueControlAction::Pause)?;
            return Ok(BridgeDispatchReply {
                accepted: ok,
                response_text: format!("Paused task `{clean}`: {ok}"),
                spawned_task_ids: vec![],
            });
        }

        if let Some(task_id) = trimmed.strip_prefix("/resume ") {
            let clean = task_id.trim();
            let ok = self
                .supervisor
                .control_task(clean, &QueueControlAction::Resume)?;
            return Ok(BridgeDispatchReply {
                accepted: ok,
                response_text: format!("Resumed task `{clean}`: {ok}"),
                spawned_task_ids: vec![],
            });
        }

        // 2. Post into OpenBot `#general` channel and trigger any `@bot` mentions or channel owner
        let author = format!("{}:{}", msg.platform, msg.sender_name);
        let (_, spawned) = self
            .supervisor
            .post_channel_message("general", &author, None, trimmed)?;
        let ids: Vec<String> = spawned.iter().map(|t| t.id.clone()).collect();

        Ok(BridgeDispatchReply {
            accepted: true,
            response_text: if ids.is_empty() {
                "Posted to #general".into()
            } else {
                format!("Queued task(s): {}", ids.join(", "))
            },
            spawned_task_ids: ids,
        })
    }
}

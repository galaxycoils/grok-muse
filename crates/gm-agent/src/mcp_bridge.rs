use crate::approval::{ApprovalRequest, InteractionHandler};
use darius_tools::{McpClient, McpToolDef, ToolOutcome};
use gm_core::error::{GmError, Result};
use gm_core::types::ComputerAction;
use gm_safety::{ActionTier, AuditLogger};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

/// Parsed MCP tool invocation emitted by the LLM inside a hybrid GUI + MCP agent step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInvocation {
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Bridge uniting `darius-tools` MCP tool execution with Grok-Muse's `ActionTier`
/// interactive approval gate and `AuditLogger`.
#[derive(Clone)]
pub struct McpToolBridge {
    client: Arc<dyn McpClient>,
}

impl McpToolBridge {
    pub fn new(client: Arc<dyn McpClient>) -> Self {
        Self { client }
    }

    pub fn list_tools(&self) -> Vec<McpToolDef> {
        self.client.list_tools().unwrap_or_default()
    }

    /// Formats available MCP tools into prompt instructions for `AgentLoop`.
    pub fn format_tools_prompt(&self) -> String {
        let tools = self.list_tools();
        if tools.is_empty() {
            return String::new();
        }

        let mut out = String::from(
            "\n\n## Available Hybrid MCP Tools\nPrefer invoking an MCP tool over GUI clicking when it directly accomplishes a sub-step. Emit JSON: `{\"action\": \"McpTool\", \"params\": {\"name\": \"<tool_name>\", \"arguments\": {...}}}`.\n",
        );
        for t in tools {
            let mode = if t.read_only { "read-only" } else { "mutating" };
            out.push_str(&format!("- `{}` ({mode}): {}\n", t.name, t.description));
        }
        out
    }

    /// Classifies an MCP tool call under the unified Grok-Muse `ActionTier` policy.
    pub fn classify_tier(&self, inv: &McpToolInvocation) -> ActionTier {
        let tools = self.list_tools();
        let def = tools.iter().find(|t| t.name == inv.name);
        classify_mcp_tool_tier(&inv.name, &inv.arguments, def)
    }

    /// Executes an MCP tool call after enforcing `ActionTier` approval and logging to `AuditLogger`.
    pub async fn execute_governed(
        &self,
        task_id: &str,
        step: u32,
        inv: &McpToolInvocation,
        interaction_handler: &Arc<dyn InteractionHandler>,
        audit: Option<&Arc<AuditLogger>>,
    ) -> Result<String> {
        let tier = self.classify_tier(inv);

        if tier.requires_confirmation() {
            let synthetic_action = ComputerAction::Click {
                token: Some(format!("mcp:{}", inv.name)),
                coords: None,
                button: None,
                click_count: None,
            };
            let req = ApprovalRequest {
                task_id: task_id.to_string(),
                step,
                action: synthetic_action,
                target_label: Some(format!("MCP Tool '{}'", inv.name)),
                reason: format!(
                    "Irreversible MCP tool '{}' with arguments {} requires user confirmation",
                    inv.name, inv.arguments
                ),
            };

            let approved = interaction_handler.request_approval(&req).await?;
            if approved {
                info!("MCP tool '{}' approved by user", inv.name);
                if let Some(logger) = audit {
                    logger.log(
                        "action_approved",
                        None,
                        None,
                        None,
                        None,
                        Some(format!("User approved MCP tool '{}'", inv.name)),
                        serde_json::json!({
                            "task_id": task_id,
                            "step": step,
                            "mcp_tool": inv.name,
                            "tier": "Irreversible",
                        }),
                    );
                }
            } else {
                warn!("MCP tool '{}' denied by user", inv.name);
                if let Some(logger) = audit {
                    logger.log(
                        "action_denied",
                        None,
                        None,
                        None,
                        None,
                        Some(format!("User denied MCP tool '{}'", inv.name)),
                        serde_json::json!({
                            "task_id": task_id,
                            "step": step,
                            "mcp_tool": inv.name,
                            "tier": "Irreversible",
                        }),
                    );
                }
                return Err(GmError::SafetyViolation(format!(
                    "Irreversible MCP tool '{}' requires user confirmation and was denied",
                    inv.name
                )));
            }
        }

        let client = Arc::clone(&self.client);
        let name = inv.name.clone();
        let args = inv.arguments.clone();
        let outcome = tokio::task::spawn_blocking(move || client.call_tool(&name, &args))
            .await
            .map_err(|e| GmError::Platform(format!("MCP task join error: {e}")))?
            .map_err(|e| GmError::Platform(format!("MCP tool '{}' failed: {e}", inv.name)))?;

        let summary = match outcome {
            ToolOutcome::Ok { preview, .. } => format!("MCP '{}' ok: {}", inv.name, preview),
            ToolOutcome::Err { message } => format!("MCP '{}' error: {}", inv.name, message),
            ToolOutcome::Interrupted => format!("MCP '{}' interrupted", inv.name),
            ToolOutcome::TimedOut => format!("MCP '{}' timed out", inv.name),
        };

        if let Some(logger) = audit {
            logger.log(
                "mcp_tool_call",
                None,
                None,
                None,
                None,
                Some(summary.clone()),
                serde_json::json!({
                    "task_id": task_id,
                    "step": step,
                    "mcp_tool": inv.name,
                    "tier": format!("{tier:?}"),
                }),
            );
        }

        Ok(summary)
    }
}

/// Classifies an MCP tool into `ActionTier::Read`, `Reversible`, or `Irreversible`.
pub fn classify_mcp_tool_tier(
    tool_name: &str,
    arguments: &serde_json::Value,
    def: Option<&McpToolDef>,
) -> ActionTier {
    let lower_name = tool_name.to_lowercase();
    let lower_args = arguments.to_string().to_lowercase();

    let is_destructive = [
        "delete",
        "remove",
        "drop",
        "destroy",
        "erase",
        "send_email",
        "send_message",
        "publish",
        "pay",
        "purchase",
        "checkout",
        "transfer",
        "force_push",
    ]
    .iter()
    .any(|kw| lower_name.contains(kw))
        || lower_args.contains("rm -rf")
        || lower_args.contains("drop table")
        || lower_args.contains("git push --force");

    if is_destructive {
        return ActionTier::Irreversible;
    }

    if let Some(d) = def
        && d.read_only
    {
        return ActionTier::Read;
    }

    if lower_name.starts_with("read_")
        || lower_name.starts_with("get_")
        || lower_name.starts_with("list_")
        || lower_name.starts_with("search_")
        || lower_name.starts_with("fetch_")
    {
        return ActionTier::Read;
    }

    ActionTier::Reversible
}

/// Parses an LLM JSON response to check if it represents a hybrid MCP tool call
/// (`{"action": "McpTool", "params": {"name": "...", "arguments": {...}}}`
/// or `{"mcp_tool": "...", "arguments": {...}}`).
pub fn try_parse_mcp_tool_call(raw_json: &str) -> Option<McpToolInvocation> {
    let val: serde_json::Value = serde_json::from_str(raw_json.trim()).ok()?;
    let obj = val.as_object()?;

    if let Some(action) = obj.get("action").and_then(|v| v.as_str())
        && action.eq_ignore_ascii_case("mcptool")
    {
        let params = obj.get("params")?;
        let name = params.get("name")?.as_str()?.to_string();
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return Some(McpToolInvocation { name, arguments });
    }

    if let Some(name) = obj.get("mcp_tool").and_then(|v| v.as_str()) {
        let arguments = obj
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return Some(McpToolInvocation {
            name: name.to_string(),
            arguments,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{CallbackInteractionHandler, NonInteractiveHandler};
    use darius_tools::LocalMcpClient;

    #[tokio::test]
    async fn test_mcp_bridge_unified_safety_tiers_and_governance() {
        let local = Arc::new(LocalMcpClient::new());
        local.add_tool(McpToolDef {
            name: "read_calendar".into(),
            description: "Reads calendar entries".into(),
            input_schema: serde_json::json!({"type": "object"}),
            requires_prior_success: false,
            read_only: true,
        });
        local.add_tool(McpToolDef {
            name: "send_email_message".into(),
            description: "Sends an external email".into(),
            input_schema: serde_json::json!({"type": "object"}),
            requires_prior_success: false,
            read_only: false,
        });

        let bridge = McpToolBridge::new(local);
        let audit = Arc::new(AuditLogger::in_memory());

        // 1. Read-only tool runs automatically even with NonInteractiveHandler
        let read_inv = McpToolInvocation {
            name: "read_calendar".into(),
            arguments: serde_json::json!({"day": "today"}),
        };
        assert_eq!(bridge.classify_tier(&read_inv), ActionTier::Read);

        let non_interactive: Arc<dyn InteractionHandler> = Arc::new(NonInteractiveHandler);
        let out = bridge
            .execute_governed("t1", 1, &read_inv, &non_interactive, Some(&audit))
            .await
            .unwrap();
        assert!(out.contains("MCP 'read_calendar' ok"));

        // 2. Irreversible MCP tool is blocked when denied
        let send_inv = McpToolInvocation {
            name: "send_email_message".into(),
            arguments: serde_json::json!({"to": "ceo@corp.com"}),
        };
        assert_eq!(bridge.classify_tier(&send_inv), ActionTier::Irreversible);
        let denied = bridge
            .execute_governed("t1", 2, &send_inv, &non_interactive, Some(&audit))
            .await;
        assert!(denied.is_err());

        // 3. Irreversible MCP tool succeeds when approved via InteractionHandler
        let approve_handler: Arc<dyn InteractionHandler> =
            Arc::new(CallbackInteractionHandler::with_approval_always(true));
        let approved_out = bridge
            .execute_governed("t1", 3, &send_inv, &approve_handler, Some(&audit))
            .await
            .unwrap();
        assert!(approved_out.contains("MCP 'send_email_message' ok"));

        // Verify JSON parser formats
        let parsed = try_parse_mcp_tool_call(
            r#"{"action":"McpTool","params":{"name":"read_calendar","arguments":{"day":"tomorrow"}}}"#,
        )
        .unwrap();
        assert_eq!(parsed.name, "read_calendar");
    }
}

use crate::approval::{ApprovalRequest, InteractionHandler};
use gm_core::error::{GmError, Result};
use gm_core::types::ComputerAction;
use gm_safety::{
    AuditCategory, AuditLogger, BlocklistManager, KillSwitch, PolicyOutcome, SecretScrubber,
    SecurityGateway,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkspaceToolCall {
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    ListFiles {
        path: Option<String>,
    },
    Shell {
        command: String,
        timeout_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceToolOutcome {
    pub summary: String,
    pub output: String,
    pub diff_preview: Option<String>,
}

/// Generates a line-level unified diff preview capped at `max_lines` (default 200),
/// matching Darius's write diff preview contract.
pub fn compute_unified_diff_preview(
    rel_path: &str,
    previous: Option<&str>,
    updated: &str,
    max_lines: usize,
) -> String {
    let Some(old) = previous else {
        let line_count = updated.lines().count();
        return format!(
            "+++ b/{rel_path} (new file, {line_count} lines, {} bytes)",
            updated.len()
        );
    };
    if old.is_empty() {
        let line_count = updated.lines().count();
        return format!(
            "+++ b/{rel_path} (populated empty file, {line_count} lines, {} bytes)",
            updated.len()
        );
    }

    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = updated.lines().collect();

    let mut out = Vec::new();
    out.push(format!("--- a/{rel_path}"));
    out.push(format!("+++ b/{rel_path}"));

    let max_idx = old_lines.len().max(new_lines.len());
    for i in 0..max_idx {
        if out.len() >= max_lines {
            out.push(format!("... (diff truncated at {max_lines} lines)"));
            break;
        }
        match (old_lines.get(i), new_lines.get(i)) {
            (Some(a), Some(b)) if a == b => {
                out.push(format!(" {a}"));
            }
            (Some(a), Some(b)) => {
                out.push(format!("-{a}"));
                if out.len() < max_lines {
                    out.push(format!("+{b}"));
                }
            }
            (Some(a), None) => {
                out.push(format!("-{a}"));
            }
            (None, Some(b)) => {
                out.push(format!("+{b}"));
            }
            (None, None) => {}
        }
    }

    out.join("\n")
}

/// Per-bot isolated workspace & macOS Seatbelt (`sandbox-exec`) execution environment.
#[derive(Debug, Clone)]
pub struct BotWorkspaceSandbox {
    bot_id: String,
    bot_root: PathBuf,
    workspace_dir: PathBuf,
    inbox_dir: PathBuf,
    outbox_dir: PathBuf,
    gateway: SecurityGateway,
    scrubber: SecretScrubber,
    allow_network: bool,
}

impl BotWorkspaceSandbox {
    pub fn new(bot_id: impl Into<String>, bot_root: impl AsRef<Path>) -> Result<Self> {
        let bot_id = bot_id.into();
        let root = bot_root.as_ref().to_path_buf();
        let workspace_dir = root.join("workspace");
        let inbox_dir = root.join("inbox");
        let outbox_dir = root.join("outbox");

        std::fs::create_dir_all(&workspace_dir)?;
        std::fs::create_dir_all(&inbox_dir)?;
        std::fs::create_dir_all(&outbox_dir)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        }

        let canonical_root = root.canonicalize().unwrap_or(root);
        let workspace_dir = canonical_root.join("workspace");
        let inbox_dir = canonical_root.join("inbox");
        let outbox_dir = canonical_root.join("outbox");

        Ok(Self {
            bot_id,
            bot_root: canonical_root,
            workspace_dir,
            inbox_dir,
            outbox_dir,
            gateway: SecurityGateway::new(BlocklistManager::new(&[])),
            scrubber: SecretScrubber::new(),
            allow_network: true,
        })
    }

    pub fn with_scrubber(mut self, scrubber: SecretScrubber) -> Self {
        self.scrubber = scrubber;
        self
    }

    pub fn with_network(mut self, allow_network: bool) -> Self {
        self.allow_network = allow_network;
        self
    }

    pub fn bot_id(&self) -> &str {
        &self.bot_id
    }

    pub fn bot_root(&self) -> &Path {
        &self.bot_root
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    pub fn inbox_dir(&self) -> &Path {
        &self.inbox_dir
    }

    pub fn outbox_dir(&self) -> &Path {
        &self.outbox_dir
    }

    /// Generate the macOS Seatbelt SBPL profile confining all file writes strictly to this bot's
    /// `workspace/` and `outbox/` directories (plus `/private/tmp`).
    pub fn generate_seatbelt_profile(&self) -> String {
        let ws = self
            .workspace_dir
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let outbox = self
            .outbox_dir
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let net_rule = if self.allow_network {
            "(allow network*)"
        } else {
            "(deny network*)"
        };

        format!(
            r#"(version 1)
(allow default)
{net_rule}
(deny file-write*)
(allow file-write*
    (subpath "{ws}")
    (subpath "{outbox}")
    (subpath "/private/tmp")
    (subpath "/tmp")
    (subpath "/private/var/folders")
    (subpath "/var/folders")
    (literal "/dev/null")
    (literal "/dev/zero")
    (literal "/dev/tty")
    (literal "/dev/urandom")
    (literal "/dev/random"))
"#
        )
    }

    /// Resolve a relative path inside the bot root (`workspace/` by default unless prefixed with `inbox/` or `outbox/`).
    fn resolve_confined_path(
        &self,
        rel_or_abs: &str,
        is_write: bool,
        audit: Option<&Arc<AuditLogger>>,
    ) -> Result<PathBuf> {
        let candidate = Path::new(rel_or_abs);
        let mapped = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else if rel_or_abs.starts_with("workspace/")
            || rel_or_abs.starts_with("inbox/")
            || rel_or_abs.starts_with("outbox/")
            || rel_or_abs == "workspace"
            || rel_or_abs == "inbox"
            || rel_or_abs == "outbox"
        {
            self.bot_root.join(candidate)
        } else {
            self.workspace_dir.join(candidate)
        };

        let decision = self
            .gateway
            .evaluate_workspace_path(&self.bot_root, &mapped, is_write);

        if decision.outcome == PolicyOutcome::Refused {
            if let Some(logger) = audit {
                logger.log_governance(
                    if is_write {
                        "workspace_write_refused"
                    } else {
                        "workspace_read_refused"
                    },
                    Some(&self.bot_id),
                    AuditCategory::Refused,
                    decision.rule_id,
                    &decision.citation,
                    format!("Refused path '{rel_or_abs}'"),
                    serde_json::json!({ "path": rel_or_abs }),
                );
            }
            return Err(GmError::SafetyViolation(format!(
                "[{}] {}",
                decision.rule_id, decision.citation
            )));
        }

        // Write operations additionally may not write into `inbox/` (reserved for incoming A2A artifacts)
        if is_write && mapped.starts_with(&self.inbox_dir) {
            let msg = format!(
                "[RULE-PATH-002] Bot '{}' cannot overwrite read-only inbox path '{}'",
                self.bot_id, rel_or_abs
            );
            if let Some(logger) = audit {
                logger.log_governance(
                    "workspace_write_refused",
                    Some(&self.bot_id),
                    AuditCategory::Refused,
                    "RULE-PATH-002",
                    &msg,
                    format!("Refused inbox write '{rel_or_abs}'"),
                    serde_json::json!({ "path": rel_or_abs }),
                );
            }
            return Err(GmError::SafetyViolation(msg));
        }

        Ok(mapped)
    }

    pub async fn execute_tool(
        &self,
        task_id: &str,
        step: u32,
        call: &WorkspaceToolCall,
        interaction_handler: &Arc<dyn InteractionHandler>,
        kill_switch: &KillSwitch,
        audit: Option<&Arc<AuditLogger>>,
    ) -> Result<WorkspaceToolOutcome> {
        match call {
            WorkspaceToolCall::ReadFile { path } => {
                let target = self.resolve_confined_path(path, false, audit)?;
                let content = std::fs::read_to_string(&target).map_err(GmError::Io)?;
                let scrubbed = self.scrubber.scrub_str(&content);
                if let Some(logger) = audit {
                    logger.log_governance(
                        "workspace_read",
                        Some(&self.bot_id),
                        AuditCategory::Permitted,
                        "RULE-STD-000",
                        "Confined workspace file read permitted",
                        format!("ReadFile({path})"),
                        serde_json::json!({ "task_id": task_id, "step": step, "path": path, "bytes": scrubbed.len() }),
                    );
                }
                Ok(WorkspaceToolOutcome {
                    summary: format!("ReadFile({path}) -> {} bytes", scrubbed.len()),
                    output: scrubbed,
                    diff_preview: None,
                })
            }

            WorkspaceToolCall::WriteFile { path, content } => {
                let target = self.resolve_confined_path(path, true, audit)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let previous = std::fs::read_to_string(&target).ok();
                let diff = compute_unified_diff_preview(path, previous.as_deref(), content, 200);
                std::fs::write(&target, content)?;
                let scrubbed_diff = self.scrubber.scrub_str(&diff);
                if let Some(logger) = audit {
                    logger.log_governance(
                        "workspace_write",
                        Some(&self.bot_id),
                        AuditCategory::Permitted,
                        "RULE-STD-000",
                        "Confined workspace file write permitted",
                        format!("WriteFile({path})"),
                        serde_json::json!({
                            "task_id": task_id,
                            "step": step,
                            "path": path,
                            "bytes": content.len(),
                            "diff_preview": scrubbed_diff,
                        }),
                    );
                }
                Ok(WorkspaceToolOutcome {
                    summary: format!("WriteFile({path}) -> {} bytes written", content.len()),
                    output: format!("Wrote {} bytes to {path}\n{scrubbed_diff}", content.len()),
                    diff_preview: Some(scrubbed_diff),
                })
            }

            WorkspaceToolCall::ListFiles { path } => {
                let rel = path.as_deref().unwrap_or(".");
                let target = self.resolve_confined_path(rel, false, audit)?;
                let mut names = Vec::new();
                if target.is_dir() {
                    for entry in std::fs::read_dir(&target)? {
                        let entry = entry?;
                        let file_name = entry.file_name().to_string_lossy().to_string();
                        let suffix = if entry.path().is_dir() { "/" } else { "" };
                        names.push(format!("{file_name}{suffix}"));
                    }
                    names.sort();
                }
                let listing = names.join("\n");
                if let Some(logger) = audit {
                    logger.log_governance(
                        "workspace_list",
                        Some(&self.bot_id),
                        AuditCategory::Permitted,
                        "RULE-STD-000",
                        "Confined workspace directory listing permitted",
                        format!("ListFiles({rel})"),
                        serde_json::json!({ "task_id": task_id, "step": step, "count": names.len() }),
                    );
                }
                Ok(WorkspaceToolOutcome {
                    summary: format!("ListFiles({rel}) -> {} entries", names.len()),
                    output: listing,
                    diff_preview: None,
                })
            }

            WorkspaceToolCall::Shell {
                command,
                timeout_ms,
            } => {
                let decision = self.gateway.evaluate_shell_command(command);
                match decision.outcome {
                    PolicyOutcome::Refused => {
                        if let Some(logger) = audit {
                            logger.log_governance(
                                "workspace_shell_refused",
                                Some(&self.bot_id),
                                AuditCategory::Refused,
                                decision.rule_id,
                                &decision.citation,
                                format!(
                                    "Refused shell command: {}",
                                    self.scrubber.scrub_str(command)
                                ),
                                serde_json::json!({ "task_id": task_id, "step": step }),
                            );
                        }
                        return Err(GmError::SafetyViolation(format!(
                            "[{}] {}",
                            decision.rule_id, decision.citation
                        )));
                    }
                    PolicyOutcome::NeedsApproval => {
                        let req = ApprovalRequest {
                            task_id: task_id.to_string(),
                            step,
                            action: ComputerAction::Type {
                                text: self.scrubber.scrub_str(command),
                            },
                            target_label: Some(format!("WorkspaceShell (bot={})", self.bot_id)),
                            reason: format!("[{}] {}", decision.rule_id, decision.citation),
                        };
                        let approved = interaction_handler.request_approval(&req).await?;
                        if !approved {
                            if let Some(logger) = audit {
                                logger.log_governance(
                                    "workspace_shell_denied",
                                    Some(&self.bot_id),
                                    AuditCategory::Refused,
                                    decision.rule_id,
                                    &decision.citation,
                                    format!(
                                        "Operator denied shell command: {}",
                                        self.scrubber.scrub_str(command)
                                    ),
                                    serde_json::json!({ "task_id": task_id, "step": step }),
                                );
                            }
                            return Err(GmError::SafetyViolation(format!(
                                "[{}] Operator denied shell command",
                                decision.rule_id
                            )));
                        }
                    }
                    PolicyOutcome::Permitted => {}
                }

                let timeout =
                    Duration::from_millis(timeout_ms.unwrap_or(15_000).clamp(100, 120_000));
                let profile = self.generate_seatbelt_profile();

                let mut cmd =
                    if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
                        let mut c = Command::new("/usr/bin/sandbox-exec");
                        c.args(["-p", &profile, "/bin/zsh", "-c", command]);
                        c
                    } else {
                        let mut c = Command::new("/bin/sh");
                        c.args(["-c", command]);
                        c
                    };

                cmd.current_dir(&self.workspace_dir)
                    .kill_on_drop(true)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                let child = cmd.spawn().map_err(GmError::Io)?;
                let output_fut = child.wait_with_output();

                let output = tokio::select! {
                    res = tokio::time::timeout(timeout, output_fut) => {
                        match res {
                            Ok(Ok(out)) => out,
                            Ok(Err(e)) => return Err(GmError::Io(e)),
                            Err(_) => {
                                if let Some(logger) = audit {
                                    logger.log_governance(
                                        "workspace_shell_timeout",
                                        Some(&self.bot_id),
                                        AuditCategory::Failed,
                                        decision.rule_id,
                                        "Shell command exceeded timeout",
                                        format!("Shell timed out after {}ms", timeout.as_millis()),
                                        serde_json::json!({ "task_id": task_id, "step": step }),
                                    );
                                }
                                return Err(GmError::Platform(format!(
                                    "Workspace shell command timed out after {}ms",
                                    timeout.as_millis()
                                )));
                            }
                        }
                    }
                    _ = async {
                        while !kill_switch.is_triggered() {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    } => {
                        return Err(GmError::SafetyViolation("Workspace shell halted by Kill Switch".into()));
                    }
                };

                let exit_code = output.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = if stderr.trim().is_empty() {
                    stdout.to_string()
                } else if stdout.trim().is_empty() {
                    stderr.to_string()
                } else {
                    format!("{stdout}\n[stderr]\n{stderr}")
                };

                let scrubbed_out = self.scrubber.scrub_str(combined.trim());
                let scrubbed_cmd = self.scrubber.scrub_str(command);

                if let Some(logger) = audit {
                    let cat = if exit_code == 0 {
                        AuditCategory::Permitted
                    } else if combined.contains("Operation not permitted") {
                        AuditCategory::Refused
                    } else {
                        AuditCategory::Failed
                    };
                    let rule = if cat == AuditCategory::Refused {
                        "RULE-PATH-002"
                    } else {
                        decision.rule_id
                    };
                    let citation = if cat == AuditCategory::Refused {
                        "macOS Seatbelt sandbox blocked write outside bot workspace"
                    } else {
                        &decision.citation
                    };
                    logger.log_governance(
                        "workspace_shell",
                        Some(&self.bot_id),
                        cat,
                        rule,
                        citation,
                        format!("Shell({scrubbed_cmd}) -> exit {exit_code}"),
                        serde_json::json!({
                            "task_id": task_id,
                            "step": step,
                            "exit_code": exit_code,
                        }),
                    );
                }

                Ok(WorkspaceToolOutcome {
                    summary: format!("Shell({scrubbed_cmd}) -> exit {exit_code}"),
                    output: scrubbed_out,
                    diff_preview: None,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::NonInteractiveHandler;

    #[tokio::test]
    async fn test_workspace_read_write_diff_and_path_confinement() {
        let dir = std::env::temp_dir().join(format!("bot_ws_test_{}", uuid::Uuid::new_v4()));
        let sandbox = BotWorkspaceSandbox::new("coder", &dir).unwrap();
        let handler: Arc<dyn InteractionHandler> = Arc::new(NonInteractiveHandler);
        let ks = KillSwitch::new();
        let audit = Arc::new(AuditLogger::in_memory());

        // 1. First write generates new-file diff preview
        let w1 = sandbox
            .execute_tool(
                "t1",
                1,
                &WorkspaceToolCall::WriteFile {
                    path: "hello.txt".into(),
                    content: "line 1\nline 2\n".into(),
                },
                &handler,
                &ks,
                Some(&audit),
            )
            .await
            .unwrap();
        assert!(w1.diff_preview.as_deref().unwrap().contains("new file"));

        // 2. Second write generates unified line diff
        let w2 = sandbox
            .execute_tool(
                "t1",
                2,
                &WorkspaceToolCall::WriteFile {
                    path: "hello.txt".into(),
                    content: "line 1\nline 2 updated\nline 3\n".into(),
                },
                &handler,
                &ks,
                Some(&audit),
            )
            .await
            .unwrap();
        let diff = w2.diff_preview.unwrap();
        assert!(diff.contains("-line 2"));
        assert!(diff.contains("+line 2 updated"));

        // 3. Escape attempt (`../../outside.txt`) is refused with RULE-PATH-002
        let escape = sandbox
            .execute_tool(
                "t1",
                3,
                &WorkspaceToolCall::WriteFile {
                    path: "../../outside.txt".into(),
                    content: "pwned".into(),
                },
                &handler,
                &ks,
                Some(&audit),
            )
            .await;
        assert!(escape.is_err());
        assert!(escape.unwrap_err().to_string().contains("RULE-PATH-002"));

        let refused =
            audit.query_governance_entries(Some(AuditCategory::Refused), Some("coder"), 10);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].rule_id, "RULE-PATH-002");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_macos_seatbelt_sandbox_exec_blocks_outside_writes() {
        let dir = std::env::temp_dir().join(format!("bot_seatbelt_test_{}", uuid::Uuid::new_v4()));
        let sandbox = BotWorkspaceSandbox::new("coder", &dir).unwrap();
        let handler: Arc<dyn InteractionHandler> = Arc::new(NonInteractiveHandler);
        let ks = KillSwitch::new();
        let audit = Arc::new(AuditLogger::in_memory());

        // 1. Writing inside workspace succeeds under sandbox-exec
        let ok_res = sandbox
            .execute_tool(
                "t-sb",
                1,
                &WorkspaceToolCall::Shell {
                    command: "echo 'inside_ok' > allowed.txt && cat allowed.txt".into(),
                    timeout_ms: Some(5000),
                },
                &handler,
                &ks,
                Some(&audit),
            )
            .await
            .unwrap();
        assert!(ok_res.output.contains("inside_ok"));
        assert!(sandbox.workspace_dir().join("allowed.txt").exists());

        // 2. Writing to $HOME outside bot workspace is blocked by kernel Seatbelt policy!
        let home = std::env::var("HOME").unwrap();
        let forbidden_file = format!(
            "{home}/.grok_muse_seatbelt_escape_probe_{}",
            uuid::Uuid::new_v4()
        );
        let blocked_res = sandbox
            .execute_tool(
                "t-sb",
                2,
                &WorkspaceToolCall::Shell {
                    command: format!("echo 'escape' > \"{forbidden_file}\""),
                    timeout_ms: Some(5000),
                },
                &handler,
                &ks,
                Some(&audit),
            )
            .await
            .unwrap();
        assert!(!Path::new(&forbidden_file).exists());
        assert!(
            blocked_res.summary.contains("exit 1")
                || blocked_res.output.contains("Operation not permitted")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

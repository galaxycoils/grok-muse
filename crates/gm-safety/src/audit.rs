use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub event_type: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub action_summary: Option<String>,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct AuditLogger {
    log_path: Option<PathBuf>,
    in_memory: Arc<Mutex<Vec<AuditEntry>>>,
}

impl AuditLogger {
    pub fn in_memory() -> Self {
        Self {
            log_path: None,
            in_memory: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let p = path.as_ref().to_path_buf();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut loaded = Vec::new();
        if p.exists()
            && let Ok(file) = std::fs::File::open(&p)
        {
            let reader = std::io::BufReader::new(file);
            for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
                let trimmed = line.trim();
                if !trimmed.is_empty()
                    && let Ok(entry) = serde_json::from_str::<AuditEntry>(trimmed)
                {
                    loaded.push(entry);
                }
            }
        }

        Ok(Self {
            log_path: Some(p),
            in_memory: Arc::new(Mutex::new(loaded)),
        })
    }

    pub fn default_logger() -> std::io::Result<Self> {
        let dir = if let Ok(custom) = std::env::var("GROK_MUSE_DATA_DIR") {
            PathBuf::from(custom)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join("Library/Application Support/GrokMuse")
        } else {
            PathBuf::from(".")
        };
        Self::with_file(dir.join("audit.jsonl"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &self,
        event_type: impl Into<String>,
        provider: Option<String>,
        model: Option<String>,
        tokens: Option<u32>,
        cost_usd: Option<f64>,
        action_summary: Option<String>,
        details: serde_json::Value,
    ) {
        let entry = AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            event_type: event_type.into(),
            provider,
            model,
            tokens,
            cost_usd,
            action_summary,
            details,
        };

        self.in_memory.lock().push(entry.clone());

        if let Some(path) = &self.log_path {
            let _ = (|| -> std::io::Result<()> {
                let line = serde_json::to_string(&entry)?;
                let mut f = OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(f, "{line}")?;
                Ok(())
            })();
        }
    }

    pub fn entries(&self) -> Vec<AuditEntry> {
        self.in_memory.lock().clone()
    }

    pub fn generate_report(&self, period: AuditPeriod, now: chrono::DateTime<Utc>) -> AuditReport {
        synthesize_audit_report(&self.entries(), period, now)
    }

    /// Log a structured OpenBot governance decision (Permitted, Refused, or Failed) with rule citation.
    #[allow(clippy::too_many_arguments)]
    pub fn log_governance(
        &self,
        event_type: impl Into<String>,
        bot_id: Option<&str>,
        category: AuditCategory,
        rule_id: &str,
        citation: &str,
        action_summary: impl Into<String>,
        mut extra_details: serde_json::Value,
    ) {
        if let Some(obj) = extra_details.as_object_mut() {
            obj.insert(
                "governance_category".into(),
                serde_json::json!(category.as_str()),
            );
            obj.insert("policy_rule_id".into(), serde_json::json!(rule_id));
            obj.insert("policy_citation".into(), serde_json::json!(citation));
            if let Some(b) = bot_id {
                obj.insert("bot_id".into(), serde_json::json!(b));
            }
        }
        self.log(
            event_type,
            None,
            None,
            None,
            None,
            Some(action_summary.into()),
            extra_details,
        );
    }

    /// Query governance audit entries filtered by category (`Permitted`, `Refused`, `Failed`) and `bot_id`.
    pub fn query_governance_entries(
        &self,
        category_filter: Option<AuditCategory>,
        bot_filter: Option<&str>,
        limit: usize,
    ) -> Vec<GovernanceAuditRecord> {
        let entries = self.entries();
        let mut results = Vec::new();

        for entry in entries.into_iter().rev() {
            let inferred_cat = entry
                .details
                .get("governance_category")
                .and_then(|v| v.as_str())
                .and_then(|s| match s {
                    "permitted" => Some(AuditCategory::Permitted),
                    "refused" => Some(AuditCategory::Refused),
                    "failed" => Some(AuditCategory::Failed),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    if entry.event_type.contains("denied")
                        || entry.event_type.contains("blocked")
                        || entry.event_type.contains("refused")
                    {
                        AuditCategory::Refused
                    } else if entry.event_type.contains("failed")
                        || entry.event_type.contains("error")
                    {
                        AuditCategory::Failed
                    } else {
                        AuditCategory::Permitted
                    }
                });

            if let Some(cat) = category_filter
                && inferred_cat != cat
            {
                continue;
            }

            let entry_bot = entry
                .details
                .get("bot_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if let Some(bf) = bot_filter
                && entry_bot.as_deref() != Some(bf)
            {
                continue;
            }

            let rule_id = entry
                .details
                .get("policy_rule_id")
                .and_then(|v| v.as_str())
                .unwrap_or("RULE-STD-000")
                .to_string();

            let citation = entry
                .details
                .get("policy_citation")
                .and_then(|v| v.as_str())
                .or_else(|| entry.details.get("reason").and_then(|v| v.as_str()))
                .unwrap_or("Standard governance policy")
                .to_string();

            results.push(GovernanceAuditRecord {
                timestamp: entry.timestamp,
                event_type: entry.event_type,
                bot_id: entry_bot,
                category: inferred_cat,
                rule_id,
                citation,
                action_summary: entry.action_summary.unwrap_or_default(),
                details: entry.details,
            });

            if results.len() >= limit {
                break;
            }
        }

        results
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditCategory {
    Permitted,
    Refused,
    Failed,
}

impl AuditCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditCategory::Permitted => "permitted",
            AuditCategory::Refused => "refused",
            AuditCategory::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceAuditRecord {
    pub timestamp: String,
    pub event_type: String,
    pub bot_id: Option<String>,
    pub category: AuditCategory,
    pub rule_id: String,
    pub citation: String,
    pub action_summary: String,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditPeriod {
    Weekly,
    Monthly,
}

impl std::str::FromStr for AuditPeriod {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "weekly" | "week" | "7d" => Ok(AuditPeriod::Weekly),
            "monthly" | "month" | "30d" => Ok(AuditPeriod::Monthly),
            _ => Err(format!(
                "Unknown period '{s}'. Expected 'weekly' or 'monthly'."
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderUsageSummary {
    pub provider: String,
    pub model: String,
    pub calls: usize,
    pub tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyComplianceSummary {
    pub total_safety_events: usize,
    pub actions_by_tier: std::collections::BTreeMap<String, usize>,
    pub irreversible_intercepted: usize,
    pub blocklist_intercepted: usize,
    pub kill_switch_activations: usize,
    pub unconfirmed_irreversible_executed: usize,
    pub compliance_rate_percent: f64,
    pub g7_compliant: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSuccessSummary {
    pub tasks_attempted: usize,
    pub tasks_succeeded: usize,
    pub tasks_failed: usize,
    pub success_rate_percent: f64,
    pub g5_compliant: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub period: AuditPeriod,
    pub start_timestamp: String,
    pub end_timestamp: String,
    pub entries_analyzed: usize,
    pub safety_compliance: SafetyComplianceSummary,
    pub spend_by_provider: Vec<ProviderUsageSummary>,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub computer_use_actions: std::collections::BTreeMap<String, usize>,
    pub task_metrics: TaskSuccessSummary,
}

impl AuditReport {
    pub fn format_report(&self) -> String {
        let period_name = match self.period {
            AuditPeriod::Weekly => "Weekly",
            AuditPeriod::Monthly => "Monthly",
        };
        let mut out = String::new();
        out.push_str("═══════════════════════════════════════════════════════════\n");
        out.push_str(&format!(
            "🛡️ Grok-Muse {} Audit Report [{} to {}]\n",
            period_name, self.start_timestamp, self.end_timestamp
        ));
        out.push_str("═══════════════════════════════════════════════════════════\n\n");

        out.push_str("📊 1. TASK EXECUTION & SUCCESS RATES\n");
        out.push_str(&format!(
            "  • Tasks Attempted:    {}\n",
            self.task_metrics.tasks_attempted
        ));
        out.push_str(&format!(
            "  • Tasks Succeeded:    {}\n",
            self.task_metrics.tasks_succeeded
        ));
        out.push_str(&format!(
            "  • Tasks Failed:       {}\n",
            self.task_metrics.tasks_failed
        ));
        out.push_str(&format!(
            "  • Task Success Rate:  {:.1}% (Target >= 70.0% [G5]) -> {}\n\n",
            self.task_metrics.success_rate_percent,
            if self.task_metrics.g5_compliant {
                "PASS"
            } else {
                "FAIL"
            }
        ));

        out.push_str("🛡️ 2. SAFETY & COMPLIANCE (G7 GATE)\n");
        out.push_str(&format!(
            "  • Total Safety Events:           {}\n",
            self.safety_compliance.total_safety_events
        ));
        out.push_str(&format!(
            "  • Irreversible Intercepted:      {} (Safely stopped for confirmation)\n",
            self.safety_compliance.irreversible_intercepted
        ));
        out.push_str(&format!(
            "  • Blocklist Violations Stopped:  {}\n",
            self.safety_compliance.blocklist_intercepted
        ));
        out.push_str(&format!(
            "  • Kill Switch Activations:       {}\n",
            self.safety_compliance.kill_switch_activations
        ));
        out.push_str(&format!(
            "  • Unconfirmed Irreversible Ran:  {}\n",
            self.safety_compliance.unconfirmed_irreversible_executed
        ));
        out.push_str(&format!(
            "  • Safety Compliance Rate:        {:.1}% -> {}\n",
            self.safety_compliance.compliance_rate_percent,
            if self.safety_compliance.g7_compliant {
                "PASS (0 unconfirmed irreversible actions)"
            } else {
                "FAIL"
            }
        ));
        if !self.safety_compliance.actions_by_tier.is_empty() {
            out.push_str("  • Action Tier Breakdown:\n");
            for (tier, count) in &self.safety_compliance.actions_by_tier {
                out.push_str(&format!("    - {tier}: {count}\n"));
            }
        }
        out.push('\n');

        out.push_str("💰 3. SPEND & TOKEN USAGE BY PROVIDER\n");
        if self.spend_by_provider.is_empty() {
            out.push_str("  No provider model calls recorded in this period.\n");
        } else {
            out.push_str(&format!(
                "  {:<10} {:<20} {:<8} {:<12} {:<10}\n",
                "Provider", "Model", "Calls", "Tokens", "Cost (USD)"
            ));
            out.push_str(&format!("  {}\n", "─".repeat(64)));
            for u in &self.spend_by_provider {
                out.push_str(&format!(
                    "  {:<10} {:<20} {:<8} {:<12} ${:.4}\n",
                    u.provider, u.model, u.calls, u.tokens, u.cost_usd
                ));
            }
            out.push_str(&format!("  {}\n", "─".repeat(64)));
            out.push_str(&format!(
                "  {:<31} {:<8} {:<12} ${:.4}\n\n",
                "Total",
                self.spend_by_provider
                    .iter()
                    .map(|u| u.calls)
                    .sum::<usize>(),
                self.total_tokens,
                self.total_cost_usd
            ));
        }

        out.push_str("🖥️ 4. COMPUTER-USE ACTION BREAKDOWN\n");
        if self.computer_use_actions.is_empty() {
            out.push_str("  No computer-use actions recorded in this period.\n");
        } else {
            for (act, count) in &self.computer_use_actions {
                out.push_str(&format!("  • {:<16} {}\n", act, count));
            }
        }
        out.push_str("═══════════════════════════════════════════════════════════\n");
        out
    }
}

pub fn parse_flexible_timestamp(s: &str) -> Option<chrono::DateTime<Utc>> {
    let trimmed = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S %z") {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S %Z") {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Some(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
    }
    None
}

pub fn synthesize_audit_report(
    entries: &[AuditEntry],
    period: AuditPeriod,
    now: chrono::DateTime<Utc>,
) -> AuditReport {
    let window_days = match period {
        AuditPeriod::Weekly => 7,
        AuditPeriod::Monthly => 30,
    };
    let start_cutoff = now - chrono::Duration::days(window_days);

    let mut filtered_entries = Vec::new();
    let max_time = now + chrono::Duration::minutes(5);
    for entry in entries {
        if let Some(ts_utc) = parse_flexible_timestamp(&entry.timestamp)
            && ts_utc >= start_cutoff
            && ts_utc <= max_time
        {
            filtered_entries.push(entry.clone());
        }
    }

    let entries_analyzed = filtered_entries.len();

    // 1. Task metrics
    let mut tasks_succeeded = 0;
    let mut tasks_failed = 0;
    let mut explicit_attempts = 0;

    // 2. Safety metrics
    let mut total_safety_events = 0;
    let mut actions_by_tier = std::collections::BTreeMap::new();
    let mut irreversible_intercepted = 0;
    let mut blocklist_intercepted = 0;
    let mut kill_switch_activations = 0;
    let mut unconfirmed_irreversible_executed = 0;

    // 3. Provider spend
    let mut provider_model_map: std::collections::HashMap<(String, String), (usize, u64, f64)> =
        std::collections::HashMap::new();

    // 4. Computer-use actions
    let mut cu_actions = std::collections::BTreeMap::new();

    for entry in &filtered_entries {
        match entry.event_type.as_str() {
            "task_start" | "task_attempt" => {
                explicit_attempts += 1;
            }
            "task_done" => {
                tasks_succeeded += 1;
            }
            "task_failed" => {
                tasks_failed += 1;
            }
            "model_call" => {
                let provider = entry.provider.clone().unwrap_or_else(|| "unknown".into());
                let model = entry.model.clone().unwrap_or_else(|| "unknown".into());
                let tokens = entry.tokens.unwrap_or(0) as u64;
                let cost = entry.cost_usd.unwrap_or(0.0);

                let entry_mut = provider_model_map
                    .entry((provider, model))
                    .or_insert((0, 0, 0.0));
                entry_mut.0 += 1;
                entry_mut.1 += tokens;
                entry_mut.2 += cost;
            }
            "cu_action" => {
                let action_type = entry
                    .details
                    .get("action_type")
                    .and_then(|v| v.as_str())
                    .or(entry.action_summary.as_deref())
                    .unwrap_or("Other")
                    .to_string();
                *cu_actions.entry(action_type).or_insert(0) += 1;

                if let Some(tier) = entry.details.get("tier").and_then(|v| v.as_str()) {
                    total_safety_events += 1;
                    *actions_by_tier.entry(tier.to_string()).or_insert(0) += 1;
                    if tier == "Irreversible" {
                        let confirmed = entry
                            .details
                            .get("user_confirmed")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if !confirmed {
                            unconfirmed_irreversible_executed += 1;
                        }
                    }
                }
            }
            "safety_check" | "safety_intercept" => {
                total_safety_events += 1;
                if let Some(tier) = entry.details.get("tier").and_then(|v| v.as_str()) {
                    *actions_by_tier.entry(tier.to_string()).or_insert(0) += 1;
                }
                let is_blocklist = entry
                    .details
                    .get("blocklist")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| {
                        entry
                            .action_summary
                            .as_deref()
                            .map(|s| s.contains("blocklist"))
                            .unwrap_or(false)
                    });
                if is_blocklist {
                    blocklist_intercepted += 1;
                } else {
                    irreversible_intercepted += 1;
                }
            }
            "kill_switch" => {
                kill_switch_activations += 1;
                total_safety_events += 1;
            }
            _ => {
                // Check if entry has provider/tokens/cost
                if let (Some(prov), Some(tok), Some(cost)) =
                    (&entry.provider, entry.tokens, entry.cost_usd)
                {
                    let model = entry.model.clone().unwrap_or_else(|| "default".into());
                    let entry_mut = provider_model_map
                        .entry((prov.clone(), model))
                        .or_insert((0, 0, 0.0));
                    entry_mut.0 += 1;
                    entry_mut.1 += tok as u64;
                    entry_mut.2 += cost;
                }
            }
        }
    }

    let tasks_attempted = explicit_attempts.max(tasks_succeeded + tasks_failed);
    let success_rate_percent = if tasks_attempted > 0 {
        (tasks_succeeded as f64 / tasks_attempted as f64) * 100.0
    } else {
        100.0
    };
    let g5_compliant = success_rate_percent >= 70.0;

    let compliance_rate_percent = if unconfirmed_irreversible_executed == 0 {
        100.0
    } else {
        let total_eval = (irreversible_intercepted + unconfirmed_irreversible_executed).max(1);
        (irreversible_intercepted as f64 / total_eval as f64) * 100.0
    };
    let g7_compliant = unconfirmed_irreversible_executed == 0;

    let mut spend_by_provider = Vec::new();
    let mut total_tokens = 0;
    let mut total_cost_usd = 0.0;

    for ((provider, model), (calls, tokens, cost_usd)) in provider_model_map {
        total_tokens += tokens;
        total_cost_usd += cost_usd;
        spend_by_provider.push(ProviderUsageSummary {
            provider,
            model,
            calls,
            tokens,
            cost_usd,
        });
    }
    spend_by_provider.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    AuditReport {
        period,
        start_timestamp: start_cutoff.format("%Y-%m-%d").to_string(),
        end_timestamp: now.format("%Y-%m-%d").to_string(),
        entries_analyzed,
        safety_compliance: SafetyComplianceSummary {
            total_safety_events,
            actions_by_tier,
            irreversible_intercepted,
            blocklist_intercepted,
            kill_switch_activations,
            unconfirmed_irreversible_executed,
            compliance_rate_percent,
            g7_compliant,
        },
        spend_by_provider,
        total_tokens,
        total_cost_usd,
        computer_use_actions: cu_actions,
        task_metrics: TaskSuccessSummary {
            tasks_attempted,
            tasks_succeeded,
            tasks_failed,
            success_rate_percent,
            g5_compliant,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_logging() {
        let logger = AuditLogger::in_memory();
        logger.log(
            "model_call",
            Some("meta".into()),
            Some("muse-spark".into()),
            Some(120),
            Some(0.002),
            None,
            serde_json::json!({"prompt_length": 85}),
        );
        let entries = logger.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, "model_call");
        assert_eq!(entries[0].tokens, Some(120));
    }

    #[test]
    fn test_audit_logging_file_persistence() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("gm_audit_test_{nanos}"));
        let file_path = temp_dir.join("audit.jsonl");

        let logger1 = AuditLogger::with_file(&file_path).unwrap();
        logger1.log(
            "task_done",
            None,
            None,
            Some(50),
            Some(0.001),
            Some("Task A completed".into()),
            serde_json::json!({}),
        );
        assert_eq!(logger1.entries().len(), 1);

        // Reopen same file
        let logger2 = AuditLogger::with_file(&file_path).unwrap();
        let entries2 = logger2.entries();
        assert_eq!(entries2.len(), 1);
        assert_eq!(
            entries2[0].action_summary.as_deref(),
            Some("Task A completed")
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_synthesize_audit_report_weekly_and_monthly() {
        let logger = AuditLogger::in_memory();
        let now = Utc::now();

        // 1. Task executions
        logger.log(
            "task_start",
            None,
            None,
            None,
            None,
            Some("Open Finder window".into()),
            serde_json::json!({}),
        );
        logger.log(
            "task_done",
            None,
            None,
            None,
            None,
            Some("Finder window opened".into()),
            serde_json::json!({}),
        );
        logger.log(
            "task_done",
            None,
            None,
            None,
            None,
            Some("Safari PDF downloaded".into()),
            serde_json::json!({}),
        );

        // 2. Model calls
        logger.log(
            "model_call",
            Some("meta".into()),
            Some("muse-spark".into()),
            Some(450),
            Some(0.0022),
            None,
            serde_json::json!({}),
        );
        logger.log(
            "model_call",
            Some("xai".into()),
            Some("grok-4-vision".into()),
            Some(300),
            Some(0.0015),
            None,
            serde_json::json!({}),
        );

        // 3. CU Actions
        logger.log(
            "cu_action",
            None,
            None,
            None,
            None,
            Some("Click element".into()),
            serde_json::json!({
                "action_type": "Click",
                "tier": "Reversible"
            }),
        );
        logger.log(
            "cu_action",
            None,
            None,
            None,
            None,
            Some("Type text".into()),
            serde_json::json!({
                "action_type": "TypeText",
                "tier": "Reversible"
            }),
        );

        // 4. Safety interception
        logger.log(
            "safety_intercept",
            None,
            None,
            None,
            None,
            Some("Intercepted Click send_button".into()),
            serde_json::json!({
                "tier": "Irreversible",
                "blocklist": false
            }),
        );
        logger.log(
            "safety_intercept",
            None,
            None,
            None,
            None,
            Some("Intercepted Passwords panel".into()),
            serde_json::json!({
                "tier": "Irreversible",
                "blocklist": true
            }),
        );

        // Generate weekly report
        let report = logger.generate_report(AuditPeriod::Weekly, now);

        // Verify task metrics
        assert_eq!(report.task_metrics.tasks_succeeded, 2);
        assert_eq!(report.task_metrics.tasks_attempted, 2);
        assert_eq!(report.task_metrics.success_rate_percent, 100.0);
        assert!(report.task_metrics.g5_compliant);

        // Verify safety compliance (G7)
        assert_eq!(report.safety_compliance.irreversible_intercepted, 1);
        assert_eq!(report.safety_compliance.blocklist_intercepted, 1);
        assert_eq!(
            report.safety_compliance.unconfirmed_irreversible_executed,
            0
        );
        assert_eq!(report.safety_compliance.compliance_rate_percent, 100.0);
        assert!(report.safety_compliance.g7_compliant);

        // Verify spend by provider
        assert_eq!(report.spend_by_provider.len(), 2);
        assert_eq!(report.total_tokens, 750);
        assert!((report.total_cost_usd - 0.0037).abs() < 1e-6);

        // Verify CU actions
        assert_eq!(report.computer_use_actions.get("Click"), Some(&1));
        assert_eq!(report.computer_use_actions.get("TypeText"), Some(&1));

        // Verify formatted output contains key indicators
        let formatted = report.format_report();
        assert!(formatted.contains("Weekly Audit Report"));
        assert!(formatted.contains("G5"));
        assert!(formatted.contains("PASS"));
        assert!(formatted.contains("muse-spark"));
        assert!(formatted.contains("grok-4-vision"));
    }

    #[test]
    fn test_synthesize_audit_report_flexible_timestamp_and_corrupt_filtering() {
        let now = Utc::now();
        let entries = vec![
            // 1. Valid RFC3339 within window
            AuditEntry {
                timestamp: now.to_rfc3339(),
                event_type: "task_done".into(),
                provider: None,
                model: None,
                tokens: None,
                cost_usd: None,
                action_summary: Some("Task 1".into()),
                details: serde_json::json!({}),
            },
            // 2. Non-RFC3339 format within window (e.g., standard SQL datetime)
            AuditEntry {
                timestamp: now.format("%Y-%m-%d %H:%M:%S").to_string(),
                event_type: "task_done".into(),
                provider: None,
                model: None,
                tokens: None,
                cost_usd: None,
                action_summary: Some("Task 2".into()),
                details: serde_json::json!({}),
            },
            // 3. Out-of-window entry (60 days ago)
            AuditEntry {
                timestamp: (now - chrono::Duration::days(60)).to_rfc3339(),
                event_type: "task_done".into(),
                provider: None,
                model: None,
                tokens: None,
                cost_usd: None,
                action_summary: Some("Old Task".into()),
                details: serde_json::json!({}),
            },
            // 4. Completely unparseable timestamp
            AuditEntry {
                timestamp: "corrupted_timestamp_string".into(),
                event_type: "task_done".into(),
                provider: None,
                model: None,
                tokens: None,
                cost_usd: None,
                action_summary: Some("Corrupt Task".into()),
                details: serde_json::json!({}),
            },
        ];

        let report = synthesize_audit_report(&entries, AuditPeriod::Weekly, now);
        // Only entries 1 and 2 should be included!
        assert_eq!(report.entries_analyzed, 2);
        assert_eq!(report.task_metrics.tasks_succeeded, 2);
    }

    #[test]
    fn test_audit_governance_categorization_and_rule_citations() {
        let logger = AuditLogger::in_memory();
        logger.log_governance(
            "workspace_shell",
            Some("coder"),
            AuditCategory::Permitted,
            "RULE-STD-000",
            "Standard workspace read/test command",
            "cargo test",
            serde_json::json!({"exit_code": 0}),
        );
        logger.log_governance(
            "workspace_write",
            Some("coder"),
            AuditCategory::Refused,
            "RULE-PATH-002",
            "Path escapes bot workspace root",
            "write /etc/hosts",
            serde_json::json!({"path": "/etc/hosts"}),
        );

        let refused =
            logger.query_governance_entries(Some(AuditCategory::Refused), Some("coder"), 10);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].rule_id, "RULE-PATH-002");
        assert!(refused[0].citation.contains("escapes bot workspace"));

        let permitted = logger.query_governance_entries(Some(AuditCategory::Permitted), None, 10);
        assert_eq!(permitted.len(), 1);
        assert_eq!(permitted[0].rule_id, "RULE-STD-000");
    }
}

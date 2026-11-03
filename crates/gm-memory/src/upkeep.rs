use crate::PersonalMemoryStore;
use chrono::{DateTime, Utc};
use darius_memory::{Record, RecordKind};
use gm_core::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpkeepConfig {
    /// Days after which transient observations/episodes expire (default: 14 days)
    pub stale_observation_days: i64,
    /// Importance threshold below which stale observations are pruned (default: 0.4)
    pub low_importance_threshold: f32,
    /// Minimum token Jaccard similarity to consider two facts duplicates (default: 0.85)
    pub fact_similarity_threshold: f64,
}

impl Default for UpkeepConfig {
    fn default() -> Self {
        Self {
            stale_observation_days: 14,
            low_importance_threshold: 0.4,
            fact_similarity_threshold: 0.85,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeCluster {
    pub topic: String,
    pub record_ids: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUpkeepReport {
    pub timestamp: String,
    pub records_scanned: usize,
    pub duplicates_removed: usize,
    pub clusters_formed: usize,
    pub stale_expired: usize,
    pub total_records_retained: usize,
    pub clusters: Vec<KnowledgeCluster>,
}

impl MemoryUpkeepReport {
    pub fn format_summary(&self) -> String {
        let mut out = String::new();
        out.push_str("═══════════════════════════════════════════════════════════\n");
        out.push_str("🧠 Grok-Muse Memory Upkeep Report\n");
        out.push_str(&format!("   Timestamp: {}\n", self.timestamp));
        out.push_str("═══════════════════════════════════════════════════════════\n\n");
        out.push_str(&format!(
            "• Records Scanned:          {}\n",
            self.records_scanned
        ));
        out.push_str(&format!(
            "• Duplicate Facts Removed:  {}\n",
            self.duplicates_removed
        ));
        out.push_str(&format!(
            "• Stale Observations Pruned:{}\n",
            self.stale_expired
        ));
        out.push_str(&format!(
            "• Knowledge Clusters:       {}\n",
            self.clusters_formed
        ));
        out.push_str(&format!(
            "• Total Records Retained:   {}\n\n",
            self.total_records_retained
        ));

        if !self.clusters.is_empty() {
            out.push_str("Active Knowledge Clusters:\n");
            for c in &self.clusters {
                out.push_str(&format!(
                    "  ▸ [{}] ({} records): {}\n",
                    c.topic,
                    c.record_ids.len(),
                    c.summary
                ));
            }
        }
        out.push_str("═══════════════════════════════════════════════════════════\n");
        out
    }
}

fn normalize_text(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn tokenize(text: &str) -> HashSet<String> {
    text.split_whitespace().map(|w| w.to_string()).collect()
}

fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

pub fn run_memory_upkeep(
    store: &PersonalMemoryStore,
    config: &UpkeepConfig,
    now: DateTime<Utc>,
) -> Result<MemoryUpkeepReport> {
    let all_records = store.all_records(10_000)?;
    let records_scanned = all_records.len();

    let mut duplicates_removed = 0;
    let mut stale_expired = 0;

    // 1. Expire stale observations / episodes
    let now_ms = now.timestamp_millis();
    let stale_ms = config.stale_observation_days * 86_400 * 1000;

    let mut active_records = Vec::new();
    for rec in all_records {
        let is_stale_candidate = rec.kind == RecordKind::Episode
            || rec.source.as_deref() == Some("observation")
            || rec
                .tags
                .iter()
                .any(|t| t == "observation" || t == "transient" || t == "stale");

        let age_ms = now_ms.saturating_sub(rec.created_at);
        if is_stale_candidate
            && age_ms >= stale_ms
            && rec.importance <= config.low_importance_threshold
        {
            let _ = store.delete_record(&rec.id)?;
            stale_expired += 1;
        } else {
            active_records.push(rec);
        }
    }

    // 2. Deduplicate facts
    let mut retained_after_dedup: Vec<Record> = Vec::new();
    let mut consumed_ids: HashSet<String> = HashSet::new();

    for i in 0..active_records.len() {
        let rec_a = &active_records[i];
        if consumed_ids.contains(&rec_a.id) {
            continue;
        }

        if rec_a.kind != RecordKind::Fact && rec_a.kind != RecordKind::Preference {
            retained_after_dedup.push(rec_a.clone());
            consumed_ids.insert(rec_a.id.clone());
            continue;
        }

        let norm_a = normalize_text(&rec_a.body);
        let tokens_a = tokenize(&norm_a);

        let mut dup_group = vec![rec_a.clone()];

        for rec_b in active_records.iter().skip(i + 1) {
            if consumed_ids.contains(&rec_b.id) || rec_b.kind != rec_a.kind {
                continue;
            }

            let norm_b = normalize_text(&rec_b.body);
            let is_duplicate = if norm_a == norm_b {
                true
            } else {
                let tokens_b = tokenize(&norm_b);
                jaccard_similarity(&tokens_a, &tokens_b) >= config.fact_similarity_threshold
            };

            if is_duplicate {
                dup_group.push(rec_b.clone());
            }
        }

        if dup_group.len() == 1 {
            retained_after_dedup.push(rec_a.clone());
            consumed_ids.insert(rec_a.id.clone());
        } else {
            // Find winner with highest importance (tie-breaker: newest created_at)
            let winner = dup_group
                .iter()
                .max_by(|x, y| {
                    x.importance
                        .partial_cmp(&y.importance)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| x.created_at.cmp(&y.created_at))
                })
                .unwrap()
                .clone();

            let mut merged_tags: HashSet<String> = winner.tags.iter().cloned().collect();

            for loser in &dup_group {
                if loser.id != winner.id {
                    let _ = store.delete_record(&loser.id)?;
                    duplicates_removed += 1;
                    for t in &loser.tags {
                        merged_tags.insert(t.clone());
                    }
                }
            }

            let mut final_tags: Vec<String> = merged_tags.into_iter().collect();
            final_tags.sort();
            let mut retained_winner = winner;
            if final_tags != retained_winner.tags {
                let _ = store.update_tags(&retained_winner.id, &final_tags)?;
                retained_winner.tags = final_tags;
            }

            retained_after_dedup.push(retained_winner);
            for item in &dup_group {
                consumed_ids.insert(item.id.clone());
            }
        }
    }

    // 3. Cluster related knowledge
    // Group records by tags (excluding internal prefixes) or primary domain words
    let mut topic_to_records: HashMap<String, Vec<String>> = HashMap::new();

    for rec in &retained_after_dedup {
        let mut rec_topics = HashSet::new();
        for tag in &rec.tags {
            if !tag.starts_with("cluster:") && !tag.is_empty() {
                rec_topics.insert(tag.to_lowercase());
            }
        }

        // Also extract common domain topics from text if tags are sparse
        let lower_body = rec.body.to_lowercase();
        let domain_keywords = [
            "calendar",
            "meeting",
            "email",
            "preferences",
            "project",
            "schedule",
            "ui",
            "security",
        ];
        for kw in domain_keywords {
            if lower_body.contains(kw) {
                rec_topics.insert(kw.to_string());
            }
        }

        for topic in rec_topics {
            topic_to_records
                .entry(topic)
                .or_default()
                .push(rec.id.clone());
        }
    }

    let mut clusters = Vec::new();
    let mut clusters_formed = 0;

    for (topic, record_ids) in topic_to_records {
        if record_ids.len() >= 2 {
            clusters_formed += 1;
            let cluster_tag = format!("cluster:{topic}");

            // Tag records with cluster
            for r_id in &record_ids {
                if let Some(r) = retained_after_dedup.iter_mut().find(|r| &r.id == r_id)
                    && !r.tags.contains(&cluster_tag)
                {
                    r.tags.push(cluster_tag.clone());
                    let _ = store.update_tags(r_id, &r.tags)?;
                }
            }

            clusters.push(KnowledgeCluster {
                topic: topic.clone(),
                record_ids,
                summary: format!("Clustered knowledge items around '{topic}'"),
            });
        }
    }

    clusters.sort_by_key(|a| std::cmp::Reverse(a.record_ids.len()));

    let total_records_retained = retained_after_dedup.len();

    Ok(MemoryUpkeepReport {
        timestamp: now.to_rfc3339(),
        records_scanned,
        duplicates_removed,
        clusters_formed,
        stale_expired,
        total_records_retained,
        clusters,
    })
}

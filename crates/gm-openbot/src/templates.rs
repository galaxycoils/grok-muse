//! Team templates with import/export, preview, and safe trigger defaults.
//! Credentials and secrets are strictly excluded. Imported triggers are disabled initially.

use crate::channels::ChannelStore;
use crate::queue::DurableTaskQueue;
use crate::roster::{BotProfile, BotRoster};
use crate::routines::Routine;
use crate::supervisor::OpenBotSupervisor;
use chrono::Utc;
use gm_core::error::{GmError, Result};
use serde::{Deserialize, Serialize};

fn error(e: impl std::fmt::Display) -> GmError {
    GmError::Config(format!("Template error: {e}"))
}

fn default_avatar() -> String {
    "🤖".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateBot {
    pub id: String,
    pub display_name: String,
    #[serde(default = "default_avatar")]
    pub avatar_emoji: String,
    pub role_title: String,
    pub system_prompt: String,
    #[serde(default)]
    pub preferred_provider: Option<String>,
    #[serde(default)]
    pub preferred_model: Option<String>,
    #[serde(default)]
    pub allow_shell: bool,
    #[serde(default)]
    pub allow_network: bool,
    #[serde(default)]
    pub allow_gui: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateChannel {
    pub id: String,
    pub name: String,
    pub topic: String,
    #[serde(default)]
    pub instructions: String,
    pub owner_bot_id: Option<String>,
    #[serde(default)]
    pub participants: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateRoutine {
    pub id: String,
    pub name: String,
    pub bot_id: String,
    pub channel_id: Option<String>,
    pub instruction: String,
    pub timezone: String,
    pub hour: u32,
    pub minute: u32,
    #[serde(default)]
    pub weekdays: Vec<u32>,
    #[serde(default)]
    pub quiet_hours: Option<crate::routines::QuietHours>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTemplate {
    pub schema_version: u32,
    pub team_name: String,
    pub description: String,
    #[serde(default)]
    pub bots: Vec<TemplateBot>,
    #[serde(default)]
    pub channels: Vec<TemplateChannel>,
    #[serde(default)]
    pub routines: Vec<TemplateRoutine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTemplatePreview {
    pub team_name: String,
    pub description: String,
    pub bot_count: usize,
    pub channel_count: usize,
    pub routine_count: usize,
    pub bots: Vec<String>,
    pub channels: Vec<String>,
    pub routines: Vec<String>,
    pub warnings: Vec<String>,
    pub credentials_excluded: bool,
    pub triggers_disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamImportSummary {
    pub team_name: String,
    pub bots_imported: usize,
    pub channels_imported: usize,
    pub routines_imported: usize,
    pub notices: Vec<String>,
}

fn has_credential_tokens(text: &str) -> bool {
    text.contains("ghp_")
        || text.contains("github_pat_")
        || text.contains("sk-")
        || text.contains("xai-")
        || text.contains("Bearer ")
}

impl TeamTemplate {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(error(format!(
                "Unsupported schema version: {}",
                self.schema_version
            )));
        }
        if self.team_name.trim().is_empty() || self.team_name.len() > 128 {
            return Err(error("Team name must be 1-128 characters"));
        }
        for bot in &self.bots {
            let id = bot.id.trim();
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                return Err(error(format!(
                    "Invalid bot ID '{}' (must be 1-64 lowercase ASCII alphanumeric, '-' or '_')",
                    bot.id
                )));
            }
            if let Some(ref prov) = bot.preferred_provider
                && !matches!(
                    prov.as_str(),
                    "meta" | "muse" | "xai" | "grok" | "claude" | "codex" | "anthropic" | "openai"
                )
            {
                return Err(error(format!(
                    "Bot '{}' specifies unsupported provider '{prov}'",
                    bot.id
                )));
            }
            let text = format!(
                "{} {} {}",
                bot.system_prompt, bot.role_title, bot.display_name
            );
            if has_credential_tokens(&text) {
                return Err(error(format!(
                    "Bot '{}' contains sensitive credential tokens; templates must not include secrets",
                    bot.id
                )));
            }
        }
        for chan in &self.channels {
            if chan.id.trim().is_empty() || chan.id.len() > 64 {
                return Err(error(format!("Invalid channel ID '{}'", chan.id)));
            }
            let text = format!("{} {} {}", chan.name, chan.topic, chan.instructions);
            if has_credential_tokens(&text) {
                return Err(error(format!(
                    "Channel '{}' contains sensitive credential tokens; templates must not include secrets",
                    chan.id
                )));
            }
        }
        for routine in &self.routines {
            if routine.id.trim().is_empty() || routine.id.len() > 64 {
                return Err(error(format!("Invalid routine ID '{}'", routine.id)));
            }
            let text = format!("{} {}", routine.name, routine.instruction);
            if has_credential_tokens(&text) {
                return Err(error(format!(
                    "Routine '{}' contains sensitive credential tokens; templates must not include secrets",
                    routine.id
                )));
            }
        }
        Ok(())
    }
}

/// Generates a preview of a team template without modifying any persistent state.
pub fn preview_template(template_json: &str) -> Result<TeamTemplatePreview> {
    let template: TeamTemplate = serde_json::from_str(template_json).map_err(error)?;
    template.validate()?;

    let mut warnings = Vec::new();
    let bot_ids: Vec<String> = template.bots.iter().map(|b| b.id.clone()).collect();

    for routine in &template.routines {
        if !bot_ids.contains(&routine.bot_id) {
            warnings.push(format!(
                "Routine '{}' references bot '{}' which is not in this template (must exist in local roster)",
                routine.id, routine.bot_id
            ));
        }
    }

    Ok(TeamTemplatePreview {
        team_name: template.team_name,
        description: template.description,
        bot_count: template.bots.len(),
        channel_count: template.channels.len(),
        routine_count: template.routines.len(),
        bots: template
            .bots
            .iter()
            .map(|b| format!("@{} ({})", b.id, b.role_title))
            .collect(),
        channels: template
            .channels
            .iter()
            .map(|c| format!("#{} - {}", c.id, c.topic))
            .collect(),
        routines: template
            .routines
            .iter()
            .map(|r| {
                format!(
                    "{} -> @{} at {:02}:{:02} [{}] (trigger initially disabled)",
                    r.name, r.bot_id, r.hour, r.minute, r.timezone
                )
            })
            .collect(),
        warnings,
        credentials_excluded: true,
        triggers_disabled: true,
    })
}

/// Exports current roster bots, channels, and routines into a portable `TeamTemplate`.
pub fn export_team(
    roster: &BotRoster,
    channels: &ChannelStore,
    queue: &DurableTaskQueue,
    team_name: &str,
    description: &str,
) -> Result<TeamTemplate> {
    let bots = roster
        .list_bots()
        .into_iter()
        .map(|b| TemplateBot {
            id: b.id,
            display_name: b.display_name,
            avatar_emoji: b.avatar_emoji,
            role_title: b.role_title,
            system_prompt: b.system_prompt,
            preferred_provider: b.preferred_provider,
            preferred_model: b.preferred_model,
            allow_shell: b.allow_shell,
            allow_network: b.allow_network,
            allow_gui: b.allow_gui,
        })
        .collect();

    let channel_infos = channels.list_channels(false)?;
    let template_channels = channel_infos
        .into_iter()
        .map(|c| TemplateChannel {
            id: c.id,
            name: c.name,
            topic: c.topic,
            instructions: c.instructions,
            owner_bot_id: c.owner_bot_id,
            participants: c.participants,
        })
        .collect();

    let routine_states = queue.list_routines()?;
    let routines = routine_states
        .into_iter()
        .map(|s| TemplateRoutine {
            id: s.routine.id,
            name: s.routine.name,
            bot_id: s.routine.bot_id,
            channel_id: s.routine.channel_id,
            instruction: s.routine.instruction,
            timezone: s.routine.timezone,
            hour: s.routine.hour,
            minute: s.routine.minute,
            weekdays: s.routine.weekdays,
            quiet_hours: s.routine.quiet_hours,
        })
        .collect();

    let template = TeamTemplate {
        schema_version: 1,
        team_name: team_name.to_string(),
        description: description.to_string(),
        bots,
        channels: template_channels,
        routines,
    };
    template.validate()?;
    Ok(template)
}

/// Imports a team template into the local supervisor state.
/// Ensures that:
/// 1. Credentials are never imported (template contains none).
/// 2. All imported routine triggers are explicitly created with `enabled = false`.
pub fn import_team(
    supervisor: &OpenBotSupervisor,
    template: &TeamTemplate,
    overwrite_bots: bool,
) -> Result<TeamImportSummary> {
    template.validate()?;
    let mut notices = Vec::new();
    let mut bots_imported = 0;
    let mut channels_imported = 0;
    let mut routines_imported = 0;

    let existing_bots = supervisor.roster().list_bots();
    let existing_bot_ids: Vec<String> = existing_bots.iter().map(|b| b.id.clone()).collect();

    for t_bot in &template.bots {
        if existing_bot_ids.contains(&t_bot.id) && !overwrite_bots {
            notices.push(format!(
                "Bot @{} already exists; skipped (use --overwrite to replace)",
                t_bot.id
            ));
            continue;
        }

        let profile = BotProfile {
            id: t_bot.id.clone(),
            display_name: t_bot.display_name.clone(),
            avatar_emoji: if t_bot.avatar_emoji.is_empty() {
                "🤖".into()
            } else {
                t_bot.avatar_emoji.clone()
            },
            role_title: t_bot.role_title.clone(),
            system_prompt: t_bot.system_prompt.clone(),
            preferred_provider: t_bot.preferred_provider.clone(),
            preferred_model: t_bot.preferred_model.clone(),
            allow_shell: t_bot.allow_shell,
            allow_network: t_bot.allow_network,
            allow_gui: t_bot.allow_gui,
            created_at: Utc::now().to_rfc3339(),
        };
        supervisor.roster().upsert_bot(profile)?;
        bots_imported += 1;
    }

    for t_chan in &template.channels {
        if let Ok(Some(_)) = supervisor.channels().get_channel(&t_chan.id) {
            // Update instructions if channel exists
            if !t_chan.instructions.is_empty() {
                let _ = supervisor
                    .channels()
                    .update_channel_instructions(&t_chan.id, &t_chan.instructions);
            }
            notices.push(format!(
                "Channel #{} already exists; instructions updated",
                t_chan.id
            ));
        } else {
            let _ = supervisor.channels().create_channel_with_instructions(
                &t_chan.id,
                &t_chan.topic,
                &t_chan.instructions,
                t_chan.owner_bot_id.as_deref(),
                &t_chan.participants,
            )?;
            channels_imported += 1;
        }
    }

    let now = Utc::now();
    for t_routine in &template.routines {
        // Enforce trigger safety: ALL imported routines are disabled initially.
        let routine = Routine {
            id: t_routine.id.clone(),
            name: t_routine.name.clone(),
            bot_id: t_routine.bot_id.clone(),
            channel_id: t_routine.channel_id.clone(),
            instruction: t_routine.instruction.clone(),
            timezone: t_routine.timezone.clone(),
            hour: t_routine.hour,
            minute: t_routine.minute,
            weekdays: t_routine.weekdays.clone(),
            quiet_hours: t_routine.quiet_hours.clone(),
            enabled: false,
        };

        if let Err(e) = supervisor.queue().save_routine(&routine, now) {
            notices.push(format!("Failed to import routine '{}': {e}", t_routine.id));
        } else {
            routines_imported += 1;
            notices.push(format!(
                "Routine '{}' imported in DISABLED state (review and enable when ready)",
                t_routine.id
            ));
        }
    }

    Ok(TeamImportSummary {
        team_name: template.team_name.clone(),
        bots_imported,
        channels_imported,
        routines_imported,
        notices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_validation_and_preview() {
        let json = r##"{
            "schema_version": 1,
            "team_name": "QA Incident Team",
            "description": "Team for triaging CI failures",
            "bots": [
                {
                    "id": "triage_bot",
                    "display_name": "Triage Bot",
                    "role_title": "Triage Specialist",
                    "system_prompt": "Investigate logs and report issues",
                    "allow_shell": true
                }
            ],
            "channels": [
                {
                    "id": "incidents",
                    "name": "#incidents",
                    "topic": "Live incidents",
                    "instructions": "Be concise"
                }
            ],
            "routines": [
                {
                    "id": "ci_check",
                    "name": "CI health check",
                    "bot_id": "triage_bot",
                    "instruction": "Check CI queue",
                    "timezone": "UTC",
                    "hour": 9,
                    "minute": 0
                }
            ]
        }"##;

        let preview = preview_template(json).unwrap();
        assert_eq!(preview.team_name, "QA Incident Team");
        assert_eq!(preview.bot_count, 1);
        assert_eq!(preview.channel_count, 1);
        assert_eq!(preview.routine_count, 1);
        assert!(preview.credentials_excluded);
        assert!(preview.triggers_disabled);
    }

    #[test]
    fn test_template_rejects_credentials() {
        let json = r##"{
            "schema_version": 1,
            "team_name": "Leaky Team",
            "description": "Invalid template with secret",
            "bots": [
                {
                    "id": "leaky_bot",
                    "display_name": "Leaky Bot",
                    "role_title": "Operator",
                    "system_prompt": "Use token ghp_SuperSecret12345 to push",
                    "allow_shell": false
                }
            ],
            "channels": [],
            "routines": []
        }"##;

        let res = preview_template(json);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("credential tokens"));
    }

    #[test]
    fn test_template_rejects_credentials_in_channels_and_routines() {
        let json_chan = r##"{
            "schema_version": 1,
            "team_name": "Leaky Channel Team",
            "description": "Invalid template",
            "bots": [{ "id": "b1", "display_name": "B1", "role_title": "R", "system_prompt": "prompt" }],
            "channels": [{ "id": "ops", "name": "#ops", "topic": "ops", "instructions": "github_pat_11AAAAA" }],
            "routines": []
        }"##;
        assert!(preview_template(json_chan).is_err());

        let json_routine = r##"{
            "schema_version": 1,
            "team_name": "Leaky Routine Team",
            "description": "Invalid template",
            "bots": [{ "id": "b1", "display_name": "B1", "role_title": "R", "system_prompt": "prompt" }],
            "channels": [],
            "routines": [{
                "id": "r1", "name": "r1", "bot_id": "b1",
                "instruction": "Authorization: Bearer my-secret-jwt",
                "timezone": "UTC", "hour": 1, "minute": 0
            }]
        }"##;
        assert!(preview_template(json_routine).is_err());
    }

    #[test]
    fn test_template_validates_bot_id_format_and_provider() {
        let invalid_bot_id = r##"{
            "schema_version": 1,
            "team_name": "Invalid ID Team",
            "description": "Invalid template",
            "bots": [{ "id": "Bad Bot ID With Spaces!", "display_name": "B", "role_title": "R", "system_prompt": "p" }],
            "channels": [],
            "routines": []
        }"##;
        assert!(preview_template(invalid_bot_id).is_err());

        let valid_claude_bot = r##"{
            "schema_version": 1,
            "team_name": "Claude Team",
            "description": "Valid template with Claude provider",
            "bots": [{
                "id": "claude_specialist",
                "display_name": "Claude Specialist",
                "role_title": "Engineer",
                "system_prompt": "Code carefully",
                "preferred_provider": "claude",
                "preferred_model": "claude-3-5-sonnet"
            }],
            "channels": [],
            "routines": []
        }"##;
        assert!(preview_template(valid_claude_bot).is_ok());
    }
}

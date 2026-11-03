use chrono::{DateTime, Utc};
use gm_core::error::{GmError, Result};
use gm_core::types::{AxDiff, AxSnapshot, ComputerAction, FrameCapture};
use gm_platform::Platform;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryStep {
    pub step_index: u32,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<FrameCapture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<AxSnapshot>,
    pub action: ComputerAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<AxDiff>,
    pub model_response: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryHeader {
    pub task_id: String,
    pub instructions: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryFooter {
    pub completed_at: DateTime<Utc>,
    pub final_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trajectory {
    pub task_id: String,
    pub instructions: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub steps: Vec<TrajectoryStep>,
    pub final_summary: Option<String>,
}

pub fn default_trajectories_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Library/Application Support/GrokMuse/trajectories")
}

#[derive(Debug, Clone)]
pub struct TrajectoryRecorder {
    trajectory: Arc<Mutex<Trajectory>>,
    directory: PathBuf,
    jsonl_path: PathBuf,
}

impl TrajectoryRecorder {
    pub fn new(task_id: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self::with_directory(task_id, instructions, default_trajectories_dir())
    }

    pub fn with_directory(
        task_id: impl Into<String>,
        instructions: impl Into<String>,
        directory: PathBuf,
    ) -> Self {
        let task_id_str = task_id.into();
        let instructions_str = instructions.into();
        let started_at = Utc::now();

        // Sanitize task_id for filesystem path
        let safe_filename = task_id_str
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let timestamp = started_at.format("%Y%m%d_%H%M%S_%3f");
        let jsonl_path = directory.join(format!("{safe_filename}_{timestamp}.jsonl"));

        let _ = std::fs::create_dir_all(&directory);

        let recorder = Self {
            trajectory: Arc::new(Mutex::new(Trajectory {
                task_id: task_id_str.clone(),
                instructions: instructions_str.clone(),
                started_at,
                completed_at: None,
                steps: Vec::new(),
                final_summary: None,
            })),
            directory,
            jsonl_path,
        };

        // Write header to JSONL file
        let header = TrajectoryHeader {
            task_id: task_id_str,
            instructions: instructions_str,
            started_at,
        };
        recorder.append_json_line(&header);

        recorder
    }

    pub fn jsonl_path(&self) -> &Path {
        &self.jsonl_path
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn record_step(
        &self,
        frame: Option<FrameCapture>,
        snapshot: Option<AxSnapshot>,
        action: ComputerAction,
        diff: Option<AxDiff>,
        model_response: String,
        duration_ms: u64,
    ) {
        let mut traj = self.trajectory.lock();
        let step_index = traj.steps.len() as u32;
        let step = TrajectoryStep {
            step_index,
            timestamp: Utc::now(),
            frame,
            snapshot,
            action,
            diff,
            model_response,
            duration_ms,
        };
        traj.steps.push(step.clone());
        drop(traj);

        self.append_json_line(&step);
    }

    pub fn finish(&self, summary: Option<String>) -> Trajectory {
        let mut traj = self.trajectory.lock();
        let completed_at = Utc::now();
        traj.completed_at = Some(completed_at);
        traj.final_summary = summary.clone();
        let finished_traj = traj.clone();
        drop(traj);

        let footer = TrajectoryFooter {
            completed_at,
            final_summary: summary,
        };
        self.append_json_line(&footer);

        finished_traj
    }

    fn append_json_line<T: Serialize>(&self, item: &T) {
        if let Ok(line) = serde_json::to_string(item) {
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.jsonl_path)
            {
                let _ = writeln!(file, "{line}");
            } else {
                warn!("Failed to open trajectory file at {:?}", self.jsonl_path);
            }
        }
    }
}

/// Offline replay mechanism for recorded trajectories.
#[derive(Debug, Clone)]
pub struct TrajectoryReplayer {
    trajectory: Trajectory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaySummary {
    pub task_id: String,
    pub total_steps: usize,
    pub actions_executed: usize,
    pub duration_ms: u64,
    pub success: bool,
}

impl TrajectoryReplayer {
    pub fn new(trajectory: Trajectory) -> Self {
        Self { trajectory }
    }

    pub fn trajectory(&self) -> &Trajectory {
        &self.trajectory
    }

    pub fn load_from_jsonl(path: impl AsRef<Path>) -> Result<Self> {
        let file = std::fs::File::open(path.as_ref()).map_err(GmError::Io)?;
        let reader = std::io::BufReader::new(file);
        Self::load_from_reader(reader)
    }

    pub fn load_from_jsonl_str(content: &str) -> Result<Self> {
        let trimmed = content.trim();
        if trimmed.starts_with('{')
            && trimmed.contains("\"steps\"")
            && let Ok(traj) = serde_json::from_str::<Trajectory>(trimmed)
        {
            return Ok(Self { trajectory: traj });
        }
        Self::load_from_reader(std::io::Cursor::new(content))
    }

    fn load_from_reader<R: std::io::BufRead>(reader: R) -> Result<Self> {
        let mut steps = Vec::new();
        let mut task_id = "replayed_task".to_string();
        let mut instructions = "".to_string();
        let mut started_at = Utc::now();
        let mut completed_at = None;
        let mut final_summary = None;

        for line_res in reader.lines() {
            let line = line_res.map_err(GmError::Io)?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Ok(meta) = serde_json::from_str::<TrajectoryHeader>(trimmed) {
                task_id = meta.task_id;
                instructions = meta.instructions;
                started_at = meta.started_at;
            } else if let Ok(footer) = serde_json::from_str::<TrajectoryFooter>(trimmed) {
                completed_at = Some(footer.completed_at);
                final_summary = footer.final_summary;
            } else if let Ok(step) = serde_json::from_str::<TrajectoryStep>(trimmed) {
                steps.push(step);
            } else {
                warn!("Unrecognized line format in trajectory file: {}", trimmed);
            }
        }

        Ok(Self {
            trajectory: Trajectory {
                task_id,
                instructions,
                started_at,
                completed_at,
                steps,
                final_summary,
            },
        })
    }

    /// Replay the recorded trajectory's actions against an active or mock Platform.
    pub async fn replay_on_platform<P: Platform>(&self, platform: &P) -> Result<ReplaySummary> {
        let start = std::time::Instant::now();
        let mut actions_executed = 0;

        for step in &self.trajectory.steps {
            debug!(
                "Replaying step {} action: {:?}",
                step.step_index, step.action
            );
            platform.act(&step.action).await?;
            actions_executed += 1;
        }

        Ok(ReplaySummary {
            task_id: self.trajectory.task_id.clone(),
            total_steps: self.trajectory.steps.len(),
            actions_executed,
            duration_ms: start.elapsed().as_millis() as u64,
            success: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_platform::MockPlatform;

    #[tokio::test]
    async fn test_trajectory_recorder_and_replayer_roundtrip() {
        let temp_dir =
            std::env::temp_dir().join(format!("gm_traj_test_{}", Utc::now().timestamp_micros()));
        let recorder = TrajectoryRecorder::with_directory(
            "test_task_100",
            "Open browser and search",
            temp_dir.clone(),
        );

        let action1 = ComputerAction::Click {
            token: Some("btn_search".into()),
            coords: None,
            button: None,
            click_count: Some(1),
        };
        let action2 = ComputerAction::Type {
            text: "Hello Rust".into(),
        };
        let action3 = ComputerAction::Done {
            summary: "Search completed".into(),
        };

        recorder.record_step(
            None,
            None,
            action1.clone(),
            None,
            "{\"action\":\"Click\"}".into(),
            45,
        );
        recorder.record_step(
            None,
            None,
            action2.clone(),
            None,
            "{\"action\":\"Type\"}".into(),
            30,
        );
        recorder.record_step(
            None,
            None,
            action3.clone(),
            None,
            "{\"action\":\"Done\"}".into(),
            15,
        );

        let traj = recorder.finish(Some("Search completed".into()));
        assert_eq!(traj.steps.len(), 3);
        assert_eq!(traj.task_id, "test_task_100");

        // Verify JSONL file exists and can be replayed
        let jsonl_path = recorder.jsonl_path();
        assert!(jsonl_path.exists());

        let replayer = TrajectoryReplayer::load_from_jsonl(jsonl_path).expect("load jsonl");
        assert_eq!(replayer.trajectory().task_id, "test_task_100");
        assert_eq!(replayer.trajectory().steps.len(), 3);

        // Replay on MockPlatform
        let mock_platform = MockPlatform::new();
        let summary = replayer
            .replay_on_platform(&mock_platform)
            .await
            .expect("replay on platform");
        assert_eq!(summary.actions_executed, 3);
        assert!(summary.success);

        let executed = mock_platform.get_executed_actions();
        assert_eq!(executed.len(), 3);
        assert_eq!(executed[0], action1);
        assert_eq!(executed[1], action2);
        assert_eq!(executed[2], action3);

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}

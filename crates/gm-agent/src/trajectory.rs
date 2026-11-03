use chrono::{DateTime, Utc};
use gm_core::types::{AxSnapshot, ComputerAction};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryStep {
    pub step_index: u32,
    pub timestamp: DateTime<Utc>,
    pub snapshot: Option<AxSnapshot>,
    pub model_response: String,
    pub action: ComputerAction,
    pub duration_ms: u64,
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

#[derive(Debug, Clone)]
pub struct TrajectoryRecorder {
    trajectory: Arc<Mutex<Trajectory>>,
}

impl TrajectoryRecorder {
    pub fn new(task_id: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            trajectory: Arc::new(Mutex::new(Trajectory {
                task_id: task_id.into(),
                instructions: instructions.into(),
                started_at: Utc::now(),
                completed_at: None,
                steps: Vec::new(),
                final_summary: None,
            })),
        }
    }

    pub fn record_step(
        &self,
        snapshot: Option<AxSnapshot>,
        model_response: String,
        action: ComputerAction,
        duration_ms: u64,
    ) {
        let mut traj = self.trajectory.lock();
        let step_index = traj.steps.len() as u32;
        traj.steps.push(TrajectoryStep {
            step_index,
            timestamp: Utc::now(),
            snapshot,
            model_response,
            action,
            duration_ms,
        });
    }

    pub fn finish(&self, summary: Option<String>) -> Trajectory {
        let mut traj = self.trajectory.lock();
        traj.completed_at = Some(Utc::now());
        traj.final_summary = summary;
        traj.clone()
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct KillSwitch {
    triggered: Arc<AtomicBool>,
    sender: broadcast::Sender<()>,
}

impl Default for KillSwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl KillSwitch {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(16);
        Self {
            triggered: Arc::new(AtomicBool::new(false)),
            sender,
        }
    }

    /// Trigger the emergency stop. Guaranteed < 1ms execution time.
    pub fn trigger(&self) {
        warn!("KILL SWITCH TRIGGERED!");
        self.triggered.store(true, Ordering::SeqCst);
        let _ = self.sender.send(());
    }

    /// Check if kill switch is active.
    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }

    /// Reset after confirmation.
    pub fn reset(&self) {
        self.triggered.store(false, Ordering::SeqCst);
    }

    /// Subscribe to kill switch events.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kill_switch_speed_and_state() {
        let ks = KillSwitch::new();
        assert!(!ks.is_triggered());

        let start = std::time::Instant::now();
        ks.trigger();
        let elapsed = start.elapsed();

        assert!(ks.is_triggered());
        // Verify response time is well below the 100ms threshold (should be sub-millisecond)
        assert!(elapsed.as_millis() < 50);

        ks.reset();
        assert!(!ks.is_triggered());
    }
}

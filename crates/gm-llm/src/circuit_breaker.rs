use crate::router::ProviderKind;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
struct ProviderState {
    recent_failures: Vec<Instant>,
    open_until: Option<Instant>,
}

/// Rolling Circuit Breaker for LLM providers.
/// Opens when `failure_threshold` failures occur within `window`,
/// remaining open for `cooldown` before half-opening/closing.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: Arc<Mutex<HashMap<ProviderKind, ProviderState>>>,
    failure_threshold: u32,
    window: Duration,
    cooldown: Duration,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            failure_threshold: 3,
            window: Duration::from_secs(60),
            cooldown: Duration::from_secs(60),
        }
    }

    pub fn with_params(failure_threshold: u32, window: Duration, cooldown: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            failure_threshold,
            window,
            cooldown,
        }
    }

    pub fn is_open(&self, provider: &ProviderKind) -> bool {
        let mut state = self.state.lock();
        let entry = state.entry(provider.clone()).or_default();
        if let Some(until) = entry.open_until {
            if Instant::now() < until {
                return true;
            } else {
                // Cooldown has expired, circuit resets
                entry.open_until = None;
                entry.recent_failures.clear();
            }
        }
        false
    }

    pub fn record_failure(&self, provider: ProviderKind) {
        let mut state = self.state.lock();
        let entry = state.entry(provider).or_default();
        let now = Instant::now();

        // Remove failures outside the window
        entry
            .recent_failures
            .retain(|&t| now.duration_since(t) <= self.window);
        entry.recent_failures.push(now);

        if entry.recent_failures.len() >= self.failure_threshold as usize {
            entry.open_until = Some(now + self.cooldown);
            entry.recent_failures.clear();
        }
    }

    pub fn record_success(&self, provider: ProviderKind) {
        let mut state = self.state.lock();
        let entry = state.entry(provider).or_default();
        entry.recent_failures.clear();
        entry.open_until = None;
    }

    pub fn reset(&self) {
        self.state.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_opens_after_three_failures() {
        let cb = CircuitBreaker::new();
        assert!(!cb.is_open(&ProviderKind::Meta));

        cb.record_failure(ProviderKind::Meta);
        assert!(!cb.is_open(&ProviderKind::Meta));

        cb.record_failure(ProviderKind::Meta);
        assert!(!cb.is_open(&ProviderKind::Meta));

        cb.record_failure(ProviderKind::Meta);
        assert!(cb.is_open(&ProviderKind::Meta));

        // Different provider should not be affected
        assert!(!cb.is_open(&ProviderKind::Xai));

        // Success resets circuit
        cb.record_success(ProviderKind::Meta);
        assert!(!cb.is_open(&ProviderKind::Meta));
    }
}

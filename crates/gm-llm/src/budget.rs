use gm_core::config::BudgetConfig;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct BudgetManager {
    config: BudgetConfig,
    daily_spent_usd: Arc<Mutex<f64>>,
    current_job_spent_usd: Arc<Mutex<f64>>,
}

impl BudgetManager {
    pub fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            daily_spent_usd: Arc::new(Mutex::new(0.0)),
            current_job_spent_usd: Arc::new(Mutex::new(0.0)),
        }
    }

    pub fn record_spend(&self, cost_usd: f64) -> Result<()> {
        let mut daily = self.daily_spent_usd.lock();
        let mut job = self.current_job_spent_usd.lock();

        if *daily + cost_usd > self.config.daily_spend_cap_usd {
            return Err(GmError::BudgetExceeded(format!(
                "Daily spend cap of ${:.2} exceeded: current=${:.2}, additional=${:.2}",
                self.config.daily_spend_cap_usd, *daily, cost_usd
            )));
        }

        if *job + cost_usd > self.config.per_job_spend_cap_usd {
            return Err(GmError::BudgetExceeded(format!(
                "Per-job spend cap of ${:.2} exceeded: current=${:.2}, additional=${:.2}",
                self.config.per_job_spend_cap_usd, *job, cost_usd
            )));
        }

        *daily += cost_usd;
        *job += cost_usd;
        Ok(())
    }

    pub fn reset_job_spend(&self) {
        *self.current_job_spent_usd.lock() = 0.0;
    }

    pub fn get_daily_spent(&self) -> f64 {
        *self.daily_spent_usd.lock()
    }

    pub fn get_job_spent(&self) -> f64 {
        *self.current_job_spent_usd.lock()
    }

    pub fn config(&self) -> &BudgetConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_enforcement() {
        let config = BudgetConfig {
            daily_spend_cap_usd: 1.0,
            per_job_spend_cap_usd: 0.5,
            max_daily_notifications: 3,
        };
        let manager = BudgetManager::new(config);
        assert!(manager.record_spend(0.3).is_ok());
        assert_eq!(manager.get_job_spent(), 0.3);

        // Exceed job spend
        assert!(manager.record_spend(0.3).is_err());

        manager.reset_job_spend();
        assert_eq!(manager.get_job_spent(), 0.0);
        assert_eq!(manager.get_daily_spent(), 0.3);

        assert!(manager.record_spend(0.4).is_ok());
        assert_eq!(manager.get_daily_spent(), 0.7);

        // Exceed daily spend
        assert!(manager.record_spend(0.4).is_err());
    }
}

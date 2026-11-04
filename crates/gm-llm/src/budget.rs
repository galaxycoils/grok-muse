use gm_core::config::BudgetConfig;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Debug)]
struct DailyLedger {
    day: chrono::NaiveDate,
    spent: f64,
    reserved: f64,
}
#[derive(Debug, Default)]
struct JobLedger {
    spent: f64,
    reserved: f64,
}
#[derive(Debug, Clone)]
pub struct BudgetManager {
    config: BudgetConfig,
    daily: Arc<Mutex<DailyLedger>>,
    job: Arc<Mutex<JobLedger>>,
}

/// An admitted request reserves its estimated maximum exposure before dispatch.
/// On cancellation/failure, retain that exposure conservatively because a provider may have billed it.
pub struct BudgetReservation {
    manager: BudgetManager,
    amount: f64,
    settled: bool,
}
impl BudgetReservation {
    pub fn release(mut self) {
        let mut daily = self.manager.daily.lock();
        let mut job = self.manager.job.lock();
        daily.reserved = (daily.reserved - self.amount).max(0.0);
        job.reserved = (job.reserved - self.amount).max(0.0);
        self.settled = true;
    }

    pub fn settle(mut self, actual: f64) -> Result<()> {
        if !actual.is_finite() || actual < 0.0 {
            return Err(GmError::BudgetExceeded("Invalid provider cost".into()));
        }
        let mut daily = self.manager.daily.lock();
        let mut job = self.manager.job.lock();
        daily.reserved = (daily.reserved - self.amount).max(0.0);
        job.reserved = (job.reserved - self.amount).max(0.0);
        daily.spent += actual;
        job.spent += actual;
        self.settled = true;
        if daily.spent > self.manager.config.daily_spend_cap_usd
            || job.spent > self.manager.config.per_job_spend_cap_usd
        {
            return Err(GmError::BudgetExceeded(
                "Actual provider usage exceeded the estimate; further requests are blocked".into(),
            ));
        }
        Ok(())
    }
}
impl Drop for BudgetReservation {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut daily = self.manager.daily.lock();
        let mut job = self.manager.job.lock();
        daily.reserved = (daily.reserved - self.amount).max(0.0);
        job.reserved = (job.reserved - self.amount).max(0.0);
        daily.spent += self.amount;
        job.spent += self.amount;
    }
}
impl BudgetManager {
    pub fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            daily: Arc::new(Mutex::new(DailyLedger {
                day: chrono::Utc::now().date_naive(),
                spent: 0.0,
                reserved: 0.0,
            })),
            job: Arc::new(Mutex::new(JobLedger::default())),
        }
    }
    pub fn scoped_job(&self) -> Self {
        Self {
            config: self.config.clone(),
            daily: self.daily.clone(),
            job: Arc::new(Mutex::new(JobLedger::default())),
        }
    }
    pub fn reserve(&self, amount: f64) -> Result<BudgetReservation> {
        if !amount.is_finite() || amount < 0.0 {
            return Err(GmError::BudgetExceeded("Invalid request estimate".into()));
        }
        let mut daily = self.daily.lock();
        let mut job = self.job.lock();
        let day = chrono::Utc::now().date_naive();
        if daily.day != day && daily.reserved == 0.0 {
            daily.day = day;
            daily.spent = 0.0;
        }
        if daily.spent + daily.reserved + amount > self.config.daily_spend_cap_usd
            || job.spent + job.reserved + amount > self.config.per_job_spend_cap_usd
        {
            return Err(GmError::BudgetExceeded(
                "Request would exceed the available daily or task budget".into(),
            ));
        }
        daily.reserved += amount;
        job.reserved += amount;
        Ok(BudgetReservation {
            manager: self.clone(),
            amount,
            settled: false,
        })
    }
    pub fn record_spend(&self, cost_usd: f64) -> Result<()> {
        self.reserve(cost_usd)?.settle(cost_usd)
    }
    pub fn reset_job_spend(&self) {
        let mut job = self.job.lock();
        if job.reserved == 0.0 {
            job.spent = 0.0;
        }
    }
    pub fn get_daily_spent(&self) -> f64 {
        self.daily.lock().spent
    }
    pub fn get_job_spent(&self) -> f64 {
        self.job.lock().spent
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

#[cfg(test)]
mod reservation_tests {
    use super::*;
    #[test]
    fn parallel_jobs_share_daily_admission_but_not_job_limits() {
        let manager = BudgetManager::new(BudgetConfig {
            daily_spend_cap_usd: 1.0,
            per_job_spend_cap_usd: 0.75,
            max_daily_notifications: 3,
        });
        let first = manager.scoped_job();
        let second = manager.scoped_job();
        let reservation = first.reserve(0.6).unwrap();
        assert!(second.reserve(0.6).is_err());
        reservation.settle(0.2).unwrap();
        second.reserve(0.6).unwrap().settle(0.3).unwrap();
        assert_eq!(first.get_job_spent(), 0.2);
        assert_eq!(second.get_job_spent(), 0.3);
        assert_eq!(manager.get_daily_spent(), 0.5);
    }
    #[test]
    fn cancelled_request_retains_estimated_exposure_and_invalid_costs_fail() {
        let manager = BudgetManager::new(BudgetConfig {
            daily_spend_cap_usd: 1.0,
            per_job_spend_cap_usd: 1.0,
            max_daily_notifications: 3,
        });
        drop(manager.reserve(0.6).unwrap());
        assert_eq!(manager.get_daily_spent(), 0.6);
        assert!(manager.reserve(0.5).is_err());
        assert!(manager.record_spend(f64::NAN).is_err());
        assert!(manager.record_spend(-1.0).is_err());
    }
}

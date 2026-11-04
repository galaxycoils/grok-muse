use gm_core::config::BudgetConfig;
use gm_core::database::Database;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use rusqlite::{TransactionBehavior, params};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
struct DailyLedger {
    day: chrono::NaiveDate,
    spent: f64,
    reserved: f64,
    poisoned: bool,
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
    store: Option<Arc<Mutex<Database>>>,
}

/// An admitted request reserves its estimated maximum exposure before dispatch.
/// On cancellation/failure, retain that exposure conservatively because a provider may have billed it.
pub struct BudgetReservation {
    manager: BudgetManager,
    amount: f64,
    settled: bool,
    id: String,
}
impl BudgetReservation {
    pub fn release(mut self) {
        let mut daily = self.manager.daily.lock();
        let mut job = self.manager.job.lock();
        if let Err(error) = self.manager.persist_settlement(&self.id, 0.0) {
            daily.poisoned = true;
            tracing::error!(%error, "Budget release failed; further requests blocked");
            return;
        }
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
        if let Err(error) = self.manager.persist_settlement(&self.id, actual) {
            daily.poisoned = true;
            return Err(error);
        }
        daily.reserved = (daily.reserved - self.amount).max(0.0);
        job.reserved = (job.reserved - self.amount).max(0.0);
        daily.spent += actual;
        job.spent += actual;
        self.settled = true;
        if (self.manager.store.is_none() && daily.spent > self.manager.config.daily_spend_cap_usd)
            || (self.manager.store.is_some()
                && self.manager.persisted_total()? > self.manager.config.daily_spend_cap_usd)
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
        if let Err(error) = self.manager.persist_settlement(&self.id, self.amount) {
            daily.poisoned = true;
            tracing::error!(%error, "Budget settlement failed; reservation remains charged");
        }
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
                poisoned: false,
            })),
            job: Arc::new(Mutex::new(JobLedger::default())),
            store: None,
        }
    }
    /// Durable admission ledger. Pending reservations count after crashes and
    /// independent connections serialize admission with BEGIN IMMEDIATE.
    pub fn open(config: BudgetConfig, storage: &Path) -> Result<Self> {
        gm_core::storage::ensure_private_dir(storage)?;
        let mut connection = Database::open(&storage.join("usage.sqlite3"))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(3))
            .map_err(db_error)?;
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(db_error)?;
        connection.migrate("usage", 1, |tx| {
            tx.execute_batch(
                "
            CREATE TABLE IF NOT EXISTS usage_v1 (
                id TEXT PRIMARY KEY, day TEXT NOT NULL, amount REAL NOT NULL CHECK(amount >= 0),
                settled INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS usage_v1_day ON usage_v1(day);",
            )
            .map_err(db_error)?;
            Ok(())
        })?;
        let manager = Self {
            store: Some(Arc::new(Mutex::new(connection))),
            ..Self::new(config)
        };
        manager.daily.lock().spent = manager.persisted_total()?;
        Ok(manager)
    }
    fn persisted_total(&self) -> Result<f64> {
        let Some(store) = &self.store else {
            return Ok(0.0);
        };
        store
            .lock()
            .query_row(
                "SELECT coalesce(sum(amount),0) FROM usage_v1 WHERE day=?1",
                [chrono::Utc::now().date_naive().to_string()],
                |row| row.get(0),
            )
            .map_err(db_error)
    }
    fn persist_settlement(&self, id: &str, actual: f64) -> Result<()> {
        if let Some(store) = &self.store {
            let changed = store
                .lock()
                .execute(
                    "UPDATE usage_v1 SET amount=?1, settled=1 WHERE id=?2 AND settled=0",
                    params![actual, id],
                )
                .map_err(db_error)?;
            if changed != 1 {
                return Err(GmError::BudgetExceeded(
                    "Usage reservation is missing or already settled".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn scoped_job(&self) -> Self {
        Self {
            config: self.config.clone(),
            daily: self.daily.clone(),
            store: self.store.clone(),
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
        if daily.poisoned
            || (self.store.is_none()
                && daily.spent + daily.reserved + amount > self.config.daily_spend_cap_usd)
            || job.spent + job.reserved + amount > self.config.per_job_spend_cap_usd
        {
            return Err(GmError::BudgetExceeded(
                "Request would exceed the available daily or task budget".into(),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        if let Some(store) = &self.store {
            let mut connection = store.lock();
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let total: f64 = tx
                .query_row(
                    "SELECT coalesce(sum(amount),0) FROM usage_v1 WHERE day=?1",
                    [day.to_string()],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
            if total + amount > self.config.daily_spend_cap_usd {
                return Err(GmError::BudgetExceeded(
                    "Persistent daily budget is exhausted".into(),
                ));
            }
            tx.execute(
                "INSERT INTO usage_v1(id,day,amount) VALUES (?1,?2,?3)",
                params![id, day.to_string(), amount],
            )
            .map_err(db_error)?;
            tx.commit().map_err(db_error)?;
        }
        daily.reserved += amount;
        job.reserved += amount;
        Ok(BudgetReservation {
            manager: self.clone(),
            amount,
            settled: false,
            id,
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
        if self.store.is_some() {
            self.persisted_total()
                .unwrap_or(self.config.daily_spend_cap_usd)
        } else {
            self.daily.lock().spent
        }
    }
    pub fn get_job_spent(&self) -> f64 {
        self.job.lock().spent
    }
    pub fn config(&self) -> &BudgetConfig {
        &self.config
    }
}

fn db_error(e: rusqlite::Error) -> GmError {
    GmError::BudgetExceeded(format!("Usage ledger is unavailable: {e}"))
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

#[cfg(test)]
mod persistence_tests {
    use super::*;
    #[test]
    fn independent_connections_and_restart_cannot_overspend() {
        let dir = std::env::temp_dir().join(format!("gm-usage-{}", uuid::Uuid::new_v4()));
        let config = BudgetConfig {
            daily_spend_cap_usd: 1.0,
            per_job_spend_cap_usd: 1.0,
            max_daily_notifications: 3,
        };
        let first = BudgetManager::open(config.clone(), &dir).unwrap();
        let second = BudgetManager::open(config.clone(), &dir).unwrap();
        let request = first.reserve(0.7).unwrap();
        assert!(second.reserve(0.4).is_err());
        request.settle(0.2).unwrap();
        let pending = second.reserve(0.6).unwrap();
        // Simulate a process dying without running reservation destructors.
        std::mem::forget(pending);
        drop(first);
        drop(second);
        let restart = BudgetManager::open(config, &dir).unwrap();
        assert!((restart.get_daily_spent() - 0.8).abs() < 1e-9);
        assert!(restart.reserve(0.3).is_err());
        let request = restart.reserve(0.1).unwrap();
        request.release();
        assert!((restart.get_daily_spent() - 0.8).abs() < 1e-9);
        drop(restart);
        let _ = std::fs::remove_dir_all(dir);
    }
}

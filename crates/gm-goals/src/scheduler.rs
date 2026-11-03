use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::sync::Arc;
use tracing::info;

pub type JobHandler = Arc<dyn Fn() + Send + Sync>;

pub struct ScheduledJob {
    pub name: String,
    pub interval_secs: u64,
    pub next_run: DateTime<Utc>,
    pub handler: JobHandler,
}

#[derive(Clone)]
pub struct ProactiveScheduler {
    jobs: Arc<Mutex<Vec<ScheduledJob>>>,
    low_power_mode: Arc<Mutex<bool>>,
    battery_level_percent: Arc<Mutex<u8>>,
}

impl Default for ProactiveScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProactiveScheduler {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(Vec::new())),
            low_power_mode: Arc::new(Mutex::new(false)),
            battery_level_percent: Arc::new(Mutex::new(100)),
        }
    }

    pub fn set_low_power_mode(&self, enabled: bool) {
        *self.low_power_mode.lock() = enabled;
    }

    pub fn set_battery_level(&self, pct: u8) {
        *self.battery_level_percent.lock() = pct;
    }

    pub fn register_job<F>(&self, name: impl Into<String>, interval_secs: u64, handler: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        let job = ScheduledJob {
            name: name.into(),
            interval_secs,
            next_run: Utc::now() + chrono::Duration::seconds(interval_secs as i64),
            handler: Arc::new(handler),
        };
        self.jobs.lock().push(job);
    }

    /// Evaluates jobs to run, coalescing missed jobs on wake and checking power conditions.
    pub fn tick(&self, now: DateTime<Utc>) -> u32 {
        if *self.low_power_mode.lock() {
            info!("Skipping scheduler tick: Low Power Mode active");
            return 0;
        }

        if *self.battery_level_percent.lock() < 20 {
            info!("Skipping scheduler tick: Battery < 20%");
            return 0;
        }

        let mut ran = 0;
        let mut jobs = self.jobs.lock();
        for job in jobs.iter_mut() {
            if now >= job.next_run {
                info!("Executing scheduled proactive job: {}", job.name);
                (job.handler)();
                ran += 1;
                // Coalesce missed runs by scheduling from `now`
                job.next_run = now + chrono::Duration::seconds(job.interval_secs as i64);
            }
        }
        ran
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_scheduler() {
        let scheduler = ProactiveScheduler::new();
        let counter = Arc::new(AtomicU32::new(0));

        let c = Arc::clone(&counter);
        scheduler.register_job("patrol", 60, move || {
            c.fetch_add(1, Ordering::SeqCst);
        });

        // Tick before next_run -> 0 ran
        let ran = scheduler.tick(Utc::now());
        assert_eq!(ran, 0);

        // Tick after next_run -> 1 ran
        let future = Utc::now() + chrono::Duration::seconds(100);
        let ran = scheduler.tick(future);
        assert_eq!(ran, 1);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Low power mode check
        scheduler.set_low_power_mode(true);
        let ran = scheduler.tick(future + chrono::Duration::seconds(100));
        assert_eq!(ran, 0);
    }
}

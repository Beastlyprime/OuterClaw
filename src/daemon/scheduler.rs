//! Internal task scheduler replacing systemd timers.
//!
//! Tracks periodic tasks (snapshot, healthcheck, LKG promotion, cloud sync)
//! and reports which ones are due on each tick.

use std::time::Instant;

/// A single scheduled periodic task.
pub struct ScheduledTask {
    pub name: String,
    pub interval_secs: u64,
    pub last_run: Option<Instant>,
    pub enabled: bool,
}

/// Scheduler that tracks multiple periodic tasks.
pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
}

impl Scheduler {
    /// Create a scheduler with the default set of tasks.
    ///
    /// - `snapshot`: every 1800s (30 min)
    /// - `healthcheck`: every 120s (2 min)
    /// - `lkg_promote`: every 7200s (2 hr)
    /// - `cloud_sync`: every 7200s (2 hr), disabled by default
    pub fn new(cloud_enabled: bool) -> Self {
        Self {
            tasks: vec![
                ScheduledTask {
                    name: "snapshot".into(),
                    interval_secs: 1800,
                    last_run: None,
                    enabled: true,
                },
                ScheduledTask {
                    name: "healthcheck".into(),
                    interval_secs: 120,
                    last_run: None,
                    enabled: true,
                },
                ScheduledTask {
                    name: "lkg_promote".into(),
                    interval_secs: 7200,
                    last_run: None,
                    enabled: true,
                },
                ScheduledTask {
                    name: "cloud_sync".into(),
                    interval_secs: 7200,
                    last_run: None,
                    enabled: cloud_enabled,
                },
            ],
        }
    }

    /// Check all tasks and return the names of those that are due.
    ///
    /// A task is due if it is enabled and either has never run or its
    /// interval has elapsed since its last run.  Due tasks have their
    /// `last_run` updated to `now`.
    pub fn tick(&mut self) -> Vec<String> {
        let now = Instant::now();
        let mut due = Vec::new();

        for task in &mut self.tasks {
            if !task.enabled {
                continue;
            }
            let is_due = match task.last_run {
                None => true,
                Some(last) => now.duration_since(last).as_secs() >= task.interval_secs,
            };
            if is_due {
                task.last_run = Some(now);
                due.push(task.name.clone());
            }
        }

        due
    }

    /// Enable or disable a task by name.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        for task in &mut self.tasks {
            if task.name == name {
                task.enabled = enabled;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_tick_fires_all_enabled() {
        let mut sched = Scheduler::new(false);
        let due = sched.tick();
        // cloud_sync disabled, so 3 tasks
        assert_eq!(due.len(), 3);
        assert!(due.contains(&"snapshot".to_string()));
        assert!(due.contains(&"healthcheck".to_string()));
        assert!(due.contains(&"lkg_promote".to_string()));
        assert!(!due.contains(&"cloud_sync".to_string()));
    }

    #[test]
    fn test_second_tick_fires_nothing() {
        let mut sched = Scheduler::new(false);
        let _ = sched.tick();
        // Immediately after, nothing should be due
        let due = sched.tick();
        assert!(due.is_empty());
    }

    #[test]
    fn test_cloud_sync_when_enabled() {
        let mut sched = Scheduler::new(true);
        let due = sched.tick();
        assert!(due.contains(&"cloud_sync".to_string()));
    }

    #[test]
    fn test_set_enabled() {
        let mut sched = Scheduler::new(false);
        sched.set_enabled("cloud_sync", true);
        let _ = sched.tick(); // consume first tick
        sched.set_enabled("cloud_sync", false);
        // Force a second tick that would normally not fire
        for task in &mut sched.tasks {
            if task.name == "cloud_sync" {
                task.last_run = None; // pretend never ran
            }
        }
        let due = sched.tick();
        assert!(!due.contains(&"cloud_sync".to_string()));
    }
}

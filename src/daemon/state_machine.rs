//! State machine for gateway process classification.
//!
//! Direct port of the Python `State` enum and `_classify()` logic from
//! `outerclaw.py`.  The classifier tracks I/O stall duration across ticks
//! to distinguish brief inference pauses from genuine hangs.

use crate::platform::ProcessMetrics;

/// Gateway process state as determined by the watchdog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum State {
    Unknown,
    Healthy,
    HeavyInference,
    PossibleHang,
    ConfirmedHang,
    Zombie,
    Down,
}

impl State {
    /// Human-readable label matching the Python `State.value` strings.
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Unknown => "UNKNOWN",
            State::Healthy => "HEALTHY",
            State::HeavyInference => "HEAVY_INFERENCE",
            State::PossibleHang => "POSSIBLE_HANG",
            State::ConfirmedHang => "CONFIRMED_HANG",
            State::Zombie => "ZOMBIE",
            State::Down => "DOWN",
        }
    }

    /// Whether this state warrants immediate attention.
    pub fn is_critical(&self) -> bool {
        matches!(self, State::ConfirmedHang | State::Zombie | State::Down)
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stateful classifier that tracks I/O stall duration across ticks.
pub struct Classifier {
    /// UNIX epoch timestamp when I/O stall was first observed.
    /// `None` means no active stall.
    pub stall_since: Option<f64>,
}

impl Classifier {
    pub fn new() -> Self {
        Self { stall_since: None }
    }

    /// Classify the current gateway process state.
    ///
    /// Direct port of Python `OuterClaw._classify()`.
    ///
    /// - `metrics`: current /proc snapshot, or `None` if PID not found
    /// - `prev_metrics`: previous collection cycle snapshot
    /// - `http_ok`: whether `/health` returned HTTP 200
    /// - `now`: current UNIX epoch timestamp
    pub fn classify(
        &mut self,
        metrics: Option<&ProcessMetrics>,
        prev_metrics: Option<&ProcessMetrics>,
        http_ok: bool,
        now: f64,
        hang_warn_secs: u64,
        hang_crit_secs: u64,
        io_delta_threshold: u64,
        ctx_switch_threshold: u64,
    ) -> State {
        // No process found -> DOWN
        let metrics = match metrics {
            Some(m) => m,
            None => {
                self.stall_since = None;
                return State::Down;
            }
        };

        // Zombie check
        if metrics.state == "Z" {
            self.stall_since = None;
            return State::Zombie;
        }

        // HTTP responds -> HEALTHY
        if http_ok {
            self.stall_since = None;
            return State::Healthy;
        }

        // HTTP down -- analyse I/O activity
        let mut io_delta: i64 = 0;
        let mut ctx_delta: i64 = 0;
        if let Some(prev) = prev_metrics {
            if prev.pid == metrics.pid {
                io_delta = (metrics.read_bytes as i64 - prev.read_bytes as i64)
                    + (metrics.write_bytes as i64 - prev.write_bytes as i64);
                ctx_delta = (metrics.voluntary_ctxt_switches as i64
                    - prev.voluntary_ctxt_switches as i64)
                    + (metrics.nonvoluntary_ctxt_switches as i64
                        - prev.nonvoluntary_ctxt_switches as i64);
            }
        }

        // Active I/O -> normal inference, reset stall timer
        if io_delta > io_delta_threshold as i64 || ctx_delta > ctx_switch_threshold as i64 {
            self.stall_since = None;
            return State::HeavyInference;
        }

        // I/O stalled -- start or continue tracking
        if self.stall_since.is_none() {
            self.stall_since = Some(now);
        }

        let stall_duration = now - self.stall_since.unwrap();

        if stall_duration > hang_crit_secs as f64 {
            return State::ConfirmedHang;
        }
        if stall_duration > hang_warn_secs as f64 {
            return State::PossibleHang;
        }

        // Brief stall, not yet concerning
        State::HeavyInference
    }
}

/// Determine what alert (if any) should fire on a state transition.
///
/// Returns `Some((level, message))` when an alert should be sent, or `None`
/// if no alert is warranted (same state, or a non-alertable transition).
pub fn alert_on_transition(
    old: State,
    new: State,
    hang_warn_secs: u64,
    hang_crit_secs: u64,
) -> Option<(&'static str, String)> {
    if old == new {
        return None;
    }

    log::info!("State transition: {} -> {}", old.as_str(), new.as_str());

    // Recovery
    if new == State::Healthy
        && matches!(
            old,
            State::Down
                | State::PossibleHang
                | State::ConfirmedHang
                | State::Zombie
                | State::Unknown
        )
    {
        return Some((
            "INFO",
            format!("Gateway recovered: {} -> HEALTHY", old.as_str()),
        ));
    }

    // Degradation
    match new {
        State::PossibleHang => Some((
            "WARNING",
            format!("Gateway may be hung (I/O stalled >{hang_warn_secs}s)"),
        )),
        State::ConfirmedHang => Some((
            "CRITICAL",
            format!("Gateway CONFIRMED hung (I/O stalled >{hang_crit_secs}s)"),
        )),
        State::Zombie => Some(("CRITICAL", "Gateway process is ZOMBIE (state=Z)".into())),
        State::Down => Some(("WARNING", "Gateway process DOWN (PID not found)".into())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_metrics(pid: u32, state: &str) -> ProcessMetrics {
        ProcessMetrics {
            pid,
            state: state.into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_no_pid_is_down() {
        let mut c = Classifier::new();
        assert_eq!(
            c.classify(None, None, false, 1000.0, 120, 300, 1_048_576, 10),
            State::Down
        );
    }

    #[test]
    fn test_zombie() {
        let mut c = Classifier::new();
        let m = make_metrics(1, "Z");
        assert_eq!(
            c.classify(Some(&m), None, false, 1000.0, 120, 300, 1_048_576, 10),
            State::Zombie
        );
    }

    #[test]
    fn test_http_ok_is_healthy() {
        let mut c = Classifier::new();
        let m = make_metrics(1, "S");
        assert_eq!(
            c.classify(Some(&m), None, true, 1000.0, 120, 300, 1_048_576, 10),
            State::Healthy
        );
    }

    #[test]
    fn test_stall_escalation() {
        let mut c = Classifier::new();
        let m = make_metrics(1, "S");

        // First tick: starts stall, returns HeavyInference
        assert_eq!(
            c.classify(Some(&m), None, false, 1000.0, 120, 300, 1_048_576, 10),
            State::HeavyInference
        );
        assert!(c.stall_since.is_some());

        // After warn threshold: PossibleHang
        assert_eq!(
            c.classify(Some(&m), Some(&m), false, 1121.0, 120, 300, 1_048_576, 10),
            State::PossibleHang
        );

        // After crit threshold: ConfirmedHang
        assert_eq!(
            c.classify(Some(&m), Some(&m), false, 1301.0, 120, 300, 1_048_576, 10),
            State::ConfirmedHang
        );
    }

    #[test]
    fn test_io_activity_resets_stall() {
        let mut c = Classifier::new();
        let m1 = ProcessMetrics {
            pid: 1,
            state: "S".into(),
            read_bytes: 0,
            ..Default::default()
        };
        // Start a stall
        c.classify(Some(&m1), None, false, 1000.0, 120, 300, 1_048_576, 10);
        assert!(c.stall_since.is_some());

        // Next tick has significant I/O
        let m2 = ProcessMetrics {
            pid: 1,
            state: "S".into(),
            read_bytes: 2_000_000,
            ..Default::default()
        };
        assert_eq!(
            c.classify(Some(&m2), Some(&m1), false, 1030.0, 120, 300, 1_048_576, 10),
            State::HeavyInference
        );
        assert!(c.stall_since.is_none());
    }

    #[test]
    fn test_alert_on_transition_recovery() {
        let result = alert_on_transition(State::Down, State::Healthy, 120, 300);
        assert!(result.is_some());
        let (level, _msg) = result.unwrap();
        assert_eq!(level, "INFO");
    }

    #[test]
    fn test_alert_on_transition_same_state() {
        assert!(alert_on_transition(State::Healthy, State::Healthy, 120, 300).is_none());
    }

    #[test]
    fn test_alert_on_transition_degradation() {
        let result = alert_on_transition(State::Healthy, State::Down, 120, 300);
        assert!(result.is_some());
        let (level, _) = result.unwrap();
        assert_eq!(level, "WARNING");
    }
}

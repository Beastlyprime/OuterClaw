use std::time::{SystemTime, UNIX_EPOCH};

/// Format a UNIX timestamp as "Xs ago", "Xm ago", etc.
pub fn fmt_ago(ts: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let delta = (now - ts) as i64;
    if delta < 0 {
        return "in the future".into();
    }
    let delta = delta as u64;
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h {}m ago", delta / 3600, (delta % 3600) / 60)
    } else {
        format!("{}d {}h ago", delta / 86400, (delta % 86400) / 3600)
    }
}

/// Format seconds as human-readable uptime: "X days Y hrs Z mins"
pub fn fmt_uptime(seconds: u64) -> String {
    let d = seconds / 86400;
    let h = (seconds % 86400) / 3600;
    let m = (seconds % 3600) / 60;
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d} day{}", if d != 1 { "s" } else { "" }));
    }
    if h > 0 {
        parts.push(format!("{h} hr{}", if h != 1 { "s" } else { "" }));
    }
    if m > 0 || parts.is_empty() {
        parts.push(format!("{m} min{}", if m != 1 { "s" } else { "" }));
    }
    parts.join(" ")
}

/// Current UNIX timestamp as f64.
pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_uptime() {
        assert_eq!(fmt_uptime(0), "0 mins");
        assert_eq!(fmt_uptime(90061), "1 day 1 hr 1 min");
        assert_eq!(fmt_uptime(7200), "2 hrs");
    }
}

use crate::platform::Platform;

/// Check I/O pressure. Returns true if OK to proceed.
pub fn check(platform: &dyn Platform, threshold: f32) -> bool {
    match platform.io_pressure_avg10() {
        Ok(Some(pressure)) => {
            if pressure >= threshold {
                log::warn!("I/O pressure high: {pressure:.1}% >= {threshold}%");
                false
            } else {
                true
            }
        }
        Ok(None) => true, // PSI unavailable, allow
        Err(e) => {
            log::warn!("Failed to read I/O pressure: {e}");
            true // fail open
        }
    }
}

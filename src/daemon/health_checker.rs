//! Health check via HTTP GET against the gateway's `/health` endpoint.

use std::time::Duration;

/// Perform an HTTP GET health check against the given URL.
///
/// Returns `true` if the server responds with HTTP 200 within the timeout,
/// `false` on any error (connection refused, timeout, non-200 status, etc.).
pub fn check_health(url: &str, timeout_secs: u64) -> bool {
    let result = ureq::builder()
        .timeout_connect(Duration::from_secs(timeout_secs))
        .timeout_read(Duration::from_secs(timeout_secs))
        .build()
        .get(url)
        .call();

    match result {
        Ok(resp) => resp.status() == 200,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unreachable_returns_false() {
        // Port 1 is almost certainly not listening
        assert!(!check_health("http://127.0.0.1:1/health", 1));
    }

    #[test]
    fn test_invalid_url_returns_false() {
        assert!(!check_health("not-a-url", 1));
    }
}

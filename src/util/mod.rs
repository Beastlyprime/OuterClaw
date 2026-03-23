pub mod atomic_write;
pub mod time_fmt;

/// Validate a session ID (alphanumeric, dash, underscore).
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_validation() {
        assert!(is_valid_session_id("abc-123_def"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("bad;injection"));
        assert!(!is_valid_session_id("bad space"));
    }
}

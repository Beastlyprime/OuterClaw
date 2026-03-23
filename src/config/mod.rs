use std::path::{Path, PathBuf};

/// OuterClaw configuration — loaded from env file + environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    // Gateway
    pub gateway_port: u16,
    pub gateway_service: String,
    pub health_timeout: u64,
    pub health_url: String,
    pub sessions_url: String,

    // Thresholds
    pub tick_interval: u64,
    pub collect_interval: u64,
    pub hang_warn_secs: u64,
    pub hang_crit_secs: u64,
    pub io_delta_threshold: u64,
    pub ctx_switch_threshold: u64,

    // Recovery
    pub restart_settle_wait: u64,
    pub recovery_cooldown: u64,
    pub kill_graceful_timeout: u64,

    // Identity
    pub identity_unlock_timeout: u64,

    // Paths
    pub openclaw_dir: PathBuf,
    pub vault_dir: PathBuf,
    pub agent_user: String,

    // Telegram
    pub tg_token: String,
    pub tg_chat: String,
    pub tg_is_dedicated: bool,

    // Vault limits
    pub max_vault_mb: u64,
    pub io_pressure_threshold: f32,

    // Cloud
    pub cloud_enabled: bool,
    pub cloud_remote: String,
    pub cloud_bandwidth: u64,

    // Misc
    pub max_response_bytes: usize,
}

impl Config {
    /// Load configuration from env file and environment variables.
    /// Priority: environment variables > env file > defaults
    pub fn load() -> Result<Self, String> {
        let env_file = PathBuf::from("/var/lib/outerclaw/config/outerclaw.env");
        let env_map = if env_file.exists() {
            parse_env_file(&env_file)?
        } else {
            std::collections::HashMap::new()
        };

        // Helper: env var > env file > default
        let get = |key: &str, default: &str| -> String {
            std::env::var(key).unwrap_or_else(|_| {
                env_map
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| default.to_string())
            })
        };

        let gateway_port: u16 = get("GATEWAY_PORT", "18789")
            .parse()
            .map_err(|_| "Invalid GATEWAY_PORT")?;

        if gateway_port == 0 {
            return Err("GATEWAY_PORT must be > 0".into());
        }

        let openclaw_dir = PathBuf::from(get("OPENCLAW_DIR", "/home/ocagent/.openclaw"));

        // Telegram: try dedicated token first, fall back to openclaw.json
        let mut tg_token = get("OUTERCLAW_TG_TOKEN", "");
        let mut tg_chat = get("OUTERCLAW_TG_CHAT", "");
        let tg_is_dedicated = !tg_token.is_empty() && !tg_chat.is_empty();

        if tg_token.is_empty() || tg_chat.is_empty() {
            if let Some((token, chat)) = load_tg_from_openclaw(&openclaw_dir) {
                if tg_token.is_empty() {
                    tg_token = token;
                }
                if tg_chat.is_empty() {
                    tg_chat = chat;
                }
            }
        }

        // Validate TG token format
        if !tg_token.is_empty() && !is_valid_tg_token(&tg_token) {
            log::warn!("Invalid TG_TOKEN format, disabling Telegram");
            tg_token.clear();
        }

        Ok(Config {
            gateway_port,
            gateway_service: "openclaw-gateway.service".into(),
            health_timeout: 5,
            health_url: format!("http://127.0.0.1:{gateway_port}/health"),
            sessions_url: format!("http://127.0.0.1:{gateway_port}/sessions"),

            tick_interval: 1,
            collect_interval: get("COLLECT_INTERVAL", "30").parse().unwrap_or(30),
            hang_warn_secs: get("HANG_WARN_SECS", "120").parse().unwrap_or(120),
            hang_crit_secs: get("HANG_CRIT_SECS", "300").parse().unwrap_or(300),
            io_delta_threshold: 1_048_576,
            ctx_switch_threshold: 10,

            restart_settle_wait: get("RESTART_SETTLE_WAIT", "90").parse().unwrap_or(90),
            recovery_cooldown: get("RECOVERY_COOLDOWN", "1800").parse().unwrap_or(1800),
            kill_graceful_timeout: 15,

            identity_unlock_timeout: 600,

            openclaw_dir,
            vault_dir: PathBuf::from("/var/lib/outerclaw"),
            agent_user: get("AGENT_USER", "ocagent"),

            tg_token,
            tg_chat,
            tg_is_dedicated,

            max_vault_mb: get("MAX_VAULT_MB", "2048").parse().unwrap_or(2048),
            io_pressure_threshold: get("IO_PRESSURE_THRESHOLD", "25.0").parse().unwrap_or(25.0),

            cloud_enabled: get("CLOUD_ENABLED", "false") == "true",
            cloud_remote: get("CLOUD_REMOTE", "outerclaw-crypt"),
            cloud_bandwidth: get("CLOUD_BANDWIDTH", "0").parse().unwrap_or(0),

            max_response_bytes: 1_048_576,
        })
    }

    /// Watched identity/config file paths
    pub fn watched_files(&self) -> Vec<PathBuf> {
        let ws = self.openclaw_dir.join("workspace");
        vec![
            ws.join("SOUL.md"),
            ws.join("AGENTS.md"),
            ws.join("USER.md"),
            self.openclaw_dir.join("openclaw.json"),
        ]
    }
}

/// Parse a shell-style env file (KEY=VALUE, # comments, no eval/source).
fn parse_env_file(path: &Path) -> Result<std::collections::HashMap<String, String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;

    let mut map = std::collections::HashMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = trimmed.split_once('=') {
            let key = key.trim();
            let val = val.trim().trim_matches('"').trim_matches('\'');
            map.insert(key.to_string(), val.to_string());
        }
    }
    Ok(map)
}

/// Load Telegram credentials from openclaw.json as fallback.
fn load_tg_from_openclaw(openclaw_dir: &Path) -> Option<(String, String)> {
    let config_path = openclaw_dir.join("openclaw.json");
    let content = std::fs::read_to_string(&config_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let tg = json.get("channels")?.get("telegram")?;
    let token = tg.get("botToken")?.as_str()?.to_string();
    let chat = tg
        .get("allowFrom")?
        .as_array()?
        .first()?
        .to_string()
        .trim_matches('"')
        .to_string();

    if token.is_empty() || chat.is_empty() {
        return None;
    }
    Some((token, chat))
}

/// Validate Telegram bot token format: digits:alphanumeric
fn is_valid_tg_token(token: &str) -> bool {
    let parts: Vec<&str> = token.splitn(2, ':').collect();
    if parts.len() != 2 {
        return false;
    }
    parts[0].chars().all(|c| c.is_ascii_digit())
        && !parts[0].is_empty()
        && parts[1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !parts[1].is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_tg_token() {
        assert!(is_valid_tg_token("123456:ABCdef_-789"));
        assert!(!is_valid_tg_token(""));
        assert!(!is_valid_tg_token("nocolon"));
        assert!(!is_valid_tg_token(":noid"));
        assert!(!is_valid_tg_token("123:bad space"));
        assert!(!is_valid_tg_token("123:bad;injection"));
    }

    #[test]
    fn test_parse_env_file() {
        let dir = std::env::temp_dir().join("outerclaw_test_env");
        std::fs::write(
            &dir,
            "# comment\nGATEWAY_PORT=18789\n AGENT_USER = ocagent \nQUOTED=\"value\"\n",
        )
        .unwrap();
        let map = parse_env_file(&dir).unwrap();
        assert_eq!(map.get("GATEWAY_PORT").unwrap(), "18789");
        assert_eq!(map.get("AGENT_USER").unwrap(), "ocagent");
        assert_eq!(map.get("QUOTED").unwrap(), "value");
        std::fs::remove_file(&dir).ok();
    }
}

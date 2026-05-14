use std::path::{Path, PathBuf};

/// Privilege and isolation mode the OuterClaw deployment runs in.
///
/// * `Sudo` — production default. Three-user isolation (agent / watchdog / admin),
///   system-level systemd units, ACL-based cross-user read access. Required for
///   any deployment exposed to untrusted input (e.g. public Discord bots).
/// * `User` — single-UID mode for personal machines where no privilege boundary
///   is needed between the agent and the watchdog. Uses `systemd --user` units
///   and per-user data directories; no ACLs, no sudo. Disables the isolation
///   guarantee in exchange for not needing root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Sudo,
    User,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Sudo => "sudo",
            Mode::User => "user",
        }
    }

    fn from_env(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "user" | "user-mode" | "usermode" => Mode::User,
            _ => Mode::Sudo,
        }
    }
}

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

    // Mode & paths
    pub mode: Mode,
    pub openclaw_dir: PathBuf,
    pub vault_dir: PathBuf,
    pub agent_user: String,
    pub agent_home: PathBuf,
    pub watchdog_user: String,

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
    /// Priority: environment variables > env file > mode-derived defaults
    pub fn load() -> Result<Self, String> {
        // Mode is read first because it determines path defaults. Source order:
        // env var only (the env file itself lives at a mode-dependent path).
        let mode = Mode::from_env(&std::env::var("OUTERCLAW_MODE").unwrap_or_default());

        let env_file = default_env_file_path(mode);
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

        // Mode-derived identity & paths. User-mode collapses agent and watchdog
        // onto the current $USER and roots everything under $HOME; sudo-mode
        // keeps the canonical three-user layout under /home/ocagent and /var/lib.
        let (current_user, home_dir) = match mode {
            Mode::User => (
                std::env::var("USER").unwrap_or_else(|_| "outerclaw".into()),
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
            ),
            Mode::Sudo => (String::new(), String::new()),
        };

        let agent_user = match mode {
            Mode::Sudo => get("AGENT_USER", "ocagent"),
            Mode::User => get("AGENT_USER", &current_user),
        };

        let agent_home_default = match mode {
            Mode::Sudo => format!("/home/{agent_user}"),
            Mode::User => home_dir.clone(),
        };
        let agent_home = PathBuf::from(get("AGENT_HOME", &agent_home_default));

        let openclaw_default = format!("{}/.openclaw", agent_home.display());
        let openclaw_dir = PathBuf::from(get("OPENCLAW_DIR", &openclaw_default));

        let vault_default = match mode {
            Mode::Sudo => "/var/lib/outerclaw".to_string(),
            Mode::User => format!("{home_dir}/.local/share/outerclaw"),
        };
        let vault_dir = PathBuf::from(get("VAULT_DIR", &vault_default));

        let watchdog_user = match mode {
            Mode::Sudo => get("WATCHDOG_USER", "outerclaw"),
            Mode::User => get("WATCHDOG_USER", &current_user),
        };

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

            mode,
            openclaw_dir,
            vault_dir,
            agent_user,
            agent_home,
            watchdog_user,

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

    // ── Vault-derived path helpers ─────────────────────────────────
    //
    // Always derive from `vault_dir` so a non-default vault (set via env or
    // VAULT_DIR override) propagates everywhere. Call sites that previously
    // hardcoded `/var/lib/outerclaw/...` should switch to these.

    pub fn bin_dir(&self) -> PathBuf {
        self.vault_dir.join("bin")
    }

    pub fn bin_path(&self) -> PathBuf {
        self.bin_dir().join("outerclaw")
    }

    pub fn config_dir(&self) -> PathBuf {
        self.vault_dir.join("config")
    }

    pub fn env_file_path(&self) -> PathBuf {
        self.config_dir().join("outerclaw.env")
    }

    pub fn rclone_config_path(&self) -> PathBuf {
        self.config_dir().join("rclone.conf")
    }

    pub fn audit_dir(&self) -> PathBuf {
        self.vault_dir.join("audit")
    }

    pub fn snapshots_dir(&self) -> PathBuf {
        self.vault_dir.join("snapshots")
    }

    pub fn lkg_dir(&self) -> PathBuf {
        self.vault_dir.join("lkg")
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

/// Default env file location for a given mode. The env var `OUTERCLAW_ENV_FILE`
/// can override this if a user has installed in a non-standard location.
fn default_env_file_path(mode: Mode) -> PathBuf {
    if let Ok(p) = std::env::var("OUTERCLAW_ENV_FILE") {
        return PathBuf::from(p);
    }
    match mode {
        Mode::Sudo => PathBuf::from("/var/lib/outerclaw/config/outerclaw.env"),
        Mode::User => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(format!("{home}/.config/outerclaw/outerclaw.env"))
        }
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
    fn test_mode_from_env() {
        assert_eq!(Mode::from_env(""), Mode::Sudo);
        assert_eq!(Mode::from_env("sudo"), Mode::Sudo);
        assert_eq!(Mode::from_env("anything-else"), Mode::Sudo);
        assert_eq!(Mode::from_env("user"), Mode::User);
        assert_eq!(Mode::from_env("USER"), Mode::User);
        assert_eq!(Mode::from_env("user-mode"), Mode::User);
        assert_eq!(Mode::from_env("usermode"), Mode::User);
    }

    #[test]
    fn test_mode_as_str() {
        assert_eq!(Mode::Sudo.as_str(), "sudo");
        assert_eq!(Mode::User.as_str(), "user");
    }

    #[test]
    fn test_default_env_file_path_sudo() {
        // Clear override so we exercise the mode-based default
        let prev = std::env::var("OUTERCLAW_ENV_FILE").ok();
        unsafe {
            std::env::remove_var("OUTERCLAW_ENV_FILE");
        }
        assert_eq!(
            default_env_file_path(Mode::Sudo),
            PathBuf::from("/var/lib/outerclaw/config/outerclaw.env")
        );
        // Restore prior state
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("OUTERCLAW_ENV_FILE", v);
            }
        }
    }

    #[test]
    fn test_vault_helper_methods() {
        let cfg = Config {
            gateway_port: 18789,
            gateway_service: "openclaw-gateway.service".into(),
            health_timeout: 5,
            health_url: "".into(),
            sessions_url: "".into(),
            tick_interval: 1,
            collect_interval: 30,
            hang_warn_secs: 120,
            hang_crit_secs: 300,
            io_delta_threshold: 0,
            ctx_switch_threshold: 0,
            restart_settle_wait: 90,
            recovery_cooldown: 1800,
            kill_graceful_timeout: 15,
            identity_unlock_timeout: 600,
            mode: Mode::Sudo,
            openclaw_dir: PathBuf::from("/home/ocagent/.openclaw"),
            vault_dir: PathBuf::from("/opt/custom-vault"),
            agent_user: "ocagent".into(),
            agent_home: PathBuf::from("/home/ocagent"),
            watchdog_user: "outerclaw".into(),
            tg_token: "".into(),
            tg_chat: "".into(),
            tg_is_dedicated: false,
            max_vault_mb: 2048,
            io_pressure_threshold: 25.0,
            cloud_enabled: false,
            cloud_remote: "".into(),
            cloud_bandwidth: 0,
            max_response_bytes: 0,
        };
        // Helpers should track vault_dir, not hardcode /var/lib/outerclaw
        assert_eq!(
            cfg.bin_path(),
            PathBuf::from("/opt/custom-vault/bin/outerclaw")
        );
        assert_eq!(
            cfg.env_file_path(),
            PathBuf::from("/opt/custom-vault/config/outerclaw.env")
        );
        assert_eq!(
            cfg.rclone_config_path(),
            PathBuf::from("/opt/custom-vault/config/rclone.conf")
        );
        assert_eq!(cfg.audit_dir(), PathBuf::from("/opt/custom-vault/audit"));
        assert_eq!(
            cfg.snapshots_dir(),
            PathBuf::from("/opt/custom-vault/snapshots")
        );
        assert_eq!(cfg.lkg_dir(), PathBuf::from("/opt/custom-vault/lkg"));
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

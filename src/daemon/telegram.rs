//! Two-way Telegram bot using getUpdates long-polling.
//!
//! Runs in a daemon thread.  Commands are queued via `std::sync::mpsc`
//! for the main thread to process (destructive actions like restart/kill
//! must happen on the main thread for safety).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Long-poll timeout for Telegram getUpdates (seconds).
const POLL_TIMEOUT: u64 = 15;
/// Back-off on API errors (seconds).
const ERROR_BACKOFF: u64 = 30;
/// Minimum interval between destructive commands (seconds).
const DESTRUCTIVE_COOLDOWN: u64 = 60;

/// A command parsed from a Telegram message, queued for the main thread.
#[derive(Debug, Clone)]
pub struct TelegramCommand {
    pub action: String,
    pub user: String,
    pub args: HashMap<String, String>,
}

/// A quick-status snapshot passed from the main thread to the bot for /status.
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub state: String,
    pub pid: String,
    pub uptime: String,
    pub rss_mb: String,
    pub last_check: String,
}

/// Two-way Telegram bot.
pub struct TelegramBot {
    token: String,
    chat_id: String,
    base_url: String,
    running: Arc<AtomicBool>,
    max_response_bytes: usize,
}

impl TelegramBot {
    pub fn new(token: &str, chat_id: &str, max_response_bytes: usize) -> Self {
        Self {
            token: token.to_string(),
            chat_id: chat_id.to_string(),
            base_url: format!("https://api.telegram.org/bot{token}"),
            running: Arc::new(AtomicBool::new(true)),
            max_response_bytes,
        }
    }

    /// Start the long-polling loop in a background thread.
    ///
    /// `cmd_tx`: channel to send parsed commands to the main thread.
    /// `status_fn`: closure that returns a status snapshot (called on the
    /// bot thread, must be Send+Sync).
    pub fn start(
        &self,
        cmd_tx: Sender<TelegramCommand>,
        status_fn: Arc<dyn Fn() -> StatusSnapshot + Send + Sync>,
    ) -> std::thread::JoinHandle<()> {
        let token = self.token.clone();
        let chat_id = self.chat_id.clone();
        let base_url = self.base_url.clone();
        let running = self.running.clone();
        let max_bytes = self.max_response_bytes;

        log::info!("TelegramBot: polling started (chat_id={chat_id})");

        std::thread::Builder::new()
            .name("telegram-bot".into())
            .spawn(move || {
                let mut poller = Poller {
                    _token: token,
                    chat_id,
                    base_url,
                    running,
                    offset: 0,
                    last_destructive_cmd: None,
                    cmd_tx,
                    status_fn,
                    max_response_bytes: max_bytes,
                };
                poller.poll_loop();
            })
            .expect("Failed to spawn telegram-bot thread")
    }

    /// Send a message to the configured chat.
    pub fn send_message(&self, text: &str) -> bool {
        send_message_impl(&self.base_url, &self.chat_id, text)
    }

    /// Signal the polling thread to stop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Internal polling state (runs on the bot thread).
struct Poller {
    _token: String,
    chat_id: String,
    base_url: String,
    running: Arc<AtomicBool>,
    offset: i64,
    last_destructive_cmd: Option<Instant>,
    cmd_tx: Sender<TelegramCommand>,
    status_fn: Arc<dyn Fn() -> StatusSnapshot + Send + Sync>,
    max_response_bytes: usize,
}

impl Poller {
    fn poll_loop(&mut self) {
        while self.running.load(Ordering::Relaxed) {
            match self.get_updates() {
                Some(updates) => {
                    if let Some(arr) = updates.as_array() {
                        for update in arr {
                            if let Some(id) = update.get("update_id").and_then(|v| v.as_i64()) {
                                self.offset = id + 1;
                            }
                            self.handle_update(update);
                        }
                    }
                }
                None => {
                    std::thread::sleep(Duration::from_secs(ERROR_BACKOFF));
                }
            }
        }
    }

    fn get_updates(&self) -> Option<serde_json::Value> {
        let url = format!("{}/getUpdates", self.base_url);
        let body = serde_json::json!({
            "offset": self.offset,
            "timeout": POLL_TIMEOUT,
            "allowed_updates": ["message"],
        });

        let resp = ureq::builder()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(POLL_TIMEOUT + 10))
            .build()
            .post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());

        match resp {
            Ok(r) => {
                let text = match r.into_string() {
                    Ok(t) if t.len() <= self.max_response_bytes => t,
                    _ => return None,
                };
                let json: serde_json::Value = serde_json::from_str(&text).ok()?;
                if json.get("ok")?.as_bool()? {
                    json.get("result").cloned()
                } else {
                    None
                }
            }
            Err(e) => {
                log::debug!("TelegramBot getUpdates error: {e}");
                None
            }
        }
    }

    fn handle_update(&mut self, update: &serde_json::Value) {
        let msg = match update.get("message") {
            Some(m) => m,
            None => return,
        };
        let text = msg
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let chat_id = msg
            .get("chat")
            .and_then(|c| c.get("id"))
            .map(|id| id.to_string().trim_matches('"').to_string())
            .unwrap_or_default();
        let username = msg
            .get("from")
            .and_then(|f| f.get("username"))
            .and_then(|u| u.as_str())
            .unwrap_or("unknown");

        // Sanitize username for safe Markdown interpolation
        let safe_username = sanitize_markdown(username);

        if !text.starts_with('/') {
            return;
        }

        // Authorization: only accept from configured chat
        if chat_id != self.chat_id {
            log::warn!(
                "TelegramBot: rejected command from unauthorized chat {chat_id} (user: {safe_username})"
            );
            return;
        }

        // Parse command (strip @botname suffix)
        let parts: Vec<&str> = text.split_whitespace().collect();
        let cmd = parts[0].to_lowercase();
        let cmd = cmd.split('@').next().unwrap_or(&cmd);

        log::info!("TelegramBot: command '{cmd}' from {safe_username}");

        // Rate-limit destructive commands
        let destructive = matches!(
            cmd,
            "/restart" | "/kill" | "/kill_session" | "/unlock_identity"
        );
        if destructive {
            if let Some(last) = self.last_destructive_cmd {
                let elapsed = last.elapsed().as_secs();
                if elapsed < DESTRUCTIVE_COOLDOWN {
                    let remaining = DESTRUCTIVE_COOLDOWN - elapsed;
                    self.send(&format!(
                        "Rate limited -- wait {remaining}s before retrying."
                    ));
                    return;
                }
            }
            self.last_destructive_cmd = Some(Instant::now());
        }

        match cmd {
            "/help" => {
                self.send(
                    "*OuterClaw Commands*\n\
                     `/status` -- Gateway state & metrics\n\
                     `/restart` -- Restart gateway service\n\
                     `/kill` -- Stop gateway process\n\
                     `/sessions` -- List active sessions\n\
                     `/kill_session <id>` -- Kill a specific session\n\
                     `/snapshots` -- List recent snapshots\n\
                     `/unlock_identity` -- Unlock identity files (10 min timeout)\n\
                     `/lock_identity` -- Lock identity files\n\
                     `/help` -- This help message",
                );
            }
            "/status" => {
                let s = (self.status_fn)();
                self.send(&format!(
                    "*OuterClaw Status*\n\
                     State: `{}`\n\
                     PID: `{}`\n\
                     Uptime: `{}`\n\
                     RSS: `{}MB`\n\
                     Last check: `{}`",
                    s.state, s.pid, s.uptime, s.rss_mb, s.last_check,
                ));
            }
            "/restart" => {
                self.send(&format!(
                    "Restarting gateway (requested by {safe_username})..."
                ));
                self.queue_cmd("restart", &safe_username, HashMap::new());
            }
            "/kill" => {
                self.send(&format!(
                    "Stopping gateway (requested by {safe_username})..."
                ));
                self.queue_cmd("kill", &safe_username, HashMap::new());
            }
            "/sessions" => {
                self.cmd_sessions();
            }
            "/kill_session" => {
                if parts.len() < 2 {
                    self.send(
                        "Usage: `/kill_session <session_id>`\n\
                         Use `/sessions` to list active sessions.",
                    );
                } else {
                    let session_id = parts[1];
                    self.send(&format!(
                        "Killing session `{session_id}` (requested by {safe_username})..."
                    ));
                    let mut args = HashMap::new();
                    args.insert("session_id".into(), session_id.to_string());
                    self.queue_cmd("kill_session", &safe_username, args);
                }
            }
            "/snapshots" => {
                self.cmd_snapshots();
            }
            "/unlock_identity" => {
                self.send(&format!(
                    "Unlocking identity files (requested by {safe_username})..."
                ));
                self.queue_cmd("unlock_identity", &safe_username, HashMap::new());
            }
            "/lock_identity" => {
                self.queue_cmd("lock_identity", &safe_username, HashMap::new());
            }
            _ => {
                self.send(&format!(
                    "Unknown command: `{cmd}`\nUse /help for available commands."
                ));
            }
        }
    }

    fn queue_cmd(&self, action: &str, user: &str, args: HashMap<String, String>) {
        let cmd = TelegramCommand {
            action: action.into(),
            user: user.into(),
            args,
        };
        if self.cmd_tx.send(cmd).is_err() {
            log::error!("TelegramBot: command channel closed");
        }
    }

    fn send(&self, text: &str) {
        send_message_impl(&self.base_url, &self.chat_id, text);
    }

    fn cmd_sessions(&self) {
        let sessions_url = "http://127.0.0.1:18789/sessions".to_string();
        match fetch_json(&sessions_url, 10, self.max_response_bytes) {
            Ok(data) => {
                let sessions = if let Some(arr) = data.as_array() {
                    arr.clone()
                } else if let Some(arr) = data.get("sessions").and_then(|v| v.as_array()) {
                    arr.clone()
                } else {
                    self.send("No active sessions.");
                    return;
                };
                if sessions.is_empty() {
                    self.send("No active sessions.");
                    return;
                }
                let mut lines = Vec::new();
                for s in sessions.iter().take(20) {
                    let sid = s
                        .get("id")
                        .or_else(|| s.get("session_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let created = s
                        .get("created")
                        .or_else(|| s.get("started_at"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let status = s
                        .get("status")
                        .or_else(|| s.get("state"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let mut line = format!("`{sid}`");
                    if !status.is_empty() {
                        line.push_str(&format!(" [{status}]"));
                    }
                    if !created.is_empty() {
                        line.push_str(&format!(" ({created})"));
                    }
                    lines.push(line);
                }
                let header = format!("*Active Sessions ({})*\n", sessions.len());
                self.send(&format!("{header}{}", lines.join("\n")));
            }
            Err(_) => {
                self.send("Failed to query sessions (gateway unreachable).");
            }
        }
    }

    fn cmd_snapshots(&self) {
        let snapshot_dir = std::path::Path::new("/var/lib/outerclaw/snapshots");
        match std::fs::read_dir(snapshot_dir) {
            Ok(entries) => {
                let mut files: Vec<(String, u64)> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_str()
                            .map(|n| n.starts_with("main-") && n.ends_with(".sqlite"))
                            .unwrap_or(false)
                    })
                    .filter_map(|e| {
                        let meta = e.metadata().ok()?;
                        let name = e.file_name().to_string_lossy().into_owned();
                        let size_kb = meta.len() / 1024;
                        Some((name, size_kb))
                    })
                    .collect();
                files.sort_by(|a, b| b.0.cmp(&a.0)); // reverse sort by name
                files.truncate(5);
                if files.is_empty() {
                    self.send("No snapshots found.");
                } else {
                    let lines: Vec<String> = files
                        .iter()
                        .map(|(name, kb)| format!("`{name}` ({kb}KB)"))
                        .collect();
                    self.send(&format!("*Recent Snapshots*\n{}", lines.join("\n")));
                }
            }
            Err(e) => {
                self.send(&format!("Error listing snapshots: {e}"));
            }
        }
    }
}

// ── Shared helpers ─────────────────────────────────────────────

/// Send a Telegram message (used by both TelegramBot and the alert module).
fn send_message_impl(base_url: &str, chat_id: &str, text: &str) -> bool {
    let url = format!("{base_url}/sendMessage");
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "Markdown",
    });
    let resp = ureq::builder()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());

    match resp {
        Ok(_) => true,
        Err(e) => {
            log::debug!("TelegramBot sendMessage error: {e}");
            false
        }
    }
}

/// Fetch JSON from a URL with timeout.
fn fetch_json(url: &str, timeout_secs: u64, max_bytes: usize) -> Result<serde_json::Value, String> {
    let resp = ureq::builder()
        .timeout_connect(Duration::from_secs(timeout_secs))
        .timeout_read(Duration::from_secs(timeout_secs))
        .build()
        .get(url)
        .call()
        .map_err(|e| format!("{e}"))?;
    let text = resp.into_string().map_err(|e| format!("{e}"))?;
    if text.len() > max_bytes {
        return Err("Response too large".into());
    }
    serde_json::from_str(&text).map_err(|e| format!("{e}"))
}

/// Escape Markdown special characters in a username for safe interpolation.
fn sanitize_markdown(input: &str) -> String {
    let specials = ['\\', '`', '*', '_', '[', ']', '(', ')'];
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        if specials.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Public helper so the alert module can also send Telegram messages.
pub fn send_telegram_message(token: &str, chat_id: &str, text: &str) -> bool {
    let base_url = format!("https://api.telegram.org/bot{token}");
    send_message_impl(&base_url, chat_id, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_markdown() {
        assert_eq!(sanitize_markdown("hello"), "hello");
        assert_eq!(sanitize_markdown("a*b_c"), "a\\*b\\_c");
        assert_eq!(sanitize_markdown("a[b]"), "a\\[b\\]");
    }
}

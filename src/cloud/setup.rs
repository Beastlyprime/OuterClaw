//! Interactive cloud backup setup.
//!
//! Rust port of `scripts/cloud-setup.sh`. Guides the user through choosing
//! a cloud provider, collecting credentials, configuring client-side
//! encryption via rclone crypt, testing the connection, and enabling the
//! cloud sync timer.

use crate::config::Config;
use crate::platform::Platform;
use dialoguer::{Input, Password, Select};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const RCLONE_CONFIG_PATH: &str = "/var/lib/outerclaw/config/rclone.conf";
const ENV_FILE_PATH: &str = "/var/lib/outerclaw/config/outerclaw.env";
const BASE_REMOTE: &str = "outerclaw-base";
const CRYPT_REMOTE: &str = "outerclaw-crypt";

/// Run the interactive cloud backup setup.
pub fn run(cfg: Config, _platform: Box<dyn Platform>) -> i32 {
    // ── Step 0: Root check ──────────────────────────────────────
    if !nix::unistd::geteuid().is_root() {
        eprintln!("ERROR: This command must be run as root (sudo outerclaw cloud setup)");
        return 1;
    }

    println!();
    println!("======================================");
    println!("   OuterClaw Cloud Backup Setup");
    println!("======================================");
    println!();

    // ── Step 1: Check / install rclone ──────────────────────────
    println!("Step 1: Check rclone");
    if !check_rclone() {
        return 1;
    }
    println!();

    // ── Step 2: Choose provider and collect credentials ─────────
    println!("Step 2: Choose cloud provider");
    println!();

    let providers = &[
        "Cloudflare R2  (recommended -- 10GB free, no egress fees)",
        "Backblaze B2   (cheapest at scale -- $6/TB/month)",
        "AWS S3         (most widely used)",
        "Other          (manual rclone config)",
        "Use existing rclone remote",
    ];

    let choice = match Select::new()
        .with_prompt("Choose provider")
        .items(providers)
        .default(0)
        .interact()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ERROR: Failed to read selection: {e}");
            return 1;
        }
    };

    let rclone_config = PathBuf::from(RCLONE_CONFIG_PATH);

    // Ensure config directory exists
    if let Some(parent) = rclone_config.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("ERROR: Cannot create config directory: {e}");
            return 1;
        }
    }

    let (base_remote_name, remote_path) = match choice {
        0 => match setup_cloudflare_r2(&rclone_config) {
            Some(v) => v,
            None => return 1,
        },
        1 => match setup_backblaze_b2(&rclone_config) {
            Some(v) => v,
            None => return 1,
        },
        2 => match setup_aws_s3(&rclone_config) {
            Some(v) => v,
            None => return 1,
        },
        3 => match setup_other(&rclone_config) {
            Some(v) => v,
            None => return 1,
        },
        4 => match setup_existing(&rclone_config) {
            Some(v) => v,
            None => return 1,
        },
        _ => {
            eprintln!("ERROR: Invalid choice");
            return 1;
        }
    };

    println!();

    // ── Step 3: Client-side encryption ──────────────────────────
    println!("Step 3: Client-side encryption");
    println!();
    println!("  All backups are encrypted BEFORE upload (AES-256).");
    println!("  The cloud provider never sees plaintext data.");
    println!();
    println!("  WARNING: SAVE YOUR PASSWORD -- without it, cloud backups are UNRECOVERABLE.");
    println!();

    let crypt_pass = match Password::new()
        .with_prompt("  Encryption password")
        .with_confirmation("  Confirm password", "  Passwords don't match, try again")
        .interact()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: Failed to read password: {e}");
            return 1;
        }
    };

    if crypt_pass.is_empty() {
        eprintln!("ERROR: Password cannot be empty");
        return 1;
    }

    // Obscure the password via rclone
    let obscured_pass = match rclone_obscure(&crypt_pass) {
        Some(p) => p,
        None => {
            eprintln!("ERROR: Failed to obscure password via rclone");
            return 1;
        }
    };

    // Append crypt remote to config (remove existing section first)
    if let Err(e) = append_crypt_section(&rclone_config, &remote_path, &obscured_pass) {
        eprintln!("ERROR: Failed to write crypt config: {e}");
        return 1;
    }
    println!("  [OK] Encryption configured (remote: {CRYPT_REMOTE})");

    // Prompt for optional password hint
    println!();
    println!("  Optional: set a password hint to help you remember the password.");
    println!("  The hint is stored IN PLAINTEXT on cloud -- do NOT include the actual password.");
    println!("  Examples: \"cat name + birth year\", \"first car + college\"");
    println!();
    let crypt_hint: String = Input::new()
        .with_prompt("  Password hint (Enter to skip)")
        .default(String::new())
        .allow_empty(true)
        .interact_text()
        .unwrap_or_default();

    // Secure the config file: chmod 600, chown outerclaw
    secure_config_file(&rclone_config);

    println!();

    // ── Step 4: Test connection ─────────────────────────────────
    println!("Step 4: Test connection");
    println!("  Testing upload + download + decrypt...");

    if !test_connection(&rclone_config) {
        return 1;
    }

    // Upload recovery hint if provided
    if !crypt_hint.is_empty() {
        upload_recovery_hint(&rclone_config, &base_remote_name, &remote_path, &crypt_hint);
    }

    println!();

    // ── Step 5: Enable cloud sync in outerclaw.env ──────────────
    println!("Step 5: Enable cloud sync");
    if let Err(e) = update_env_file() {
        eprintln!("ERROR: Failed to update env file: {e}");
        return 1;
    }
    println!("  [OK] Cloud sync enabled in outerclaw.env");

    // Enable timer if installed
    enable_sync_timer();

    println!();

    // ── Done ────────────────────────────────────────────────────
    println!("======================================");
    println!("   Cloud backup setup complete!");
    println!("======================================");
    println!();
    println!("  Remote:     {CRYPT_REMOTE} -> {remote_path}");
    println!("  Encryption: AES-256 (client-side, rclone crypt)");
    println!("  Schedule:   Every 2 hours");
    println!("  Config:     {RCLONE_CONFIG_PATH}");
    println!();
    println!("  DISASTER RECOVERY -- SAVE THIS INFO:");
    println!("    To restore on a new machine, you need:");
    println!("      1. Cloud provider credentials (API keys)");
    println!("      2. Encryption password");
    println!();
    println!("  Commands:");
    println!("    Manual sync:   sudo systemctl start oc-cloud-sync.service");
    println!("    Check timer:   systemctl list-timers oc-cloud-sync.timer");
    println!(
        "    View logs:     tail -f {}/audit/cloud-sync.log",
        cfg.vault_dir.display()
    );
    println!("    List backups:  sudo outerclaw cloud restore --list");
    println!("    Show hint:     sudo outerclaw cloud restore --show-hint");
    println!();

    0
}

/// Check if rclone is installed. If not, prompt user to install.
fn check_rclone() -> bool {
    match Command::new("which").arg("rclone").output() {
        Ok(output) if output.status.success() => {
            // Get version
            let ver = Command::new("rclone")
                .arg("version")
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()
                        .and_then(|s| s.lines().next().map(|l| l.to_string()))
                })
                .unwrap_or_else(|| "unknown".to_string());
            println!("  [OK] rclone installed: {ver}");
            true
        }
        _ => {
            eprintln!("  rclone is not installed.");
            eprintln!("  Install it manually: https://rclone.org/install/");
            eprintln!("  Quick install: curl -fsSL https://rclone.org/install.sh | sudo bash");
            false
        }
    }
}

/// Set up Cloudflare R2 credentials. Returns (base_remote_name, remote_path).
fn setup_cloudflare_r2(config_path: &Path) -> Option<(String, String)> {
    println!();
    println!("  Setting up Cloudflare R2...");
    println!("  You need: Account ID, Access Key ID, Secret Access Key");
    println!("  Get these from: Cloudflare Dashboard -> R2 -> Manage R2 API Tokens");
    println!();

    let account_id: String = Input::new()
        .with_prompt("  Account ID")
        .interact_text()
        .ok()?;
    let access_key: String = Input::new()
        .with_prompt("  Access Key ID")
        .interact_text()
        .ok()?;
    let secret_key = Password::new()
        .with_prompt("  Secret Access Key")
        .interact()
        .ok()?;
    let bucket: String = Input::new()
        .with_prompt("  Bucket name")
        .default("outerclaw-backups".to_string())
        .interact_text()
        .ok()?;

    let config_content = format!(
        "[{BASE_REMOTE}]\n\
type = s3\n\
provider = Cloudflare\n\
access_key_id = {access_key}\n\
secret_access_key = {secret_key}\n\
endpoint = https://{account_id}.r2.cloudflarestorage.com\n\
acl = private\n\
no_check_bucket = true\n\
region = auto\n"
    );

    if let Err(e) = fs::write(config_path, &config_content) {
        eprintln!("ERROR: Cannot write rclone config: {e}");
        return None;
    }
    println!("  [OK] Cloudflare R2 configured");
    Some((BASE_REMOTE.to_string(), format!("{BASE_REMOTE}:{bucket}")))
}

/// Set up Backblaze B2 credentials.
fn setup_backblaze_b2(config_path: &Path) -> Option<(String, String)> {
    println!();
    println!("  Setting up Backblaze B2...");
    println!("  You need: Application Key ID, Application Key");
    println!("  Get these from: Backblaze -> App Keys");
    println!();

    let key_id: String = Input::new()
        .with_prompt("  Application Key ID")
        .interact_text()
        .ok()?;
    let app_key = Password::new()
        .with_prompt("  Application Key")
        .interact()
        .ok()?;
    let bucket: String = Input::new()
        .with_prompt("  Bucket name")
        .default("outerclaw-backups".to_string())
        .interact_text()
        .ok()?;

    let config_content = format!(
        "[{BASE_REMOTE}]\n\
type = b2\n\
account = {key_id}\n\
key = {app_key}\n"
    );

    if let Err(e) = fs::write(config_path, &config_content) {
        eprintln!("ERROR: Cannot write rclone config: {e}");
        return None;
    }
    println!("  [OK] Backblaze B2 configured");
    Some((BASE_REMOTE.to_string(), format!("{BASE_REMOTE}:{bucket}")))
}

/// Set up AWS S3 credentials.
fn setup_aws_s3(config_path: &Path) -> Option<(String, String)> {
    println!();
    println!("  Setting up AWS S3...");
    println!("  You need: Access Key ID, Secret Access Key, Region");
    println!();

    let access_key: String = Input::new()
        .with_prompt("  Access Key ID")
        .interact_text()
        .ok()?;
    let secret_key = Password::new()
        .with_prompt("  Secret Access Key")
        .interact()
        .ok()?;
    let region: String = Input::new()
        .with_prompt("  Region")
        .default("us-east-1".to_string())
        .interact_text()
        .ok()?;
    let bucket: String = Input::new()
        .with_prompt("  Bucket name")
        .default("outerclaw-backups".to_string())
        .interact_text()
        .ok()?;

    let config_content = format!(
        "[{BASE_REMOTE}]\n\
type = s3\n\
provider = AWS\n\
access_key_id = {access_key}\n\
secret_access_key = {secret_key}\n\
region = {region}\n\
acl = private\n"
    );

    if let Err(e) = fs::write(config_path, &config_content) {
        eprintln!("ERROR: Cannot write rclone config: {e}");
        return None;
    }
    println!("  [OK] AWS S3 configured");
    Some((BASE_REMOTE.to_string(), format!("{BASE_REMOTE}:{bucket}")))
}

/// Set up "Other" provider via manual rclone config.
fn setup_other(config_path: &Path) -> Option<(String, String)> {
    println!();
    println!("  Opening rclone interactive config...");
    println!("  Create a remote, then return here.");
    println!();

    let status = Command::new("rclone")
        .arg("config")
        .arg("--config")
        .arg(config_path)
        .status();

    match status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("ERROR: rclone config failed");
            return None;
        }
    }

    let remote_name: String = Input::new()
        .with_prompt("  Remote name you created")
        .interact_text()
        .ok()?;
    let custom_path: String = Input::new()
        .with_prompt("  Bucket/path")
        .interact_text()
        .ok()?;

    Some((remote_name.clone(), format!("{remote_name}:{custom_path}")))
}

/// Use an existing rclone remote.
fn setup_existing(config_path: &Path) -> Option<(String, String)> {
    println!();

    if config_path.exists() {
        println!("  Using existing config at {}", config_path.display());
        println!("  Available remotes:");
        let _ = Command::new("rclone")
            .args(["listremotes", "--config", &config_path.to_string_lossy()])
            .status();
    } else {
        // Try to find user's rclone config
        let sudo_user = std::env::var("SUDO_USER").unwrap_or_else(|_| "root".to_string());
        let default_config = PathBuf::from(format!("/home/{sudo_user}/.config/rclone/rclone.conf"));

        if default_config.exists() {
            if let Err(e) = fs::copy(&default_config, config_path) {
                eprintln!("ERROR: Cannot copy rclone config: {e}");
                return None;
            }
            println!(
                "  [OK] Copied rclone config from {}",
                default_config.display()
            );
            println!("  Available remotes:");
            let _ = Command::new("rclone")
                .args(["listremotes", "--config", &config_path.to_string_lossy()])
                .status();
        } else {
            eprintln!(
                "ERROR: No rclone config found. Run 'rclone config' first or choose another option."
            );
            return None;
        }
    }
    println!();

    let remote_name: String = Input::new()
        .with_prompt("  Remote name to use")
        .interact_text()
        .ok()?;
    let existing_path: String = Input::new()
        .with_prompt("  Bucket/path")
        .interact_text()
        .ok()?;

    Some((
        remote_name.clone(),
        format!("{remote_name}:{existing_path}"),
    ))
}

/// Run `rclone obscure <password>` and return the obscured string.
fn rclone_obscure(password: &str) -> Option<String> {
    let output = Command::new("rclone")
        .args(["obscure", password])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let obscured = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if obscured.is_empty() {
        return None;
    }
    Some(obscured)
}

/// Append (or replace) the crypt remote section in the rclone config.
fn append_crypt_section(
    config_path: &Path,
    remote_path: &str,
    obscured_pass: &str,
) -> Result<(), String> {
    let mut content = if config_path.exists() {
        fs::read_to_string(config_path).map_err(|e| format!("Cannot read config: {e}"))?
    } else {
        String::new()
    };

    // Remove existing crypt section if present
    if let Some(start) = content.find(&format!("[{CRYPT_REMOTE}]")) {
        let end = content[start + 1..]
            .find("\n[")
            .map(|pos| start + 1 + pos)
            .unwrap_or(content.len());
        content.replace_range(start..end, "");
        // Clean up extra blank lines
        while content.ends_with("\n\n\n") {
            content.pop();
        }
    }

    // Append crypt section
    let crypt_section = format!(
        "\n[{CRYPT_REMOTE}]\n\
type = crypt\n\
remote = {remote_path}\n\
password = {obscured_pass}\n\
filename_encryption = off\n\
directory_name_encryption = false\n"
    );
    content.push_str(&crypt_section);

    fs::write(config_path, &content).map_err(|e| format!("Cannot write config: {e}"))?;
    Ok(())
}

/// Set mode 0600 and chown to outerclaw on the config file.
fn secure_config_file(config_path: &Path) {
    // chmod 600
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = fs::set_permissions(config_path, fs::Permissions::from_mode(0o600)) {
        log::warn!("Cannot chmod rclone config: {e}");
    }

    // chown outerclaw:outerclaw
    if let Ok(Some(user)) = nix::unistd::User::from_name("outerclaw") {
        let gid = nix::unistd::Group::from_name("outerclaw")
            .ok()
            .flatten()
            .map(|g| g.gid)
            .unwrap_or(user.gid);
        if let Err(e) = nix::unistd::chown(config_path, Some(user.uid), Some(gid)) {
            log::warn!("Cannot chown rclone config: {e}");
        }
    }
}

/// Test the rclone connection: upload, download, verify, cleanup.
fn test_connection(config_path: &Path) -> bool {
    let config_str = config_path.to_string_lossy();

    // Create test file
    let test_dir = std::env::temp_dir();
    let test_file = test_dir.join("outerclaw-cloud-test.txt");
    let ts = crate::alert::format_utc_now();
    if let Err(e) = fs::write(&test_file, format!("OuterClaw cloud test {ts}")) {
        eprintln!("ERROR: Cannot create test file: {e}");
        return false;
    }

    // Upload
    let upload = Command::new("rclone")
        .args([
            "copyto",
            &test_file.to_string_lossy(),
            &format!("{CRYPT_REMOTE}:_test/connection-test.txt"),
            "--config",
            &config_str,
        ])
        .output();

    match upload {
        Ok(o) if o.status.success() => {
            println!("  [OK] Upload test passed");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("ERROR: Upload test failed -- check credentials, bucket name, and network");
            if !stderr.is_empty() {
                eprintln!("  rclone: {stderr}");
            }
            let _ = fs::remove_file(&test_file);
            return false;
        }
        Err(e) => {
            eprintln!("ERROR: Cannot run rclone: {e}");
            let _ = fs::remove_file(&test_file);
            return false;
        }
    }

    // Download and verify
    let dl_file = test_dir.join("outerclaw-cloud-test-dl.txt");
    let download = Command::new("rclone")
        .args([
            "copyto",
            &format!("{CRYPT_REMOTE}:_test/connection-test.txt"),
            &dl_file.to_string_lossy(),
            "--config",
            &config_str,
        ])
        .output();

    let dl_ok = match download {
        Ok(o) if o.status.success() => {
            // Compare contents
            let orig = fs::read(&test_file).unwrap_or_default();
            let downloaded = fs::read(&dl_file).unwrap_or_default();
            if orig == downloaded {
                println!("  [OK] Download + decrypt test passed");
                true
            } else {
                eprintln!("ERROR: Decryption verification failed -- data mismatch");
                false
            }
        }
        _ => {
            eprintln!("ERROR: Download test failed -- check credentials and permissions");
            false
        }
    };

    // Cleanup remote test files
    let _ = Command::new("rclone")
        .args([
            "delete",
            &format!("{CRYPT_REMOTE}:_test/"),
            "--config",
            &config_str,
        ])
        .output();
    let _ = Command::new("rclone")
        .args([
            "rmdir",
            &format!("{CRYPT_REMOTE}:_test/"),
            "--config",
            &config_str,
        ])
        .output();

    // Cleanup local test files
    let _ = fs::remove_file(&test_file);
    let _ = fs::remove_file(&dl_file);

    dl_ok
}

/// Upload a recovery hint (unencrypted) to the base remote.
fn upload_recovery_hint(
    config_path: &Path,
    _base_remote_name: &str,
    remote_path: &str,
    hint: &str,
) {
    let config_str = config_path.to_string_lossy();
    let ts = crate::alert::format_utc_now();
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let hint_content = format!(
        "OuterClaw Cloud Backup -- Recovery Info\n\
=======================================\n\
Created:  {ts}\n\
Hostname: {hostname}\n\
\n\
Password hint: {hint}\n\
\n\
To restore on a new machine:\n\
  1. Install rclone: curl -fsSL https://rclone.org/install.sh | bash\n\
  2. Run: sudo outerclaw cloud setup\n\
     - Choose the same provider and bucket\n\
     - Enter the password from your hint above\n\
  3. Run: sudo outerclaw cloud restore --list\n"
    );

    let hint_file = std::env::temp_dir().join("outerclaw-recovery-hint.txt");
    if fs::write(&hint_file, &hint_content).is_err() {
        log::warn!("Cannot create recovery hint temp file");
        return;
    }

    let result = Command::new("rclone")
        .args([
            "copyto",
            &hint_file.to_string_lossy(),
            &format!("{remote_path}/RECOVERY-HINT.txt"),
            "--config",
            &config_str,
        ])
        .output();

    match result {
        Ok(o) if o.status.success() => {
            println!("  [OK] Recovery hint uploaded to cloud (plaintext, on base remote)");
        }
        _ => {
            println!("  [WARN] Could not upload recovery hint -- save it manually");
        }
    }

    let _ = fs::remove_file(&hint_file);
}

/// Update outerclaw.env with CLOUD_ENABLED=true, CLOUD_REMOTE, CLOUD_BANDWIDTH.
fn update_env_file() -> Result<(), String> {
    let env_path = PathBuf::from(ENV_FILE_PATH);

    let mut content = if env_path.exists() {
        fs::read_to_string(&env_path).map_err(|e| format!("Cannot read env file: {e}"))?
    } else {
        String::new()
    };

    // Update or add CLOUD_ENABLED
    content = set_env_var(&content, "CLOUD_ENABLED", "true");

    // Update or add CLOUD_REMOTE
    content = set_env_var(&content, "CLOUD_REMOTE", CRYPT_REMOTE);

    // Add CLOUD_BANDWIDTH if missing
    if !content.contains("CLOUD_BANDWIDTH=") {
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push_str("CLOUD_BANDWIDTH=0\n");
    }

    // Ensure there's a comment header if we added cloud vars for the first time
    if !content.contains("# Cloud backup") {
        // Insert comment before first CLOUD_ var
        if let Some(pos) = content.find("CLOUD_ENABLED=") {
            content.insert_str(pos, "# Cloud backup (configured by cloud-setup)\n");
        }
    }

    fs::write(&env_path, &content).map_err(|e| format!("Cannot write env file: {e}"))?;
    Ok(())
}

/// Set or update a KEY=VALUE in env file content. Returns the updated content.
fn set_env_var(content: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut found = false;

    for line in &mut lines {
        if line.starts_with(&prefix) {
            *line = format!("{key}={value}");
            found = true;
            break;
        }
    }

    if !found {
        lines.push(format!("{key}={value}"));
    }

    let mut result = lines.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Enable the cloud sync timer if the systemd unit is installed.
fn enable_sync_timer() {
    let timer_path = Path::new("/etc/systemd/system/oc-cloud-sync.timer");
    if timer_path.exists() {
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        let status = Command::new("systemctl")
            .args(["enable", "--now", "oc-cloud-sync.timer"])
            .status();
        match status {
            Ok(s) if s.success() => {
                println!("  [OK] Cloud sync timer enabled (every 2 hours)");
            }
            _ => {
                println!("  [WARN] Could not enable cloud sync timer");
            }
        }
    } else {
        println!("  [WARN] Timer not installed -- run deploy first, then re-run cloud setup");
    }
}

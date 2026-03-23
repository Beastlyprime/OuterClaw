//! Post-mortem forensic data collector.
//!
//! Rust port of `scripts/postmortem-collect.sh`. Collects systemd state,
//! journal logs, /proc data, system context, dmesg, and coredump metadata
//! when a service crashes. Each collection step is wrapped to catch all
//! errors and log warnings rather than failing.

use crate::alert::send_alert;
use crate::cli::PostmortemArgs;
use crate::config::Config;
use crate::platform::Platform;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum number of postmortem directories to retain.
const MAX_POSTMORTEMS: usize = 20;

/// Secret-bearing environment variable name patterns.
const SECRET_PATTERNS: &[&str] = &[
    "TOKEN",
    "KEY",
    "SECRET",
    "PASSWORD",
    "APIKEY",
    "API_KEY",
    "CREDENTIAL",
    "PRIVATE",
    "AUTH",
    "JWT",
    "DSN",
    "_URL",
    "COOKIE",
    "SESSION",
];

/// Run postmortem collection. Returns 0 (always succeeds best-effort).
pub fn run(args: PostmortemArgs, cfg: Config, _platform: Box<dyn Platform>) -> i32 {
    let unit = &args.unit;

    // ── Step 1: Validate unit name ────────────────────────────────
    if !is_valid_unit_name(unit) {
        eprintln!("FATAL: Invalid unit name: '{unit}'");
        return 1;
    }

    let ts = timestamp_str();
    let pm_dir = cfg
        .vault_dir
        .join("postmortem")
        .join(format!("{ts}-{unit}"));

    if let Err(e) = fs::create_dir_all(&pm_dir) {
        eprintln!("Cannot create postmortem dir: {e}");
        return 1;
    }

    let service = format!("{unit}.service");

    // ── Step 2: Collect systemd state ─────────────────────────────
    collect_safe("systemd state", || {
        let output = systemctl_show_properties(
            &service,
            &[
                "ExecMainPID",
                "ExecMainStatus",
                "Result",
                "ActiveState",
                "SubState",
                "NRestarts",
                "MemoryCurrent",
                "MemoryPeak",
                "CPUUsageNSec",
                "TasksCurrent",
                "ExecMainStartTimestamp",
                "ExecMainExitTimestamp",
                "StatusText",
                "InvocationID",
            ],
        );

        let mut content = String::new();
        content.push_str("=== Systemd Service State ===\n");
        content.push_str(&format!(
            "Collected: {}\n\n",
            crate::alert::format_utc_now()
        ));
        content.push_str(&output);

        write_file(&pm_dir.join("01-systemd-state.txt"), &content)
    });

    // ── Step 3: Journal last 200 ──────────────────────────────────
    collect_safe("journal last 200", || {
        let output = run_command(
            "journalctl",
            &[
                "-u",
                &service,
                "-n",
                "200",
                "--no-pager",
                "--output=short-iso",
            ],
        );
        write_file(&pm_dir.join("02-journal-last200.txt"), &output)
    });

    // ── Step 4: Journal errors ────────────────────────────────────
    collect_safe("journal errors", || {
        let output = run_command(
            "journalctl",
            &[
                "-u",
                &service,
                "-n",
                "50",
                "--no-pager",
                "-p",
                "err",
                "--output=short-iso",
            ],
        );
        write_file(&pm_dir.join("03-journal-errors.txt"), &output)
    });

    // ── Step 5: /proc data if PID available ───────────────────────
    let pid = get_main_pid(&service);

    if let Some(pid) = pid {
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        if proc_dir.exists() {
            collect_proc_data(&pm_dir, pid, &proc_dir);
        } else {
            write_proc_unavailable(&pm_dir, pid, &cfg);
        }
    } else {
        write_proc_unavailable(&pm_dir, 0, &cfg);
    }

    // ── Step 6: System context ────────────────────────────────────
    collect_safe("system context", || {
        let mut content = String::new();
        content.push_str(&format!(
            "=== System Context at {} ===\n\n",
            crate::alert::format_utc_now()
        ));

        content.push_str("--- uptime ---\n");
        content.push_str(&run_command("uptime", &[]));
        content.push_str("\n\n--- free ---\n");
        content.push_str(&run_command("free", &["-m"]));
        content.push_str("\n\n--- df ---\n");
        content.push_str(&run_command("df", &["-h", "/", "/tmp", "/home"]));
        content.push_str("\n\n--- loadavg ---\n");
        content.push_str(&read_file_safe("/proc/loadavg"));
        content.push_str("\n\n--- top 10 memory consumers ---\n");
        content.push_str(&run_command("ps", &["aux", "--sort=-%mem"]));
        content.push_str("\n\n--- top 10 CPU consumers ---\n");
        content.push_str(&run_command("ps", &["aux", "--sort=-%cpu"]));

        write_file(&pm_dir.join("15-system-context.txt"), &content)
    });

    // ── Step 7: dmesg last 5min ───────────────────────────────────
    collect_safe("dmesg", || {
        let output = run_command("dmesg", &["--since=-5min", "--time-format=iso"]);
        if output.trim().is_empty() {
            // Fallback: tail of dmesg
            let fallback = run_command("dmesg", &[]);
            let lines: Vec<&str> = fallback.lines().collect();
            let tail: String = lines
                .iter()
                .rev()
                .take(100)
                .rev()
                .copied()
                .collect::<Vec<&str>>()
                .join("\n");
            write_file(&pm_dir.join("16-dmesg.txt"), &tail)
        } else {
            write_file(&pm_dir.join("16-dmesg.txt"), &output)
        }
    });

    // ── Step 8: Coredump info ─────────────────────────────────────
    collect_safe("coredump", || {
        if !command_exists("coredumpctl") {
            return Ok(());
        }
        let list = run_command("coredumpctl", &["list", "--since=-5min", "--no-pager"]);
        if !list.trim().is_empty() {
            let mut content = list;
            content.push('\n');
            content.push_str(&run_command(
                "coredumpctl",
                &["info", "--since=-5min", "--no-pager"],
            ));
            write_file(&pm_dir.join("17-coredump-info.txt"), &content)?;
        }
        Ok(())
    });

    // ── Step 9: Generate summary ──────────────────────────────────
    collect_safe("summary", || generate_summary(&pm_dir, unit, &ts, pid));

    // ── Step 10: Prune old postmortems ────────────────────────────
    prune_postmortems(&cfg.vault_dir.join("postmortem"));

    // ── Step 11: Fix ownership to outerclaw ───────────────────────
    fix_ownership_outerclaw(&pm_dir);

    // ── Alert ─────────────────────────────────────────────────────
    send_alert(
        "WARNING",
        &format!(
            "Post-mortem collected for {unit} (PID {}). Path: {}",
            pid.unwrap_or(0),
            pm_dir.display()
        ),
        &cfg,
    );

    0
}

/// Validate that a unit name contains only safe characters.
fn is_valid_unit_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Collect /proc data for a living process.
fn collect_proc_data(pm_dir: &Path, pid: u32, proc_dir: &Path) {
    // Status
    collect_safe("proc status", || {
        let mut content = format!("PID {pid} still alive, collecting /proc data...\n\n");
        content.push_str(&read_file_safe(&proc_dir.join("status").to_string_lossy()));
        write_file(&pm_dir.join("04-proc-status.txt"), &content)
    });

    // IO
    collect_safe("proc io", || {
        let content = read_file_safe(&proc_dir.join("io").to_string_lossy());
        write_file(&pm_dir.join("05-proc-io.txt"), &content)
    });

    // Kernel stack
    collect_safe("proc stack", || {
        let content = read_file_safe(&proc_dir.join("stack").to_string_lossy());
        write_file(&pm_dir.join("06-proc-kernel-stack.txt"), &content)
    });

    // Wchan
    collect_safe("proc wchan", || {
        let content = read_file_safe(&proc_dir.join("wchan").to_string_lossy());
        write_file(&pm_dir.join("07-proc-wchan.txt"), &content)
    });

    // Cmdline (NUL-separated)
    collect_safe("proc cmdline", || {
        let raw = fs::read(proc_dir.join("cmdline")).unwrap_or_default();
        let content: String = raw
            .iter()
            .map(|&b| if b == 0 { ' ' } else { b as char })
            .collect();
        write_file(&pm_dir.join("08-proc-cmdline.txt"), &content)
    });

    // Environ (NUL-separated, redact secrets)
    collect_safe("proc environ", || {
        let raw = fs::read(proc_dir.join("environ")).unwrap_or_default();
        let text: String = raw
            .iter()
            .map(|&b| if b == 0 { '\n' } else { b as char })
            .collect();
        let redacted = redact_secrets(&text);
        write_file(&pm_dir.join("09-proc-environ.txt"), &redacted)
    });

    // OOM score
    collect_safe("proc oom_score", || {
        let content = read_file_safe(&proc_dir.join("oom_score").to_string_lossy());
        write_file(&pm_dir.join("10-proc-oom-score.txt"), &content)
    });

    collect_safe("proc oom_score_adj", || {
        let content = read_file_safe(&proc_dir.join("oom_score_adj").to_string_lossy());
        write_file(&pm_dir.join("10-proc-oom-score-adj.txt"), &content)
    });

    // File descriptors
    collect_safe("proc fd", || {
        let fd_dir = proc_dir.join("fd");
        let mut list_content = String::new();
        let mut count: u64 = 0;
        let mut targets = Vec::new();

        if let Ok(entries) = fs::read_dir(&fd_dir) {
            for entry in entries.flatten() {
                count += 1;
                let path = entry.path();
                let link = fs::read_link(&path)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "?".into());
                list_content.push_str(&format!("{} -> {}\n", path.display(), link));
                targets.push(link);
            }
        }

        write_file(&pm_dir.join("11-proc-fd-list.txt"), &list_content)?;
        write_file(&pm_dir.join("11-proc-fd-count.txt"), &format!("{count}\n"))?;

        // Count unique targets
        targets.sort();
        let mut target_counts = Vec::new();
        let mut i = 0;
        while i < targets.len() {
            let target = &targets[i];
            let mut c = 0u64;
            while i < targets.len() && &targets[i] == target {
                c += 1;
                i += 1;
            }
            target_counts.push((c, target.clone()));
        }
        target_counts.sort_by(|a, b| b.0.cmp(&a.0));
        let targets_str: String = target_counts
            .iter()
            .map(|(c, t)| format!("{c:>6} {t}\n"))
            .collect();
        write_file(&pm_dir.join("11-proc-fd-targets.txt"), &targets_str)
    });

    // Sockets
    collect_safe("proc sockets", || {
        let pid_filter = format!("pid={pid}");
        let tcp_listen = run_command("ss", &["-tlnp"]);
        let tcp_estab = run_command("ss", &["-tnp"]);

        let mut content = String::new();
        for line in tcp_listen.lines() {
            if line.contains(&pid_filter) {
                content.push_str(line);
                content.push('\n');
            }
        }
        for line in tcp_estab.lines() {
            if line.contains(&pid_filter) {
                content.push_str(line);
                content.push('\n');
            }
        }
        write_file(&pm_dir.join("12-proc-sockets.txt"), &content)
    });

    // Cgroup stats
    collect_safe("cgroup stats", || {
        let cgroup_content = read_file_safe(&proc_dir.join("cgroup").to_string_lossy());
        // Parse cgroup path from first line: "0::<path>"
        let cgroup_path = cgroup_content
            .lines()
            .next()
            .and_then(|line| {
                let parts: Vec<&str> = line.splitn(3, ':').collect();
                parts.get(2).map(|s| s.to_string())
            })
            .unwrap_or_default();

        if cgroup_path.is_empty() || cgroup_path.contains("..") {
            return Ok(());
        }

        let cgroup_dir = PathBuf::from(format!("/sys/fs/cgroup{cgroup_path}"));
        if !cgroup_dir.is_dir() {
            return Ok(());
        }

        let mem_stat = read_file_safe(&cgroup_dir.join("memory.stat").to_string_lossy());
        if !mem_stat.is_empty() {
            write_file(&pm_dir.join("13-cgroup-memory-stat.txt"), &mem_stat)?;
        }

        let mem_current = read_file_safe(&cgroup_dir.join("memory.current").to_string_lossy());
        if !mem_current.is_empty() {
            write_file(&pm_dir.join("13-cgroup-memory-current.txt"), &mem_current)?;
        }

        let mem_peak = read_file_safe(&cgroup_dir.join("memory.peak").to_string_lossy());
        if !mem_peak.is_empty() {
            write_file(&pm_dir.join("13-cgroup-memory-peak.txt"), &mem_peak)?;
        }

        let cpu_stat = read_file_safe(&cgroup_dir.join("cpu.stat").to_string_lossy());
        if !cpu_stat.is_empty() {
            write_file(&pm_dir.join("13-cgroup-cpu-stat.txt"), &cpu_stat)?;
        }

        Ok(())
    });

    // Process tree
    collect_safe("process tree", || {
        let output = run_command("pstree", &["-p", &pid.to_string()]);
        write_file(&pm_dir.join("14-process-tree.txt"), &output)
    });
}

/// Write the "proc unavailable" marker file and attach last known snapshot.
fn write_proc_unavailable(pm_dir: &Path, pid: u32, cfg: &Config) {
    let pid_str = if pid > 0 {
        pid.to_string()
    } else {
        "unknown".into()
    };

    let mut content = format!(
        "PID {pid_str} not available (already reaped).\n\
         Relying on journal, systemd state, and OuterClaw's last /proc snapshot.\n"
    );

    let last_snapshot = cfg.vault_dir.join("audit/gateway-proc-latest.json");
    if last_snapshot.exists() {
        let _ = fs::copy(
            &last_snapshot,
            pm_dir.join("04-proc-last-known-snapshot.json"),
        );
        content.push_str(
            "\nLast known /proc snapshot (from OuterClaw monitor) attached as \
             04-proc-last-known-snapshot.json\n",
        );
    }

    let _ = write_file(&pm_dir.join("04-proc-UNAVAILABLE.txt"), &content);
}

/// Generate the 00-SUMMARY.txt report.
fn generate_summary(pm_dir: &Path, unit: &str, ts: &str, pid: Option<u32>) -> Result<(), String> {
    let pid_str = pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "unknown".into());

    let mut content = String::new();
    content.push_str("================================================================\n");
    content.push_str("            POST-MORTEM REPORT\n");
    content.push_str("================================================================\n\n");
    content.push_str(&format!("Unit:      {unit}.service\n"));
    content.push_str(&format!("Timestamp: {ts}\n"));
    content.push_str(&format!("PID:       {pid_str}\n"));
    content.push_str(&format!("Directory: {}\n\n", pm_dir.display()));

    // Systemd Result
    content.push_str("-- Systemd Result --\n");
    let systemd_state = read_file_safe(&pm_dir.join("01-systemd-state.txt").to_string_lossy());
    for line in systemd_state.lines() {
        if line.contains("Result")
            || line.contains("ExecMainStatus")
            || line.contains("ActiveState")
            || line.contains("NRestarts")
        {
            content.push_str(line);
            content.push('\n');
        }
    }
    if systemd_state.is_empty() {
        content.push_str("(not available)\n");
    }
    content.push('\n');

    // Last 5 Errors
    content.push_str("-- Last 5 Errors --\n");
    let errors = read_file_safe(&pm_dir.join("03-journal-errors.txt").to_string_lossy());
    let error_lines: Vec<&str> = errors.lines().collect();
    let start = if error_lines.len() > 5 {
        error_lines.len() - 5
    } else {
        0
    };
    for line in &error_lines[start..] {
        content.push_str(line);
        content.push('\n');
    }
    if error_lines.is_empty() {
        content.push_str("(no errors captured)\n");
    }
    content.push('\n');

    // OOM Killer
    content.push_str("-- OOM Killer --\n");
    let mut oom_found = false;
    for dmesg_file in &["16-dmesg.txt"] {
        let dmesg = read_file_safe(&pm_dir.join(dmesg_file).to_string_lossy());
        for line in dmesg.lines() {
            let lower = line.to_lowercase();
            if lower.contains("oom")
                || lower.contains("killed process")
                || lower.contains("out of memory")
            {
                content.push_str(line);
                content.push('\n');
                oom_found = true;
            }
        }
    }
    if !oom_found {
        content.push_str("(no OOM events)\n");
    }
    content.push('\n');

    // Memory at Death
    content.push_str("-- Memory at Death --\n");
    let proc_status = pm_dir.join("04-proc-status.txt");
    let proc_snapshot = pm_dir.join("04-proc-last-known-snapshot.json");
    if proc_status.exists() {
        let status = read_file_safe(&proc_status.to_string_lossy());
        for line in status.lines() {
            if line.contains("VmRSS")
                || line.contains("VmPeak")
                || line.contains("VmSize")
                || line.contains("Threads")
            {
                content.push_str(line);
                content.push('\n');
            }
        }
    } else if proc_snapshot.exists() {
        let json = read_file_safe(&proc_snapshot.to_string_lossy());
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json) {
            if let Some(obj) = val.as_object() {
                for (k, v) in obj {
                    if k.starts_with("Vm")
                        || k.starts_with("Thread")
                        || k.starts_with("fd_")
                        || k.starts_with("oom_")
                    {
                        content.push_str(&format!("{k}: {v}\n"));
                    }
                }
            }
        }
    }
    content.push('\n');

    // Files collected
    content.push_str("-- Files Collected --\n");
    if let Ok(entries) = fs::read_dir(pm_dir) {
        let mut files: Vec<String> = entries
            .flatten()
            .map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                format!("  {size:>10}  {name}")
            })
            .collect();
        files.sort();
        for f in &files {
            content.push_str(f);
            content.push('\n');
        }
    }

    write_file(&pm_dir.join("00-SUMMARY.txt"), &content)
}

/// Redact secret values from environment text.
///
/// For lines matching `KEY=VALUE` where KEY contains a secret pattern,
/// replaces VALUE with `<REDACTED>`.
fn redact_secrets(text: &str) -> String {
    let mut result = String::with_capacity(text.len());

    for line in text.lines() {
        if let Some(eq_pos) = line.find('=') {
            let key = &line[..eq_pos];
            let key_upper = key.to_uppercase();
            let is_secret = SECRET_PATTERNS.iter().any(|pat| key_upper.contains(pat));

            if is_secret {
                result.push_str(key);
                result.push_str("=<REDACTED>");
            } else {
                result.push_str(line);
            }
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    result
}

/// Get the main PID from systemd for a service.
fn get_main_pid(service: &str) -> Option<u32> {
    let output = Command::new("systemctl")
        .args(["show", service, "--property=ExecMainPID", "--value"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let pid: u32 = raw.trim().parse().ok()?;

    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

/// Show multiple properties from systemctl show.
fn systemctl_show_properties(service: &str, properties: &[&str]) -> String {
    let props = properties.join(",");
    run_command(
        "systemctl",
        &["show", service, &format!("--property={props}")],
    )
}

/// Run a command and return stdout. Returns empty string on failure.
fn run_command(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
        Err(e) => {
            log::debug!("Command '{cmd}' failed: {e}");
            String::new()
        }
    }
}

/// Read a file's content. Returns empty string on failure.
fn read_file_safe(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Check if a command exists on the system.
fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write content to a file. Creates parent directories if needed.
fn write_file(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(mut f) => f
            .write_all(content.as_bytes())
            .map_err(|e| format!("Write to {}: {e}", path.display())),
        Err(e) => Err(format!("Open {}: {e}", path.display())),
    }
}

/// Run a collection function, catching and logging any errors.
fn collect_safe(label: &str, f: impl FnOnce() -> Result<(), String>) {
    match f() {
        Ok(()) => {
            log::debug!("Collected: {label}");
        }
        Err(e) => {
            log::warn!("Failed to collect {label}: {e}");
        }
    }
}

/// Prune old postmortem directories, keeping only the newest MAX_POSTMORTEMS.
fn prune_postmortems(pm_base: &Path) {
    if !pm_base.exists() {
        return;
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(pm_base)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    // Sort descending by name (newest first)
    entries.sort();
    entries.reverse();

    for old in entries.into_iter().skip(MAX_POSTMORTEMS) {
        log::debug!("Pruning old postmortem: {}", old.display());
        let _ = fs::remove_dir_all(&old);
    }
}

/// Fix ownership on a directory to the outerclaw user (best-effort).
fn fix_ownership_outerclaw(path: &Path) {
    if let Ok(Some(usr)) = nix::unistd::User::from_name("outerclaw") {
        let _ = chown_recursive(path, usr.uid, usr.gid);
        // Also set permissions to 700
        set_permissions_recursive(path, 0o700);
    }
}

/// Recursively chown a directory tree.
fn chown_recursive(
    path: &Path,
    uid: nix::unistd::Uid,
    gid: nix::unistd::Gid,
) -> Result<(), String> {
    let _ = nix::unistd::chown(path, Some(uid), Some(gid));

    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    chown_recursive(&p, uid, gid)?;
                } else {
                    let _ = nix::unistd::chown(&p, Some(uid), Some(gid));
                }
            }
        }
    }

    Ok(())
}

/// Recursively set permissions on a directory tree.
fn set_permissions_recursive(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(mode);
        let _ = fs::set_permissions(path, perms);
    }

    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    set_permissions_recursive(&p, mode);
                } else if let Ok(meta) = fs::metadata(&p) {
                    let mut perms = meta.permissions();
                    perms.set_mode(mode);
                    let _ = fs::set_permissions(&p, perms);
                }
            }
        }
    }
}

/// Generate a local-time timestamp in `YYYYMMDD-HHMMSS` format.
fn timestamp_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[allow(clippy::unnecessary_cast)]
    let t = secs as libc::time_t;
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_unit_name() {
        assert!(is_valid_unit_name("openclaw-gateway"));
        assert!(is_valid_unit_name("oc-outerclaw.service"));
        assert!(is_valid_unit_name("my_unit"));
        assert!(!is_valid_unit_name(""));
        assert!(!is_valid_unit_name("bad;name"));
        assert!(!is_valid_unit_name("../traversal"));
        assert!(!is_valid_unit_name("has space"));
    }

    #[test]
    fn test_redact_secrets() {
        let input = "HOME=/home/user\nAPI_TOKEN=secret123\nPATH=/usr/bin\nDB_PASSWORD=hunter2\n";
        let redacted = redact_secrets(input);
        assert!(redacted.contains("HOME=/home/user"));
        assert!(redacted.contains("API_TOKEN=<REDACTED>"));
        assert!(redacted.contains("PATH=/usr/bin"));
        assert!(redacted.contains("DB_PASSWORD=<REDACTED>"));
        assert!(!redacted.contains("secret123"));
        assert!(!redacted.contains("hunter2"));
    }

    #[test]
    fn test_redact_case_insensitive() {
        let input = "my_secret=abc\nMY_SECRET=def\nMy_Token=ghi\n";
        let redacted = redact_secrets(input);
        assert!(redacted.contains("my_secret=<REDACTED>"));
        assert!(redacted.contains("MY_SECRET=<REDACTED>"));
        assert!(redacted.contains("My_Token=<REDACTED>"));
    }
}

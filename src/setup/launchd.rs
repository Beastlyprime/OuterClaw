//! macOS launchd plist templates for OuterClaw setup.
//!
//! Generates launchd plist XML for each OuterClaw service component.
//! Plists are installed to `/Library/LaunchDaemons/` and managed by launchctl.

const BIN: &str = "/usr/local/bin/outerclaw";
const VAULT: &str = "/var/lib/outerclaw";
const ENV_FILE: &str = "/var/lib/outerclaw/config/outerclaw.env";
const LOG_DIR: &str = "/var/lib/outerclaw/audit";

// ---------------------------------------------------------------------------
// com.outerclaw.daemon.plist — the main watchdog daemon
// ---------------------------------------------------------------------------

/// Generate the launchd plist for the OuterClaw watchdog daemon.
///
/// This is the primary daemon that monitors OpenClaw, manages the state
/// machine, handles auto-recovery, and provides the Telegram bot interface.
pub fn daemon_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.outerclaw.daemon</string>

    <key>ProgramArguments</key>
    <array>
        <string>{BIN}</string>
        <string>daemon</string>
    </array>

    <key>UserName</key>
    <string>outerclaw</string>
    <key>GroupName</key>
    <string>staff</string>

    <key>WorkingDirectory</key>
    <string>{VAULT}</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>OUTERCLAW_ENV_FILE</key>
        <string>{ENV_FILE}</string>
    </dict>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>

    <key>ThrottleInterval</key>
    <integer>10</integer>

    <key>StandardOutPath</key>
    <string>{LOG_DIR}/daemon-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{LOG_DIR}/daemon-stderr.log</string>

    <key>SoftResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>1024</integer>
    </dict>

    <key>HardResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>2048</integer>
    </dict>

    <key>ProcessType</key>
    <string>Standard</string>

    <key>LowPriorityIO</key>
    <false/>

    <key>Nice</key>
    <integer>-5</integer>
</dict>
</plist>
"#
    )
}

// ---------------------------------------------------------------------------
// com.outerclaw.snapshot.plist — 30-minute interval snapshots
// ---------------------------------------------------------------------------

/// Generate the launchd plist for the snapshot timer (30-minute interval).
///
/// Runs both SQLite and file snapshots as separate invocations.
pub fn snapshot_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.outerclaw.snapshot</string>

    <key>ProgramArguments</key>
    <array>
        <string>/bin/bash</string>
        <string>-c</string>
        <string>{BIN} snapshot --sqlite-only &amp;&amp; {BIN} snapshot --files-only</string>
    </array>

    <key>UserName</key>
    <string>outerclaw</string>
    <key>GroupName</key>
    <string>staff</string>

    <key>WorkingDirectory</key>
    <string>{VAULT}</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>OUTERCLAW_ENV_FILE</key>
        <string>{ENV_FILE}</string>
    </dict>

    <key>StartInterval</key>
    <integer>1800</integer>

    <key>StandardOutPath</key>
    <string>{LOG_DIR}/snapshot-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{LOG_DIR}/snapshot-stderr.log</string>

    <key>Nice</key>
    <integer>15</integer>

    <key>LowPriorityIO</key>
    <true/>

    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#
    )
}

// ---------------------------------------------------------------------------
// com.outerclaw.healthcheck.plist — 2-minute interval health check
// ---------------------------------------------------------------------------

/// Generate the launchd plist for the health check timer (2-minute interval).
pub fn healthcheck_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.outerclaw.healthcheck</string>

    <key>ProgramArguments</key>
    <array>
        <string>{BIN}</string>
        <string>healthcheck</string>
    </array>

    <key>UserName</key>
    <string>outerclaw</string>
    <key>GroupName</key>
    <string>staff</string>

    <key>WorkingDirectory</key>
    <string>{VAULT}</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>OUTERCLAW_ENV_FILE</key>
        <string>{ENV_FILE}</string>
    </dict>

    <key>StartInterval</key>
    <integer>120</integer>

    <key>StandardOutPath</key>
    <string>{LOG_DIR}/healthcheck-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{LOG_DIR}/healthcheck-stderr.log</string>

    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#
    )
}

// ---------------------------------------------------------------------------
// com.outerclaw.lkg-promote.plist — 2-hour interval LKG promotion
// ---------------------------------------------------------------------------

/// Generate the launchd plist for the LKG promotion timer (2-hour interval).
pub fn lkg_promote_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.outerclaw.lkg-promote</string>

    <key>ProgramArguments</key>
    <array>
        <string>{BIN}</string>
        <string>promote-lkg</string>
    </array>

    <key>UserName</key>
    <string>outerclaw</string>
    <key>GroupName</key>
    <string>staff</string>

    <key>WorkingDirectory</key>
    <string>{VAULT}</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>OUTERCLAW_ENV_FILE</key>
        <string>{ENV_FILE}</string>
    </dict>

    <key>StartInterval</key>
    <integer>7200</integer>

    <key>StandardOutPath</key>
    <string>{LOG_DIR}/lkg-promote-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{LOG_DIR}/lkg-promote-stderr.log</string>

    <key>Nice</key>
    <integer>15</integer>

    <key>LowPriorityIO</key>
    <true/>

    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#
    )
}

// ---------------------------------------------------------------------------
// All plist metadata — used by install/uninstall to iterate
// ---------------------------------------------------------------------------

/// All launchd plist labels managed by OuterClaw.
pub const PLIST_LABELS: &[&str] = &[
    "com.outerclaw.daemon",
    "com.outerclaw.snapshot",
    "com.outerclaw.healthcheck",
    "com.outerclaw.lkg-promote",
];

/// Return (label, plist_content) pairs for all OuterClaw plists.
pub fn all_plists() -> Vec<(&'static str, String)> {
    vec![
        ("com.outerclaw.daemon", daemon_plist()),
        ("com.outerclaw.snapshot", snapshot_plist()),
        ("com.outerclaw.healthcheck", healthcheck_plist()),
        ("com.outerclaw.lkg-promote", lkg_promote_plist()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_plist_content() {
        let content = daemon_plist();
        assert!(content.contains("com.outerclaw.daemon"));
        assert!(content.contains("/usr/local/bin/outerclaw"));
        assert!(content.contains("<string>daemon</string>"));
        assert!(content.contains("<key>KeepAlive</key>"));
        assert!(content.contains("<key>RunAtLoad</key>"));
        assert!(content.contains("outerclaw"));
    }

    #[test]
    fn test_snapshot_plist_content() {
        let content = snapshot_plist();
        assert!(content.contains("com.outerclaw.snapshot"));
        assert!(content.contains("snapshot --sqlite-only"));
        assert!(content.contains("snapshot --files-only"));
        assert!(content.contains("<integer>1800</integer>"));
        assert!(content.contains("<key>LowPriorityIO</key>"));
    }

    #[test]
    fn test_healthcheck_plist_content() {
        let content = healthcheck_plist();
        assert!(content.contains("com.outerclaw.healthcheck"));
        assert!(content.contains("<string>healthcheck</string>"));
        assert!(content.contains("<integer>120</integer>"));
    }

    #[test]
    fn test_lkg_promote_plist_content() {
        let content = lkg_promote_plist();
        assert!(content.contains("com.outerclaw.lkg-promote"));
        assert!(content.contains("<string>promote-lkg</string>"));
        assert!(content.contains("<integer>7200</integer>"));
    }

    #[test]
    fn test_all_plists_count() {
        let plists = all_plists();
        assert_eq!(plists.len(), 4);
        assert_eq!(PLIST_LABELS.len(), 4);
    }

    #[test]
    fn test_plists_are_valid_xml_structure() {
        for (label, content) in all_plists() {
            assert!(
                content.contains("<?xml version=\"1.0\""),
                "Missing XML declaration in {label}"
            );
            assert!(
                content.contains("<!DOCTYPE plist"),
                "Missing DOCTYPE in {label}"
            );
            assert!(
                content.contains("<plist version=\"1.0\">"),
                "Missing plist root in {label}"
            );
            assert!(
                content.contains("</plist>"),
                "Missing closing plist tag in {label}"
            );
            assert!(
                content.contains(&format!("<string>{label}</string>")),
                "Label mismatch in {label}"
            );
        }
    }
}

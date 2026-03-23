//! Cross-platform config/identity file watcher using the `notify` crate.
//!
//! Replaces the Python `ConfigWatcher` which used raw inotify via ctypes.
//! The `notify` crate provides cross-platform file watching (inotify on Linux,
//! FSEvents on macOS, ReadDirectoryChanges on Windows).

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

/// Cooldown between alerts for the same file (seconds).
const ALERT_COOLDOWN_SECS: u64 = 60;

/// Watches critical config and identity files for modifications.
pub struct ConfigFileWatcher {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    watched_paths: Vec<PathBuf>,
    alert_cooldown: HashMap<PathBuf, Instant>,
}

impl ConfigFileWatcher {
    /// Create a new watcher and start monitoring all existing paths.
    pub fn new(paths: &[PathBuf]) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let watcher = RecommendedWatcher::new(
            move |res| {
                // Send all events through the channel; errors too.
                let _ = tx.send(res);
            },
            notify::Config::default(),
        )
        .map_err(|e| format!("Failed to create file watcher: {e}"))?;

        let mut cfw = Self {
            watcher,
            rx,
            watched_paths: paths.to_vec(),
            alert_cooldown: HashMap::new(),
        };

        // Add watches for all existing paths
        for path in paths {
            cfw.add_watch(path);
        }

        Ok(cfw)
    }

    /// Add a watch for a single path (non-recursive).
    fn add_watch(&mut self, path: &Path) {
        if !path.exists() {
            log::warn!("ConfigFileWatcher: {path:?} does not exist, skipping");
            return;
        }
        match self.watcher.watch(path, RecursiveMode::NonRecursive) {
            Ok(()) => log::info!("ConfigFileWatcher: watching {path:?}"),
            Err(e) => log::warn!("ConfigFileWatcher: cannot watch {path:?}: {e}"),
        }
    }

    /// Drain pending events and fire alerts (with per-file cooldown).
    ///
    /// `alert_fn(level, message)` is called for each alertable event.
    pub fn process_events(&mut self, alert_fn: &dyn Fn(&str, &str)) {
        let now = Instant::now();

        // Non-blocking drain of all pending events
        while let Ok(event_result) = self.rx.try_recv() {
            let event = match event_result {
                Ok(e) => e,
                Err(e) => {
                    log::debug!("ConfigFileWatcher: notify error: {e}");
                    continue;
                }
            };

            // We care about modifications, attribute changes, removals, and renames
            let event_desc = describe_event_kind(&event.kind);
            if event_desc.is_empty() {
                continue;
            }

            for path in &event.paths {
                // Only alert on paths we're actually watching
                if !self.watched_paths.contains(path) {
                    continue;
                }

                // Cooldown: don't flood alerts for repeated edits
                if let Some(last) = self.alert_cooldown.get(path) {
                    if now.duration_since(*last).as_secs() < ALERT_COOLDOWN_SECS {
                        continue;
                    }
                }
                self.alert_cooldown.insert(path.clone(), now);

                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");

                let (level, msg) = if is_identity_file(filename) {
                    (
                        "CRITICAL",
                        format!(
                            "IDENTITY FILE TAMPERED: {filename} was {event_desc}. \
                             chattr +i may have been removed!"
                        ),
                    )
                } else {
                    (
                        "WARNING",
                        format!("Config file changed: {filename} ({event_desc})"),
                    )
                };

                log::warn!("ConfigFileWatcher: {path:?} {event_desc}");
                alert_fn(level, &msg);
            }
        }
    }

    /// Re-add watches for files that were deleted and then re-created.
    ///
    /// This should be called periodically (e.g. every 5 minutes) because
    /// some platforms remove the watch when the file is deleted or moved.
    pub fn re_watch_missing(&mut self) {
        for path in self.watched_paths.clone() {
            if path.exists() {
                // Try to re-add (idempotent on most platforms)
                self.add_watch(&path);
            }
        }
    }
}

/// Whether a filename is an identity file (SOUL.md, AGENTS.md, USER.md).
fn is_identity_file(filename: &str) -> bool {
    matches!(filename, "SOUL.md" | "AGENTS.md" | "USER.md")
}

/// Describe a `notify` event kind as a human-readable string.
fn describe_event_kind(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Modify(_) => "modified",
        EventKind::Create(_) => "created",
        EventKind::Remove(_) => "deleted",
        EventKind::Access(_) => "", // not alertable
        EventKind::Other => "changed",
        EventKind::Any => "changed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_identity_file() {
        assert!(is_identity_file("SOUL.md"));
        assert!(is_identity_file("AGENTS.md"));
        assert!(is_identity_file("USER.md"));
        assert!(!is_identity_file("openclaw.json"));
        assert!(!is_identity_file("random.txt"));
    }

    #[test]
    fn test_describe_event_kind() {
        assert_eq!(
            describe_event_kind(&EventKind::Modify(notify::event::ModifyKind::Any)),
            "modified"
        );
        assert_eq!(
            describe_event_kind(&EventKind::Remove(notify::event::RemoveKind::Any)),
            "deleted"
        );
        assert_eq!(
            describe_event_kind(&EventKind::Access(notify::event::AccessKind::Any)),
            ""
        );
    }
}

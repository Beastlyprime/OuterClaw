use crate::cli::SnapshotArgs;
use crate::config::Config;
use crate::platform::Platform;

/// Run snapshot: dispatches to SQLite and/or file snapshot based on CLI flags.
pub fn run(args: SnapshotArgs, cfg: Config, _platform: Box<dyn Platform>) -> i32 {
    let mut result = 0;
    if !args.files_only {
        result |= super::snapshot_sqlite::run_sqlite(&cfg);
    }
    if !args.sqlite_only {
        result |= super::snapshot_files::run_files(&cfg);
    }
    result
}

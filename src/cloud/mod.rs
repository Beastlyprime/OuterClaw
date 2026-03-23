use crate::cli::CloudArgs;
use crate::config::Config;
use crate::platform::Platform;

pub mod restore;
pub mod setup;
pub mod sync;

/// Dispatch cloud subcommands.
pub fn run(args: CloudArgs, cfg: Config, platform: Box<dyn Platform>) -> i32 {
    match args.action {
        crate::cli::CloudAction::Setup => setup::run(cfg, platform),
        crate::cli::CloudAction::Sync => sync::run(cfg, platform),
        crate::cli::CloudAction::Restore(restore_args) => restore::run(restore_args, cfg, platform),
    }
}

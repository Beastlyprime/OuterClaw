mod alert;
mod cli;
mod cloud;
mod config;
mod daemon;
mod platform;
mod setup;
mod util;
mod vault;

use clap::Parser;
use cli::Cli;

fn main() {
    let cli = Cli::parse();

    env_logger::Builder::new()
        .filter_level(if cli.debug {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .format_timestamp_secs()
        .init();

    let platform = platform::detect();
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };

    let exit_code = match cli.command {
        cli::Command::Daemon => daemon::run(cfg, platform),
        cli::Command::Status => daemon::status::show(cfg, platform),
        cli::Command::Version => {
            println!(
                "OuterClaw v{} ({})",
                env!("CARGO_PKG_VERSION"),
                option_env!("VERGEN_GIT_SHA").unwrap_or("dev"),
            );
            0
        }
        cli::Command::Setup(args) => setup::install(args, cfg, platform),
        cli::Command::Deploy => setup::deploy(cfg, platform),
        cli::Command::Uninstall(args) => setup::uninstall(args, cfg, platform),
        cli::Command::Snapshot(args) => vault::snapshot::run(args, cfg, platform),
        cli::Command::PromoteLkg => vault::promote_lkg::run(cfg, platform),
        cli::Command::Rollback(args) => vault::rollback::run(args, cfg, platform),
        cli::Command::AutoRecover => vault::auto_recover::run(cfg, platform),
        cli::Command::Healthcheck => vault::healthcheck::run(cfg, platform),
        cli::Command::PreStartCheck => vault::pre_start_check::run(cfg, platform),
        cli::Command::Postmortem(args) => vault::postmortem::run(args, cfg, platform),
        cli::Command::Identity(args) => daemon::identity::run(args, platform),
        cli::Command::Cloud(args) => cloud::run(args, cfg, platform),
        cli::Command::Completions(args) => cli::completions(args),
    };

    std::process::exit(exit_code);
}

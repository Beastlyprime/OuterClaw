use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "outerclaw",
    about = "External watchdog & data protection for OpenClaw"
)]
pub struct Cli {
    /// Enable debug logging
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the watchdog daemon
    Daemon,

    /// Show guardian status report
    Status,

    /// Print version information
    Version,

    /// Install OuterClaw (first-time setup)
    Setup(SetupArgs),

    /// Idempotent deployment update
    Deploy,

    /// Uninstall OuterClaw
    Uninstall(UninstallArgs),

    /// Take a snapshot
    Snapshot(SnapshotArgs),

    /// Promote latest snapshot to LKG
    PromoteLkg,

    /// Human-triggered rollback
    Rollback(RollbackArgs),

    /// Automated LKG recovery (called by daemon)
    AutoRecover,

    /// Run health check
    Healthcheck,

    /// Pre-start validation for gateway
    PreStartCheck,

    /// Collect forensic postmortem data
    Postmortem(PostmortemArgs),

    /// Manage identity file immutability
    Identity(IdentityArgs),

    /// Cloud backup operations
    Cloud(CloudArgs),

    /// Generate shell completions
    Completions(CompletionsArgs),
}

#[derive(clap::Args)]
pub struct SetupArgs {
    /// Lightweight mode: keep existing user, don't create ocagent
    #[arg(long)]
    pub lightweight: bool,

    /// Non-interactive mode with defaults
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(clap::Args)]
pub struct UninstallArgs {
    /// Skip confirmation prompt
    #[arg(long, short)]
    pub yes: bool,

    /// Also remove vault data
    #[arg(long)]
    pub remove_vault: bool,

    /// Also remove outerclaw/ocagent users
    #[arg(long)]
    pub remove_users: bool,
}

#[derive(clap::Args)]
pub struct SnapshotArgs {
    /// Only snapshot SQLite database
    #[arg(long)]
    pub sqlite_only: bool,

    /// Only snapshot files (MEMORY.md, memory/, config)
    #[arg(long)]
    pub files_only: bool,
}

#[derive(clap::Args)]
pub struct RollbackArgs {
    /// Path to specific LKG to restore from
    pub path: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct PostmortemArgs {
    /// Systemd unit name to collect data for
    #[arg(default_value = "openclaw-gateway")]
    pub unit: String,
}

#[derive(clap::Args)]
pub struct IdentityArgs {
    #[command(subcommand)]
    pub action: IdentityAction,
}

#[derive(Subcommand)]
pub enum IdentityAction {
    /// Lock identity files (chattr +i / chflags immutable)
    Lock,
    /// Unlock identity files (chattr -i / chflags noimmutable)
    Unlock,
}

#[derive(clap::Args)]
pub struct CloudArgs {
    #[command(subcommand)]
    pub action: CloudAction,
}

#[derive(Subcommand)]
pub enum CloudAction {
    /// Interactive cloud backup setup
    Setup,
    /// Sync snapshots and LKG to cloud
    Sync,
    /// Restore from cloud backup
    Restore(CloudRestoreArgs),
}

#[derive(clap::Args)]
pub struct CloudRestoreArgs {
    /// List available backups
    #[arg(long)]
    pub list: bool,

    /// Show recovery hint from cloud
    #[arg(long)]
    pub show_hint: bool,

    /// Restore a specific LKG
    #[arg(long)]
    pub restore_lkg: Option<String>,

    /// Restore a specific snapshot
    #[arg(long)]
    pub restore_snapshot: Option<String>,
}

#[derive(clap::Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    pub shell: clap_complete::Shell,
}

pub fn completions(args: CompletionsArgs) -> i32 {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    clap_complete::generate(args.shell, &mut cmd, "outerclaw", &mut std::io::stdout());
    0
}

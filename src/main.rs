mod commit_msg;
mod manage;
mod pre_push;
mod util;

use util::ResultExt as _;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Git hooks integration (internal use only).
    #[command(subcommand, hide = true)]
    Hook(HookCommands),
    /// Configure the current branch to be managed by GHerrit.
    Manage,
    /// Configure the current branch to be unmanaged by GHerrit.
    Unmanage,
}

#[derive(Subcommand)]
enum HookCommands {
    /// Git pre-push hook.
    PrePush,
    /// Git post-checkout hook.
    PostCheckout {
        prev: String,
        new: String,
        flag: String,
    },
    /// Git commit-msg hook.
    CommitMsg {
        /// The file containing the commit message.
        file: String,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            let level = record.level();
            if level == log::Level::Info {
                writeln!(buf, "[gherrit] {}", record.args())
            } else {
                writeln!(buf, "[gherrit] [{}] {}", level, record.args())
            }
        })
        .init();

    // Limit concurrency to avoid hitting GitHub's abuse limits.
    rayon::ThreadPoolBuilder::new()
        .num_threads(6)
        .build_global()
        .unwrap();

    let cli = Cli::parse();
    let repo = util::Repo::open(".").unwrap_or_exit("Failed to open repo");

    match cli.command {
        Commands::Hook(cmd) => match cmd {
            HookCommands::PrePush => pre_push::run(&repo),
            HookCommands::PostCheckout { prev, new, flag } => {
                manage::post_checkout(&repo, &prev, &new, &flag)
            }
            HookCommands::CommitMsg { file } => commit_msg::run(&repo, &file),
        },
        Commands::Manage => manage::set_state(&repo, manage::State::Managed),
        Commands::Unmanage => manage::set_state(&repo, manage::State::Unmanaged),
    }
}

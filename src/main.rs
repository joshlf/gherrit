mod commit_msg;
mod install;
mod manage;
mod pre_push;
mod util;

use clap::{Parser, Subcommand};
use eyre::{Result, WrapErr};
use manage::State;

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
    Manage {
        /// Force the configuration update, overwriting any manual changes.
        #[arg(long, short)]
        force: bool,

        /// Configure the branch to be public (`git push` syncs PRs *and* pushes the branch itself).
        #[arg(long, group = "visibility")]
        public: bool,

        /// Configure the branch to be private (`git push` syncs PRs *only*; does not push the branch itself).
        #[arg(long, group = "visibility")]
        private: bool,
    },
    /// Configure the current branch to be unmanaged by GHerrit.
    Unmanage {
        /// Force the configuration update, overwriting any manual changes.
        #[arg(long, short)]
        force: bool,
    },
    /// Install GHerrit Git hooks.
    Install {
        /// Overwrite existing hooks not managed by GHerrit
        #[arg(long, short)]
        force: bool,
        /// Allow installation to global/external hooks directory
        #[arg(long)]
        allow_global: bool,
    },
}

#[derive(Subcommand)]
enum HookCommands {
    /// Git pre-push hook.
    PrePush,
    /// Git post-checkout hook.
    PostCheckout { prev: String, new: String, flag: String },
    /// Git commit-msg hook.
    CommitMsg {
        /// The file containing the commit message.
        file: String,
    },
}

use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format({
            use std::io::Write as _;

            use owo_colors::OwoColorize as _;

            let prefix = "[gherrit]".bold().green().to_string();
            let level_style_error = " [ERROR]".red().to_string();
            let level_style_warn = " [WARN]".yellow().to_string();
            let level_style_info = "".to_string();
            let level_style_debug = " [DEBUG]".purple().to_string();
            let level_style_trace = " [TRACE]".dimmed().to_string();

            move |buf, record| {
                let level_style = match record.level() {
                    log::Level::Error => &level_style_error,
                    log::Level::Warn => &level_style_warn,
                    log::Level::Info => &level_style_info,
                    log::Level::Debug => &level_style_debug,
                    log::Level::Trace => &level_style_trace,
                };

                writeln!(buf, "{prefix}{level_style} {}", record.args())
            }
        })
        .init();

    if let Err(e) = color_eyre::install() {
        log::error!("Failed to install color_eyre: {}", e);
    }

    if let Err(e) = run() {
        format!("{:#}", e).lines().for_each(|line| log::error!("{}", line));
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    // Limit concurrency to avoid hitting GitHub's abuse limits.
    rayon::ThreadPoolBuilder::new().num_threads(6).build_global().unwrap();

    let cli = Cli::parse();
    let repo = util::Repo::open(".").wrap_err("Failed to open repo")?;

    match cli.command {
        Commands::Hook(cmd) => match cmd {
            HookCommands::PrePush => {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(pre_push::run(&repo))?;
            }
            HookCommands::PostCheckout { prev, new, flag } => {
                manage::post_checkout(&repo, &prev, &new, &flag)?
            }
            HookCommands::CommitMsg { file } => commit_msg::run(&repo, &file)?,
        },
        Commands::Manage { force, public, private } => {
            let target_state = if public {
                State::Public
            } else if private {
                State::Private
            } else {
                // If no flag provided, preserve current state (enforcing config) or default to private.
                let (_, state) = repo.read_current_branch_and_state()?;
                match state {
                    Some(State::Public) => State::Public,
                    Some(State::Private) => State::Private,
                    Some(State::Unmanaged) | None => State::Private,
                }
            };
            manage::set_state(&repo, target_state, force)?
        }
        Commands::Unmanage { force } => manage::set_state(&repo, State::Unmanaged, force)?,
        Commands::Install { force, allow_global } => install::install(&repo, force, allow_global)?,
    }

    Ok(())
}

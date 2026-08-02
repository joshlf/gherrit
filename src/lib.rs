mod commit_msg;
mod install;
mod manage;
mod pre_push;
mod util;

use clap::{Parser, Subcommand};
use eyre::{Result, WrapErr};
use manage::State;
pub(crate) use util::{cmd_macro as cmd, re_macro as re};

pub struct Runtime {
    github_endpoint: pre_push::GithubEndpoint,
    id_entropy: fn() -> commit_msg::IdEntropy,
}

impl Runtime {
    pub fn production() -> Self {
        Self { github_endpoint: pre_push::GithubEndpoint::Production, id_entropy: rand::random }
    }

    #[cfg(feature = "test-driver")]
    #[doc(hidden)]
    pub fn test(github_api_url: Option<String>) -> Self {
        Self {
            github_endpoint: github_api_url
                .map_or(pre_push::GithubEndpoint::Disabled, pre_push::GithubEndpoint::Custom),
            id_entropy: || [0; commit_msg::ID_ENTROPY_BYTES],
        }
    }
}

#[derive(Parser)]
#[command(version, about, long_about = None, bin_name = "gherrit")]
pub struct Cli {
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
    PrePush {
        /// Name of the remote being pushed to.
        #[arg(requires = "remote_location")]
        remote_name: Option<String>,
        /// Location of the remote being pushed to.
        #[arg(requires = "remote_name")]
        remote_location: Option<String>,
    },
    /// Git post-checkout hook.
    PostCheckout { prev: String, new: String, flag: String },
    /// Git commit-msg hook.
    CommitMsg {
        /// The file containing the commit message.
        file: String,
    },
}

/// Executes one parsed GHerrit command using explicitly constructed runtime
/// dependencies.
///
/// This boundary is asynchronous and fallible: it does not install process
/// hooks, parse process arguments, terminate the process, or create a nested
/// async runtime. Standalone binaries own those policies.
pub async fn dispatch(cli: Cli, runtime: Runtime) -> Result<()> {
    let repo = util::Repo::open(".").wrap_err("Failed to open repo")?;

    match cli.command {
        Commands::Hook(cmd) => match cmd {
            HookCommands::PrePush { .. } => {
                pre_push::run(&repo, &runtime.github_endpoint).await?;
            }
            HookCommands::PostCheckout { prev, new, flag } => {
                manage::post_checkout(&repo, &prev, &new, &flag)?
            }
            HookCommands::CommitMsg { file } => commit_msg::run(&repo, &file, runtime.id_entropy)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_runtime_uses_only_the_production_endpoint() {
        assert!(matches!(
            Runtime::production().github_endpoint,
            pre_push::GithubEndpoint::Production
        ));
    }

    #[cfg(feature = "test-driver")]
    #[test]
    fn test_runtime_uses_only_an_explicit_endpoint() {
        assert!(matches!(Runtime::test(None).github_endpoint, pre_push::GithubEndpoint::Disabled));

        let runtime = Runtime::test(Some("http://127.0.0.1:1234".to_string()));
        let pre_push::GithubEndpoint::Custom(endpoint) = runtime.github_endpoint else {
            panic!("an explicit test endpoint must select the custom adapter");
        };
        assert_eq!(endpoint, "http://127.0.0.1:1234");
    }
}

mod auth;
mod command;
mod config;
mod layout;
mod repository;

pub use auth::get_github_token;
pub use command::{CommandExt, cmd};
pub(crate) use command::{clear_git_repository_overrides, cmd_macro};
pub use repository::{FirstParentCommitsBetweenError, HeadState, RemoteName, Repo};
pub(crate) use repository::{GitDirIdentity, literal_graph_open_options, require_git_config_env};

#[cfg(test)]
mod tests;

pub(crate) use config::BranchConfigValues;
#[cfg(test)]
pub(crate) use repository::parse_git_version;

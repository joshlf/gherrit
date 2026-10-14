mod auth;
mod command;
mod config;
mod layout {
    //! Path roles are distinct in a linked worktree. Keep the original spelling
    //! for child processes; canonical identity is a separate repository contract.

    use std::path::Path;

    use super::Repo;

    #[derive(Clone, Copy, Debug)]
    pub(crate) struct RepositoryLayout<'repo> {
        /// HEAD, the index, and rebase state belong to this worktree.
        pub(crate) worktree_git_dir: &'repo Path,
        /// Objects, ordinary refs, common config, and default hooks are shared.
        pub(crate) common_git_dir: &'repo Path,
        pub(crate) workdir: Option<&'repo Path>,
    }

    impl Repo {
        pub(crate) fn layout(&self) -> RepositoryLayout<'_> {
            RepositoryLayout {
                worktree_git_dir: self.inner.path(),
                common_git_dir: self.inner.common_dir(),
                workdir: self.inner.workdir(),
            }
        }
    }
}
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

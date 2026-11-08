//! Shared helpers for Git repositories, configuration, and child commands.
//!
//! The helpers return repository facts, command results, and the paths callers
//! need. The command using them decides what may be published.

mod auth;
mod command;
mod config;
/// Paths used by a linked worktree.
///
/// Child processes receive paths with their original spelling. Determining
/// a canonical repository identity is a separate contract.
mod layout {

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
pub(crate) use command::{cmd_macro, re_macro};
pub use repository::{CommitsBetweenError, HeadState, Remote, Repo};
#[cfg(test)]
mod tests;

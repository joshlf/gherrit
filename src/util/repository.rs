use std::path::PathBuf;

use eyre::{Context as _, OptionExt, Result, bail, eyre};
use gix::{Commit, Id, state::InProgress};

use crate::{manage::State, re};

/// Represents the state of the HEAD reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// HEAD points to a local branch (e.g., `refs/heads/main`). We are fully
    /// "on" this branch.
    Attached(String),
    /// HEAD is detached (e.g. during a rebase), but we know which branch we are
    /// conceptually working on.
    Pending(String),
    /// HEAD is detached and we don't know of any associated branch.
    Detached,
}

impl HeadState {
    /// Returns the logical branch name if one exists (Attached or Pending).
    pub fn name(&self) -> Option<&str> {
        match self {
            HeadState::Attached(name) | HeadState::Pending(name) => Some(name),
            HeadState::Detached => None,
        }
    }
}

pub struct Repo {
    pub(super) inner: gix::Repository,
    pub(super) current_branch: HeadState,
}

impl Repo {
    pub fn open(path: &str) -> Result<Self> {
        // NOTE: `gix::discover` is used instead of `gix::open` so that
        // `gherrit` doesn't need to be run from the root of the repository.
        let inner = gix::discover(path)?;
        let current_branch = get_current_branch(&inner)?;
        Ok(Self { inner, current_branch })
    }

    pub fn current_branch(&self) -> &HeadState {
        &self.current_branch
    }

    pub fn is_newly_created_branch(&self, branch_name: &str) -> Result<bool> {
        let Some(reference) = self.inner.try_find_reference(branch_name)? else {
            // Only a missing ref establishes an unborn branch. Malformed or
            // unreadable references must not select that policy.
            return Ok(true);
        };

        // Get the most recent reflog entry
        let latest_log = reference
            .log_iter()
            .rev()? // Iterate newest-to-oldest
            .ok_or_eyre("No reflog entries found")?
            .next()
            .transpose()?;

        // Check if the previous OID is the Null Object ID (0000...)
        Ok(latest_log.is_some_and(|log| log.previous_oid.is_null()))
    }

    pub fn default_remote_name(&self) -> Result<String> {
        Ok(self.config_string("gherrit.remote")?.unwrap_or_else(|| "origin".to_string()))
    }

    pub fn default_remote(&self) -> Result<Remote> {
        let remote_name = self.default_remote_name()?;
        let remote_url = self
            .config_string(&format!("remote.{}.url", remote_name))?
            .ok_or_else(|| eyre!("Remote '{}' missing URL", remote_name))?;
        let (owner, repo_name) = get_repo_owner_name(remote_url.as_str())?;
        Ok(Remote { owner, repo_name })
    }

    fn find_default_branches(&self, remote_name: &str) -> Result<Vec<String>> {
        let mut branches = Vec::new();

        // Try to infer the default branch from the remote HEAD.
        let remote_head_ref = format!("refs/remotes/{}/HEAD", remote_name);
        if let Some(head_ref) = self.inner.try_find_reference(&remote_head_ref)? {
            let target_name = head_ref.target().try_name().map(|n| n.as_bstr().to_string());
            if let Some(target) = target_name {
                let prefix = format!("refs/remotes/{}/", remote_name);
                if let Some(stripped) = target.strip_prefix(&prefix) {
                    branches.push(stripped.to_string());
                }
            }
        }

        // An absent setting allows the usual fallback; a failed read does not.
        if let Some(default_branch) = self.config_string("init.defaultBranch")? {
            branches.push(default_branch);
        }

        // Check for common local branch names
        for branch in ["main", "master", "trunk"] {
            if self.inner.try_find_reference(&format!("refs/heads/{branch}"))?.is_some() {
                branches.push(branch.to_string());
            }
        }

        // Default fallback
        branches.push("main".to_string());

        Ok(branches)
    }

    pub fn find_default_branch_on_default_remote(&self) -> Result<String> {
        let branches = self.find_default_branches(&self.default_remote_name()?)?;
        Ok(branches.first().cloned().unwrap_or_else(|| "main".to_string()))
    }

    pub fn is_a_default_branch_on_default_remote(&self, branch_name: &str) -> Result<bool> {
        let branches = self.find_default_branches(&self.default_remote_name()?)?;
        Ok(branches.iter().any(|b| b == branch_name))
    }

    // Check whether the branch is managed by GHerrit.
    pub fn is_managed(&self, branch_name: &str) -> Result<bool> {
        match State::read_from(self, branch_name)? {
            Some(State::Unmanaged) => Ok(false),
            Some(State::Private | State::Public) => Ok(true),
            None => {
                bail!(
                    "It is unclear whether branch '{branch_name}' should be managed by GHerrit.\n\
                    Run 'gherrit manage' to sync it as a GHerrit stack.\n\
                    Run 'gherrit unmanage' to push it as a standard Git branch."
                );
            }
        }
    }

    pub fn read_current_branch_and_state(&self) -> Result<(String, Option<State>)> {
        let branch_name = self.current_branch();
        let branch_name = match branch_name {
            HeadState::Attached(bn) | HeadState::Pending(bn) => bn,
            HeadState::Detached => {
                bail!("Cannot get management state in detached HEAD");
            }
        };

        let state = State::read_from(self, branch_name)?;
        Ok((branch_name.clone(), state))
    }
}

pub enum CommitsBetweenError {
    NotAncestor,
    Eyre(eyre::Error),
}

impl Repo {
    /// Returns the commits from `ancestor` to `descendant` (in that order).
    pub fn commits_between(
        &self,
        ancestor: Id<'_>,
        descendant: Id<'_>,
    ) -> Result<Vec<Commit<'_>>, CommitsBetweenError> {
        // The graph API can report no base for missing endpoints. Establish
        // that both requested objects are commits before interpreting absence.
        for id in [ancestor, descendant] {
            id.object()
                .map_err(|error| CommitsBetweenError::Eyre(error.into()))?
                .try_into_commit()
                .map_err(|error| CommitsBetweenError::Eyre(error.into()))?;
        }
        let is_ancestor = match self.inner.merge_base(ancestor, descendant) {
            Ok(merge_base) => merge_base.detach() == ancestor,
            Err(gix::repository::merge_base::Error::NotFound { .. }) => {
                // Missing parents may also be skipped by merge-base. Only
                // classify unrelated history after a complete readable walk.
                for entry in self
                    .rev_walk([ancestor, descendant])
                    .all()
                    .map_err(|error| CommitsBetweenError::Eyre(error.into()))?
                {
                    entry.map_err(|error| CommitsBetweenError::Eyre(error.into()))?;
                }
                false
            }
            Err(error) => return Err(CommitsBetweenError::Eyre(error.into())),
        };
        if !is_ancestor {
            return Err(CommitsBetweenError::NotAncestor);
        }

        let mut commits = self
            .rev_walk([descendant])
            .all()
            .map_err(|e| CommitsBetweenError::Eyre(e.into()))?
            .take_while(|res| res.as_ref().map(|info| info.id != ancestor).unwrap_or(true))
            .map(|res| -> color_eyre::eyre::Result<_> { Ok(res?.object()?) })
            .collect::<Result<Vec<_>, _>>()
            .map_err(CommitsBetweenError::Eyre)?;
        commits.reverse();
        Ok(commits)
    }
}

impl std::ops::Deref for Repo {
    type Target = gix::Repository;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub struct Remote {
    pub owner: String,
    pub repo_name: String,
}

impl Remote {
    pub fn pr_url(&self, pr_number: u64) -> String {
        format!("https://github.com/{}/{}/pull/{}", self.owner, self.repo_name, pr_number)
    }

    pub fn repo_url_relative(&self) -> String {
        format!("/{}/{}", self.owner, self.repo_name)
    }
}

/// Determines the current HEAD state.
fn get_current_branch(repo: &gix::Repository) -> Result<HeadState> {
    if let Some(name) = repo.head()?.referent_name() {
        let name = name.shorten().to_string();
        return Ok(HeadState::Attached(name));
    }

    // Try to recover the branch name – we only care about states that detach
    // HEAD but preserve a branch identity. All other states besides these two
    // are either unreachable (because they're states in which the HEAD is
    // considered attached, and so we would have already returned above) or
    // are states in which we don't have any branch name at all.
    if let Some(InProgress::Rebase) | Some(InProgress::RebaseInteractive) = repo.state() {
        let git_dir = repo.path();
        let try_read_ref = |path: PathBuf| -> Result<Option<String>> {
            match std::fs::read_to_string(&path) {
                Ok(content) => Ok(Some(
                    content
                        .trim()
                        .strip_prefix("refs/heads/")
                        .unwrap_or(content.trim())
                        .to_string(),
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error).wrap_err_with(|| {
                    format!("Failed to read rebase branch metadata {}", path.display())
                }),
            }
        };

        if let Some(name) = try_read_ref(git_dir.join("rebase-merge/head-name"))? {
            return Ok(HeadState::Pending(name));
        }

        if let Some(name) = try_read_ref(git_dir.join("rebase-apply/head-name"))? {
            return Ok(HeadState::Pending(name));
        }
    }

    Ok(HeadState::Detached)
}

/// Parses the owner and repository name from a remote URL.
///
/// Supports the following formats:
/// - https://github.com/owner/repo(.git)
/// - git@github.com:owner/repo(.git)
/// - http://localhost:port/owner/repo(.git)
/// - /absolute/path/to/owner/repo(.git) (for tests)
/// - owner/repo (for tests)
pub(super) fn get_repo_owner_name(remote_url: &str) -> Result<(String, String)> {
    let re = re!(r"^(?:.*[/:])?(?P<owner>[^/:]+)/(?P<repo>[^/]+?)(?:\.git)?$");

    let caps = re
        .captures(remote_url)
        .ok_or_else(|| eyre!("Unsupported remote URL format: {remote_url}"))?;
    let owner = caps.name("owner").unwrap().as_str().to_string();
    let repo = caps.name("repo").unwrap().as_str().to_string();
    Ok((owner, repo))
}

#[cfg(test)]
mod observation_tests {
    use gix::prelude::ObjectIdExt as _;

    use super::*;

    fn fixture() -> testutil::TestContext {
        testutil::TestContextBuilder::new(std::env::current_exe().unwrap())
            .with_initial_commit()
            .build()
    }

    #[test]
    fn absent_refs_allow_fallback_but_malformed_refs_do_not() {
        let ctx = fixture();
        let repo = Repo::open(ctx.repo_path.to_str().unwrap()).unwrap();
        assert!(repo.is_newly_created_branch("refs/heads/missing").unwrap());
        assert_eq!(repo.default_remote_name().unwrap(), "origin");
        assert_eq!(repo.find_default_branch_on_default_remote().unwrap(), "main");
        let path = ctx.repo_path.join(".git/refs/heads/broken");
        std::fs::write(path, "not an object ID\n").unwrap();
        assert!(repo.is_newly_created_branch("refs/heads/broken").is_err());
        let path = ctx.repo_path.join(".git/refs/remotes/origin/HEAD");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "broken remote HEAD\n").unwrap();
        assert!(repo.find_default_branch_on_default_remote().is_err());
    }

    #[test]
    fn invalid_utf8_config_does_not_select_fallback() {
        use std::io::Write as _;
        for section in ["[gherrit]\nremote", "[init]\ndefaultBranch"] {
            let ctx = fixture();
            let mut config = std::fs::OpenOptions::new()
                .append(true)
                .open(ctx.repo_path.join(".git/config"))
                .unwrap();
            config.write_all(format!("\n{section} = ").as_bytes()).unwrap();
            config.write_all(b"\xff\n").unwrap();
            let repo = Repo::open(ctx.repo_path.to_str().unwrap()).unwrap();
            assert!(repo.find_default_branch_on_default_remote().is_err());
        }
    }

    #[test]
    fn missing_object_is_not_unrelated_history() {
        let ctx = fixture();
        let first = ctx.head_oid();
        ctx.run_git(&["checkout", "--orphan", "unrelated"]);
        ctx.run_git(&["commit", "--allow-empty", "--no-verify", "-m", "Other root"]);
        let repo = Repo::open(ctx.repo_path.to_str().unwrap()).unwrap();
        let first = gix::ObjectId::from_hex(first.as_bytes()).unwrap().attach(&repo.inner);
        let head = repo.rev_parse_single("HEAD").unwrap();
        assert!(matches!(repo.commits_between(first, head), Err(CommitsBetweenError::NotAncestor)));
        let missing = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
            .unwrap()
            .attach(&repo.inner);
        assert!(matches!(repo.commits_between(missing, head), Err(CommitsBetweenError::Eyre(_))));
    }

    #[test]
    fn missing_parent_is_not_unrelated_history() {
        let ctx = fixture();
        let tree = ctx.git_cmd().args(["show", "-s", "--format=%T", "HEAD"]).output().unwrap();
        let raw = format!(
            "tree {}\nparent 1111111111111111111111111111111111111111\nauthor Fixture <fixture@example.com> 0 +0000\ncommitter Fixture <fixture@example.com> 0 +0000\n\nMissing parent\n",
            String::from_utf8(tree.stdout).unwrap().trim()
        );
        let path = ctx.repo_path.join("missing-parent-commit");
        std::fs::write(&path, raw).unwrap();
        let output =
            ctx.git_cmd().args(["hash-object", "-w", "-t", "commit"]).arg(path).output().unwrap();
        assert!(output.status.success());
        let repo = Repo::open(ctx.repo_path.to_str().unwrap()).unwrap();
        let first = repo.rev_parse_single("HEAD").unwrap();
        let broken =
            gix::ObjectId::from_hex(String::from_utf8(output.stdout).unwrap().trim().as_bytes())
                .unwrap()
                .attach(&repo.inner);
        assert!(matches!(repo.commits_between(first, broken), Err(CommitsBetweenError::Eyre(_))));
    }

    #[test]
    fn failed_rebase_metadata_read_is_not_detached_head() {
        let ctx = fixture();
        ctx.run_git(&["checkout", "--detach"]);
        let path = ctx.repo_path.join(".git/rebase-merge/head-name");
        std::fs::create_dir_all(&path).unwrap();
        let error = Repo::open(ctx.repo_path.to_str().unwrap()).err().unwrap();
        assert!(format!("{error:#}").contains("Failed to read rebase branch metadata"));
        std::fs::remove_dir(&path).unwrap();
        // Missing metadata retains the old detached fallback.
        assert_eq!(
            Repo::open(ctx.repo_path.to_str().unwrap()).unwrap().current_branch(),
            &HeadState::Detached
        );
    }
}

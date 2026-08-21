use std::{
    env,
    ffi::OsStr,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Command,
    str,
};

use eyre::{OptionExt, Result, WrapErr, bail, eyre};
use gix::{Commit, Id, bstr::ByteSlice, state::InProgress};

use crate::manage::State;

/// Constructs a `std::process::Command`.
///
/// # Usage
///
/// The first argument must be a string literal representing the command and any initial arguments.
/// This string is split by whitespace to determine the command name and initial arguments.
///
/// Subsequent arguments are treated as individual arguments to the command. They are NOT split
/// by whitespace, allowing for safe passing of arguments that contain spaces.
///
/// # Example
///
/// ```text
/// // - "git" is the command; "config" is an argument.
/// // - "branch.{branch_name}.gherritManaged" is a single argument (even if it contains spaces when formatted).
/// // - `state` is a single argument (even if it contains spaces when formatted).
/// cmd!("git config", "branch.{branch_name}.gherritManaged", state)
/// ```
macro_rules! cmd {
    ($bin:literal $(, $($rest:tt)*)?) => {{
        // The first argument is a literal, so we can safely split it by whitespace.
        // This allows `cmd!("git config", ...)` to work as expected.
        let bin_str: &str = $bin;
        let parts: Vec<&str> = bin_str.split_whitespace().collect();
        let (bin, pre_args) = match parts.as_slice() {
            [bin, args @ ..] => (bin, args),
            [] => panic!("Command cannot be empty"),
        };

        #[allow(unused_mut)]
        let mut args: Vec<String> = pre_args.iter().map(|s| s.to_string()).collect();
        $crate::cmd!(@inner args $(, $($rest)*)?);

        log::debug!("exec: {} {}", bin, args.iter().map(|s| if s.contains(" ") {
            format!("'{}'", s)
        } else {
            s.clone()
        }).collect::<Vec<_>>().join(" "));
        $crate::util::cmd(bin, &args)
    }};

    // String literal (treated as a format string, but not broken apart).
    (@inner $vec:ident, $l:literal $(, $($rest:tt)*)?) => {
        $vec.push(format!($l));
        $crate::cmd!(@inner $vec $(, $($rest)*)?);
    };

    // Expression (not broken apart).
    (@inner $vec:ident, $e:expr $(, $($rest:tt)*)?) => {
        $vec.push($e.to_string());
        $crate::cmd!(@inner $vec $(, $($rest)*)?);
    };

    (@inner $vec:ident $(,)?) => {};
}
pub(crate) use cmd as cmd_macro;

macro_rules! re {
    ($name:ident, $re:literal) => {
        fn $name() -> &'static regex::Regex {
            $crate::re!(@inner $re)
        }
    };
    ($re:literal) => {
        $crate::re!(@inner $re)
    };
    (@inner $re:literal) => {{
        static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new($re).unwrap());
        &*RE
    }};
}
pub(crate) use re as re_macro;

pub fn cmd<I: AsRef<OsStr>>(name: &str, args: impl IntoIterator<Item = I>) -> Command {
    let mut c = Command::new(name);
    if name == "git" {
        // Replacement objects and implicit promisor fetches can make Git
        // subprocesses observe a different graph from the one sent to the
        // remote. Keep every production Git invocation on the literal local
        // graph.
        c.arg("--no-replace-objects");
        c.env("GIT_NO_REPLACE_OBJECTS", "1");
        c.env("GIT_NO_LAZY_FETCH", "1");
        for variable in [
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_GRAFT_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_REPLACE_REF_BASE",
            "GIT_SHALLOW_FILE",
        ] {
            c.env_remove(variable);
        }
    }
    c.args(args);
    c
}

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
    inner: gix::Repository,
    current_branch: HeadState,
}

fn literal_graph_open_options() -> gix::sec::trust::Mapping<gix::open::Options> {
    fn harden(mut options: gix::open::Options) -> gix::open::Options {
        // Object-related environment variables include replacement controls
        // and alternate object databases. Denying the whole category prevents
        // an inherited variable from taking precedence over the explicit
        // replacement-free setting below.
        options.permissions.env.objects = gix::sec::Permission::Deny;

        // In gix 0.75, `true` here means that replacement-object discovery is
        // disabled. The polarity is intentionally the opposite of the value
        // which makes this release load replacements.
        options.cli_overrides(["core.useReplaceRefs=true"])
    }

    let mut options = gix::sec::trust::Mapping::<gix::open::Options>::default();
    options.full.modify(harden);
    options.reduced.modify(harden);
    options
}

impl Repo {
    pub fn open(path: &str) -> Result<Self> {
        // NOTE: `gix::discover` is used instead of `gix::open` so that
        // `gherrit` doesn't need to be run from the root of the repository.
        let inner = gix::ThreadSafeRepository::discover_opts(
            path,
            Default::default(),
            literal_graph_open_options(),
        )?
        .to_thread_local();
        let current_branch = get_current_branch(&inner)?;
        Ok(Self { inner, current_branch })
    }

    /// Rejects repository state which can rewrite or truncate publication
    /// history.
    ///
    /// The safety checks performed by the pre-push hook are meaningful only
    /// for the graph GitHub receives. Git has no flag which disables legacy
    /// graft files, and a shallow boundary hides real ancestry. This check must
    /// therefore run before publication graph traversal.
    pub fn ensure_publishable_history(&self) -> Result<()> {
        let common_dir = self.inner.common_dir();
        reject_nonempty_history_file(
            &common_dir.join("info/grafts"),
            "the common Git directory's info/grafts file",
            "grafts rewrite commit ancestry",
        )?;
        if let Some(grafts) = env::var_os("GIT_GRAFT_FILE").filter(|path| !path.is_empty()) {
            reject_nonempty_history_file(
                &self.git_environment_path(grafts)?,
                "the file named by GIT_GRAFT_FILE",
                "the enclosing Git push retains that graft setting after the hook returns",
            )?;
        }

        // Always inspect the real common shallow file. gix permits its
        // effective shallow path to be redirected, which must not hide the
        // ordinary Git boundary from publication validation.
        reject_nonempty_history_file(
            &common_dir.join("shallow"),
            "the common Git directory's shallow file",
            "shallow history omits commit ancestry",
        )?;
        reject_nonempty_history_file(
            &self.inner.shallow_file(),
            "the effective shallow file",
            "shallow history omits commit ancestry",
        )?;
        if let Some(shallow) = env::var_os("GIT_SHALLOW_FILE").filter(|path| !path.is_empty()) {
            reject_nonempty_history_file(
                &self.git_environment_path(shallow)?,
                "the file named by GIT_SHALLOW_FILE",
                "the enclosing Git push retains that shallow boundary after the hook returns",
            )?;
        }

        if self.has_promisor_remote()? {
            let output = cmd("git", ["--version"])
                .checked_output()
                .wrap_err("Failed to determine the installed Git version")?;
            require_git_no_lazy_fetch(&output.stdout)?;
        }

        Ok(())
    }

    fn git_environment_path(&self, path: std::ffi::OsString) -> Result<PathBuf> {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path);
        }

        // Git runs an installed hook from the worktree root in a non-bare
        // repository and from the Git directory in a bare repository. Resolve
        // the enclosing push's relative environment path from the same place.
        Ok(match self.inner.workdir() {
            Some(workdir) => workdir.join(path),
            None => env::current_dir()?.join(path),
        })
    }

    fn has_promisor_remote(&self) -> Result<bool> {
        let config = self.inner.config_snapshot();
        if config.string("extensions.partialClone").is_some() {
            return Ok(true);
        }

        let Some(remotes) = config.sections_by_name("remote") else {
            return Ok(false);
        };
        for remote in remotes {
            match remote.value_implicit("promisor") {
                Some(None) => return Ok(true),
                Some(Some(value)) => {
                    let value = gix::config::Boolean::try_from(value)
                        .wrap_err("Invalid remote promisor configuration")?;
                    if bool::from(value) {
                        return Ok(true);
                    }
                }
                None => {}
            }
        }
        Ok(false)
    }

    pub fn current_branch(&self) -> &HeadState {
        &self.current_branch
    }

    pub fn config_string(&self, key: &str) -> Result<Option<String>> {
        let Some(cow) = self.inner.config_snapshot().string(key) else {
            return Ok(None);
        };
        let s = std::str::from_utf8(cow.as_ref())?;
        Ok(Some(s.trim().to_string()))
    }

    pub fn config_path(&self, key: &str) -> Result<Option<PathBuf>> {
        let snapshot = self.inner.config_snapshot();
        let Some(path_val) = snapshot.path(key) else {
            return Ok(None);
        };

        let bstr: &gix::bstr::BStr = path_val.as_ref();
        let raw_path = bstr.to_path()?.to_path_buf();

        // Resolve relative paths against the workdir (or gitdir for bare repos)
        if raw_path.is_absolute() {
            Ok(Some(raw_path))
        } else {
            // Use `.canonicalize` to guarantee an absolute path.
            let root = self.workdir().unwrap_or(self.path()).canonicalize()?;
            Ok(Some(root.join(raw_path)))
        }
    }

    pub fn is_newly_created_branch(&self, branch_name: &str) -> Result<bool> {
        let reference = match self.inner.find_reference(branch_name) {
            Ok(r) => r,
            // If the branch reference doesn't exist yet, it's an "unborn branch".
            // This happens, for example, during `git checkout --orphan <name>`:
            // HEAD points to `refs/heads/<name>`, but the ref itself isn't
            // created until the first commit. In this case, it is definitionally
            // a newly created branch.
            Err(_) => return Ok(true),
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

    pub fn default_remote_name(&self) -> String {
        self.config_string("gherrit.remote")
            .unwrap_or_default()
            .unwrap_or_else(|| "origin".to_string())
    }

    pub fn default_remote(&self) -> Result<Remote> {
        let remote_name = self.default_remote_name();
        let remote_url = self
            .config_string(&format!("remote.{}.url", remote_name))?
            .ok_or_else(|| eyre!("Remote '{}' missing URL", remote_name))?;
        let (owner, repo_name) = get_repo_owner_name(remote_url.as_str())?;
        Ok(Remote { owner, repo_name })
    }

    fn find_default_branches(&self, remote_name: &str) -> Vec<String> {
        let mut branches = Vec::new();

        // Try to infer the default branch from the remote HEAD.
        let remote_head_ref = format!("refs/remotes/{}/HEAD", remote_name);
        if let Ok(head_ref) = self.inner.find_reference(&remote_head_ref) {
            let target_name = head_ref.target().try_name().map(|n| n.as_bstr().to_string());
            if let Some(target) = target_name {
                let prefix = format!("refs/remotes/{}/", remote_name);
                if let Some(stripped) = target.strip_prefix(&prefix) {
                    branches.push(stripped.to_string());
                }
            }
        }

        // Check git config
        //
        // Note that we swallow errors (e.g. invalid UTF-8) here.
        if let Some(default_branch) = self.config_string("init.defaultBranch").ok().flatten() {
            branches.push(default_branch);
        }

        // Check for common local branch names
        let locals = ["main", "master", "trunk"]
            .into_iter()
            .filter(|b| self.find_reference(&format!("refs/heads/{b}")).is_ok())
            .map(String::from);
        branches.extend(locals);

        // Default fallback
        branches.push("main".to_string());

        branches
    }

    pub fn find_default_branch_on_default_remote(&self) -> String {
        let branches = self.find_default_branches(&self.default_remote_name());
        branches.first().cloned().unwrap_or_else(|| "main".to_string())
    }

    pub fn is_a_default_branch_on_default_remote(&self, branch_name: &str) -> bool {
        let branches = self.find_default_branches(&self.default_remote_name());
        branches.iter().any(|b| b == branch_name)
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

fn reject_nonempty_history_file(path: &Path, description: &str, reason: &str) -> Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            bail!("GHerrit cannot publish because {description} is not a regular file");
        }
        Ok(metadata) if metadata.len() != 0 => {
            bail!("GHerrit cannot publish while {description} is nonempty because {reason}");
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| format!("Failed to inspect {description}")),
    }
}

fn parse_git_version(output: &[u8]) -> Result<(u64, u64)> {
    let version = str::from_utf8(output)?
        .trim()
        .strip_prefix("git version ")
        .ok_or_else(|| eyre!("Unexpected `git --version` output"))?;
    let mut components = version.split('.');
    let major = components
        .next()
        .ok_or_else(|| eyre!("Git version omitted its major component"))?
        .parse()
        .wrap_err("Git reported an invalid major version")?;
    let minor = components
        .next()
        .ok_or_else(|| eyre!("Git version omitted its minor component"))?
        .parse()
        .wrap_err("Git reported an invalid minor version")?;
    Ok((major, minor))
}

fn require_git_no_lazy_fetch(output: &[u8]) -> Result<()> {
    let (major, minor) = parse_git_version(output)?;
    if (major, minor) < (2, 45) {
        bail!(
            "GHerrit requires Git 2.45 or newer for a promisor repository so implicit object \
             fetches can be disabled; found Git {major}.{minor}"
        );
    }
    Ok(())
}

pub enum FirstParentCommitsBetweenError {
    NotOnFirstParentPath,
    Eyre(eyre::Error),
}

impl Repo {
    /// Returns the first-parent commits from `ancestor` to `descendant`.
    pub fn first_parent_commits_between(
        &self,
        ancestor: Id<'_>,
        descendant: Id<'_>,
    ) -> Result<Vec<Commit<'_>>, FirstParentCommitsBetweenError> {
        // A GHerrit stack is a first-parent path. A merge base is insufficient:
        // it can be reachable only through another parent of a merge commit.
        // Walk the one path that defines stack order and require it to reach the
        // requested ancestor.
        let mut commits = Vec::new();
        for commit in self
            .rev_walk([descendant])
            .first_parent_only()
            .all()
            .map_err(|e| FirstParentCommitsBetweenError::Eyre(e.into()))?
        {
            let commit = commit.map_err(|e| FirstParentCommitsBetweenError::Eyre(e.into()))?;
            if commit.id == ancestor {
                commits.reverse();
                return Ok(commits);
            }
            commits
                .push(commit.object().map_err(|e| FirstParentCommitsBetweenError::Eyre(e.into()))?);
        }

        Err(FirstParentCommitsBetweenError::NotOnFirstParentPath)
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
        let try_read_ref = |path: PathBuf| -> Option<String> {
            std::fs::read_to_string(path).ok().map(|content| {
                content.trim().strip_prefix("refs/heads/").unwrap_or(content.trim()).to_string()
            })
        };

        if let Some(name) = try_read_ref(git_dir.join("rebase-merge/head-name")) {
            return Ok(HeadState::Pending(name));
        }

        if let Some(name) = try_read_ref(git_dir.join("rebase-apply/head-name")) {
            return Ok(HeadState::Pending(name));
        }
    }

    Ok(HeadState::Detached)
}

pub trait CommandExt {
    fn success(&mut self) -> Result<()>;
    fn checked_output(&mut self) -> Result<std::process::Output>;
}

impl CommandExt for Command {
    fn success(&mut self) -> Result<()> {
        let status = self.status()?;
        if !status.success() {
            bail!("Command failed with status: {}", status);
        }
        Ok(())
    }

    fn checked_output(&mut self) -> Result<std::process::Output> {
        let output = self.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Command {self:?} failed with status: {}. Stderr: {stderr}", output.status,);
        }
        Ok(output)
    }
}

pub fn get_github_token() -> Result<String> {
    // Priority 1: GITHUB_TOKEN env var
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }

    // Priority 2: gh auth token
    let output = cmd!("gh auth token").output().wrap_err("Failed to run `gh auth token`")?;

    if output.status.success() {
        let token = String::from_utf8(output.stdout)?.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    Err(eyre!(
        "Could not find GitHub token. Please set GITHUB_TOKEN environment variable or login via `gh auth login`."
    ))
}

/// Parses the owner and repository name from a remote URL.
///
/// Supports the following formats:
/// - https://github.com/owner/repo(.git)
/// - git@github.com:owner/repo(.git)
/// - http://localhost:port/owner/repo(.git)
/// - /absolute/path/to/owner/repo(.git) (for tests)
/// - owner/repo (for tests)
fn get_repo_owner_name(remote_url: &str) -> Result<(String, String)> {
    let re = re!(r"^(?:.*[/:])?(?P<owner>[^/:]+)/(?P<repo>[^/]+?)(?:\.git)?$");

    let caps = re
        .captures(remote_url)
        .ok_or_else(|| eyre!("Unsupported remote URL format: {remote_url}"))?;
    let owner = caps.name("owner").unwrap().as_str().to_string();
    let repo = caps.name("repo").unwrap().as_str().to_string();
    Ok((owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_commands_use_the_literal_local_graph_without_lazy_fetches() {
        let command = cmd("git", ["status"]);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(arguments, ["--no-replace-objects", "status"]);
        let environment = command.get_envs().collect::<std::collections::HashMap<_, _>>();
        for variable in ["GIT_NO_LAZY_FETCH", "GIT_NO_REPLACE_OBJECTS"] {
            assert_eq!(environment[OsStr::new(variable)], Some(OsStr::new("1")));
        }
        for variable in [
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_GRAFT_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_REPLACE_REF_BASE",
            "GIT_SHALLOW_FILE",
        ] {
            assert_eq!(environment[OsStr::new(variable)], None);
        }

        let command = cmd("gh", ["auth", "token"]);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("auth"), OsStr::new("token")]
        );
        assert!(command.get_envs().next().is_none());
    }

    #[test]
    fn literal_graph_open_options_deny_object_environment_at_every_trust_level() {
        let options = literal_graph_open_options();
        for trust in [gix::sec::Trust::Full, gix::sec::Trust::Reduced] {
            assert_eq!(options.by_level(trust).permissions.env.objects, gix::sec::Permission::Deny);
        }
    }

    #[test]
    fn git_versions_are_parsed_for_no_lazy_fetch_support() {
        for (output, expected) in [
            ("git version 2.44.0\n", (2, 44)),
            ("git version 2.45.0\n", (2, 45)),
            ("git version 2.48.1 (Apple Git-154)\n", (2, 48)),
            ("git version 3.0.0.windows.1\n", (3, 0)),
        ] {
            assert_eq!(parse_git_version(output.as_bytes()).unwrap(), expected);
        }

        for output in [b"2.45.0\n".as_slice(), b"git version invalid\n", b"git version 2\n"] {
            assert!(parse_git_version(output).is_err());
        }

        let error = require_git_no_lazy_fetch(b"git version 2.44.9\n").unwrap_err();
        assert!(error.to_string().contains("requires Git 2.45 or newer"));
        require_git_no_lazy_fetch(b"git version 2.45.0\n").unwrap();
        require_git_no_lazy_fetch(b"git version 3.0.0\n").unwrap();
    }

    #[test]
    #[should_panic(expected = "Command cannot be empty")]
    fn test_cmd_macro_empty_panic() {
        cmd!("");
    }

    #[test]
    #[should_panic(expected = "Command cannot be empty")]
    fn test_cmd_macro_whitespace_panic() {
        cmd!("   ");
    }

    #[test]
    fn test_get_repo_owner_name() {
        for (url, (owner, repo)) in [
            ("https://github.com/owner/repo.git", ("owner", "repo")),
            ("https://github.com/owner/repo", ("owner", "repo")),
            ("git@github.com:owner/repo.git", ("owner", "repo")),
            ("git@github.com:owner/repo", ("owner", "repo")),
            ("alias:owner/repo.git", ("owner", "repo")),
            ("alias:owner/repo", ("owner", "repo")),
            ("http://localhost:3000/owner/repo.git", ("owner", "repo")),
            ("http://my-gh.com/owner/repo", ("owner", "repo")),
            ("/tmp/test/owner/repo.git", ("owner", "repo")),
            ("/tmp/owner/repo", ("owner", "repo")),
            ("owner/repo", ("owner", "repo")),
            ("https://github.com/user-name/repo", ("user-name", "repo")),
            ("https://github.com/user_name/repo", ("user_name", "repo")),
            ("https://github.com/user.name/repo", ("user.name", "repo")),
            ("https://github.com/user/repo-name", ("user", "repo-name")),
            ("https://github.com/user/repo_name", ("user", "repo_name")),
            ("https://github.com/user/repo.name", ("user", "repo.name")),
            ("https://github.com/user/repo.name.git", ("user", "repo.name")),
        ] {
            let expect = (owner.to_string(), repo.to_string());
            assert_eq!(get_repo_owner_name(url).unwrap(), expect);
        }
    }
}

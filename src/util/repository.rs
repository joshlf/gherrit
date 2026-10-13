use std::{
    env,
    ffi::OsStr,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    str,
};

use eyre::{OptionExt, Result, WrapErr, bail, eyre};
use gix::{Commit, ObjectId, bstr::ByteSlice as _, state::InProgress};

use super::{CommandExt as _, clear_git_repository_overrides};
use crate::{cmd, manage::State};

pub(super) const REMOTE_PROMISOR_CONFIG_PATTERN: &str =
    r"^remote\..*\.(promisor|partialclonefilter)$";

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

/// A logical branch identity and its exact `HEAD` target.
///
/// This value exists only while `HEAD` is attached to a local branch. A
/// missing target is the real unborn-branch state, while detached `HEAD` is
/// represented by the absence of this value. A symbolic `HEAD` outside the
/// local-branch namespace is rejected.
pub(crate) struct BranchHeadSnapshot {
    branch_name: String,
    target: Option<gix::ObjectId>,
}

impl BranchHeadSnapshot {
    pub(crate) fn into_parts(self) -> (String, Option<gix::ObjectId>) {
        (self.branch_name, self.target)
    }
}

pub struct Repo {
    pub(super) inner: gix::Repository,
    current_branch: HeadState,
    git_dir_identity: GitDirIdentity,
}

/// The canonical per-worktree Git directory selected by repository discovery.
///
/// Linked worktrees share objects and refs but have distinct Git directories.
/// Keeping that distinction prevents an ambient recursion marker from one
/// worktree from suppressing hooks in another.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct GitDirIdentity(PathBuf);

impl GitDirIdentity {
    pub(crate) fn as_os_str(&self) -> &OsStr {
        self.0.as_os_str()
    }
}

pub(super) fn literal_graph_open_options() -> gix::sec::trust::Mapping<gix::open::Options> {
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

/// The configured Git remote whose push repository GHerrit publishes to.
///
/// Configuration decoding is fallible: an unreadable `gherrit.remote` never
/// silently turns into `origin`. Control characters are rejected so the name
/// remains safe to repeat in Git arguments and diagnostics.
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteName(String);

impl RemoteName {
    pub(crate) fn from_config(value: &[u8]) -> Result<Self> {
        let value = std::str::from_utf8(value)
            .wrap_err("The configured GHerrit remote is not valid UTF-8")?;
        if value.is_empty() {
            bail!("The configured GHerrit remote is empty");
        }
        if value.chars().any(char::is_control) {
            bail!("The configured GHerrit remote contains a control character");
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(super) fn default_remote_name_from_values<'a>(
    values: impl IntoIterator<Item = &'a [u8]>,
) -> Result<RemoteName> {
    let mut values = values.into_iter();
    let Some(value) = values.next() else {
        return Ok(RemoteName("origin".to_owned()));
    };
    if values.next().is_some() {
        bail!("The GHerrit remote is configured more than once");
    }
    RemoteName::from_config(value)
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
        let git_dir_identity = GitDirIdentity(
            fs::canonicalize(inner.path())
                .wrap_err("Failed to identify the repository's Git directory")?,
        );
        Ok(Self { inner, current_branch, git_dir_identity })
    }

    pub(crate) fn git_dir_identity(&self) -> &GitDirIdentity {
        &self.git_dir_identity
    }

    /// Rejects repository state which can rewrite or truncate publication
    /// history.
    ///
    /// The safety checks performed by the pre-push hook are meaningful only
    /// for the graph GitHub receives. Git has no flag which disables legacy
    /// graft files, and a shallow boundary hides real ancestry. This check must
    /// therefore run before publication graph traversal.
    pub fn ensure_publishable_history(&self) -> Result<()> {
        let common_dir = self.layout().common_git_dir;
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

    /// Returns whether repository configuration declares a promisor object
    /// source, either through partial-clone configuration or a promisor remote.
    ///
    /// History validation and repository preflight use the same interpretation
    /// when deciding whether missing ancestry may be repaired with an explicit
    /// refetch.
    pub(crate) fn has_promisor_remote(&self) -> Result<bool> {
        let configured_by_extension = self
            .partial_clone_remote_from_direct_common_config()?
            .is_some_and(|remote| promisor_loader_accepts_remote_name(&remote));

        // Git's promisor loader does not use ordinary last-value-wins lookup.
        // It visits every occurrence, adds a remote for any true promisor
        // value or partial-clone filter, and never removes one for a later
        // false value. Ask Git for that resolved occurrence stream so
        // includes, scopes, implicit values, and repeated keys have exactly
        // the same meaning here as they do to Git itself.
        let mut command =
            cmd("git", ["config", "--null", "--get-regexp", REMOTE_PROMISOR_CONFIG_PATTERN]);
        clear_git_repository_overrides(&mut command);
        command
            .env("GIT_DIR", self.inner.path())
            .env("GIT_COMMON_DIR", self.inner.common_dir())
            .current_dir(self.inner.workdir().unwrap_or(self.inner.path()));
        if let Some(worktree) = self.inner.workdir() {
            command.env("GIT_WORK_TREE", worktree);
        }

        let output =
            command.bounded_output().wrap_err("Failed to inspect remote promisor configuration")?;
        match output.status.code() {
            Some(0) => Ok(configured_by_extension | parse_remote_promisor_config(&output.stdout)?),
            Some(1) if output.stdout.is_empty() => Ok(configured_by_extension),
            _ => bail!("Invalid remote promisor configuration"),
        }
    }

    /// Reads `extensions.partialClone` exactly as Git reads repository format:
    /// from the common directory's direct `config` file, without includes or
    /// configuration inherited from another scope.
    fn partial_clone_remote_from_direct_common_config(&self) -> Result<Option<Vec<u8>>> {
        let mut command = cmd("git", ["config", "--file"]);
        command.arg(self.inner.common_dir().join("config")).args([
            "--no-includes",
            "--null",
            "--get",
            "extensions.partialClone",
        ]);
        clear_git_repository_overrides(&mut command);
        // These are Git command scope rather than part of the repository
        // format file. Other bound children keep them so enclosing `git -c`
        // settings continue to reach hooks.
        command.env_remove("GIT_CONFIG_PARAMETERS");
        command.env_remove("GIT_CONFIG_COUNT");

        let output = command
            .bounded_output()
            .wrap_err("Failed to inspect the repository partial-clone configuration")?;
        match output.status.code() {
            Some(0) if output.stderr.is_empty() => output
                .stdout
                .strip_suffix(b"\0")
                .map(|remote| Some(remote.to_owned()))
                .ok_or_else(|| eyre!("Git returned malformed partial-clone configuration")),
            Some(1) if output.stdout.is_empty() && output.stderr.is_empty() => Ok(None),
            _ => bail!("Invalid repository partial-clone configuration"),
        }
    }

    pub fn current_branch(&self) -> &HeadState {
        &self.current_branch
    }

    /// Reads the logical branch identity and exact `HEAD` target together.
    ///
    /// `gix::Head` is a snapshot of the persisted HEAD and referent state. Use
    /// its retained target rather than resolving the name again after another
    /// process has had an opportunity to move the branch.
    pub(crate) fn branch_head_snapshot(&self) -> Result<Option<BranchHeadSnapshot>> {
        branch_head_snapshot(&self.inner)
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

    pub fn default_remote_name(&self) -> Result<RemoteName> {
        let snapshot = self.inner.config_snapshot();
        let values = snapshot.strings("gherrit.remote").unwrap_or_default();
        default_remote_name_from_values(values.iter().map(|value| value.as_ref().as_bytes()))
    }

    fn find_default_branches(&self, remote_name: &RemoteName) -> Result<Vec<String>> {
        let mut branches = Vec::new();

        // Try to infer the default branch from the remote HEAD.
        let remote_head_ref = format!("refs/remotes/{}/HEAD", remote_name.as_str());
        if let Ok(head_ref) = self.inner.find_reference(&remote_head_ref) {
            let target_name = head_ref.target().try_name().map(|n| n.as_bstr().to_string());
            if let Some(target) = target_name {
                let prefix = format!("refs/remotes/{}/", remote_name.as_str());
                if let Some(stripped) = target.strip_prefix(&prefix) {
                    branches.push(stripped.to_string());
                }
            }
        }

        // Check git config
        //
        if let Some(default_branch) = self.config_string("init.defaultBranch")? {
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

        Ok(branches)
    }

    pub fn is_a_default_branch_on_default_remote(&self, branch_name: &str) -> Result<bool> {
        let remote_name = self.default_remote_name()?;
        let branches = self.find_default_branches(&remote_name)?;
        Ok(branches.iter().any(|branch| branch == branch_name))
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

pub(super) fn parse_remote_promisor_config(output: &[u8]) -> Result<bool> {
    let Some(records) = output.strip_suffix(b"\0") else {
        bail!("Git returned malformed remote promisor configuration");
    };
    records.split(|byte| *byte == 0).try_fold(false, |has_promisor, record| {
        let newline = record.iter().position(|byte| *byte == b'\n');
        let (key, value) = newline
            .map_or((record, None), |newline| (&record[..newline], Some(&record[newline + 1..])));

        if key.ends_with(b".promisor") {
            let value = match value {
                Some(value) => bool::from(
                    gix::config::Boolean::try_from(value.as_bstr())
                        .wrap_err("Invalid remote promisor configuration")?,
                ),
                None => true,
            };
            let remote = promisor_remote_name(key, b".promisor")?;
            Ok(has_promisor | (value && promisor_loader_accepts_remote_name(remote)))
        } else if key.ends_with(b".partialclonefilter") {
            let remote = promisor_remote_name(key, b".partialclonefilter")?;
            if !promisor_loader_accepts_remote_name(remote) {
                return Ok(has_promisor);
            }
            if value.is_none() {
                bail!("Invalid remote partial-clone filter configuration");
            }
            Ok(true)
        } else {
            bail!("Git returned unexpected remote promisor configuration");
        }
    })
}

pub(super) fn promisor_remote_name<'a>(key: &'a [u8], suffix: &[u8]) -> Result<&'a [u8]> {
    key.strip_prefix(b"remote.")
        .and_then(|key| key.strip_suffix(suffix))
        .ok_or_else(|| eyre!("Git returned unexpected remote promisor configuration"))
}

/// Git ignores a promisor remote whose name starts with `/` so that a remote
/// name cannot be confused with a repository path.
pub(super) fn promisor_loader_accepts_remote_name(name: &[u8]) -> bool {
    !name.starts_with(b"/")
}

pub(super) fn reject_nonempty_history_file(
    path: &Path,
    description: &str,
    reason: &str,
) -> Result<()> {
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

pub(crate) fn parse_git_version(output: &[u8]) -> Result<(u64, u64)> {
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

pub(super) fn require_git_no_lazy_fetch(output: &[u8]) -> Result<()> {
    let (major, minor) = parse_git_version(output)?;
    if (major, minor) < (2, 45) {
        bail!(
            "GHerrit requires Git 2.45 or newer for a promisor repository so implicit object \
             fetches can be disabled; found Git {major}.{minor}"
        );
    }
    Ok(())
}

pub(crate) fn require_git_config_env() -> Result<()> {
    let output = cmd("git", ["--version"])
        .checked_output()
        .wrap_err("Failed to determine the installed Git version")?;
    require_git_config_env_output(&output.stdout)
}

pub(super) fn require_git_config_env_output(output: &[u8]) -> Result<()> {
    let (major, minor) = parse_git_version(output)?;
    if (major, minor) < (2, 31) {
        bail!(
            "GHerrit requires Git 2.31 or newer to bind the private publication destination without placing it in the top-level Git process arguments; found Git {major}.{minor}"
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
        ancestor: ObjectId,
        descendant: ObjectId,
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

pub(super) fn branch_head_snapshot(repo: &gix::Repository) -> Result<Option<BranchHeadSnapshot>> {
    let head = repo.head()?;
    let Some(name) = head.referent_name() else {
        return Ok(None);
    };
    let Some((gix::refs::Category::LocalBranch, short_name)) = name.category_and_short_name()
    else {
        bail!("Cannot push because symbolic HEAD does not target a local branch");
    };
    let branch_name = decode_branch_name(short_name.as_ref())?;
    let target = head.id().map(|id| id.detach());
    Ok(Some(BranchHeadSnapshot { branch_name, target }))
}

/// Determines the logical branch identity from one retained HEAD snapshot.
pub(super) fn get_head_state(repo: &gix::Repository, head: &gix::Head<'_>) -> Result<HeadState> {
    if let Some(name) = head.referent_name() {
        let name = decode_branch_name(name.shorten().as_ref())?;
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
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("Failed to read {}", path.display()));
                }
            };
            let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
            let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
            let bytes = bytes.strip_prefix(b"refs/heads/").unwrap_or(bytes);
            Ok(Some(decode_branch_name(bytes)?))
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

pub(super) fn get_current_branch(repo: &gix::Repository) -> Result<HeadState> {
    let head = repo.head()?;
    get_head_state(repo, &head)
}

pub(super) fn decode_branch_name(bytes: &[u8]) -> Result<String> {
    if bytes.is_empty() {
        bail!("GHerrit requires a nonempty branch name");
    }
    Ok(str::from_utf8(bytes)
        .wrap_err("GHerrit requires branch names to be valid UTF-8")?
        .to_owned())
}

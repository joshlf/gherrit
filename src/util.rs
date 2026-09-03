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

const REMOTE_PROMISOR_CONFIG_PATTERN: &str = r"^remote\..*\.(promisor|partialclonefilter)$";

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

/// Removes inherited variables which can select a different repository or
/// replace its local configuration with another file.
///
/// `GIT_CONFIG_PARAMETERS` and the numbered `GIT_CONFIG_*` variables remain
/// intact: an enclosing `git -c` invocation deliberately passes those command
/// scope settings into hooks and GHerrit's bound children must preserve them.
pub(crate) fn clear_git_repository_overrides(command: &mut Command) {
    for variable in [
        "GIT_CONFIG",
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        command.env_remove(variable);
    }
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

fn default_remote_name_from_values<'a>(
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
            command.output().wrap_err("Failed to inspect remote promisor configuration")?;
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
            .output()
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

fn parse_remote_promisor_config(output: &[u8]) -> Result<bool> {
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

fn promisor_remote_name<'a>(key: &'a [u8], suffix: &[u8]) -> Result<&'a [u8]> {
    key.strip_prefix(b"remote.")
        .and_then(|key| key.strip_suffix(suffix))
        .ok_or_else(|| eyre!("Git returned unexpected remote promisor configuration"))
}

/// Git ignores a promisor remote whose name starts with `/` so that a remote
/// name cannot be confused with a repository path.
fn promisor_loader_accepts_remote_name(name: &[u8]) -> bool {
    !name.starts_with(b"/")
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

pub(crate) fn require_git_config_env() -> Result<()> {
    let output = cmd("git", ["--version"])
        .checked_output()
        .wrap_err("Failed to determine the installed Git version")?;
    require_git_config_env_output(&output.stdout)
}

fn require_git_config_env_output(output: &[u8]) -> Result<()> {
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

fn command_invocation(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| format!("{part:?}"))
        .collect::<Vec<_>>()
        .join(" ")
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
            let invocation = command_invocation(self);
            bail!("Command {invocation} failed with status: {}. Stderr: {stderr}", output.status,);
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

#[cfg(test)]
mod tests {
    use super::*;

    const PROMISOR_CONFIGURATION_FIXTURE_MODE: &str = "GHERRIT_PROMISOR_CONFIGURATION_FIXTURE_MODE";
    const PROMISOR_CONFIGURATION_FIXTURE_TEST: &str =
        "util::tests::promisor_configuration_process_fixture";

    fn promisor_configuration_fixture(
        current_dir: &Path,
        global: &Path,
        overridden_config: &Path,
        expected: bool,
    ) {
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", PROMISOR_CONFIGURATION_FIXTURE_TEST, "--nocapture"])
            .current_dir(current_dir)
            .env(PROMISOR_CONFIGURATION_FIXTURE_MODE, expected.to_string())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", global)
            .env("GIT_CONFIG", overridden_config)
            .env("GIT_CONFIG_PARAMETERS", "'extensions.partialClone=parameters'")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "extensions.partialClone")
            .env("GIT_CONFIG_VALUE_0", "command")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "promisor fixture failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn promisor_configuration_process_fixture() {
        let Ok(expected) = env::var(PROMISOR_CONFIGURATION_FIXTURE_MODE) else {
            return;
        };
        let expected: bool = expected.parse().unwrap();
        let repository = Repo::open(".").unwrap();
        assert_eq!(repository.has_promisor_remote().unwrap(), expected);
    }

    #[test]
    fn remote_names_are_decoded_and_validated_without_fallback() {
        for value in [b"origin".as_slice(), b"-publish", "rémote".as_bytes()] {
            assert_eq!(RemoteName::from_config(value).unwrap().as_str().as_bytes(), value);
        }

        for value in [b"".as_slice(), b"bad\nname", b"bad\rname", b"bad\0name", b"\xff"] {
            assert!(RemoteName::from_config(value).is_err(), "value: {value:?}");
        }
    }

    #[test]
    fn the_default_remote_requires_at_most_one_configured_value() {
        assert_eq!(default_remote_name_from_values(std::iter::empty()).unwrap().as_str(), "origin");
        assert_eq!(
            default_remote_name_from_values([b"publish".as_slice()]).unwrap().as_str(),
            "publish"
        );

        for values in [
            [b"origin".as_slice(), b"origin".as_slice()],
            [b"origin".as_slice(), b"publish".as_slice()],
        ] {
            let error = default_remote_name_from_values(values)
                .err()
                .expect("repeated values must be rejected");
            assert_eq!(error.to_string(), "The GHerrit remote is configured more than once");
        }
    }

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
    fn clearing_repository_overrides_preserves_command_scope_configuration() {
        let mut command = Command::new("git");
        command
            .env("GIT_CONFIG", "redirected.config")
            .env("GIT_CONFIG_PARAMETERS", "'remote.origin.proxy=preserved'")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "remote.origin.proxyAuthMethod")
            .env("GIT_CONFIG_VALUE_0", "digest");

        clear_git_repository_overrides(&mut command);

        let environment = command.get_envs().collect::<std::collections::HashMap<_, _>>();
        assert_eq!(environment[OsStr::new("GIT_CONFIG")], None);
        for variable in
            ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"]
        {
            assert!(environment[OsStr::new(variable)].is_some(), "variable={variable}");
        }
    }

    #[test]
    fn command_diagnostics_render_only_program_and_arguments() {
        let mut command = Command::new("git");
        command.args(["status", "path with spaces"]);
        command.env("VISIBLE_TO_CHILD_ONLY", "secret");
        command.env_remove("REMOVED_FROM_CHILD");

        assert_eq!(command_invocation(&command), r#""git" "status" "path with spaces""#);
    }

    #[test]
    fn literal_graph_open_options_deny_object_environment_at_every_trust_level() {
        let options = literal_graph_open_options();
        for trust in [gix::sec::Trust::Full, gix::sec::Trust::Reduced] {
            assert_eq!(options.by_level(trust).permissions.env.objects, gix::sec::Permission::Deny);
        }
    }

    #[test]
    fn git_directory_identity_is_stable_and_worktree_specific() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        let linked = context.dir.path().join("linked-identity");
        context
            .git_cmd()
            .args(["worktree", "add", "--detach"])
            .arg(&linked)
            .arg("HEAD")
            .assert()
            .success();

        let main = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let reopened = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let linked = Repo::open(linked.to_str().unwrap()).unwrap();

        assert!(main.git_dir_identity() == reopened.git_dir_identity());
        assert!(main.git_dir_identity() != linked.git_dir_identity());
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
    fn promisor_configuration_is_additive_across_every_occurrence() {
        for output in [
            b"remote.origin.promisor\ntrue\0remote.origin.promisor\nfalse\0".as_slice(),
            b"remote.origin.promisor\0remote.origin.promisor\nfalse\0",
            b"remote.origin.partialclonefilter\nblob:none\0",
            b"remote.origin.partialclonefilter\n\0",
            b"remote.origin.promisor\ntrue\0remote./cache.promisor\ntrue\0",
            b"remote.origin.promisor\ntrue\0remote./cache.promisor\nfalse\0",
            b"remote.origin.promisor\ntrue\0remote./cache.promisor\0",
            b"remote.origin.promisor\ntrue\0remote./cache.partialclonefilter\0",
        ] {
            assert!(parse_remote_promisor_config(output).unwrap(), "output: {output:?}");
        }

        for output in [
            b"remote.origin.promisor\nfalse\0".as_slice(),
            b"remote.origin.promisor\n\0remote.origin.promisor\nfalse\0",
            b"remote./cache.promisor\ntrue\0",
            b"remote./cache.partialclonefilter\nblob:none\0",
            b"remote./cache.partialclonefilter\0",
        ] {
            assert!(!parse_remote_promisor_config(output).unwrap(), "output: {output:?}");
        }

        assert!(
            parse_remote_promisor_config(
                b"remote./cache.promisor\ntrue\0remote.origin.promisor\ntrue\0"
            )
            .unwrap()
        );

        for output in [
            b"remote.origin.promisor\ninvalid\0".as_slice(),
            b"remote.origin.promisor\ntrue\0remote.origin.promisor\ninvalid\0",
            b"remote./cache.promisor\ninvalid\0",
            b"remote.origin.partialclonefilter\0",
            b"remote.origin.promisor\ntrue",
            b"remote.origin.unexpected\ntrue\0",
        ] {
            assert!(parse_remote_promisor_config(output).is_err(), "output: {output:?}");
        }
    }

    #[test]
    fn git_config_preserves_each_promisor_occurrence_for_decoding() {
        let parse = |contents: &str| {
            let config = tempfile::NamedTempFile::new().unwrap();
            fs::write(config.path(), contents).unwrap();
            let output = cmd("git", ["config", "--file"])
                .arg(config.path())
                .args(["--null", "--get-regexp", REMOTE_PROMISOR_CONFIG_PATTERN])
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            parse_remote_promisor_config(&output.stdout)
        };

        for contents in [
            "[remote \"origin\"]\n\tpromisor = true\n\tpromisor = false\n",
            "[remote \"origin\"]\n\tpromisor\n\tpromisor = false\n",
            "[remote \"origin\"]\n\tpartialCloneFilter = blob:none\n",
            "[remote \"origin\"]\n\tpartialCloneFilter =\n",
        ] {
            assert!(parse(contents).unwrap(), "contents:\n{contents}");
        }
        assert!(!parse("[remote \"origin\"]\n\tpromisor =\n\tpromisor = false\n").unwrap());
        for contents in [
            "[remote \"origin\"]\n\tpromisor = true\n\tpromisor = invalid\n",
            "[remote \"origin\"]\n\tpartialCloneFilter\n",
        ] {
            assert!(parse(contents).is_err(), "contents:\n{contents}");
        }
    }

    #[test]
    fn slash_leading_promisor_remote_names_are_ignored_like_git() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        context.run_git(&["config", "core.repositoryFormatVersion", "1"]);
        context.run_git(&["config", "extensions.partialClone", "/cache"]);
        context.run_git(&["config", "remote./cache.promisor", "true"]);
        context.run_git(&["config", "remote./cache.partialCloneFilter", "blob:none"]);

        let repository = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        assert!(!repository.has_promisor_remote().unwrap());

        context.run_git(&["config", "remote.origin.promisor", "true"]);
        let repository = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        assert!(repository.has_promisor_remote().unwrap());
    }

    #[test]
    fn promisor_configuration_uses_direct_common_config_and_bound_repository() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        let global = context.dir.path().join("global.config");
        fs::write(&global, "[extensions]\n\tpartialClone = global\n").unwrap();
        let redirected = context.dir.path().join("redirected.config");
        fs::write(
            &redirected,
            "[extensions]\n\tpartialClone = redirected\n[remote \"redirected\"]\n\tpromisor = true\n",
        )
        .unwrap();
        let empty = context.dir.path().join("empty.config");
        fs::write(&empty, "").unwrap();

        // Global, command, and GIT_CONFIG scopes may describe a different
        // repository. None may create a partial-clone extension here.
        promisor_configuration_fixture(&context.repo_path, &global, &redirected, false);

        context.run_git(&["config", "core.repositoryFormatVersion", "1"]);
        context.run_git(&["config", "extensions.partialClone", "origin"]);
        // A linked worktree reads the common configuration, not its own
        // per-worktree Git directory.
        let linked = context.dir.path().join("linked");
        context
            .git_cmd()
            .args(["worktree", "add", "--detach"])
            .arg(&linked)
            .arg("HEAD")
            .assert()
            .success();
        promisor_configuration_fixture(&linked, &global, &empty, true);

        context.run_git(&["config", "--unset-all", "extensions.partialClone"]);
        context.run_git(&["config", "remote.origin.promisor", "true"]);
        // `GIT_CONFIG` must neither supply a false promisor remote nor hide
        // the selected repository's local promisor remote.
        promisor_configuration_fixture(&context.repo_path, &global, &empty, true);
    }

    #[test]
    fn config_env_support_requires_git_2_31() {
        let error = require_git_config_env_output(b"git version 2.30.9\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "GHerrit requires Git 2.31 or newer to bind the private publication destination without placing it in the top-level Git process arguments; found Git 2.30"
        );
        require_git_config_env_output(b"git version 2.31.0\n").unwrap();
        require_git_config_env_output(b"git version 3.0.0\n").unwrap();
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
}

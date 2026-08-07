use std::{ffi::OsStr, process::Command};

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
/// ```rust
/// // - "git" is the command; "config" is an argument.
/// // - "branch.{branch_name}.gherritManaged" is a single argument (even if it contains spaces when formatted).
/// // - `state` is a single argument (even if it contains spaces when formatted).
/// cmd!("git config", "branch.{branch_name}.gherritManaged", state)
/// ```
#[macro_export]
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
        cmd!(@inner args $(, $($rest)*)?);

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
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    // Expression (not broken apart).
    (@inner $vec:ident, $e:expr $(, $($rest:tt)*)?) => {
        $vec.push($e.to_string());
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    (@inner $vec:ident $(,)?) => {};
}

#[macro_export]
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

pub fn cmd<I: AsRef<OsStr>>(name: &str, args: impl IntoIterator<Item = I>) -> Command {
    let mut c = Command::new(name);
    c.args(args);
    c
}

use std::path::PathBuf;

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
        let name = self.default_remote_name();
        let fetch_urls = self.effective_remote_urls(&name, false)?;
        let push_urls = self.effective_remote_urls(&name, true)?;

        let [fetch_url] = fetch_urls.as_slice() else {
            bail!(
                "Remote `{name}` has {} effective fetch URLs; GHerrit requires exactly one publication destination",
                fetch_urls.len()
            );
        };
        let [push_url] = push_urls.as_slice() else {
            bail!(
                "Remote `{name}` has {} effective push URLs; GHerrit refuses multi-destination publication",
                push_urls.len()
            );
        };

        let workdir = self.workdir().unwrap_or(self.path());
        let fetch_endpoint = RemoteEndpoint::parse(fetch_url, workdir)?;
        let push_endpoint = RemoteEndpoint::parse(push_url, workdir)?;
        if !fetch_endpoint.same_authority(&push_endpoint) {
            bail!(
                "Remote `{name}` has effective fetch and push URLs that resolve to different Git authorities; GHerrit refuses to observe one repository and publish to another"
            );
        }
        if !__TESTING && !fetch_endpoint.is_github_com() {
            bail!(
                "Remote `{name}` cannot be authenticated against GitHub.com. Use a direct github.com URL rather than a local path, mirror, or SSH host alias."
            );
        }

        // `git remote get-url` expands at most one URL-rewrite step. Reusing
        // that returned string as an explicit URL in a later `fetch`,
        // `ls-remote`, or `push` subjects it to URL rewriting again. An
        // attacker-controlled second rewrite could therefore redirect the
        // authenticated-looking URL to another repository while preserving
        // identical leases. Prove that both pinned URLs are fixed points under
        // the complete active Git configuration before any network operation.
        self.validate_pinned_remote_url(&name, "fetch", &fetch_endpoint.git_url, false)?;
        self.validate_pinned_remote_url(&name, "push", &push_endpoint.git_url, true)?;

        let (owner, repo_name) = get_repo_owner_name(&fetch_endpoint.git_url)?;
        let (push_owner, push_repo_name) = get_repo_owner_name(&push_endpoint.git_url)?;
        Ok(Remote {
            name,
            push_url: push_endpoint.git_url,
            owner,
            repo_name,
            push_owner,
            push_repo_name,
        })
    }

    fn effective_remote_urls(&self, name: &str, push: bool) -> Result<Vec<String>> {
        let mut args = vec!["remote", "get-url"];
        if push {
            args.push("--push");
        }
        args.extend(["--all", name]);

        let mut command = cmd("git", args);
        command.current_dir(self.workdir().unwrap_or(self.path()));
        let output = command.checked_output().wrap_err_with(|| {
            format!(
                "Failed to resolve effective {} URLs for remote `{name}`",
                if push { "push" } else { "fetch" }
            )
        })?;
        let urls = std::str::from_utf8(&output.stdout)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if urls.is_empty() {
            bail!("Remote `{name}` has no effective {} URL", if push { "push" } else { "fetch" });
        }
        Ok(urls)
    }

    fn validate_pinned_remote_url(
        &self,
        remote_name: &str,
        role: &str,
        url: &str,
        push: bool,
    ) -> Result<()> {
        let workdir = self.workdir().unwrap_or(self.path());

        // Ask Git itself to apply `url.*.insteadOf`, including values from
        // system, global, local, worktree, included, and environment-injected
        // configuration. The URL is safe to reuse only if Git returns it
        // byte-for-byte unchanged.
        let mut command = cmd("git", ["ls-remote", "--get-url", url]);
        command.current_dir(workdir);
        let output = command.checked_output().wrap_err_with(|| {
            format!("Failed to validate the pinned {role} URL for remote `{remote_name}`")
        })?;
        let resolved = std::str::from_utf8(&output.stdout)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        let [resolved] = resolved.as_slice() else {
            bail!(
                "Pinned {role} URL for remote `{remote_name}` resolved to {} URLs during fixed-point validation",
                resolved.len()
            );
        };
        if *resolved != url {
            bail!(
                "Pinned {role} URL `{url}` for remote `{remote_name}` is not a fixed point under active Git URL rewrites; Git would resolve it again to `{resolved}`"
            );
        }

        if !push {
            return Ok(());
        }

        // `git ls-remote --get-url` models `insteadOf`, but an explicit
        // `git push <url>` also applies `pushInsteadOf`. Enumerate the complete
        // active config (with includes) and reject every prefix which could
        // rewrite the pinned push URL. Rejecting all matching prefixes is
        // intentionally stricter than reproducing Git's longest-prefix choice:
        // it proves that no push rewrite can apply at all.
        let mut command = cmd(
            "git",
            ["config", "--null", "--includes", "--get-regexp", r"^url\..*\.pushinsteadof$"],
        );
        command.current_dir(workdir);
        let output = command.output()?;
        match output.status.code() {
            Some(0 | 1) => {}
            code => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "Failed to inspect active pushInsteadOf rewrites for remote `{remote_name}`: exit {code:?}. Stderr: {stderr}"
                );
            }
        }

        for entry in output.stdout.split(|byte| *byte == b'\0').filter(|entry| !entry.is_empty()) {
            let Some(separator) = entry.iter().position(|byte| *byte == b'\n') else {
                bail!("Git returned a malformed pushInsteadOf configuration entry");
            };
            let key = std::str::from_utf8(&entry[..separator])?;
            let prefix = std::str::from_utf8(&entry[separator + 1..])?;
            if prefix.is_empty() || url.starts_with(prefix) {
                bail!(
                    "Pinned push URL `{url}` for remote `{remote_name}` can still be rewritten by active Git configuration `{key}` with prefix `{prefix}`"
                );
            }
        }

        Ok(())
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
        // If there is no common ancestor (e.g., an orphan branch), `merge_base`
        // returns an error. We treat this as "not an ancestor".
        let is_ancestor = self
            .inner
            .merge_base(ancestor, descendant)
            .map(|merge_base| merge_base.detach() == ancestor)
            .unwrap_or(false);
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteAuthority {
    Local(PathBuf),
    Network(String),
    Unqualified(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteEndpoint {
    authority: RemoteAuthority,
    git_url: String,
}

impl RemoteEndpoint {
    fn parse(url: &str, workdir: &std::path::Path) -> Result<Self> {
        let path = if let Some(path) = url.strip_prefix("file://") {
            Some(PathBuf::from(path))
        } else {
            let candidate = PathBuf::from(url);
            if candidate.is_absolute()
                || url.starts_with("./")
                || url.starts_with("../")
                || workdir.join(&candidate).exists()
            {
                Some(candidate)
            } else {
                None
            }
        };
        if let Some(path) = path {
            let path = if path.is_absolute() { path } else { workdir.join(path) };
            let path = path.canonicalize().unwrap_or(path);
            return Ok(Self {
                git_url: path.to_string_lossy().into_owned(),
                authority: RemoteAuthority::Local(path),
            });
        }

        if let Some((scheme, remainder)) = url.split_once("://") {
            if scheme.eq_ignore_ascii_case("file") {
                return Err(eyre!("Invalid file remote URL `{url}`"));
            }
            let authority = remainder.split('/').next().unwrap_or_default();
            let authority = authority.rsplit('@').next().unwrap_or(authority);
            if authority.is_empty() {
                return Err(eyre!("Remote URL `{url}` has no network authority"));
            }
            let mut authority = authority.trim_end_matches('.').to_ascii_lowercase();
            let default_port = match scheme.to_ascii_lowercase().as_str() {
                "http" => Some(":80"),
                "https" => Some(":443"),
                "ssh" => Some(":22"),
                _ => None,
            };
            if let Some(port) = default_port
                && authority.ends_with(port)
            {
                authority.truncate(authority.len() - port.len());
            }
            return Ok(Self {
                authority: RemoteAuthority::Network(authority),
                git_url: url.to_string(),
            });
        }

        if let Some((authority, _)) = url.split_once(':')
            && !authority.contains('/')
        {
            let authority = authority.rsplit('@').next().unwrap_or(authority);
            if !authority.is_empty() {
                return Ok(Self {
                    authority: RemoteAuthority::Network(
                        authority.trim_end_matches('.').to_ascii_lowercase(),
                    ),
                    git_url: url.to_string(),
                });
            }
        }

        Ok(Self {
            authority: RemoteAuthority::Unqualified(url.to_string()),
            git_url: url.to_string(),
        })
    }

    fn same_authority(&self, other: &Self) -> bool {
        match (&self.authority, &other.authority) {
            (RemoteAuthority::Network(left), RemoteAuthority::Network(right)) => {
                github_authority(left) == github_authority(right)
            }
            _ => self.authority == other.authority,
        }
    }

    fn is_github_com(&self) -> bool {
        matches!(
            &self.authority,
            RemoteAuthority::Network(authority) if github_authority(authority) == "github.com"
        )
    }
}

fn github_authority(authority: &str) -> &str {
    match authority {
        // GitHub documents ssh.github.com:443 as its SSH-over-HTTPS-port
        // endpoint. `ssh://` URLs retain the non-default port during parsing,
        // while scp-like URLs have no separate port field.
        "ssh.github.com" | "ssh.github.com:443" => "github.com",
        authority => authority,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub push_url: String,
    pub owner: String,
    pub repo_name: String,
    pub push_owner: String,
    pub push_repo_name: String,
}

impl Remote {
    pub fn git_url(&self) -> &str {
        &self.push_url
    }

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

pub const __TESTING: bool = option_env!("GHERRIT_TEST_BUILD").is_some();

#[cfg(test)]
mod tests {
    use super::*;

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
    fn remote_endpoints_pin_one_github_authority() {
        let workdir = std::path::Path::new(".");
        let https =
            RemoteEndpoint::parse("https://github.com/owner/repository.git", workdir).unwrap();
        let ssh =
            RemoteEndpoint::parse("git@ssh.github.com:owner/repository.git", workdir).unwrap();
        let ssh_over_443 =
            RemoteEndpoint::parse("ssh://git@ssh.github.com:443/owner/repository.git", workdir)
                .unwrap();
        assert!(https.same_authority(&ssh));
        assert!(https.same_authority(&ssh_over_443));
        assert!(https.is_github_com());
        assert!(ssh.is_github_com());
        assert!(ssh_over_443.is_github_com());

        let other = RemoteEndpoint::parse("git@example.com:owner/repository.git", workdir).unwrap();
        assert!(!https.same_authority(&other));
        assert!(!other.is_github_com());
    }

    #[test]
    fn remote_endpoints_canonicalize_equivalent_local_paths() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("owner/repository.git");
        std::fs::create_dir_all(&repository).unwrap();
        let relative = repository.strip_prefix(root.path()).unwrap().to_string_lossy();
        let path = RemoteEndpoint::parse(&relative, root.path()).unwrap();
        let file = RemoteEndpoint::parse(&format!("file://{}", repository.display()), root.path())
            .unwrap();
        assert!(path.same_authority(&file));
        assert_eq!(path.git_url, file.git_url);
    }

    #[test]
    fn test_get_repo_owner_name() {
        for (url, (owner, repo)) in [
            ("https://github.com/owner/repo.git", ("owner", "repo")),
            ("https://github.com/owner/repo", ("owner", "repo")),
            ("git@github.com:owner/repo.git", ("owner", "repo")),
            ("git@github.com:owner/repo", ("owner", "repo")),
            ("ssh://git@ssh.github.com:443/owner/repo.git", ("owner", "repo")),
            ("alias:owner/repo.git", ("owner", "repo")),
            ("alias:owner/repo", ("owner", "repo")),
            ("http://localhost:3000/owner/repo.git", ("owner", "repo")),
            ("http://my-gh.com/owner/repo", ("owner", "repo")),
            ("/tmp/test/owner/repo.git", ("owner", "repo")),
            ("/tmp/owner/repo", ("owner", "repo")),
            ("owner/repo", ("owner", "repo")),
        ] {
            let expect = (owner.to_string(), repo.to_string());
            assert_eq!(get_repo_owner_name(url).unwrap(), expect);
        }
    }
}

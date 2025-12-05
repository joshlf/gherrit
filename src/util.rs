use std::ffi::OsStr;
use std::process::Command;

use eyre::Result;
use gix::bstr::ByteSlice as _;

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
        util::cmd(bin, &args)
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
            re!(@inner $re)
        }
    };
    ($re:literal) => {
        re!(@inner $re)
    };
    (@inner $re:literal) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new($re).unwrap())
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
    /// HEAD points to a local branch (e.g., `refs/heads/main`).
    Attached(String),
    /// HEAD is detached, but we are in the middle of a rebase of this branch.
    Rebasing(String),
    /// HEAD is detached and no rebase context was found.
    Detached,
}

impl HeadState {
    /// Returns the logical branch name if one exists (Attached or Rebasing).
    pub fn name(&self) -> Option<&str> {
        match self {
            HeadState::Attached(name) | HeadState::Rebasing(name) => Some(name),
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
        Ok(Self {
            inner,
            current_branch,
        })
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

    pub fn config_bool(&self, key: &str) -> Result<Option<bool>> {
        Ok(self.inner.config_snapshot().try_boolean(key).transpose()?)
    }

    pub fn is_newly_created_branch(&self, branch_name: &str) -> Result<bool> {
        // If this branch was just created (as opposed to having been checked out
        // from an existing branch), then its earliest reflog entry will have the
        // message "branch: Created from ...".
        Ok(self
            .inner
            .find_reference(branch_name)?
            .log_iter()
            .rev()?
            .into_iter()
            .flatten()
            .next()
            .transpose()?
            .is_some_and(|log| log.message.contains_str("branch: Created from")))
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
    let head = repo.head()?;

    // 1. Try standard Attached HEAD
    if let Some(name) = head.referent_name() {
        let name = name.shorten().to_string();
        return Ok(HeadState::Attached(name));
    }

    // 2. Try Rebase Detection (Rebase-Merge or Rebase-Apply)
    let git_dir = repo.path();

    let try_read_ref = |path: PathBuf| -> Option<String> {
        std::fs::read_to_string(path).ok().map(|content| {
            content
                .trim()
                .strip_prefix("refs/heads/")
                .unwrap_or(content.trim())
                .to_string()
        })
    };

    if let Some(name) = try_read_ref(git_dir.join("rebase-merge/head-name")) {
        return Ok(HeadState::Rebasing(name));
    }

    if let Some(name) = try_read_ref(git_dir.join("rebase-apply/head-name")) {
        return Ok(HeadState::Rebasing(name));
    }

    // 3. Fallback to Detached
    Ok(HeadState::Detached)
}

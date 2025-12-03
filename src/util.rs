use std::error::Error;
use std::ffi::OsStr;
use std::process::{Command, ExitStatus, Output};

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
        (|| -> &'static regex::Regex { re!(@inner $re) })()
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

pub trait CommandExt {
    fn unwrap_status(self) -> ExitStatus;
    fn unwrap_output(self) -> Output;
}

impl CommandExt for Command {
    fn unwrap_status(mut self) -> ExitStatus {
        self.status().unwrap()
    }

    fn unwrap_output(mut self) -> Output {
        self.output().unwrap()
    }
}

pub trait ResultExt<T, E> {
    fn unwrap_or_exit(self, prefix: &str) -> T;
}

impl<T, E: std::fmt::Display> ResultExt<T, E> for Result<T, E> {
    fn unwrap_or_exit(self, prefix: &str) -> T {
        match self {
            Ok(t) => t,
            Err(e) => {
                if prefix.is_empty() {
                    log::error!("{}", e);
                } else {
                    log::error!("{}: {}", prefix, e);
                }
                std::process::exit(1);
            }
        }
    }
}

pub fn to_trimmed_string_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[derive(Debug)]
pub enum BranchError {
    DetachedHead,
    Gix(Box<dyn Error>),
}

impl std::fmt::Display for BranchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BranchError::DetachedHead => write!(f, "Detached HEAD"),
            BranchError::Gix(e) => write!(f, "Gix error: {}", e),
        }
    }
}

impl Error for BranchError {}

pub fn get_current_branch() -> Result<(gix::Repository, String), BranchError> {
    let repo = gix::open(".").map_err(|e| BranchError::Gix(Box::new(e)))?;
    let head = repo.head().map_err(|e| BranchError::Gix(Box::new(e)))?;
    let head_ref = head.try_into_referent().ok_or(BranchError::DetachedHead)?;
    let branch_name = head_ref.name().shorten().to_string();
    Ok((repo, branch_name))
}

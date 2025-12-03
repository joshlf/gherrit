use std::error::Error;
use std::ffi::OsStr;
use std::process::{Command, ExitStatus, Output};

#[macro_export]
macro_rules! cmd {
    ($bin:literal $(, $($rest:tt)*)?) => {{
        let bin_str = format!($bin);
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

    // Parenthesized group: ($(...))
    (@inner $vec:ident, ($($fmt:tt)+) $(, $($rest:tt)*)?) => {
        $vec.push(format!($($fmt)+));
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    // String literal (treated as a format string)
    (@inner $vec:ident, $l:literal $(, $($rest:tt)*)?) => {
        let formatted = format!($l);
        for s in formatted.split_whitespace() {
            $vec.push(s.to_string());
        }
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    // Expression
    (@inner $vec:ident, $e:expr $(, $($rest:tt)*)?) => {
        $vec.push($e.to_string());
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    // Base cases
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

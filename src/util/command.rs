//! Builds child commands and checks their exit status.
//!
//! Commands run directly, without a shell. Callers choose arguments, captured
//! input/output, and how to manage the child process. These helpers do not add
//! timeouts, retry failures, or undo changes made before a command fails.

use std::{ffi::OsStr, process::Command};

use eyre::{Result, bail};

/// Constructs a [`std::process::Command`].
///
/// # Usage
///
/// The first argument is a string literal containing the command and any
/// initial arguments. It is split on whitespace. Each subsequent argument
/// remains a single argument, even if it contains spaces.
///
/// # Panics
///
/// Panics if the first string contains no non-whitespace characters.
///
/// # Examples
///
/// ```text
/// cmd!("git config", "branch.{branch_name}.gherritManaged", state)
/// ```
///
/// Here `git` is the command and `config` is its first argument. The formatted
/// branch key and `state` are each passed as one argument, including any spaces
/// in their values.
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
    c.args(args);
    c
}

pub trait CommandExt {
    /// Runs the command with its configured input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the child cannot be started or waited for, or exits
    /// with a nonzero status.
    fn success(&mut self) -> Result<()>;
    /// Returns captured output after checking the command's exit status.
    ///
    /// Error messages include stderr, so callers must check whether it contains
    /// sensitive data.
    ///
    /// # Errors
    ///
    /// Returns an error if starting the child or collecting its output fails,
    /// or if the child exits with a nonzero status.
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

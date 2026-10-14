use std::{ffi::OsStr, process::Command};

use eyre::{Result, bail};

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
    c.args(args);
    c
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

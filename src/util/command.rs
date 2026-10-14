use std::{ffi::OsStr, io::Write as _, process::Command};

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
        for variable in ["GIT_GRAFT_FILE", "GIT_REPLACE_REF_BASE", "GIT_SHALLOW_FILE"] {
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
// `GIT_CONFIG` affects only `git config`, unlike ordinary Git commands.
// Clearing it makes configuration queries match the bound operation.
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

pub trait CommandExt {
    fn bounded_output(&mut self) -> Result<std::process::Output>;
    fn success(&mut self) -> Result<()>;
    fn checked_output(&mut self) -> Result<std::process::Output>;
    fn checked_output_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<std::process::Output>;
}

pub(super) fn command_invocation(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| format!("{part:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl CommandExt for Command {
    fn bounded_output(&mut self) -> Result<std::process::Output> {
        local_output(self)
    }
    fn success(&mut self) -> Result<()> {
        let output = self.bounded_output()?;
        // Preserve the old status-entry output contract after bounded capture.
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        let status = output.status;
        if !status.success() {
            bail!("Command failed with status: {}", status);
        }
        Ok(())
    }

    fn checked_output(&mut self) -> Result<std::process::Output> {
        self.checked_output_with_timeout(std::time::Duration::from_secs(60))
    }

    fn checked_output_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<std::process::Output> {
        let output = crate::subprocess::local_output(self, None, timeout, 4 * 1024 * 1024)?;
        if !output.status.success() {
            let stderr = output.stderr.escape_ascii().to_string();
            let invocation = command_invocation(self);
            bail!("Command {invocation} failed with status: {}. Stderr: {stderr}", output.status,);
        }
        Ok(output)
    }
}

/// Local commands never consult a remote: one minute and four MiB of stdout.
/// Stderr retains the supervisor's final 16 KiB.
pub(crate) fn local_output(command: &mut Command) -> Result<std::process::Output> {
    Ok(crate::pre_push::subprocess::local_output(
        command,
        None,
        std::time::Duration::from_secs(60),
        4 * 1024 * 1024,
    )?)
}

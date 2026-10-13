use std::{path::PathBuf, process::Command};

use eyre::{Result, WrapErr, bail};
use gix::bstr::ByteSlice as _;

use super::{CommandExt as _, Repo, clear_git_repository_overrides, cmd};

impl Repo {
    pub fn config_string(&self, key: &str) -> Result<Option<String>> {
        let Some(cow) = self.inner.config_snapshot().string(key) else {
            return Ok(None);
        };
        let s = std::str::from_utf8(cow.as_ref())?;
        Ok(Some(s.trim().to_string()))
    }

    fn local_config_command(&self) -> Command {
        let mut command = cmd("git", ["config", "--local", "--no-includes"]);
        clear_git_repository_overrides(&mut command);
        command
            .env("GIT_DIR", self.inner.path())
            .env("GIT_COMMON_DIR", self.inner.common_dir())
            .current_dir(self.inner.workdir().unwrap_or(self.inner.path()));
        if let Some(worktree) = self.inner.workdir() {
            command.env("GIT_WORK_TREE", worktree);
        }
        command
    }

    /// Reads every interpreted byte value for a key from repository-local
    /// configuration only.
    ///
    /// Management ownership is established and repaired in this same scope.
    /// In particular, global values and included files cannot make GHerrit
    /// claim configuration which its local writes would not replace.
    pub(crate) fn local_config_values(&self, key: &str) -> Result<Vec<Vec<u8>>> {
        let mut command = self.local_config_command();
        command.args(["--null", "--get-all", key]);

        let output = command.output().wrap_err("Failed to inspect local Git configuration")?;
        match output.status.code() {
            Some(0) if output.stderr.is_empty() => {
                let Some(values) = output.stdout.strip_suffix(b"\0") else {
                    bail!("Git returned malformed local configuration");
                };
                Ok(values.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect())
            }
            Some(1) if output.stdout.is_empty() && output.stderr.is_empty() => Ok(Vec::new()),
            _ => bail!("Invalid local Git configuration"),
        }
    }

    pub(crate) fn replace_local_config(&self, key: &str, value: &str) -> Result<()> {
        self.local_config_command().args(["--replace-all", key, value]).success()
    }

    pub(crate) fn unset_local_config(&self, key: &str) -> Result<()> {
        self.local_config_command().args(["--unset-all", key]).success()
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
}

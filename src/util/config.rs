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

    fn effective_config_command(&self) -> Command {
        let mut command = cmd("git", ["config"]);
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

    fn local_config_command(&self) -> Command {
        let mut command = self.effective_config_command();
        command.args(["--local", "--no-includes"]);
        command
    }

    /// Return every interpreted value with its scope, in Git's order. A
    /// temporary command value must not hide a persistent worktree override.
    pub(crate) fn effective_config_values(&self, key: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let output = self
            .effective_config_command()
            .args(["--null", "--show-scope", "--get-all", key])
            .bounded_output()?;
        if output.status.code() == Some(1) && output.stdout.is_empty() {
            return Ok(Vec::new());
        }
        if !output.status.success() {
            bail!("Failed to observe effective Git configuration");
        }
        let bytes = output
            .stdout
            .strip_suffix(b"\0")
            .ok_or_else(|| eyre::eyre!("Git returned malformed effective configuration"))?;
        let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
        if fields.len() % 2 != 0 {
            bail!("Git returned malformed effective configuration");
        }
        fields
            .chunks_exact(2)
            .map(|pair| Ok((std::str::from_utf8(pair[0])?.to_owned(), pair[1].to_vec())))
            .collect()
    }

    /// Simulate local edits without touching the live config. Included files
    /// retain their ordering and relative paths; higher scopes are checked
    /// separately because a local write cannot override them.
    pub(crate) fn preview_local_config(
        &self,
        edits: &[(&str, Option<&str>)],
        keys: &[String],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        use std::io::Write as _;
        let mut preview = tempfile::NamedTempFile::new_in(self.inner.common_dir())?;
        preview.write_all(&std::fs::read(self.inner.common_dir().join("config"))?)?;
        preview.flush()?;
        let command = || {
            let mut command = self.effective_config_command();
            // A --file query models only the edited local file and includes.
            // Command-scope settings are checked against the live view.
            command.env_remove("GIT_CONFIG_PARAMETERS").env_remove("GIT_CONFIG_COUNT");
            command.arg("--file").arg(preview.path());
            command
        };
        // includeIf.hasconfig scans remote URLs from all scopes. Seed that
        // unchanged condition context into the private file so a global or
        // command URL cannot hide an active local include during the preview.
        let urls = self
            .effective_config_command()
            .args(["--null", "--get-regexp", r"^remote\..*\.url$"])
            .bounded_output()?;
        match urls.status.code() {
            Some(0) => {
                let records = urls
                    .stdout
                    .strip_suffix(b"\0")
                    .ok_or_else(|| eyre::eyre!("Malformed config URL context"))?;
                for record in records.split(|byte| *byte == 0) {
                    let split = record
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .ok_or_else(|| eyre::eyre!("Malformed config URL context"))?;
                    let (key, value) = (&record[..split], &record[split + 1..]);
                    command()
                        .args(["--no-includes", "--add"])
                        .arg(key.as_bstr().to_os_str()?)
                        .arg(value.as_bstr().to_os_str()?)
                        .success()?;
                }
            }
            Some(1) if urls.stdout.is_empty() => {}
            _ => bail!("Failed to inspect conditional include context"),
        }
        for (key, value) in edits {
            let mut edit = command();
            match value {
                Some(value) => {
                    edit.args(["--no-includes", "--replace-all", key, value]);
                }
                None => {
                    edit.args(["--no-includes", "--unset-all", key]);
                }
            }
            edit.success()?;
        }
        keys.iter()
            .map(|key| {
                let output =
                    command().args(["--includes", "--null", "--get", key]).bounded_output()?;
                match output.status.code() {
                    Some(0) => output
                        .stdout
                        .strip_suffix(b"\0")
                        .map(|value| Some(value.to_vec()))
                        .ok_or_else(|| eyre::eyre!("Malformed preview config value")),
                    Some(1) if output.stdout.is_empty() => Ok(None),
                    _ => bail!("Failed to inspect planned local Git configuration"),
                }
            })
            .collect()
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

        let output =
            command.bounded_output().wrap_err("Failed to inspect local Git configuration")?;
        match output.status.code() {
            Some(0) => {
                let Some(values) = output.stdout.strip_suffix(b"\0") else {
                    bail!("Git returned malformed local configuration");
                };
                Ok(values.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect())
            }
            Some(1) if output.stdout.is_empty() => Ok(Vec::new()),
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

/// Complete direct-local values for the four keys owned by branch management.
/// Empty vectors mean absence only after the one exact-key query completes.
pub(crate) struct BranchConfigValues {
    pub(crate) push_remote: Vec<Vec<u8>>,
    pub(crate) remote: Vec<Vec<u8>>,
    pub(crate) merge: Vec<Vec<u8>>,
    pub(crate) management: Vec<Vec<u8>>,
}

fn escape_config_subsection(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '.' | '^' | '$' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn decode_branch_config(branch: &str, output: &[u8]) -> Result<BranchConfigValues> {
    let mut result = BranchConfigValues {
        push_remote: Vec::new(),
        remote: Vec::new(),
        merge: Vec::new(),
        management: Vec::new(),
    };
    if output.is_empty() {
        return Ok(result);
    }
    let records = output
        .strip_suffix(b"\0")
        .ok_or_else(|| eyre::eyre!("Git returned malformed local configuration"))?;
    for record in records.split(|byte| *byte == 0) {
        let (key, value) = record
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or((record, b"".as_slice()), |index| (&record[..index], &record[index + 1..]));
        let prefix = format!("branch.{branch}.");
        let suffix = key
            .strip_prefix(prefix.as_bytes())
            .ok_or_else(|| eyre::eyre!("Git returned an unrequested local configuration key"))?;
        let values = match suffix {
            b"pushremote" => &mut result.push_remote,
            b"remote" => &mut result.remote,
            b"merge" => &mut result.merge,
            b"gherritmanaged" => &mut result.management,
            _ => bail!("Git returned an unrequested local configuration key"),
        };
        values.push(value.to_owned());
    }
    Ok(result)
}

impl Repo {
    /// Read management and owned branch fields in one interpreted occurrence
    /// stream, preserving local scope, duplicates, empty values, and raw bytes.
    pub(crate) fn local_branch_config(&self, branch: &str) -> Result<BranchConfigValues> {
        let branch_pattern = escape_config_subsection(branch);
        let pattern =
            format!(r"^branch\.{branch_pattern}\.(pushremote|remote|merge|gherritmanaged)$");
        let output = self
            .local_config_command()
            .args(["--null", "--get-regexp", &pattern])
            .bounded_output()
            .wrap_err("Failed to inspect local Git configuration")?;
        match output.status.code() {
            Some(0) => decode_branch_config(branch, &output.stdout),
            Some(1) if output.stdout.is_empty() => decode_branch_config(branch, b""),
            _ => bail!("Invalid local Git configuration"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_matches_independent_git_reads_for_exact_case_sensitive_subsections() {
        let context = testutil::TestContextBuilder::new("unused").build();
        let branch = "Case.sensitive+(name){value}";
        for (suffix, value) in [
            ("pushRemote", "."),
            ("remote", ""),
            ("merge", "line\none"),
            ("gherritManaged", "managedPrivate"),
        ] {
            context.run_git(&["config", "--add", &format!("branch.{branch}.{suffix}"), value]);
        }
        context.run_git(&["config", "--add", &format!("branch.{branch}.remote"), "duplicate"]);
        context.run_git(&["config", "branch.case.sensitive+(name){value}.remote", "unrequested"]);
        let repository = Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let values = repository.local_branch_config(branch).unwrap();
        for (suffix, actual) in [
            ("pushRemote", values.push_remote),
            ("remote", values.remote),
            ("merge", values.merge),
            ("gherritManaged", values.management),
        ] {
            assert_eq!(
                actual,
                repository.local_config_values(&format!("branch.{branch}.{suffix}")).unwrap()
            );
        }
        let missing = repository.local_branch_config("absent").unwrap();
        assert!(missing.management.is_empty() && missing.remote.is_empty());
    }

    #[test]
    fn snapshot_decoder_retains_implicit_empty_and_non_utf8_values_and_rejects_partial_records() {
        let values = decode_branch_config(
            "a",
            b"branch.a.remote\0branch.a.remote\n\xff\0branch.a.merge\nline\none\0",
        )
        .unwrap();
        assert_eq!(values.remote, [Vec::new(), vec![0xff]]);
        assert_eq!(values.merge, [b"line\none".to_vec()]);
        for invalid in [
            b"branch.a.remote\nvalue".as_slice(),
            b"branch.other.remote\nvalue\0",
            b"branch.a.unrequested\nvalue\0",
            b"\0",
        ] {
            assert!(decode_branch_config("a", invalid).is_err(), "{invalid:?}");
        }
    }
}

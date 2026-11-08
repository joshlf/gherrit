//! Pushes Git batches and saves the selected local version refs.
//!
//! The child process and its diagnostics stay with the code that starts Git.
//! `publication` builds the command plan. This module reads branch tips, runs
//! Git, checks its exit status, and writes local refs. Earlier successful
//! batches may remain published if anything later fails.

use std::{collections::HashMap, process::Stdio};

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::refs::transaction::PreviousValue;

use super::{
    collect::Commit,
    identity::ChangeId,
    publication::{PushTarget, Version, plan_push, push_batches},
    refs,
    remote::observe_managed_branches,
};
use crate::{re, util};

#[allow(clippy::too_many_lines)]
pub(super) fn push_to_origin(
    repo: &util::Repo,
    commits: Vec<Commit>,
) -> Result<Vec<VersionedCommit>> {
    let gherrit_ids: Vec<ChangeId> = commits.iter().map(|c| c.gherrit_id().clone()).collect();

    // Read branch tips for the leases. ls-remote fetches neither objects nor
    // version tags, and its results may become stale. Git must check each
    // branch's expected value when applying the update.
    let remote_branch_states = observe_managed_branches(repo, &gherrit_ids)?;

    // Read local versions once for this attempt. Detect read failures or
    // exhausted version numbers before pushing any batch.
    let local_versions = get_local_versions(repo, &gherrit_ids)
        .wrap_err("Failed to read local version refs; no publication batch was pushed")?;
    let commits = commits
        .into_iter()
        .map(|commit| {
            let id = commit.gherrit_id();
            let version = Version::next_after(local_versions[id]).ok_or_else(|| {
                eyre!("Local version counter exhausted for '{id}'; no publication batch was pushed")
            })?;
            Ok(VersionedCommit { commit, version })
        })
        .collect::<Result<Vec<_>>>()?;

    // Each chunk is a separate Git transaction. Save local version refs after
    // it succeeds. A later chunk may fail while earlier chunks stay published.
    // Version numbers come from local tags; reading remote branch tips above
    // does not repair missing or stale local history.
    for chunk in push_batches(&commits) {
        let mut targets = Vec::with_capacity(chunk.len());

        for entry in chunk {
            let c = &entry.commit;

            // Keep a missing tip distinct from a parsed object ID until
            // building the command, where Git uses an empty lease value for a
            // new branch.
            let expected_remote_tip = remote_branch_states.get(c.gherrit_id()).copied();

            targets.push(PushTarget {
                object_id: c.id(),
                gherrit_id: c.gherrit_id(),
                version: entry.version,
                expected_remote_tip,
            });
        }

        let plan = plan_push(&repo.default_remote_name()?, &targets);

        log::info!("Pushing chunk to remote...");
        let mut child = util::cmd("git", plan.arguments)
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err("Failed to run `git push`")?;

        // Keep Git's diagnostics except the invitation to create a PR, since
        // this command creates the PR itself. Buffer consecutive remote: lines
        // so the filter can recognize an invitation that spans several lines.
        let status = finish_push(&mut child, std::io::stderr().lock())?;
        if !status.success() {
            // A lease failure is a likely cause: another writer may have
            // changed the branch or version tag.
            let r = repo.default_remote_name()?;
            bail!(
                "`git push` failed. The remote might be ahead or changed. Run `git fetch {r}` to sync."
            );
        }

        // The remote batch has already succeeded. Try every local ref and
        // report failed local writes separately from a push that never took
        // effect.
        let mut persistence_errors = Vec::new();
        for tag in plan.persisted_tags {
            let name = refs::version_tag(tag.gherrit_id.as_str(), tag.version.get());
            if let Err(error) = repo.reference(
                name.clone(),
                tag.object_id,
                PreviousValue::Any,
                "gherrit: persist local version state",
            ) {
                persistence_errors.push(format!("{name}: {error}"));
            }
        }
        if !persistence_errors.is_empty() {
            bail!(
                "Remote publication batch succeeded, but local version refs were not all persisted: {}. Remote changes are already applied; inspect and fetch version refs before retrying.",
                persistence_errors.join("; ")
            );
        }
    }

    Ok(commits)
}

fn without_pr_invitation(block: &str) -> std::borrow::Cow<'_, str> {
    re!(r"(?m)\n?^remote:\s*\nremote: Create a pull request for '.*' on GitHub by visiting:\s*\nremote:\s*https://github\.com/.*\nremote:\s*$").replace(block, "")
}

/// Waits for Git after consuming or dropping its diagnostics pipe.
///
/// The function waits even if forwarding diagnostics fails. A nonzero exit
/// status is returned to the caller rather than treated as an error here.
///
/// # Errors
///
/// Returns an error if forwarding diagnostics or waiting for Git fails. Once
/// Git starts, an I/O error does not mean the push made no remote changes.
fn finish_push(
    child: &mut std::process::Child,
    output: impl std::io::Write,
) -> Result<std::process::ExitStatus> {
    let diagnostics = match child.stderr.take() {
        Some(stderr) => relay_push_diagnostics(std::io::BufReader::new(stderr), output),
        None => Err(std::io::Error::other("Git push stderr pipe is unavailable")),
    };
    complete_push(child, diagnostics)
}

// Only process completion varies here. Pipe ownership and forwarding stay in
// finish_push, so cleanup cannot accidentally keep a diagnostics reader alive.
trait PushProcess {
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus>;
    fn kill(&mut self) -> std::io::Result<()>;
}

impl PushProcess for std::process::Child {
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.wait()
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.kill()
    }
}

fn complete_push(
    child: &mut impl PushProcess,
    diagnostics: std::io::Result<()>,
) -> Result<std::process::ExitStatus> {
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            // Try to clean up after waiting fails. The remote result is still
            // unknown even if killing Git and waiting again both succeed.
            let termination = child.kill();
            let reaping = child.wait();
            bail!(
                "Failed to wait for git push: {error}; termination: {termination:?}; reaping: {reaping:?}. Remote changes may have applied; inspect remote refs before retrying."
            );
        }
    };
    diagnostics.wrap_err_with(|| format!("Failed to relay git push diagnostics (child status {status}). Remote changes may have applied; inspect remote refs before retrying."))?;
    Ok(status)
}

fn relay_push_diagnostics(
    mut reader: impl std::io::BufRead,
    mut output: impl std::io::Write,
) -> std::io::Result<()> {
    fn flush(buffer: &mut Vec<String>, output: &mut impl std::io::Write) -> std::io::Result<()> {
        if !buffer.is_empty() {
            let block = buffer.join("\n");
            let cleaned = without_pr_invitation(&block);
            if !cleaned.is_empty() {
                writeln!(output, "{cleaned}")?;
            }
            buffer.clear();
        }
        Ok(())
    }
    let result = (|| {
        let mut buffer = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            // Match BufRead::lines for text, including CRLF and a final line
            // with no newline. Forward non-UTF-8 diagnostics as bytes.
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            match std::str::from_utf8(&line) {
                Ok(text) if text.trim_start().starts_with("remote:") => {
                    buffer.push(text.to_owned())
                }
                _ => {
                    flush(&mut buffer, &mut output)?;
                    output.write_all(&line)?;
                    output.write_all(b"\n")?;
                }
            }
        }
        flush(&mut buffer, &mut output)
    })();
    if result.is_err() {
        // Do not leave Git blocked on stderr when the output stream fails. If
        // reading fails too, dropping our pipe closes it.
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    }
    result
}

fn local_version_entry(name: &str) -> Option<(&str, usize)> {
    let tail = name.strip_prefix("refs/tags/gherrit/")?;
    let (id, version) = tail.split_once("/v")?;
    // Keep parsing the number after the last 'v', including unusual existing
    // spellings. Changing which archive names are accepted is a separate
    // decision.
    let version = version.rsplit('v').next()?.parse().ok()?;
    Some((id, version))
}

fn get_local_versions(repo: &util::Repo, ids: &[ChangeId]) -> Result<HashMap<ChangeId, usize>> {
    // An ID with no recognized local tag starts at zero. Scan refs once for the
    // whole stack. Stop on decoding or iteration errors so incomplete history
    // cannot silently produce a lower version number.
    let mut versions = ids.iter().map(|id| (id.clone(), 0)).collect::<HashMap<_, _>>();
    let references = repo.references().map_err(|error| eyre!(error))?;
    for reference in references.all().map_err(|error| eyre!(error))? {
        let reference = reference.map_err(|error| eyre!(error))?;
        let name = reference.name().as_bstr().to_string();
        if let Some((id, version)) = local_version_entry(&name)
            && let Some(current) = versions.get_mut(id)
        {
            *current = (*current).max(version);
        }
    }
    Ok(versions)
}

/// A commit paired with its selected version.
///
/// The pair stays together through the push and PR rendering, so the version
/// does not need to be looked up again by ID.
pub(super) struct VersionedCommit {
    commit: Commit,
    version: Version,
}

impl VersionedCommit {
    // Other modules can read this pair but cannot construct or change it. They
    // receive it after the Git batches and local writes succeed. This records
    // those steps' success, not a fresh check or lock of remote refs.
    pub(super) fn commit(&self) -> &Commit {
        &self.commit
    }

    pub(super) fn version(&self) -> Version {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailedOutput;

    impl std::io::Write for FailedOutput {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn push_relay_preserves_text_and_non_utf8_diagnostics() {
        let input = b"remote: server warning\r\nlocal diagnostic\r\nremote: \xff\nlast";
        let mut output = Vec::new();
        relay_push_diagnostics(std::io::Cursor::new(input), &mut output).unwrap();
        assert_eq!(output, b"remote: server warning\nlocal diagnostic\nremote: \xff\nlast\n");
    }

    #[test]
    fn push_relay_drains_after_output_or_read_failure() {
        let mut reader = std::io::Cursor::new(b"first\nsecond\nlast\n");
        assert!(relay_push_diagnostics(&mut reader, FailedOutput).is_err());
        assert_eq!(reader.position(), reader.get_ref().len() as u64);

        struct ReadFailure {
            reader: std::io::Cursor<Vec<u8>>,
            fail: bool,
        }
        impl std::io::Read for ReadFailure {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.reader.read(bytes)
            }
        }
        impl std::io::BufRead for ReadFailure {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                if std::mem::take(&mut self.fail) {
                    return Err(std::io::Error::other("fixture read failure"));
                }
                self.reader.fill_buf()
            }
            fn consume(&mut self, n: usize) {
                self.reader.consume(n);
            }
        }
        let mut reader =
            ReadFailure { reader: std::io::Cursor::new(b"remaining bytes".to_vec()), fail: true };
        let error = relay_push_diagnostics(&mut reader, Vec::new()).unwrap_err();
        assert_eq!(error.to_string(), "fixture read failure");
        assert_eq!(reader.reader.position(), reader.reader.get_ref().len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn push_child_is_drained_and_reaped_after_output_failure() {
        let mut child = std::process::Command::new("sh")
            .env_clear()
            .args(["-c", "i=0; while [ $i -lt 10000 ]; do echo diagnostic >&2; i=$((i+1)); done"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let error = finish_push(&mut child, FailedOutput).unwrap_err();
        assert!(format!("{error:#}").contains("Remote changes may have applied"));
        assert!(child.try_wait().unwrap().unwrap().success());

        let mut child = std::process::Command::new("sh")
            .env_clear()
            .args(["-c", "exit 3"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(finish_push(&mut child, Vec::new()).unwrap().code(), Some(3));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn local_version_index_retains_legacy_numeric_suffixes() {
        for (name, version) in [("v2", 2), ("v0002", 2), ("v+2", 2), ("vjunkv3", 3)] {
            let full = format!("refs/tags/gherrit/Gcase/{name}");
            assert_eq!(local_version_entry(&full), Some(("Gcase", version)));
        }
        for name in ["refs/heads/Gcase", "refs/tags/gherrit/Gcase/vno", "refs/tags/other/v2"] {
            assert!(local_version_entry(name).is_none());
        }
    }

    #[test]
    fn push_diagnostics_only_remove_the_complete_github_invitation() {
        let invitation = "remote: \nremote: Create a pull request for 'G1' on GitHub by visiting:\nremote: https://github.com/owner/repo/pull/new/G1\nremote: ";
        assert_eq!(without_pr_invitation(invitation), "");
        assert_eq!(
            without_pr_invitation(&format!("remote: warning\n{invitation}")),
            "remote: warning"
        );
        for ordinary in
            ["remote: warning", "fatal: push failed", "remote: https://example.com/path"]
        {
            assert_eq!(without_pr_invitation(ordinary), ordinary);
        }
    }

    #[cfg(unix)]
    #[test]
    fn push_completion_preserves_status_and_cleans_up_every_wait_failure() {
        use std::os::unix::process::ExitStatusExt as _;

        struct Process {
            events: Vec<&'static str>,
            first_wait_fails: bool,
            kill_fails: bool,
            reap_fails: bool,
            code: i32,
        }
        impl PushProcess for Process {
            fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
                let first = self.events.is_empty();
                self.events.push("wait");
                if (first && self.first_wait_fails) || (!first && self.reap_fails) {
                    Err(std::io::Error::other(if first { "initial wait" } else { "cleanup wait" }))
                } else {
                    Ok(std::process::ExitStatus::from_raw(self.code << 8))
                }
            }
            fn kill(&mut self) -> std::io::Result<()> {
                self.events.push("kill");
                if self.kill_fails { Err(std::io::Error::other("cleanup kill")) } else { Ok(()) }
            }
        }
        for code in [0, 3] {
            for diagnostics_fail in [false, true] {
                for first_wait_fails in [false, true] {
                    for kill_fails in [false, true] {
                        for reap_fails in [false, true] {
                            let mut process = Process {
                                events: Vec::new(),
                                first_wait_fails,
                                kill_fails,
                                reap_fails,
                                code,
                            };
                            let diagnostics = if diagnostics_fail {
                                Err(std::io::Error::other("relay error"))
                            } else {
                                Ok(())
                            };
                            let result = complete_push(&mut process, diagnostics);
                            if first_wait_fails {
                                assert_eq!(process.events, ["wait", "kill", "wait"]);
                                let error = format!("{:#}", result.unwrap_err());
                                assert!(
                                    error.contains("Failed to wait for git push: initial wait")
                                );
                                assert_eq!(error.contains("cleanup kill"), kill_fails);
                                assert_eq!(error.contains("cleanup wait"), reap_fails);
                                assert!(error.contains("Remote changes may have applied"));
                            } else {
                                assert_eq!(process.events, ["wait"]);
                                match result {
                                    Ok(status) => {
                                        assert!(!diagnostics_fail);
                                        assert_eq!(status.code(), Some(code));
                                    }
                                    Err(error) => {
                                        assert!(diagnostics_fail);
                                        let text = format!("{error:#}");
                                        assert!(text.contains("relay error"));
                                        assert!(text.contains("child status"));
                                        assert!(text.contains("Remote changes may have applied"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_push_pipe_still_reaps_the_child() {
        let mut child = std::process::Command::new("sh")
            .env_clear()
            .args(["-c", "exit 3"])
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let error = format!("{:#}", finish_push(&mut child, Vec::new()).unwrap_err());
        assert!(error.contains("Git push stderr pipe is unavailable"));
        assert!(error.contains("Remote changes may have applied"));
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(3));
    }

    #[test]
    fn push_relay_covers_buffer_flush_and_newline_write_failures() {
        struct Output {
            writes_left: usize,
        }
        impl std::io::Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.writes_left == 0 {
                    return Err(std::io::Error::other("output failure"));
                }
                self.writes_left -= 1;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for input in [
            b"local\nremaining\n".as_slice(),
            b"remote: warning\nlocal\nremaining\n",
            b"remote: warning\n",
        ] {
            for writes_left in [0, 1] {
                let mut reader = std::io::Cursor::new(input);
                let result = relay_push_diagnostics(&mut reader, Output { writes_left });
                assert_eq!(result.unwrap_err().to_string(), "output failure");
                assert_eq!(reader.position(), input.len() as u64);
            }
        }
        let mut output = Vec::new();
        relay_push_diagnostics(std::io::Cursor::new(b"\nremote: \nremote: Create a pull request for 'Ga' on GitHub by visiting:\nremote: https://github.com/o/r/pull/new/Ga\nremote: \nlast\r"), &mut output).unwrap();
        assert_eq!(output, b"\nlast\r\n");
    }
}

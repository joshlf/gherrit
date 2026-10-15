//! Git fixture.

use super::*;

pub(super) fn deterministic_gherrit_id(mut sequence: u64) -> String {
    const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut encoded = [BASE32[0]; 32];
    encoded.iter_mut().rev().for_each(|digit| {
        *digit = BASE32[(sequence % BASE32.len() as u64) as usize];
        sequence /= BASE32.len() as u64;
    });
    assert_eq!(sequence, 0, "GHerrit ID sequence exceeds its 32-digit encoding");

    format!("G{}", core::str::from_utf8(&encoded).expect("base32 alphabet is UTF-8"))
}

pub(super) fn run_git_cmd(
    environment: &TestEnvironment,
    system_git: &Path,
    path: &Path,
    args: &[&str],
) {
    environment.command(system_git).current_dir(path).args(args).assert().success();
}

#[cfg(unix)]
pub(super) fn install_git_interceptor(path: &Path, _gherrit_bin: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let git = path.join("git");
    fs::write(&git, "#!/bin/sh\nexec \"$GHERRIT_TEST_DRIVER\" __test-git \"$@\"\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
pub(super) fn install_git_interceptor(path: &Path, gherrit_bin: &Path) {
    // `Command::new("git")` resolves native executables without consulting
    // PATHEXT, so a batch-file wrapper named `git.cmd` would be bypassed.
    hard_link_or_copy(gherrit_bin, &path.join("git.exe"));
}

pub(super) fn install_gherrit_binary(path: &Path, gherrit_bin: &Path) {
    let gherrit_dst = path.join(if cfg!(windows) { "gherrit.exe" } else { "gherrit" });
    hard_link_or_copy(gherrit_bin, &gherrit_dst);
}

/// Installs a test executable without copying a large Cargo artifact when the
/// fixture and target directory share a filesystem.
pub(super) fn hard_link_or_copy(source: &Path, destination: &Path) {
    if let Err(link_error) = fs::hard_link(source, destination) {
        fs::copy(source, destination).unwrap_or_else(|copy_error| {
            panic!(
                "failed to install test executable {:?}: hard link failed ({link_error}); \
                 copy fallback failed ({copy_error})",
                destination
            )
        });
    }
}

pub(super) fn init_git_bare_repo(environment: &TestEnvironment, system_git: &Path, path: &Path) {
    fs::create_dir(path).unwrap();
    run_git_cmd(environment, system_git, path, &["init", "--bare"]);
    run_git_cmd(environment, system_git, path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
}

pub(super) fn init_git_repo(
    environment: &TestEnvironment,
    system_git: &Path,
    path: &Path,
    remote_path: Option<&Path>,
) {
    let run = |args| run_git_cmd(environment, system_git, path, args);
    run(&["init"]);
    run(&["config", "core.hooksPath", ".git/hooks"]);
    // Pin both the actual branch and the configuration consulted by default-
    // branch discovery. Ambient Git defaults must not choose fixture topology.
    run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    run(&["config", "init.defaultBranch", "main"]);
    // Explicitly unmanage main to satisfy strict config checks
    run(&["config", "branch.main.gherritManaged", "false"]);
    if let Some(remote_path) = remote_path {
        let remote_url = remote_path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
        run(&["remote", "add", "origin", &remote_url]);
    }
}

use crate::cmd;
use crate::util::{self, BranchError, CommandExt, ResultExt as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Managed,
    Unmanaged,
}

pub fn get_state(
    repo: &gix::Repository,
    branch_name: &str,
) -> Result<Option<State>, gix::config::value::Error> {
    let key = format!("branch.{}.gherritManaged", branch_name);
    match util::get_config_bool(repo, &key)? {
        Some(true) => Ok(Some(State::Managed)),
        Some(false) => Ok(Some(State::Unmanaged)),
        None => Ok(None),
    }
}

pub fn set_state(state: State) {
    let (_, branch_name) =
        util::get_current_branch().unwrap_or_exit("Failed to get current branch");

    cmd!(
        "git config",
        "branch.{branch_name}.gherritManaged",
        match state {
            State::Managed => "true",
            State::Unmanaged => "false",
        }
    )
    .unwrap_status();
    match state {
        State::Managed => {
            // Set pushRemote to "." (current directory).
            // This makes `git push` a local no-op.
            // Result:
            // 1. `pre-push` hook runs and syncs to GitHub (exit 0).
            // 2. Git pushes to `.` (succeeds instantly with no effect).
            // 3. User sees success, and `origin/branch` is NOT updated
            //    (Private).
            cmd!("git config", "branch.{branch_name}.pushRemote", ".").unwrap_status();

            log::info!("Branch '{branch_name}' is now managed by GHerrit.");
            log::info!("  - 'git push' is now configured to sync your stack WITHOUT updating 'origin/{branch_name}'");
            log::info!("  - To allow pushing this branch to origin (making it public), run:");
            log::info!("    git config --unset branch.{branch_name}.pushRemote");
        }
        State::Unmanaged => {
            // Remove the pushRemote override to restore standard Git behavior.
            // Use `.unwrap_status()` and ignore errors (in case the config key
            // doesn't exist).
            let _ = cmd!("git config --unset", "branch.{branch_name}.pushRemote").unwrap_status();

            log::info!("Branch '{branch_name}' is now unmanaged by GHerrit.");
            log::info!("  - Standard 'git push' behavior has been restored.");
        }
    }
}

pub fn post_checkout(_prev: &str, _new: &str, flag: &str) {
    // Only run on branch switches (flag=1)
    if flag != "1" {
        return;
    }

    let (repo, branch_name) = match util::get_current_branch() {
        Ok(res) => res,
        Err(BranchError::DetachedHead) => return, // Detached HEAD (e.g. during rebase); do nothing.
        Err(e) => {
            log::error!("Failed to get current branch: {}", e);
            return;
        }
    };

    // Idempotency check: Bail if the branch management state is already set.
    if get_state(&repo, &branch_name)
        .unwrap_or_exit("Failed to parse gherritState")
        .is_some()
    {
        log::debug!("Branch '{}' is already configured.", branch_name);
        return;
    }

    // Creation detection: Bail if we're just checking out an already-existing branch.
    let is_new = util::is_newly_created_branch(&repo, &branch_name)
        .unwrap_or_exit("Failed to check if branch is new");
    if !is_new {
        log::debug!("Branch '{}' is not newly created.", branch_name);
        return;
    }

    let upstream_remote = util::get_config_string(&repo, &format!("branch.{branch_name}.remote"))
        .unwrap_or_exit("Failed to read config");
    let upstream_merge = util::get_config_string(&repo, &format!("branch.{branch_name}.merge"))
        .unwrap_or_exit("Failed to read config");

    let has_upstream = upstream_remote.is_some() && upstream_merge.is_some();
    let is_origin_main = if has_upstream {
        let remote = upstream_remote.as_deref().unwrap_or("");
        let merge = upstream_merge.as_deref().unwrap_or("");
        remote == "origin" && merge == "refs/heads/main"
    } else {
        false
    };

    if has_upstream && !is_origin_main {
        // Condition A: Shared Branch
        set_state(State::Unmanaged);
        log::info!("Branch initialized as UNMANAGED (Collaboration Mode).");
    } else {
        // Condition B: New Stack
        set_state(State::Managed);
        log::info!("Branch initialized as MANAGED (Stack Mode).");
        log::info!("To opt-out, run: gherrit unmanage");
    }
}

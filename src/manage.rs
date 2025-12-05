use crate::cmd;
use crate::util::{self, CommandExt, HeadState, ResultExt as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Managed,
    Unmanaged,
}

pub fn get_state(
    repo: &util::Repo,
    branch_name: &str,
) -> Result<Option<State>, gix::config::value::Error> {
    let key = format!("branch.{}.gherritManaged", branch_name);
    match util::get_config_bool(repo, &key)? {
        Some(true) => Ok(Some(State::Managed)),
        Some(false) => Ok(Some(State::Unmanaged)),
        None => Ok(None),
    }
}

/// Configures the Git branch state for GHerrit management.
///
/// Sets/unsets the following config values:
/// - `branch.<name>.gherritManaged` (boolean): Indicates whether the branch is
///   managed by GHerrit.
/// - `branch.<name>.pushRemote` (string): Set to "." when managed, unset when
///   unmanaged. Causes `git push` to be a no-op by pushing to the local
///   repository instead of pushing to the remote repository.
/// - `branch.<name>.remote` (string)/`branch.<name>.merge` (string): Set to
///   "."/"refs/heads/<branch>" when managed, unset when unmanaged. Satisfies
///   Git's requirement that an upstream branch be set to suppress "fatal: The
///   current branch has no upstream branch" errors.
pub fn set_state(repo: &util::Repo, state: State) {
    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Rebasing(bn) => bn,
        HeadState::Detached => {
            log::error!("Cannot set state for detached HEAD");
            std::process::exit(1);
        }
    };

    let key_managed = format!("branch.{branch_name}.gherritManaged");
    let key_push_remote = format!("branch.{branch_name}.pushRemote");
    let key_remote = format!("branch.{branch_name}.remote");
    let key_merge = format!("branch.{branch_name}.merge");
    let self_merge_ref = format!("refs/heads/{branch_name}");

    match state {
        State::Managed => {
            cmd!("git config", key_managed, "true").unwrap_status();
            cmd!("git config", key_push_remote, ".").unwrap_status();
            cmd!("git config", key_remote, ".").unwrap_status();
            cmd!("git config", key_merge, &self_merge_ref).unwrap_status();

            log::info!("Branch '{branch_name}' is now managed by GHerrit.");
            log::info!("  - 'git push' is configured to sync your stack WITHOUT updating 'origin/{branch_name}'.");
            log::info!("  - To allow pushing this branch to origin (making it public), run:");
            log::info!("    git config branch.{branch_name}.pushRemote origin");
        }
        State::Unmanaged => {
            cmd!("git config", key_managed, "false").unwrap_status();
            cmd!("git config --unset", key_push_remote).unwrap_status();

            let current_remote = util::get_config_string(repo, &key_remote).unwrap_or(None);
            let current_merge = util::get_config_string(repo, &key_merge).unwrap_or(None);

            if current_remote.as_deref() == Some(".")
                && current_merge.as_deref() == Some(&self_merge_ref)
            {
                cmd!("git config --unset", key_remote).unwrap_status();
                cmd!("git config --unset", key_merge).unwrap_status();
                log::info!("  - Removed local self-tracking configuration.");
            }

            log::info!("Branch '{branch_name}' is now unmanaged by GHerrit.");
            log::info!("  - Standard 'git push' behavior has been restored.");
            log::info!("  - Local self-tracking removed. You may need to set a new upstream (e.g., git push -u origin {branch_name}).");
        }
    }
}

pub fn post_checkout(repo: &util::Repo, _prev: &str, _new: &str, flag: &str) {
    // Only run on branch switches (flag=1)
    if flag != "1" {
        return;
    }

    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) => bn,
        HeadState::Rebasing(_) | HeadState::Detached => return,
    };

    // Idempotency check: Bail if the branch management state is already set.
    if get_state(repo, branch_name)
        .unwrap_or_exit("Failed to parse gherritState")
        .is_some()
    {
        log::debug!("Branch '{}' is already configured.", branch_name);
        return;
    }

    // Creation detection: Bail if we're just checking out an already-existing branch.
    let is_new = repo
        .is_newly_created_branch(branch_name)
        .unwrap_or_exit("Failed to check if branch is new");
    if !is_new {
        log::debug!("Branch '{}' is not newly created.", branch_name);
        return;
    }

    let upstream_remote = util::get_config_string(repo, &format!("branch.{branch_name}.remote"))
        .unwrap_or_exit("Failed to read config");
    let upstream_merge = util::get_config_string(repo, &format!("branch.{branch_name}.merge"))
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
        set_state(repo, State::Unmanaged);
        log::info!("Branch initialized as UNMANAGED (Collaboration Mode).");
    } else {
        // Condition B: New Stack
        set_state(repo, State::Managed);
        log::info!("Branch initialized as MANAGED (Stack Mode).");
        log::info!("To opt-out, run: gherrit unmanage");
    }
}

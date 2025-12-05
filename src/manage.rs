use crate::cmd;
use crate::util::{self, HeadState};
use eyre::{bail, Result, WrapErr};
use owo_colors::OwoColorize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Managed,
    Unmanaged,
}

pub fn get_state(repo: &util::Repo, branch_name: &str) -> Result<Option<State>> {
    let key = format!("branch.{}.gherritManaged", branch_name);
    match repo.config_bool(&key)? {
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
pub fn set_state(repo: &util::Repo, state: State) -> Result<()> {
    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Pending(bn) => bn,
        HeadState::Detached => {
            bail!("Cannot set state for detached HEAD");
        }
    };

    let key = |suffix: &str| format!("branch.{branch_name}.{suffix}");
    let self_merge_ref = format!("refs/heads/{branch_name}");
    match state {
        State::Managed => {
            cmd!("git config", key("gherritManaged"), "true").status()?;
            cmd!("git config", key("gherritManaged"), "true").status()?;

            let current_push_remote = repo.config_string(&key("pushRemote"))?;
            let custom_push_remote = match current_push_remote.as_deref() {
                Some(".") => None, // Already set to "."; nothing to do.
                Some(remote) => Some(remote),
                None => {
                    cmd!("git config", key("pushRemote"), ".").status()?;
                    None
                }
            };

            cmd!("git config", key("remote"), ".").status()?;
            cmd!("git config", key("merge"), &self_merge_ref).status()?;

            let branch_name_yellow = branch_name.yellow();
            log::info!(
                "Branch '{branch_name_yellow}' is now {} by GHerrit.",
                "managed".green()
            );
            if let Some(remote) = custom_push_remote {
                let remote_yellow = remote.yellow();
                log::warn!("Branch '{branch_name_yellow}' has a custom pushRemote '{remote_yellow}'. GHerrit did NOT overwrite it.");
                log::warn!("  - Running `git push` will push to '{remote_yellow}' in addition to syncing via GHerrit.");
                log::warn!("  - To configure GHerrit to sync your stack WITHOUT pushing to origin (making it private), run:");
                log::warn!("    git config {} .", key("pushRemote"));
                log::warn!("  - To allow pushing this branch to origin (making it public), run:");
                log::warn!("    git config {} origin", key("pushRemote"));
            } else {
                log::info!("  - 'git push' is configured to sync your stack WITHOUT updating 'origin/{branch_name}'.");
                log::info!("  - To allow pushing this branch to origin (making it public), run:");
                log::info!("    git config {} origin", key("pushRemote"));
            }
        }
        State::Unmanaged => {
            cmd!("git config", key("gherritManaged"), "false").status()?;
            cmd!("git config", key("gherritManaged"), "false").status()?;

            let current_push_remote = repo.config_string(&key("pushRemote"))?;
            let custom_push_remote = match current_push_remote.as_deref() {
                Some(".") => {
                    cmd!("git config", key("pushRemote"), ".").status()?;
                    None
                }
                Some(remote) => Some(remote),
                None => None, // Already unset; nothing to do.
            };

            let current_remote = repo.config_string(&key("remote"))?;
            let current_merge = repo.config_string(&key("merge"))?;

            if current_remote.as_deref() == Some(".")
                && current_merge.as_deref() == Some(&self_merge_ref)
            {
                cmd!("git config --unset", key("remote")).status()?;
                cmd!("git config --unset", key("merge")).status()?;
            }

            let branch_name_yellow = branch_name.yellow();
            log::info!(
                "Branch '{branch_name_yellow}' is now {} by GHerrit.",
                "unmanaged".red()
            );
            log::info!("  - Standard 'git push' behavior has been restored.");
            if let Some(remote) = custom_push_remote {
                log::warn!("Branch '{branch_name_yellow}' has a custom pushRemote '{remote}'. GHerrit did NOT unset it.");
            } else {
                log::info!("  - Local self-tracking removed. You may need to set a new upstream (e.g., git push -u origin {branch_name}).");
            }
        }
    }
    Ok(())
}

pub fn post_checkout(repo: &util::Repo, _prev: &str, _new: &str, flag: &str) -> Result<()> {
    // Only run on branch switches (flag=1)
    if flag != "1" {
        return Ok(());
    }

    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) => bn,
        HeadState::Pending(_) | HeadState::Detached => return Ok(()),
    };

    // Idempotency check: Bail if the branch management state is already set.
    if get_state(repo, branch_name)
        .wrap_err("Failed to parse gherritState")?
        .is_some()
    {
        log::debug!(" Branch '{}' is already configured.", branch_name);
        return Ok(());
    }

    // Creation detection: Bail if we're just checking out an already-existing branch.
    let is_new = repo
        .is_newly_created_branch(branch_name)
        .wrap_err("Failed to check if branch is new")?;
    if !is_new {
        log::debug!(" Branch '{}' is not newly created.", branch_name);
        return Ok(());
    }

    let upstream_remote = repo
        .config_string(&format!("branch.{branch_name}.remote"))
        .wrap_err("Failed to read config")?;
    let upstream_merge = repo
        .config_string(&format!("branch.{branch_name}.merge"))
        .wrap_err("Failed to read config")?;

    let is_default_branch = if let (Some(remote), Some(merge)) =
        (upstream_remote.as_deref(), upstream_merge.as_deref())
    {
        let branch_name = merge.strip_prefix("refs/heads/").unwrap_or(merge);
        ({
            // Check if the upstream matches the remote's HEAD (e.g. origin/HEAD
            // -> origin/main)
            let expected_target = format!("refs/remotes/{remote}/{branch_name}");
            let remote_head_ref = format!("refs/remotes/{remote}/HEAD");
            repo.find_reference(&remote_head_ref)
                .ok()
                .and_then(|r| r.target().try_name().map(|n| n.to_string()))
                .is_some_and(|target| target == expected_target)
        }) || {
            // Fallback: Check config or standard conventions
            let configured_default = repo.config_string("init.defaultBranch").ok().flatten();
            configured_default.as_deref() == Some(branch_name)
                || matches!(branch_name, "main" | "master" | "trunk")
        }
    } else {
        false
    };

    let has_upstream = upstream_remote.is_some() && upstream_merge.is_some();
    if has_upstream && !is_default_branch {
        // Condition A: Shared Branch
        set_state(repo, State::Unmanaged)?;
        log::info!(
            "Branch initialized as {}.",
            "UNMANAGED (Collaboration Mode)".yellow()
        );
    } else {
        // Condition B: New Stack
        set_state(repo, State::Managed)?;
        log::info!("Branch initialized as {}.", "MANAGED (Stack Mode)".green());
        log::info!("To opt-out, run: gherrit unmanage");
    }

    Ok(())
}

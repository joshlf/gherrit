// TODO: Review changes to this file.

use crate::cmd;
use crate::util::{self, HeadState};
use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unmanaged,
    Private,
    Public,
}

pub fn get_state(repo: &util::Repo, branch_name: &str) -> Result<State> {
    let key = format!("branch.{}.gherritManaged", branch_name);
    match repo.config_string(&key)?.as_deref() {
        Some("managedPublic") => Ok(State::Public),
        Some("managedPrivate") | Some("true") => Ok(State::Private),
        Some("false") | None => Ok(State::Unmanaged),
        Some(unknown) => bail!(
            "Invalid gherritManaged value: '{}'. Expected 'managedPublic', 'managedPrivate', 'true', or 'false'.",
            unknown
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct BranchConfig {
    pub push_remote: Option<String>,
    pub remote: Option<String>,
    pub merge: Option<String>,
}

impl BranchConfig {
    fn expected(state: State, branch_name: &str, default_remote: &str) -> BranchConfig {
        let self_merge_ref = format!("refs/heads/{branch_name}");

        BranchConfig {
            push_remote: match state {
                State::Unmanaged => None,
                State::Private => Some(".".to_string()),
                State::Public => Some(default_remote.to_string()),
            },
            remote: match state {
                State::Unmanaged => None,
                State::Private | State::Public => Some(".".to_string()),
            },
            merge: match state {
                State::Unmanaged => None,
                State::Private | State::Public => Some(self_merge_ref),
            },
        }
    }

    fn read_from(repo: &util::Repo, branch_name: &str) -> Result<BranchConfig> {
        let key = |suffix: &str| format!("branch.{branch_name}.{suffix}");
        Ok(BranchConfig {
            push_remote: repo.config_string(&key("pushRemote"))?,
            remote: repo.config_string(&key("remote"))?,
            merge: repo.config_string(&key("merge"))?,
        })
    }
}

/// Configures the Git branch state for GHerrit management.
pub fn set_state(repo: &util::Repo, new_state: State, force: bool) -> Result<()> {
    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Pending(bn) => bn,
        HeadState::Detached => {
            bail!("Cannot set state for detached HEAD");
        }
    };

    let default_remote = repo.default_remote_name();

    // Step 1: Determine Old Context and Expectation
    let old_state = get_state(repo, branch_name)?;
    let expected_old_config = BranchConfig::expected(old_state, branch_name, &default_remote);

    // Step 2: Check for Drift
    let current_config = BranchConfig::read_from(repo, branch_name)?;
    if current_config != expected_old_config && !force {
        log::warn!(
            "Configuration drift detected for branch {}.",
            branch_name.yellow()
        );
        let (article, state) = match old_state {
            State::Unmanaged => ("an", "unmanaged"),
            State::Private => ("a", "private"),
            State::Public => ("a", "public"),
        };
        log::warn!(
            "The current git configuration does not match the expected state for {article} {} branch.",
            state.yellow(),
        );

        let check_diff = |key: &str, current: &Option<String>, expected: &Option<String>| {
            if current != expected {
                let curr_s = current.as_deref().unwrap_or("<unset>");
                let exp_s = expected.as_deref().unwrap_or("<unset>");
                log::warn!(
                    "  - {key}: current='{}', expected='{}'",
                    curr_s.yellow(),
                    exp_s.yellow()
                );
            }
        };

        check_diff(
            "pushRemote",
            &current_config.push_remote,
            &expected_old_config.push_remote,
        );
        check_diff(
            "remote",
            &current_config.remote,
            &expected_old_config.remote,
        );
        check_diff("merge", &current_config.merge, &expected_old_config.merge);

        log::warn!("Use --force to overwrite manual changes.");
        return Ok(());
    }

    // Step 3: Apply New State
    let key = |suffix: &str| format!("branch.{branch_name}.{suffix}");
    let state_val = match new_state {
        State::Unmanaged => "false",
        State::Private => "managedPrivate",
        State::Public => "managedPublic",
    };
    cmd!("git config", key("gherritManaged"), state_val).status()?;

    let new_config = BranchConfig::expected(new_state, branch_name, &default_remote);

    let apply_config = |k: String, v: Option<String>| -> Result<()> {
        if let Some(val) = v {
            cmd!("git config", k, val).status()?;
        } else {
            // Only unset if it is currently set to avoid error
            if repo.config_string(&k)?.is_some() {
                cmd!("git config --unset", k).status()?;
            }
        }
        Ok(())
    };

    apply_config(key("pushRemote"), new_config.push_remote)?;
    apply_config(key("remote"), new_config.remote)?;
    apply_config(key("merge"), new_config.merge)?;

    let branch_name_yellow = branch_name.yellow();
    match new_state {
        State::Unmanaged => {
            log::info!(
                "Branch {branch_name_yellow} is now {} by GHerrit.",
                "unmanaged".red()
            );
        }
        State::Private => {
            log::info!(
                "Branch {branch_name_yellow} is now {} by GHerrit (Private/Stack Mode).",
                "managed".green()
            );
            log::info!("  - Pushes will go to local loopback (.).");
        }
        State::Public => {
            log::info!(
                "Branch {branch_name_yellow} is now {} by GHerrit (Public Mode).",
                "managed".green()
            );
            log::info!("  - Pushes will go to '{}'.", default_remote);
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

    // Idempotency check: Bail if the branch management state is explicitly managed (Private or Public).
    let current_state = get_state(repo, branch_name).wrap_err("Failed to parse gherritState")?;
    if matches!(current_state, State::Private | State::Public) {
        log::debug!(
            " Branch '{}' is already configured as {:?}.",
            branch_name,
            current_state
        );
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

    let is_default_branch = upstream_merge
        .as_deref()
        .map(|merge| {
            let branch_name = merge.strip_prefix("refs/heads/").unwrap_or(merge);
            repo.is_a_default_branch_on_default_remote(branch_name)
        })
        .unwrap_or(false);

    let has_upstream = upstream_remote.is_some() && upstream_merge.is_some();
    let branch_name_yellow = branch_name.yellow();
    if has_upstream && !is_default_branch {
        // Condition A: Shared Branch
        log::info!("Detected {branch_name_yellow} as a shared branch.");
        set_state(repo, State::Unmanaged, true)?;
        log::info!("To have GHerrit manage this branch, run: gherrit manage");
    } else {
        // Condition B: New Stack
        log::info!("Detected {branch_name_yellow} as a new branch.");
        set_state(repo, State::Private, true)?;
        log::info!("To opt-out, run: gherrit unmanage");
    }

    Ok(())
}

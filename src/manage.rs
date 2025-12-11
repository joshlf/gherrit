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

impl State {
    const UNMANAGED: &str = "false";
    const PRIVATE: &str = "managedPrivate";
    const PUBLIC: &str = "managedPublic";

    pub fn read_from(repo: &util::Repo, branch_name: &str) -> Result<Option<State>> {
        let key = format!("branch.{}.gherritManaged", branch_name);
        match repo.config_string(&key)?.as_deref() {
            Some(State::PUBLIC) => Ok(Some(State::Public)),
            Some(State::PRIVATE) => Ok(Some(State::Private)),
            Some(State::UNMANAGED) => Ok(Some(State::Unmanaged)),
            None => Ok(None),
            Some(unknown) => bail!(
                "Invalid gherritManaged value: {}. Expected {}, {}, or {}.",
                unknown.yellow(),
                State::PUBLIC.yellow(),
                State::PRIVATE.yellow(),
                State::UNMANAGED.yellow()
            ),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct BranchConfig {
    push_remote: Option<String>,
    remote: Option<String>,
    merge: Option<String>,
}

impl BranchConfig {
    fn expected(state: Option<State>, branch_name: &str, default_remote: &str) -> BranchConfig {
        let self_merge_ref = format!("refs/heads/{branch_name}");
        BranchConfig {
            push_remote: match state {
                Some(State::Unmanaged) | None => None,
                Some(State::Private) => Some(".".to_string()),
                Some(State::Public) => Some(default_remote.to_string()),
            },
            remote: match state {
                Some(State::Unmanaged) | None => None,
                Some(State::Private | State::Public) => Some(".".to_string()),
            },
            merge: match state {
                Some(State::Unmanaged) | None => None,
                Some(State::Private | State::Public) => Some(self_merge_ref),
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
    use State::*;

    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Pending(bn) => bn,
        HeadState::Detached => {
            bail!("Cannot set state for detached HEAD");
        }
    };

    let default_remote = repo.default_remote_name();

    // Step 1: Determine Old Context and Expectation
    let old_state = State::read_from(repo, branch_name)?;

    let current_config = match (old_state, new_state) {
        (Some(Unmanaged), Unmanaged) => {
            log::debug!(
                "Branch {} is already in the desired state ({new_state:?}).",
                branch_name.yellow(),
            );
            return Ok(());
        }
        // This transition just has the effect of clarifying the user's intent
        // to unmanage the branch. It doesn't change the state, and so we leave
        // configuration values unchanged. Any configuration values that are set
        // represent custom configuration (since GHerrit doesn't set any values
        // in the unmanaged state), and will be detected if the user ever
        // attempts to transition to a managed state.
        (None, Unmanaged) => BranchConfig::read_from(repo, branch_name)?,
        // This transition will clobber (when transitioning to a managed state)
        // or delete (when transitioning to an unmanaged state) any custom
        // configuration values that the user has set. We include `X -> X`
        // transitions here (where `X = Private | Public`) because the user may
        // perform `gherrit manage --force` in order to *keep* the current
        // management state but forceably clobber any custom configuration
        // values.
        //
        // Note a subtlety: This check will reject configuration values that are
        // unexpected for the *current* state but are correct for the *new*
        // state. This may seem surprising, but it is important: If we allowed
        // this transition, GHerrit would effectively "adopt" ownership of the
        // user's custom configuration values since it would go from them being
        // *unexpected* to *expected*. Thus, on any *subsequent* transition,
        // GHerrit would think it owned them, and would clobber them as it saw
        // fit.
        (Some(Unmanaged) | None, Private | Public)
        | (Some(Private), Unmanaged | Public | Private)
        | (Some(Public), Unmanaged | Private | Public) => {
            let current_config = BranchConfig::read_from(repo, branch_name)?;
            let expected_old_config =
                BranchConfig::expected(old_state, branch_name, &default_remote);
            if current_config != expected_old_config {
                // FIXME(#219): Add the ability to save the user's custom
                // configuration so it can be restored during a subsequent
                // `gherrit unmanage`.
                log::warn!(
                    "Configuration drift detected for branch {}.",
                    branch_name.yellow()
                );
                let (article, state) = match old_state {
                    Some(State::Unmanaged) | None => ("an", "unmanaged"),
                    Some(State::Private) => ("a", "private"),
                    Some(State::Public) => ("a", "public"),
                };
                log::warn!(
                    "The current git configuration does not match the expected state for {article} {} branch.",
                    state.yellow(),
                );

                let check_diff =
                    |key: &str, current: &Option<String>, expected: &Option<String>| {
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

                if force {
                    log::warn!("Overwriting manual changes (--force).");
                } else {
                    log::warn!("Use --force to overwrite manual changes.");
                    return Ok(());
                }
            }

            current_config
        }
    };

    // Step 3: Apply New State
    let key = |suffix: &str| format!("branch.{branch_name}.{suffix}");
    let state_val = match new_state {
        State::Unmanaged => State::UNMANAGED,
        State::Private => State::PRIVATE,
        State::Public => State::PUBLIC,
    };
    cmd!("git config", key("gherritManaged"), state_val).status()?;

    let apply_config = |k: String, old: Option<String>, new: Option<String>| -> Result<()> {
        use crate::util::CommandExt as _;
        match (old, new) {
            (old, Some(new)) => {
                if old.as_ref() != Some(&new) {
                    cmd!("git config", k, new).success()
                } else {
                    Ok(())
                }
            }
            (Some(_), None) => cmd!("git config --unset", k).success(),
            (None, None) => Ok(()),
        }
    };

    let new_config = BranchConfig::expected(Some(new_state), branch_name, &default_remote);
    apply_config(
        key("pushRemote"),
        current_config.push_remote,
        new_config.push_remote,
    )?;
    apply_config(key("remote"), current_config.remote, new_config.remote)?;
    apply_config(key("merge"), current_config.merge, new_config.merge)?;

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
                "Branch {branch_name_yellow} is now {} by GHerrit in private mode.",
                "managed".green()
            );
            log::info!(
                "  - 'git push' will sync PRs only, but will not push {branch_name_yellow} itself."
            );
        }
        State::Public => {
            log::info!(
                "Branch {branch_name_yellow} is now {} by GHerrit in public mode.",
                "managed".green()
            );
            log::info!(
                "  - 'git push' will sync PRs and will also push {branch_name_yellow} itself."
            );
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

    // Idempotency check: Bail if the branch management state is explicitly managed.
    let current_state =
        State::read_from(repo, branch_name).wrap_err("Failed to parse gherritState")?;
    let state_str = match current_state {
        Some(State::Unmanaged) | None => None,
        Some(State::Private) => Some("private"),
        Some(State::Public) => Some("public"),
    };
    if let Some(state) = state_str {
        log::debug!(
            "Branch {} is already configured as {} by GHerrit.",
            branch_name.yellow(),
            state.yellow()
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
        set_state(repo, State::Unmanaged, false)?;
        log::info!("To have GHerrit manage this branch, run: gherrit manage");
    } else {
        // Condition B: New Stack
        log::info!("Detected {branch_name_yellow} as a new branch.");
        set_state(repo, State::Private, false)?;
        log::info!("To opt-out, run: gherrit unmanage");
    }

    Ok(())
}

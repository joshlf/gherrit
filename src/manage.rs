use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

use crate::{
    cmd,
    util::{self, CommandExt as _, HeadState},
};

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

    fn config_value(self) -> &'static str {
        match self {
            State::Unmanaged => State::UNMANAGED,
            State::Private => State::PRIVATE,
            State::Public => State::PUBLIC,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

    fn value(&self, key: BranchConfigKey) -> &Option<String> {
        match key {
            BranchConfigKey::PushRemote => &self.push_remote,
            BranchConfigKey::Remote => &self.remote,
            BranchConfigKey::Merge => &self.merge,
        }
    }

    fn differences_from(&self, expected: &BranchConfig) -> Vec<ConfigDifference> {
        BranchConfigKey::ALL
            .into_iter()
            .filter(|&key| self.value(key) != expected.value(key))
            .map(|key| ConfigDifference {
                key,
                current: self.value(key).clone(),
                expected: expected.value(key).clone(),
            })
            .collect()
    }

    fn updates_to(&self, desired: &BranchConfig) -> Vec<ConfigUpdate> {
        BranchConfigKey::ALL
            .into_iter()
            .filter(|&key| self.value(key) != desired.value(key))
            .map(|key| ConfigUpdate { key, value: desired.value(key).clone() })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchConfigKey {
    PushRemote,
    Remote,
    Merge,
}

impl BranchConfigKey {
    const ALL: [BranchConfigKey; 3] =
        [BranchConfigKey::PushRemote, BranchConfigKey::Remote, BranchConfigKey::Merge];

    fn suffix(self) -> &'static str {
        match self {
            BranchConfigKey::PushRemote => "pushRemote",
            BranchConfigKey::Remote => "remote",
            BranchConfigKey::Merge => "merge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigDifference {
    key: BranchConfigKey,
    current: Option<String>,
    expected: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigUpdate {
    key: BranchConfigKey,
    value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransitionRequest {
    branch_name: String,
    default_remote: String,
    current_state: Option<State>,
    requested_state: State,
    force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TransitionPreparation {
    Ready(TransitionPlan),
    RequiresConfig(ConfigTransition),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigTransition {
    expected_current: BranchConfig,
    desired: BranchConfig,
    requested_state: State,
    force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TransitionPlan {
    NoChange,
    PreserveDrift {
        differences: Vec<ConfigDifference>,
    },
    Apply {
        state: State,
        config_updates: Vec<ConfigUpdate>,
        overwritten_drift: Vec<ConfigDifference>,
    },
}

fn prepare_transition(request: TransitionRequest) -> TransitionPreparation {
    let TransitionRequest { branch_name, default_remote, current_state, requested_state, force } =
        request;

    match (current_state, requested_state) {
        (Some(State::Unmanaged), State::Unmanaged) => {
            TransitionPreparation::Ready(TransitionPlan::NoChange)
        }
        // Recording an explicit unmanaged state must preserve configuration that
        // GHerrit did not create or take ownership of.
        (None, State::Unmanaged) => TransitionPreparation::Ready(TransitionPlan::Apply {
            state: State::Unmanaged,
            config_updates: Vec::new(),
            overwritten_drift: Vec::new(),
        }),
        _ => {
            // Compare against the old state's expected configuration, not the
            // requested state's configuration. Otherwise a transition could
            // silently adopt a user's custom value that happens to match the
            // new state, then treat that value as GHerrit-owned in the future.
            let expected_current =
                BranchConfig::expected(current_state, &branch_name, &default_remote);
            let desired =
                BranchConfig::expected(Some(requested_state), &branch_name, &default_remote);
            TransitionPreparation::RequiresConfig(ConfigTransition {
                expected_current,
                desired,
                requested_state,
                force,
            })
        }
    }
}

impl ConfigTransition {
    fn plan(self, current_config: BranchConfig) -> TransitionPlan {
        let differences = current_config.differences_from(&self.expected_current);

        if !differences.is_empty() && !self.force {
            return TransitionPlan::PreserveDrift { differences };
        }

        TransitionPlan::Apply {
            state: self.requested_state,
            config_updates: current_config.updates_to(&self.desired),
            overwritten_drift: differences,
        }
    }
}

fn log_configuration_drift(
    branch_name: &str,
    current_state: Option<State>,
    differences: &[ConfigDifference],
) {
    log::warn!("Configuration drift detected for branch {}.", branch_name.yellow());
    let (article, state) = match current_state {
        Some(State::Unmanaged) | None => ("an", "unmanaged"),
        Some(State::Private) => ("a", "private"),
        Some(State::Public) => ("a", "public"),
    };
    log::warn!(
        "The current git configuration does not match the expected state for {article} {} branch.",
        state.yellow(),
    );

    differences.iter().for_each(|difference| {
        let current = difference.current.as_deref().unwrap_or("<unset>");
        let expected = difference.expected.as_deref().unwrap_or("<unset>");
        let key = difference.key.suffix();
        log::warn!("  - {key}: current='{}', expected='{}'", current.yellow(), expected.yellow());
    });
}

fn apply_config_update(branch_name: &str, update: ConfigUpdate) -> Result<()> {
    let key = format!("branch.{branch_name}.{}", update.key.suffix());
    match update.value {
        Some(value) => cmd!("git config", key, value).success(),
        None => cmd!("git config --unset", key).success(),
    }
}

/// Configures the Git branch state for GHerrit management.
pub fn set_state(repo: &util::Repo, new_state: State, force: bool) -> Result<()> {
    let (branch_name, old_state) = repo.read_current_branch_and_state()?;
    let default_remote = repo.default_remote_name();
    let preparation = prepare_transition(TransitionRequest {
        branch_name: branch_name.clone(),
        default_remote,
        current_state: old_state,
        requested_state: new_state,
        force,
    });
    let plan = match preparation {
        TransitionPreparation::Ready(plan) => plan,
        TransitionPreparation::RequiresConfig(transition) => {
            transition.plan(BranchConfig::read_from(repo, &branch_name)?)
        }
    };

    let state = match plan {
        TransitionPlan::NoChange => {
            log::debug!(
                "Branch {} is already in the desired state ({new_state:?}).",
                branch_name.yellow(),
            );
            return Ok(());
        }
        TransitionPlan::PreserveDrift { differences } => {
            // FIXME(#219): Add the ability to save the user's custom
            // configuration so it can be restored during a subsequent
            // `gherrit unmanage`.
            log_configuration_drift(&branch_name, old_state, &differences);
            log::warn!("Use --force to overwrite manual changes.");
            return Ok(());
        }
        TransitionPlan::Apply { state, config_updates, overwritten_drift } => {
            if !overwritten_drift.is_empty() {
                log_configuration_drift(&branch_name, old_state, &overwritten_drift);
                log::warn!("Overwriting manual changes (--force).");
            }

            config_updates
                .into_iter()
                .try_for_each(|update| apply_config_update(&branch_name, update))?;
            state
        }
    };

    let state_key = format!("branch.{branch_name}.gherritManaged");
    cmd!("git config", state_key, state.config_value()).success()?;

    let branch_name_y = branch_name.yellow();
    match state {
        State::Unmanaged => {
            let unmanaged_r = "unmanaged".red();
            log::info!("Branch {branch_name_y} is now {unmanaged_r} by GHerrit.");
        }
        #[rustfmt::skip]
        State::Private => {
            let managed_g = "managed".green();
            log::info!("Branch {branch_name_y} is now {managed_g} by GHerrit in private mode.");
            log::info!("  - 'git push' will sync PRs only, but will not push {branch_name_y} itself.");
        }
        State::Public => {
            let managed_g = "managed".green();
            log::info!("Branch {branch_name_y} is now {managed_g} by GHerrit in public mode.");
            log::info!("  - 'git push' will sync PRs and will also push {branch_name_y} itself.");
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
    let is_new =
        repo.is_newly_created_branch(branch_name).wrap_err("Failed to check if branch is new")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const BRANCH: &str = "feature";
    const DEFAULT_REMOTE: &str = "origin";
    const STATES: [Option<State>; 4] =
        [None, Some(State::Unmanaged), Some(State::Private), Some(State::Public)];
    const REQUESTED_STATES: [State; 3] = [State::Unmanaged, State::Private, State::Public];

    fn request(
        current_state: Option<State>,
        requested_state: State,
        force: bool,
    ) -> TransitionRequest {
        TransitionRequest {
            branch_name: BRANCH.to_string(),
            default_remote: DEFAULT_REMOTE.to_string(),
            current_state,
            requested_state,
            force,
        }
    }

    fn branch_config(
        push_remote: Option<&str>,
        remote: Option<&str>,
        merge: Option<&str>,
    ) -> BranchConfig {
        BranchConfig {
            push_remote: push_remote.map(str::to_string),
            remote: remote.map(str::to_string),
            merge: merge.map(str::to_string),
        }
    }

    fn assert_differences(
        differences: &[ConfigDifference],
        current: &BranchConfig,
        expected: &BranchConfig,
    ) {
        let expected_keys = BranchConfigKey::ALL
            .into_iter()
            .filter(|&key| current.value(key) != expected.value(key))
            .collect::<Vec<_>>();

        assert_eq!(
            differences.len(),
            expected_keys.len(),
            "wrong number of differences for {current:?} versus {expected:?}"
        );
        differences.iter().zip(expected_keys).for_each(|(difference, key)| {
            assert_eq!(difference.key, key);
            assert_eq!(&difference.current, current.value(key));
            assert_eq!(&difference.expected, expected.value(key));
        });
    }

    fn apply_updates(mut config: BranchConfig, updates: &[ConfigUpdate]) -> BranchConfig {
        let mut updated = [false; 3];
        updates.iter().for_each(|update| {
            let index = match update.key {
                BranchConfigKey::PushRemote => 0,
                BranchConfigKey::Remote => 1,
                BranchConfigKey::Merge => 2,
            };
            assert!(!updated[index], "duplicate update for {:?}", update.key);
            updated[index] = true;
            assert_ne!(config.value(update.key), &update.value, "update must change a value");
            match update.key {
                BranchConfigKey::PushRemote => config.push_remote.clone_from(&update.value),
                BranchConfigKey::Remote => config.remote.clone_from(&update.value),
                BranchConfigKey::Merge => config.merge.clone_from(&update.value),
            }
        });
        config
    }

    #[test]
    fn expected_branch_configuration_is_owned_and_state_specific() {
        assert_eq!(BranchConfig::expected(None, BRANCH, DEFAULT_REMOTE), BranchConfig::default());
        assert_eq!(
            BranchConfig::expected(Some(State::Unmanaged), BRANCH, DEFAULT_REMOTE),
            BranchConfig::default()
        );
        assert_eq!(
            BranchConfig::expected(Some(State::Private), BRANCH, DEFAULT_REMOTE),
            branch_config(Some("."), Some("."), Some("refs/heads/feature"))
        );
        assert_eq!(
            BranchConfig::expected(Some(State::Public), BRANCH, DEFAULT_REMOTE),
            branch_config(Some("origin"), Some("."), Some("refs/heads/feature"))
        );
    }

    #[test]
    fn unmanaged_no_op_and_intent_recording_need_no_config_observation() {
        [false, true].into_iter().for_each(|force| {
            assert_eq!(
                prepare_transition(request(Some(State::Unmanaged), State::Unmanaged, force)),
                TransitionPreparation::Ready(TransitionPlan::NoChange)
            );
            assert_eq!(
                prepare_transition(request(None, State::Unmanaged, force)),
                TransitionPreparation::Ready(TransitionPlan::Apply {
                    state: State::Unmanaged,
                    config_updates: Vec::new(),
                    overwritten_drift: Vec::new(),
                })
            );
        });
    }

    #[test]
    fn every_config_affecting_transition_requires_an_observation() {
        STATES.into_iter().for_each(|current_state| {
            REQUESTED_STATES.into_iter().for_each(|requested_state| {
                [false, true].into_iter().for_each(|force| {
                    let special = matches!(
                        (current_state, requested_state),
                        (Some(State::Unmanaged), State::Unmanaged)
                            | (None, State::Unmanaged)
                    );
                    assert_eq!(
                        matches!(
                            prepare_transition(request(current_state, requested_state, force)),
                            TransitionPreparation::RequiresConfig(_)
                        ),
                        !special,
                        "wrong observation decision for {current_state:?} -> {requested_state:?}, force={force}"
                    );
                });
            });
        });
    }

    #[test]
    fn config_transition_policy_is_exhaustive_and_minimal() {
        let values = [None, Some("."), Some("origin"), Some("custom"), Some("refs/heads/feature")];
        let mut cases = 0;

        STATES.into_iter().for_each(|current_state| {
            REQUESTED_STATES.into_iter().for_each(|requested_state| {
                [false, true].into_iter().for_each(|force| {
                    let TransitionPreparation::RequiresConfig(transition) =
                        prepare_transition(request(current_state, requested_state, force))
                    else {
                        return;
                    };

                    values.into_iter().for_each(|push_remote| {
                        values.into_iter().for_each(|remote| {
                            values.into_iter().for_each(|merge| {
                                cases += 1;
                                let current = branch_config(push_remote, remote, merge);
                                let expected_current = transition.expected_current.clone();
                                let desired = transition.desired.clone();
                                let plan = transition.clone().plan(current.clone());
                                let drifted = current != expected_current;

                                match plan {
                                    TransitionPlan::PreserveDrift { differences } => {
                                        assert!(drifted);
                                        assert!(!force);
                                        assert_differences(
                                            &differences,
                                            &current,
                                            &expected_current,
                                        );
                                    }
                                    TransitionPlan::Apply {
                                        state,
                                        config_updates,
                                        overwritten_drift,
                                    } => {
                                        assert!(force || !drifted);
                                        assert_eq!(state, requested_state);
                                        assert_differences(
                                            &overwritten_drift,
                                            &current,
                                            &expected_current,
                                        );
                                        assert_eq!(
                                            apply_updates(current, &config_updates),
                                            desired,
                                            "updates did not produce the desired configuration"
                                        );
                                    }
                                    TransitionPlan::NoChange => {
                                        panic!("config transition unexpectedly became a no-op")
                                    }
                                }
                            });
                        });
                    });
                });
            });
        });

        assert_eq!(cases, 2_500);
    }

    #[test]
    fn transition_does_not_adopt_config_owned_by_the_user() {
        let TransitionPreparation::RequiresConfig(transition) =
            prepare_transition(request(Some(State::Private), State::Public, false))
        else {
            panic!("private-to-public transition must inspect configuration");
        };
        let already_public = BranchConfig::expected(Some(State::Public), BRANCH, DEFAULT_REMOTE);

        assert_eq!(
            transition.plan(already_public),
            TransitionPlan::PreserveDrift {
                differences: vec![ConfigDifference {
                    key: BranchConfigKey::PushRemote,
                    current: Some("origin".to_string()),
                    expected: Some(".".to_string()),
                }],
            }
        );
    }
}

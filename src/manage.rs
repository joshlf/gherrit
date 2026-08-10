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

    fn changes_to<'a>(
        &'a self,
        desired: &'a BranchConfig,
    ) -> impl Iterator<Item = (BranchConfigKey, Option<&'a str>, Option<&'a str>)> + 'a {
        BranchConfigKey::ALL.into_iter().filter_map(move |key| {
            let current = self.value(key).as_deref();
            let desired = desired.value(key).as_deref();
            (current != desired).then_some((key, current, desired))
        })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionKind {
    /// The branch is already explicitly unmanaged.
    NoChange,
    /// Record unmanaged intent without claiming ownership of branch config.
    RecordUnmanaged,
    /// Reconcile branch config before recording the requested state.
    ReconcileConfig,
}

fn transition_kind(current_state: Option<State>, requested_state: State) -> TransitionKind {
    match (current_state, requested_state) {
        (Some(State::Unmanaged), State::Unmanaged) => TransitionKind::NoChange,
        // Recording unmanaged intent for a branch with no prior GHerrit state
        // leaves its configuration untouched. GHerrit does not own those values;
        // a later transition to a managed state will detect them as drift.
        (None, State::Unmanaged) => TransitionKind::RecordUnmanaged,
        // Every other transition may replace or remove branch configuration.
        // This includes a managed X -> X transition so `--force` can repair
        // configuration drift without changing the management state.
        _ => TransitionKind::ReconcileConfig,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriftDecision {
    NoDrift,
    Block,
    Overwrite,
}

fn drift_decision(current: &BranchConfig, expected: &BranchConfig, force: bool) -> DriftDecision {
    match (current == expected, force) {
        (true, _) => DriftDecision::NoDrift,
        (false, false) => DriftDecision::Block,
        (false, true) => DriftDecision::Overwrite,
    }
}

fn log_configuration_drift(
    branch_name: &str,
    current_state: Option<State>,
    current: &BranchConfig,
    expected: &BranchConfig,
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

    current.changes_to(expected).for_each(|(key, current, expected)| {
        let current = current.unwrap_or("<unset>");
        let expected = expected.unwrap_or("<unset>");
        let key = key.suffix();
        log::warn!("  - {key}: current='{}', expected='{}'", current.yellow(), expected.yellow());
    });
}

fn apply_config_update(
    branch_name: &str,
    config_key: BranchConfigKey,
    value: Option<&str>,
) -> Result<()> {
    let key = format!("branch.{branch_name}.{}", config_key.suffix());
    match value {
        Some(value) => cmd!("git config", key, value).success(),
        None => cmd!("git config --unset", key).success(),
    }
}

/// Configures the Git branch state for GHerrit management.
pub fn set_state(repo: &util::Repo, new_state: State, force: bool) -> Result<()> {
    let (branch_name, old_state) = repo.read_current_branch_and_state()?;
    match transition_kind(old_state, new_state) {
        TransitionKind::NoChange => {
            log::debug!(
                "Branch {} is already in the desired state ({new_state:?}).",
                branch_name.yellow(),
            );
            return Ok(());
        }
        TransitionKind::RecordUnmanaged => {}
        TransitionKind::ReconcileConfig => {
            let default_remote = repo.default_remote_name();
            let current = BranchConfig::read_from(repo, &branch_name)?;
            // Compare against the old state's expected configuration, not the
            // requested state's configuration. Otherwise a transition could
            // silently adopt a user's custom value that happens to match the
            // new state, then treat that value as GHerrit-owned in the future.
            let expected = BranchConfig::expected(old_state, &branch_name, &default_remote);
            let desired = BranchConfig::expected(Some(new_state), &branch_name, &default_remote);

            match drift_decision(&current, &expected, force) {
                DriftDecision::NoDrift => {}
                DriftDecision::Block => {
                    // FIXME(#219): Add the ability to save the user's custom
                    // configuration so it can be restored during a subsequent
                    // `gherrit unmanage`.
                    log_configuration_drift(&branch_name, old_state, &current, &expected);
                    log::warn!("Use --force to overwrite manual changes.");
                    return Ok(());
                }
                DriftDecision::Overwrite => {
                    log_configuration_drift(&branch_name, old_state, &current, &expected);
                    log::warn!("Overwriting manual changes (--force).");
                }
            }

            current
                .changes_to(&desired)
                .try_for_each(|(key, _, value)| apply_config_update(&branch_name, key, value))?;
        }
    }

    let state_key = format!("branch.{branch_name}.gherritManaged");
    cmd!("git config", state_key, new_state.config_value()).success()?;

    let branch_name_y = branch_name.yellow();
    match new_state {
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
    const CONFIG_VALUES: [Option<&str>; 5] =
        [None, Some("."), Some("origin"), Some("custom"), Some("refs/heads/feature")];

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

    fn branch_configs() -> impl Iterator<Item = BranchConfig> {
        CONFIG_VALUES.into_iter().flat_map(|push_remote| {
            CONFIG_VALUES.into_iter().flat_map(move |remote| {
                CONFIG_VALUES
                    .into_iter()
                    .map(move |merge| branch_config(push_remote, remote, merge))
            })
        })
    }

    fn apply_changes(mut config: BranchConfig, desired: &BranchConfig) -> BranchConfig {
        let changes = config
            .changes_to(desired)
            .map(|(key, current, desired)| {
                (key, current.map(str::to_string), desired.map(str::to_string))
            })
            .collect::<Vec<_>>();
        let mut updated = [false; 3];
        let mut previous_index = None;
        changes.into_iter().for_each(|(key, current, desired)| {
            let index = match key {
                BranchConfigKey::PushRemote => 0,
                BranchConfigKey::Remote => 1,
                BranchConfigKey::Merge => 2,
            };
            assert!(previous_index.is_none_or(|previous| previous < index));
            previous_index = Some(index);
            assert!(!updated[index], "duplicate update for {key:?}");
            updated[index] = true;
            assert_eq!(config.value(key), &current);
            assert_ne!(current, desired, "update must change a value");
            match key {
                BranchConfigKey::PushRemote => config.push_remote = desired,
                BranchConfigKey::Remote => config.remote = desired,
                BranchConfigKey::Merge => config.merge = desired,
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
        assert_eq!(
            BranchConfig::expected(Some(State::Private), BRANCH, "."),
            BranchConfig::expected(Some(State::Public), BRANCH, "."),
            "private and public config may coincide for an unusual remote name"
        );
    }

    #[test]
    fn transition_kind_is_exhaustive() {
        STATES.into_iter().for_each(|current_state| {
            REQUESTED_STATES.into_iter().for_each(|requested_state| {
                let expected = match (current_state, requested_state) {
                    (Some(State::Unmanaged), State::Unmanaged) => TransitionKind::NoChange,
                    (None, State::Unmanaged) => TransitionKind::RecordUnmanaged,
                    _ => TransitionKind::ReconcileConfig,
                };
                assert_eq!(
                    transition_kind(current_state, requested_state),
                    expected,
                    "wrong transition kind for {current_state:?} -> {requested_state:?}"
                );
            });
        });
    }

    #[test]
    fn config_drift_decision_has_exactly_three_outcomes() {
        let expected = BranchConfig::expected(Some(State::Private), BRANCH, DEFAULT_REMOTE);
        let drifted = branch_config(Some("custom"), Some("."), Some("refs/heads/feature"));

        assert_eq!(drift_decision(&expected, &expected, false), DriftDecision::NoDrift);
        assert_eq!(drift_decision(&expected, &expected, true), DriftDecision::NoDrift);
        assert_eq!(drift_decision(&drifted, &expected, false), DriftDecision::Block);
        assert_eq!(drift_decision(&drifted, &expected, true), DriftDecision::Overwrite);
    }

    #[test]
    fn config_changes_are_ordered_and_minimal() {
        let cases = branch_configs()
            .flat_map(|current| branch_configs().map(move |desired| (current.clone(), desired)))
            .inspect(|(current, desired)| {
                assert_eq!(&apply_changes(current.clone(), desired), desired);
            })
            .count();

        assert_eq!(cases, 15_625);
    }

    #[test]
    fn transition_does_not_adopt_config_owned_by_the_user() {
        let expected_private = BranchConfig::expected(Some(State::Private), BRANCH, DEFAULT_REMOTE);
        let already_public = BranchConfig::expected(Some(State::Public), BRANCH, DEFAULT_REMOTE);

        assert_eq!(drift_decision(&already_public, &expected_private, false), DriftDecision::Block);
        assert_eq!(
            drift_decision(&already_public, &expected_private, true),
            DriftDecision::Overwrite
        );
        assert_eq!(
            already_public.changes_to(&already_public).collect::<Vec<_>>(),
            Vec::new(),
            "the desired configuration is already present, but is not GHerrit-owned"
        );
    }
}

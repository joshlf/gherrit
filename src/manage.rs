use eyre::{Result, WrapErr, bail};
use owo_colors::OwoColorize;

use crate::util::{self, HeadState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unmanaged,
    Private,
    Public,
}

/// A checked public branch whose remote ref cannot overlap GHerrit's owned
/// head or base namespaces.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PublicBranchName(String);

impl PublicBranchName {
    pub(crate) fn new(value: String) -> Result<Self> {
        let full_name = format!("refs/heads/{value}");
        if gix::refs::FullName::try_from(full_name.as_str()).is_err() {
            bail!("A public GHerrit branch must have a valid Git branch name");
        }
        if value == "gherrit-bases" || value.starts_with("gherrit-bases/") {
            bail!(
                "A public GHerrit branch cannot use GHerrit's reserved 'gherrit-bases' namespace"
            );
        }
        let first_component = value.split('/').next().expect("validated branch is nonempty");
        if first_component.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            bail!(
                "A public GHerrit branch's first path component must contain a character other than an ASCII letter or digit so it cannot collide with a change-owned head"
            );
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl State {
    const UNMANAGED: &str = "false";
    const PRIVATE: &str = "managedPrivate";
    const PUBLIC: &str = "managedPublic";

    pub fn read_from(repo: &util::Repo, branch_name: &str) -> Result<Option<State>> {
        let key = format!("branch.{}.gherritManaged", branch_name);
        match repo.local_config_values(&key)?.as_slice() {
            [value] if value == State::PUBLIC.as_bytes() => Ok(Some(State::Public)),
            [value] if value == State::PRIVATE.as_bytes() => Ok(Some(State::Private)),
            [value] if value == State::UNMANAGED.as_bytes() => Ok(Some(State::Unmanaged)),
            [] => Ok(None),
            [_] => bail!(
                "Invalid gherritManaged value. Expected {}, {}, or {}.",
                State::PUBLIC.yellow(),
                State::PRIVATE.yellow(),
                State::UNMANAGED.yellow()
            ),
            _ => bail!("gherritManaged must be configured exactly once."),
        }
    }

    /// Reads the branch's checked management intent or reports how to choose
    /// it.
    pub fn read_required_from(repo: &util::Repo, branch_name: &str) -> Result<State> {
        let Some(state) = Self::read_from(repo, branch_name)? else {
            bail!(
                "It is unclear whether branch '{branch_name}' should be managed by GHerrit.\n\
                 Run 'gherrit manage' to configure it as a GHerrit stack.\n\
                 Run 'gherrit unmanage' to push it as a standard Git branch."
            );
        };
        Ok(state)
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
enum BranchConfigValue {
    #[default]
    Missing,
    One(String),
    Invalid,
}

impl BranchConfigValue {
    fn expected(value: Option<String>) -> Self {
        value.map_or(Self::Missing, Self::One)
    }

    fn read(repo: &util::Repo, key: &str) -> Result<Self> {
        Ok(match repo.local_config_values(key)?.as_slice() {
            [] => Self::Missing,
            [value] => std::str::from_utf8(value)
                .map_or(Self::Invalid, |value| Self::One(value.to_owned())),
            _ => Self::Invalid,
        })
    }

    fn one(&self) -> Option<&str> {
        match self {
            Self::One(value) => Some(value),
            Self::Missing | Self::Invalid => None,
        }
    }

    fn diagnostic(&self) -> &str {
        match self {
            Self::Missing => "<unset>",
            Self::One(value) if value.len() <= 128 && !value.chars().any(char::is_control) => value,
            Self::One(_) => "<control-bearing or oversized>",
            Self::Invalid => "<invalid or repeated>",
        }
    }

    fn equals_expected(&self, expected: Option<&str>) -> bool {
        match (self, expected) {
            (Self::Missing, None) => true,
            (Self::One(observed), Some(expected)) => observed == expected,
            (Self::Missing | Self::Invalid, Some(_)) | (Self::One(_) | Self::Invalid, None) => {
                false
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BranchConfig {
    push_remote: BranchConfigValue,
    remote: BranchConfigValue,
    merge: BranchConfigValue,
}

impl BranchConfig {
    fn expected(state: Option<State>, branch_name: &str) -> BranchConfig {
        let self_merge_ref = format!("refs/heads/{branch_name}");
        BranchConfig {
            push_remote: BranchConfigValue::expected(match state {
                Some(State::Unmanaged) | None => None,
                Some(State::Private | State::Public) => Some(".".to_string()),
            }),
            remote: BranchConfigValue::expected(match state {
                Some(State::Unmanaged) | None => None,
                Some(State::Private | State::Public) => Some(".".to_string()),
            }),
            merge: BranchConfigValue::expected(match state {
                Some(State::Unmanaged) | None => None,
                Some(State::Private | State::Public) => Some(self_merge_ref),
            }),
        }
    }

    fn read_from(repo: &util::Repo, branch_name: &str) -> Result<BranchConfig> {
        let key = |suffix: &str| format!("branch.{branch_name}.{suffix}");
        Ok(BranchConfig {
            push_remote: BranchConfigValue::read(repo, &key("pushRemote"))?,
            remote: BranchConfigValue::read(repo, &key("remote"))?,
            merge: BranchConfigValue::read(repo, &key("merge"))?,
        })
    }

    fn value(&self, key: BranchConfigKey) -> &BranchConfigValue {
        match key {
            BranchConfigKey::PushRemote => &self.push_remote,
            BranchConfigKey::Remote => &self.remote,
            BranchConfigKey::Merge => &self.merge,
        }
    }

    fn changes_to<'a>(
        &'a self,
        desired: &'a BranchConfig,
    ) -> impl Iterator<Item = (BranchConfigKey, &'a BranchConfigValue, Option<&'a str>)> + 'a {
        BranchConfigKey::ALL.into_iter().filter_map(move |key| {
            let current = self.value(key);
            let desired = desired.value(key).one();
            (!current.equals_expected(desired)).then_some((key, current, desired))
        })
    }
}

/// The mutually exclusive ownership evidence available for existing branch
/// configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchConfigOwnership {
    Current,
    LegacyPublic,
    Drifted,
}

fn classify_public_branch_config(
    observed: &BranchConfig,
    current: &BranchConfig,
    read_legacy_remote: impl FnOnce() -> Option<util::RemoteName>,
) -> BranchConfigOwnership {
    if observed == current {
        return BranchConfigOwnership::Current;
    }

    // A legacy public configuration differs from the current form only in
    // `pushRemote`. Reject every other shape before consulting the separate
    // legacy destination setting: malformed unrelated configuration must not
    // turn an ordinary drift report into a command failure.
    if observed.remote != current.remote || observed.merge != current.merge {
        return BranchConfigOwnership::Drifted;
    }
    let Some(push_remote) = observed.push_remote.one() else {
        return BranchConfigOwnership::Drifted;
    };
    // An unreadable or non-unique legacy remote supplies no evidence that
    // GHerrit owns this candidate. Preserve it as drift instead of failing the
    // management command.
    let Some(legacy_remote) = read_legacy_remote() else {
        return BranchConfigOwnership::Drifted;
    };

    if push_remote == legacy_remote.as_str() {
        BranchConfigOwnership::LegacyPublic
    } else {
        BranchConfigOwnership::Drifted
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
enum ConfigReconciliation {
    ReconcileCurrent,
    MigrateLegacyPublic,
    BlockDrift,
    OverwriteDrift,
}

fn config_reconciliation(ownership: BranchConfigOwnership, force: bool) -> ConfigReconciliation {
    match (ownership, force) {
        (BranchConfigOwnership::Current, _) => ConfigReconciliation::ReconcileCurrent,
        (BranchConfigOwnership::LegacyPublic, _) => ConfigReconciliation::MigrateLegacyPublic,
        (BranchConfigOwnership::Drifted, false) => ConfigReconciliation::BlockDrift,
        (BranchConfigOwnership::Drifted, true) => ConfigReconciliation::OverwriteDrift,
    }
}

fn classify_owned_branch_config(
    repo: &util::Repo,
    current_state: Option<State>,
    branch_name: &str,
    current: &BranchConfig,
) -> BranchConfigOwnership {
    let expected = BranchConfig::expected(current_state, branch_name);
    match current_state {
        Some(State::Public) => {
            classify_public_branch_config(current, &expected, || repo.default_remote_name().ok())
        }
        Some(State::Unmanaged | State::Private) | None => {
            if current == &expected {
                BranchConfigOwnership::Current
            } else {
                BranchConfigOwnership::Drifted
            }
        }
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
        let current = current.diagnostic();
        let expected = expected.unwrap_or("<unset>");
        let key = key.suffix();
        log::warn!("  - {key}: current='{}', expected='{}'", current.yellow(), expected.yellow());
    });
}

fn apply_config_update(
    repo: &util::Repo,
    branch_name: &str,
    config_key: BranchConfigKey,
    value: Option<&str>,
) -> Result<()> {
    let key = format!("branch.{branch_name}.{}", config_key.suffix());
    match value {
        Some(value) => repo.replace_local_config(&key, value),
        None => repo.unset_local_config(&key),
    }
}

/// Configures the Git branch state for GHerrit management.
pub fn set_state(repo: &util::Repo, new_state: State, force: bool) -> Result<()> {
    let (branch_name, old_state) = repo.read_current_branch_and_state()?;
    if new_state == State::Public {
        PublicBranchName::new(branch_name.clone()).wrap_err(
            "This branch cannot be managed as public. Rename it to a compatible branch name, or run `gherrit manage --private` to keep its stack private",
        )?;
    }
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
            let current = BranchConfig::read_from(repo, &branch_name)?;
            // Compare against the old state's expected configuration, not the
            // requested state's configuration. Otherwise a transition could
            // silently adopt a user's custom value that happens to match the
            // new state, then treat that value as GHerrit-owned in the future.
            let expected = BranchConfig::expected(old_state, &branch_name);
            let desired = BranchConfig::expected(Some(new_state), &branch_name);
            // `--force` is explicit authority to replace any non-current
            // owned configuration. It must remain usable even when unrelated
            // destination configuration is malformed; exact legacy
            // recognition is needed only for the unforced migration path.
            let ownership = if force && current != expected {
                BranchConfigOwnership::Drifted
            } else {
                classify_owned_branch_config(repo, old_state, &branch_name, &current)
            };

            match config_reconciliation(ownership, force) {
                ConfigReconciliation::ReconcileCurrent => {}
                ConfigReconciliation::MigrateLegacyPublic => {
                    log::info!(
                        "Migrating branch {} from GHerrit's legacy public configuration.",
                        branch_name.yellow(),
                    );
                }
                ConfigReconciliation::BlockDrift => {
                    // FIXME(#219): Add the ability to save the user's custom
                    // configuration so it can be restored during a subsequent
                    // `gherrit unmanage`.
                    log_configuration_drift(&branch_name, old_state, &current, &expected);
                    log::warn!("Use --force to overwrite manual changes.");
                    return Ok(());
                }
                ConfigReconciliation::OverwriteDrift => {
                    log_configuration_drift(&branch_name, old_state, &current, &expected);
                    log::warn!("Overwriting manual changes (--force).");
                }
            }

            current.changes_to(&desired).try_for_each(|(key, _, value)| {
                apply_config_update(repo, &branch_name, key, value)
            })?;
        }
    }

    let state_key = format!("branch.{branch_name}.gherritManaged");
    repo.replace_local_config(&state_key, new_state.config_value())?;

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
            log::info!(
                "  - 'git push' will publish the stack without projecting {branch_name_y} to the push destination."
            );
        }
        State::Public => {
            let managed_g = "managed".green();
            log::info!("Branch {branch_name_y} is now {managed_g} by GHerrit in public mode.");
            log::info!(
                "  - 'git push' will publish the stack and project {branch_name_y}'s captured tip to its same-named remote branch."
            );
        }
    }

    Ok(())
}

fn checked_out_branch<'a>(flag: &str, head: &'a HeadState) -> Option<&'a str> {
    match (flag, head) {
        ("1", HeadState::Attached(branch_name)) => Some(branch_name),
        _ => None,
    }
}

fn configured_management_mode(state: Option<State>) -> Option<&'static str> {
    match state {
        Some(State::Private) => Some("private"),
        Some(State::Public) => Some("public"),
        Some(State::Unmanaged) | None => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostCheckoutMergeTarget {
    DefaultBranch,
    OtherBranch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostCheckoutBranchObservation {
    Existing,
    NewlyCreated {
        has_upstream_remote: bool,
        upstream_merge_target: Option<PostCheckoutMergeTarget>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostCheckoutBranchKind {
    Existing,
    Shared,
    NewStack,
}

fn classify_post_checkout_branch(
    observation: PostCheckoutBranchObservation,
) -> PostCheckoutBranchKind {
    match observation {
        PostCheckoutBranchObservation::Existing => PostCheckoutBranchKind::Existing,
        PostCheckoutBranchObservation::NewlyCreated {
            has_upstream_remote: true,
            upstream_merge_target: Some(PostCheckoutMergeTarget::OtherBranch),
        } => PostCheckoutBranchKind::Shared,
        PostCheckoutBranchObservation::NewlyCreated { .. } => PostCheckoutBranchKind::NewStack,
    }
}

pub fn post_checkout(repo: &util::Repo, _prev: &str, _new: &str, flag: &str) -> Result<()> {
    let Some(branch_name) = checked_out_branch(flag, repo.current_branch()) else {
        return Ok(());
    };

    let current_state =
        State::read_from(repo, branch_name).wrap_err("Failed to parse gherritState")?;
    if let Some(mode) = configured_management_mode(current_state) {
        log::debug!(
            "Branch {} is already configured as {} by GHerrit.",
            branch_name.yellow(),
            mode.yellow()
        );
        return Ok(());
    }

    let newly_created =
        repo.is_newly_created_branch(branch_name).wrap_err("Failed to check if branch is new")?;
    let observation = if newly_created {
        let upstream_remote = repo
            .config_string(&format!("branch.{branch_name}.remote"))
            .wrap_err("Failed to read config")?;
        let upstream_merge = repo
            .config_string(&format!("branch.{branch_name}.merge"))
            .wrap_err("Failed to read config")?;
        let upstream_merge_target = upstream_merge
            .as_deref()
            .map(|merge| {
                let merge_branch_name = merge.strip_prefix("refs/heads/").unwrap_or(merge);
                repo.is_a_default_branch_on_default_remote(merge_branch_name).map(|is_default| {
                    if is_default {
                        PostCheckoutMergeTarget::DefaultBranch
                    } else {
                        PostCheckoutMergeTarget::OtherBranch
                    }
                })
            })
            .transpose()?;

        PostCheckoutBranchObservation::NewlyCreated {
            has_upstream_remote: upstream_remote.is_some(),
            upstream_merge_target,
        }
    } else {
        PostCheckoutBranchObservation::Existing
    };

    let branch_name_yellow = branch_name.yellow();
    match classify_post_checkout_branch(observation) {
        PostCheckoutBranchKind::Existing => {
            log::debug!(" Branch '{}' is not newly created.", branch_name);
        }
        PostCheckoutBranchKind::Shared => {
            log::info!("Detected {branch_name_yellow} as a shared branch.");
            set_state(repo, State::Unmanaged, false)?;
            log::info!("To have GHerrit manage this branch, run: gherrit manage");
        }
        PostCheckoutBranchKind::NewStack => {
            log::info!("Detected {branch_name_yellow} as a new branch.");
            set_state(repo, State::Private, false)?;
            log::info!("To opt-out, run: gherrit unmanage");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOSITORY_SELECTION_FIXTURE_MODE: &str = "GHERRIT_REPOSITORY_SELECTION_FIXTURE";
    const REPOSITORY_SELECTION_FIXTURE_TEST: &str =
        "manage::tests::repository_selection_override_fixture";

    #[test]
    fn repository_selection_override_fixture() {
        let Ok(selected) = std::env::var(REPOSITORY_SELECTION_FIXTURE_MODE) else {
            return;
        };
        let redirected = std::env::var("GHERRIT_REDIRECTED_REPOSITORY").unwrap();
        let repository = util::Repo::open(&selected).unwrap();

        // SAFETY: the parent runs only this exact test in a fresh process, so
        // no other thread reads the environment. The process exits after the
        // assertion and never needs the original values restored.
        unsafe {
            std::env::set_var("GIT_DIR", std::path::Path::new(&redirected).join(".git"));
            std::env::set_var("GIT_COMMON_DIR", std::path::Path::new(&redirected).join(".git"));
            std::env::set_var("GIT_WORK_TREE", &redirected);
            std::env::set_var("GIT_CONFIG", std::path::Path::new(&redirected).join(".git/config"));
        }
        set_state(&repository, State::Private, false).unwrap();
    }

    #[test]
    fn management_reads_and_writes_only_the_opened_repository() {
        let selected = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        selected.checkout_new("selected-repository");
        let redirected = testutil::TestContextBuilder::new("unused").with_initial_commit().build();

        let output = selected
            .reexec_test_cmd()
            .args(["--exact", REPOSITORY_SELECTION_FIXTURE_TEST, "--nocapture"])
            .env(REPOSITORY_SELECTION_FIXTURE_MODE, &selected.repo_path)
            .env("GHERRIT_REDIRECTED_REPOSITORY", &redirected.repo_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "repository-selection fixture failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert_eq!(
            selected
                .git_cmd()
                .args(["config", "--local", "--get", "branch.selected-repository.gherritManaged"])
                .output()
                .unwrap()
                .stdout,
            b"managedPrivate\n"
        );
        redirected
            .git_cmd()
            .args(["config", "--local", "--get", "branch.selected-repository.gherritManaged"])
            .assert()
            .code(1);
    }

    #[test]
    fn public_branches_are_disjoint_from_every_owned_ref() {
        for branch in ["feature-/one", "feature-one", "feature.one", "café", "gherrit-bases-other"]
        {
            assert_eq!(PublicBranchName::new(branch.to_owned()).unwrap().as_str(), branch);
        }

        for branch in [
            "Gchange",
            "Gchange/child",
            "feature",
            "feature/one",
            "gherrit-bases",
            "gherrit-bases/Gchange",
        ] {
            assert!(PublicBranchName::new(branch.to_owned()).is_err(), "branch={branch:?}");
        }
    }

    const BRANCH: &str = "feature";
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
            push_remote: BranchConfigValue::expected(push_remote.map(str::to_string)),
            remote: BranchConfigValue::expected(remote.map(str::to_string)),
            merge: BranchConfigValue::expected(merge.map(str::to_string)),
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
                (key, current.clone(), BranchConfigValue::expected(desired.map(str::to_string)))
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
    fn post_checkout_only_inspects_attached_branch_switches() {
        ["0", "1", "unexpected"].into_iter().for_each(|flag| {
            let attached = HeadState::Attached(BRANCH.to_string());
            let expected = (flag == "1").then_some(BRANCH);
            assert_eq!(checked_out_branch(flag, &attached), expected, "flag={flag}");
            assert_eq!(checked_out_branch(flag, &HeadState::Pending(BRANCH.to_string())), None);
            assert_eq!(checked_out_branch(flag, &HeadState::Detached), None);
        });
    }

    #[test]
    fn post_checkout_only_reclassifies_unconfigured_or_unmanaged_branches() {
        assert_eq!(configured_management_mode(None), None);
        assert_eq!(configured_management_mode(Some(State::Unmanaged)), None);
        assert_eq!(configured_management_mode(Some(State::Private)), Some("private"));
        assert_eq!(configured_management_mode(Some(State::Public)), Some("public"));
    }

    #[test]
    fn post_checkout_branch_classification_is_exhaustive() {
        assert_eq!(
            classify_post_checkout_branch(PostCheckoutBranchObservation::Existing),
            PostCheckoutBranchKind::Existing
        );

        let merge_targets = [
            None,
            Some(PostCheckoutMergeTarget::DefaultBranch),
            Some(PostCheckoutMergeTarget::OtherBranch),
        ];
        [false, true].into_iter().for_each(|has_upstream_remote| {
            merge_targets.into_iter().for_each(|upstream_merge_target| {
                let expected = match (has_upstream_remote, upstream_merge_target) {
                    (true, Some(PostCheckoutMergeTarget::OtherBranch)) => {
                        PostCheckoutBranchKind::Shared
                    }
                    _ => PostCheckoutBranchKind::NewStack,
                };
                let observation = PostCheckoutBranchObservation::NewlyCreated {
                    has_upstream_remote,
                    upstream_merge_target,
                };

                assert_eq!(
                    classify_post_checkout_branch(observation),
                    expected,
                    "observation={observation:?}"
                );
            });
        });
    }

    #[test]
    fn expected_branch_configuration_is_owned_and_state_specific() {
        assert_eq!(BranchConfig::expected(None, BRANCH), BranchConfig::default());
        assert_eq!(BranchConfig::expected(Some(State::Unmanaged), BRANCH), BranchConfig::default());
        assert_eq!(
            BranchConfig::expected(Some(State::Private), BRANCH),
            branch_config(Some("."), Some("."), Some("refs/heads/feature"))
        );
        assert_eq!(
            BranchConfig::expected(Some(State::Public), BRANCH),
            branch_config(Some("."), Some("."), Some("refs/heads/feature"))
        );
    }

    #[test]
    fn public_configuration_ownership_classification_is_exhaustive() {
        let current = BranchConfig::expected(Some(State::Public), BRANCH);
        let legacy = branch_config(Some("origin"), Some("."), Some("refs/heads/feature"));
        let cases = branch_configs()
            .inspect(|observed| {
                let legacy_candidate = observed != &current
                    && observed.push_remote.one().is_some()
                    && observed.remote == current.remote
                    && observed.merge == current.merge;
                let remote_read = std::cell::Cell::new(false);
                let expected = if observed == &current {
                    BranchConfigOwnership::Current
                } else if observed == &legacy {
                    BranchConfigOwnership::LegacyPublic
                } else {
                    BranchConfigOwnership::Drifted
                };
                assert_eq!(
                    classify_public_branch_config(observed, &current, || {
                        remote_read.set(true);
                        util::RemoteName::from_config(b"origin").ok()
                    }),
                    expected,
                    "observed={observed:?}"
                );
                assert_eq!(remote_read.get(), legacy_candidate, "observed={observed:?}");
            })
            .count();

        assert_eq!(cases, 125);
    }

    #[test]
    fn current_public_configuration_wins_when_the_legacy_remote_is_loopback() {
        let current = BranchConfig::expected(Some(State::Public), BRANCH);
        let legacy = branch_config(Some("."), Some("."), Some("refs/heads/feature"));

        assert_eq!(current, legacy);
        assert_eq!(
            classify_public_branch_config(&current, &current, || {
                panic!("current configuration must not read the legacy remote")
            }),
            BranchConfigOwnership::Current
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
    fn configuration_reconciliation_truth_table_is_exhaustive() {
        let ownerships = [
            BranchConfigOwnership::Current,
            BranchConfigOwnership::LegacyPublic,
            BranchConfigOwnership::Drifted,
        ];
        let cases = ownerships
            .into_iter()
            .flat_map(|ownership| [false, true].map(move |force| (ownership, force)))
            .inspect(|&(ownership, force)| {
                let expected = match (ownership, force) {
                    (BranchConfigOwnership::Current, _) => ConfigReconciliation::ReconcileCurrent,
                    (BranchConfigOwnership::LegacyPublic, _) => {
                        ConfigReconciliation::MigrateLegacyPublic
                    }
                    (BranchConfigOwnership::Drifted, false) => ConfigReconciliation::BlockDrift,
                    (BranchConfigOwnership::Drifted, true) => ConfigReconciliation::OverwriteDrift,
                };
                assert_eq!(config_reconciliation(ownership, force), expected);
            })
            .count();

        assert_eq!(cases, 6);
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
    fn branch_config_diagnostics_do_not_emit_hostile_values() {
        assert_eq!(BranchConfigValue::One("ordinary".to_owned()).diagnostic(), "ordinary");
        assert_eq!(
            BranchConfigValue::One("line one\nline two".to_owned()).diagnostic(),
            "<control-bearing or oversized>"
        );
        assert_eq!(
            BranchConfigValue::One("x".repeat(129)).diagnostic(),
            "<control-bearing or oversized>"
        );
    }

    #[test]
    fn visibility_transition_preserves_the_shared_noop_configuration() {
        let expected_private = BranchConfig::expected(Some(State::Private), BRANCH);
        let already_public = BranchConfig::expected(Some(State::Public), BRANCH);

        assert_eq!(already_public, expected_private);
        assert_eq!(
            already_public.changes_to(&already_public).collect::<Vec<_>>(),
            Vec::new(),
            "visibility is publication intent, not enclosing-push configuration"
        );
    }
}

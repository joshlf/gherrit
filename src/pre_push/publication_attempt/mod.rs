//! One exact-local publication attempt.
//!
//! This module captures local intent once, makes the bounded observations
//! needed to plan from that intent, and consumes the resulting plan through
//! its acknowledgement-gated effects.

mod body;
mod github;
mod history;
mod plan;
mod refs;
mod remote;
#[cfg(test)]
mod semantic_oracle;
mod version;

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use gix::ObjectId;
use owo_colors::OwoColorize as _;

use self::{
    github::{Github, ObservedGithub},
    history::ValidatedChangeHistory,
};
use super::{
    GithubEndpoint, Invocation,
    destination::{DefaultBranch, ObservedPublicBranch, PushDestination, RemoteBranchState},
    local::LocalStack,
};
use crate::{
    manage::{PublicBranchName, State},
    util,
};

/// A public branch checked against the repository default observed by the
/// same publication attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PublicBranch(PublicBranchName);

impl PublicBranch {
    fn new(name: PublicBranchName, default_branch: &DefaultBranch) -> Result<Self> {
        if ref_paths_conflict(name.as_str(), default_branch.name()) {
            bail!("A public GHerrit branch cannot conflict with the repository default branch");
        }
        Ok(Self(name))
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A checked public branch paired with the exact state observed for it.
struct ObservedPublicProjection {
    branch: PublicBranch,
    state: RemoteBranchState,
}

impl ObservedPublicProjection {
    fn new(observed: ObservedPublicBranch, default_branch: &DefaultBranch) -> Result<Self> {
        let (name, state) = observed.into_parts();
        Ok(Self { branch: PublicBranch::new(name, default_branch)?, state })
    }

    fn into_parts(self) -> (PublicBranch, RemoteBranchState) {
        (self.branch, self.state)
    }
}

fn ref_paths_conflict(left: &str, right: &str) -> bool {
    left == right
        || left.strip_prefix(right).is_some_and(|suffix| suffix.starts_with('/'))
        || right.strip_prefix(left).is_some_and(|suffix| suffix.starts_with('/'))
}

/// The complete local decision captured before remote observation begins.
enum LocalPublicationIntent {
    Unmanaged(String),
    Managed(ManagedLocalPublication),
}

/// One managed branch's immutable local input for this attempt.
struct ManagedLocalPublication {
    branch_name: String,
    head: ObjectId,
    public_branch: Option<PublicBranchName>,
}

/// Local intent, initial remote evidence, and the sole destination capability
/// which produced that evidence.
///
/// Production can construct this value only by observing the public branch
/// selected by the same captured branch/head pair used to collect `stack`.
/// This keeps the initial default/public evidence inseparable from its
/// destination and local intent. Complete remote and GitHub evidence joins it
/// in [`CompletePublicationObservation`] before planning.
struct ObservedLocalPublication {
    destination: PushDestination,
    stack: LocalStack,
    public_branch: Option<ObservedPublicProjection>,
}

/// Every observation which authorizes one nonempty publication plan.
///
/// The only production constructor is the straight-line parallel observation
/// in [`ObservedLocalPublication::publish`]. Planning cannot receive remote
/// histories or GitHub evidence separately from the local intent and exact
/// destination which produced them.
struct CompletePublicationObservation {
    local: ObservedLocalPublication,
    histories: Box<[ValidatedChangeHistory]>,
    github: ObservedGithub,
}

impl CompletePublicationObservation {
    fn into_parts(
        self,
    ) -> (ObservedLocalPublication, Box<[ValidatedChangeHistory]>, ObservedGithub) {
        (self.local, self.histories, self.github)
    }

    #[cfg(test)]
    fn for_plan_test(
        local: ObservedLocalPublication,
        histories: Box<[ValidatedChangeHistory]>,
        github: ObservedGithub,
    ) -> Self {
        Self { local, histories, github }
    }
}

impl LocalPublicationIntent {
    fn capture(repository: &util::Repo) -> Result<Self> {
        let head = repository
            .branch_head_snapshot()?
            .ok_or_else(|| eyre!("Cannot push from detached HEAD"))?;
        let (branch_name, target) = head.into_parts();

        let public_branch = match State::read_required_from(repository, &branch_name)? {
            State::Unmanaged => return Ok(Self::Unmanaged(branch_name)),
            State::Private => None,
            State::Public => Some(PublicBranchName::new(branch_name.clone())?),
        };
        let target = target.ok_or_else(|| {
            eyre!("Cannot publish managed branch '{branch_name}' because it has no commits")
        })?;
        Ok(Self::Managed(ManagedLocalPublication { branch_name, head: target, public_branch }))
    }
}

impl ManagedLocalPublication {
    async fn observe(
        self,
        repository: &util::Repo,
        destination: PushDestination,
    ) -> Result<ObservedLocalPublication> {
        let Self { branch_name, head, public_branch } = self;
        let initial = destination.observe_initial(public_branch).await?;
        let (destination, default_branch, public_branch) = initial.into_parts();
        let public_branch = public_branch
            .map(|observed| ObservedPublicProjection::new(observed, &default_branch))
            .transpose()?;
        let stack = LocalStack::collect_captured(repository, &branch_name, head, default_branch)
            .wrap_err("Failed to collect commits")?;
        Ok(ObservedLocalPublication { destination, stack, public_branch })
    }
}

impl ObservedLocalPublication {
    fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    fn len(&self) -> usize {
        self.stack.len()
    }

    fn into_parts(self) -> (PushDestination, LocalStack, Option<ObservedPublicProjection>) {
        (self.destination, self.stack, self.public_branch)
    }

    #[cfg(test)]
    fn for_plan_test(
        destination: PushDestination,
        stack: LocalStack,
        public_branch: Option<ObservedPublicBranch>,
    ) -> Self {
        let public_branch = public_branch.map(|observed| {
            ObservedPublicProjection::new(observed, stack.default_branch()).unwrap()
        });
        Self { destination, stack, public_branch }
    }

    async fn publish(self, repository: &util::Repo, endpoint: &GithubEndpoint) -> Result<usize> {
        if self.is_empty() {
            plan::plan_empty_publication(self)?.execute().await?;
            return Ok(0);
        }
        if endpoint.is_disabled() {
            bail!(
                "The GHerrit test driver cannot publish PRs without a configured GitHub endpoint"
            );
        }
        if let Some(api_url) = endpoint.custom_url() {
            log::warn!("Using custom GitHub API URL: {api_url}");
        }

        let count = self.len();
        let github = Github::new(endpoint, &self.destination)?;
        let (histories, observed) = tokio::try_join!(
            remote::observe_and_validate_histories(&self.stack, repository, &self.destination),
            github.observe_local_pull_requests(&self.stack),
        )?;
        let observation =
            CompletePublicationObservation { local: self, histories, github: observed };
        plan::plan_publication(observation)?.execute().await?;
        Ok(count)
    }
}

/// Runs the prepared publication protocol behind one private boundary.
///
/// Callers cannot assemble a destination, observation, client, plan, or
/// effect. This function derives each value from the supplied repository and
/// consumes the complete attempt before returning.
pub(super) async fn run(
    repository: &util::Repo,
    endpoint: &GithubEndpoint,
    invocation: Invocation,
) -> Result<()> {
    let managed = match LocalPublicationIntent::capture(repository)? {
        LocalPublicationIntent::Unmanaged(branch_name) => {
            log::info!("Branch {} is UNMANAGED. Allowing standard push.", branch_name.yellow());
            return Ok(());
        }
        LocalPublicationIntent::Managed(managed) => managed,
    };
    invocation.require_managed_noop()?;
    let branch_name = managed.branch_name.clone();
    log::info!("Branch {} is MANAGED. Publishing stack...", branch_name.yellow());

    let configured_remote = repository
        .default_remote_name()
        .wrap_err("Failed to read the configured GHerrit remote")?;
    let destination = PushDestination::resolve(repository, configured_remote)?;
    let count =
        managed.observe(repository, destination).await?.publish(repository, endpoint).await?;
    if count == 0 {
        log::info!("No commits to publish.");
    } else {
        let noun = if count == 1 { "commit" } else { "commits" };
        log::info!("Successfully published {count} {noun}.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_capture_models_each_checked_management_mode() {
        enum Expected {
            Unmanaged,
            Managed(Option<&'static str>),
        }

        for (branch_name, configured_state, expected) in [
            ("ordinary-stack", "false", Expected::Unmanaged),
            ("private-stack", testutil::MANAGED_PRIVATE, Expected::Managed(None)),
            ("public-stack", testutil::MANAGED_PUBLIC, Expected::Managed(Some("public-stack"))),
        ] {
            let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
            context.checkout_new(branch_name);
            context.set_config(
                &format!("branch.{branch_name}.gherritManaged"),
                Some(configured_state),
            );
            let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
            let expected_head = ObjectId::from_hex(context.head_oid().as_bytes())
                .expect("fixture HEAD is an object ID");

            match (expected, LocalPublicationIntent::capture(&repository).unwrap()) {
                (Expected::Unmanaged, LocalPublicationIntent::Unmanaged(observed)) => {
                    assert_eq!(observed, branch_name);
                }
                (Expected::Managed(expected_public), LocalPublicationIntent::Managed(observed)) => {
                    assert_eq!(observed.branch_name, branch_name);
                    assert_eq!(observed.head, expected_head);
                    assert_eq!(
                        observed.public_branch.as_ref().map(PublicBranchName::as_str),
                        expected_public
                    );
                }
                _ => panic!("capture disagreed with configured management state"),
            }
        }
    }

    #[test]
    fn local_capture_rejects_rebase_metadata_as_publication_authority() {
        let context = testutil::TestContextBuilder::new("unused").with_initial_commit().build();
        context.checkout_new("feature");
        context.set_config("branch.feature.gherritManaged", Some(testutil::MANAGED_PRIVATE));
        context.run_git(&["checkout", "--detach"]);
        let rebase = context.repo_path.join(".git/rebase-merge");
        std::fs::create_dir_all(&rebase).unwrap();
        std::fs::write(rebase.join("head-name"), "refs/heads/feature\n").unwrap();

        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let error = match LocalPublicationIntent::capture(&repository) {
            Ok(_) => panic!("rebase metadata must not authorize publication"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "Cannot push from detached HEAD");
    }

    #[tokio::test]
    async fn captured_empty_public_stack_projects_unicode_c1_to_the_exact_default_tip() {
        const BRANCH: &str = "public-\u{85}stack";

        let context =
            testutil::TestContextBuilder::new("unused").with_remote().with_initial_commit().build();
        context.checkout_new(BRANCH);
        context
            .set_config(&format!("branch.{BRANCH}.gherritManaged"), Some(testutil::MANAGED_PUBLIC));
        let repository = util::Repo::open(context.repo_path.to_str().unwrap()).unwrap();
        let expected = ObjectId::from_hex(context.head_oid().as_bytes()).unwrap();
        let LocalPublicationIntent::Managed(managed) =
            LocalPublicationIntent::capture(&repository).unwrap()
        else {
            panic!("the configured public branch must be managed")
        };
        let remote = repository.default_remote_name().unwrap();
        let destination = PushDestination::resolve(&repository, remote).unwrap();

        let observed = managed.observe(&repository, destination).await.unwrap();
        assert!(observed.is_empty());
        plan::plan_empty_publication(observed).unwrap().execute().await.unwrap();

        assert_eq!(
            context.remote_ref_oid(&format!("refs/heads/{BRANCH}")),
            Some(expected.to_string())
        );
    }
}

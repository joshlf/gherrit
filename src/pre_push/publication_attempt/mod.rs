//! One exact-local publication attempt.
//!
//! [`run`] is the only operation visible to the parent pre-push module. It
//! derives the destination and local stack, observes Git and GitHub, plans the
//! complete publication, and consumes every effect stage before returning.
//! No observation, client, action, receipt, or continuation crosses this
//! module boundary.

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

use self::github::Github;
use super::{GithubEndpoint, Invocation, destination::PushDestination, local::LocalStack};
use crate::{
    manage::{PublicBranchName, State},
    util::{self, HeadState},
};

/// The complete local decision made before an attempt can yield to remote
/// work.
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

impl LocalPublicationIntent {
    fn capture(repository: &util::Repo) -> Result<Self> {
        let head = repository.head_snapshot()?;
        let branch_name = match head.state() {
            HeadState::Attached(name) | HeadState::Pending(name) => name.clone(),
            HeadState::Detached => bail!("Cannot push from detached HEAD"),
        };

        let public_branch = match State::read_required_from(repository, &branch_name)? {
            State::Unmanaged => return Ok(Self::Unmanaged(branch_name)),
            State::Private => None,
            State::Public => Some(PublicBranchName::new(branch_name.clone())?),
        };
        let target = head.target().ok_or_else(|| {
            eyre!("Cannot publish managed branch '{branch_name}' because it has no commits")
        })?;
        Ok(Self::Managed(ManagedLocalPublication { branch_name, head: target, public_branch }))
    }
}

/// Runs the complete publication protocol behind one private boundary.
///
/// Callers cannot assemble a destination, observation, client, plan, or
/// effect. This function derives each value from the supplied repository and
/// consumes the complete attempt before returning.
pub(super) async fn run(
    repository: &util::Repo,
    endpoint: &GithubEndpoint,
    invocation: Invocation,
) -> Result<()> {
    let ManagedLocalPublication { branch_name, head, public_branch } =
        match LocalPublicationIntent::capture(repository)? {
            LocalPublicationIntent::Unmanaged(branch_name) => {
                log::info!("Branch {} is UNMANAGED. Allowing standard push.", branch_name.yellow());
                return Ok(());
            }
            LocalPublicationIntent::Managed(managed) => managed,
        };
    invocation.require_managed_noop()?;
    log::info!("Branch {} is MANAGED. Publishing stack...", branch_name.yellow());

    let configured_remote = repository
        .default_remote_name()
        .wrap_err("Failed to read the configured GHerrit remote")?;
    let destination = PushDestination::resolve(repository, configured_remote)?;
    let initial = destination.observe_initial(public_branch).await?;
    let (default_branch, observed_public_branch) = initial.into_parts();
    let stack = LocalStack::collect(
        repository,
        &branch_name,
        head,
        &default_branch,
        destination.configured_remote(),
    )
    .wrap_err("Failed to collect commits")?;
    let public_branch =
        plan::plan_public_branch(observed_public_branch, &default_branch, stack.tip())?;

    if stack.is_empty() {
        plan::plan_empty_publication(&destination, public_branch.as_ref())?
            .execute(&destination)
            .await?;
        log::info!("No commits to publish.");
        return Ok(());
    }
    if endpoint.is_disabled() {
        bail!("The GHerrit test driver cannot publish PRs without a configured GitHub endpoint");
    }
    if let Some(api_url) = endpoint.custom_url() {
        log::warn!("Using custom GitHub API URL: {api_url}");
    }

    let github = Github::new(endpoint, &destination)?;
    let (validated, observed) = tokio::try_join!(
        remote::observe_and_validate_histories(&stack, repository, destination),
        github.observe_local_pull_requests(&stack),
    )?;
    let count = stack.len();
    plan::plan_publication(validated, observed, public_branch, stack)?.execute().await?;
    let noun = if count == 1 { "commit" } else { "commits" };
    log::info!("Successfully published {count} {noun}.");
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
}

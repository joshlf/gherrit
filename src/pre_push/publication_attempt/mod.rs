//! One exact-local publication attempt.
//!
//! [`run`] is the only operation visible to the parent pre-push module. It
//! derives the destination and local stack, observes Git and GitHub, plans the
//! complete publication, and consumes every effect stage before returning.
//! No observation, client, action, receipt, or continuation crosses this
//! module boundary.

#![cfg_attr(
    test,
    allow(dead_code, reason = "the complete exact workflow remains dormant until activation")
)]

mod body;
mod github;
mod history;
mod plan;
mod refs;
mod remote;
mod version;

use color_eyre::eyre::{Context as _, Result, bail};
use owo_colors::OwoColorize as _;

use self::github::Github;
use super::{GithubEndpoint, destination::PushDestination, local::LocalStack};
use crate::util::{self, HeadState};

/// Runs the complete publication protocol behind one private boundary.
///
/// Callers cannot assemble a destination, observation, client, plan, or
/// effect. This function derives each value from the supplied repository and
/// consumes the complete attempt before returning.
pub(super) async fn run(repository: &util::Repo, endpoint: &GithubEndpoint) -> Result<()> {
    let branch_name = match repository.current_branch() {
        HeadState::Attached(name) | HeadState::Pending(name) => name,
        HeadState::Detached => bail!("Cannot push from detached HEAD"),
    };

    if !repository.is_managed(branch_name)? {
        log::info!("Branch {} is UNMANAGED. Allowing standard push.", branch_name.yellow());
        return Ok(());
    }
    log::info!("Branch {} is MANAGED. Publishing stack...", branch_name.yellow());

    let configured_remote = repository
        .default_remote_name()
        .wrap_err("Failed to read the configured GHerrit remote")?;
    let destination = PushDestination::resolve(repository, configured_remote)?;
    let default_branch = destination.observe_default_branch().await?;
    let stack = LocalStack::collect(repository, &default_branch, destination.configured_remote())
        .wrap_err("Failed to collect commits")?;

    if stack.is_empty() {
        log::info!("No commits to publish.");
        return Ok(());
    }
    if endpoint.is_disabled() {
        bail!("The GHerrit test driver cannot sync PRs without a configured GitHub endpoint");
    }
    if let Some(api_url) = endpoint.custom_url() {
        log::warn!("Using custom GitHub API URL: {api_url}");
    }

    let github = Github::new(endpoint, &destination)?;
    let public_branch = super::public_stack_branch(repository, branch_name);
    let (validated, observed) = tokio::try_join!(
        remote::observe_and_validate_histories(&stack, repository, destination),
        github.observe_local_pull_requests(&stack),
    )?;
    let count = stack.len();
    plan::plan_publication(validated, observed, public_branch, stack)?.execute().await?;
    log::info!("Successfully published {count} commits.");
    Ok(())
}

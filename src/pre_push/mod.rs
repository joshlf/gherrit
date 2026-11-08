//! Publishes the local stack as Git refs and GitHub pull requests.
//!
//! The command reads and checks commits, looks up PRs and rejects closed or
//! merged ones, pushes Git refs, then creates missing PRs and updates their
//! details. Git is pushed first so branches exist before PR creation. All PR
//! numbers are known before navigation links for the whole stack are rendered.
//!
//! These steps are not one transaction. Each Git batch is atomic, but later Git
//! or GraphQL batches may fail after earlier ones succeed. Errors stop the
//! command without undoing remote changes. Publishing a nonempty managed stack
//! creates version tags even when its commit objects have not changed.
//!
//! Separate modules read commits, run Git, match commits to PRs, and send
//! GraphQL requests. Each handles its own data and failures. This module keeps
//! the initial command checks and the order of writes in one place.

use color_eyre::eyre::{Context as _, Result, bail};
use gix::reference::Category;
use octocrab::Octocrab;
use owo_colors::OwoColorize;

use crate::util::{self, HeadState};

mod autosquash;
mod batching;
mod body;
mod collect;
mod github;
mod identity;
mod publication;
mod push;
mod reconcile;
/// Names for managed branches and version tags.
///
/// These helpers construct names shared by publication, remote reads, and
/// local tag writes. Callers handle parsing and validation.
mod refs {

    pub(super) fn head(id: &str) -> String {
        format!("refs/heads/{id}")
    }

    pub(super) fn version_tag_prefix(id: &str) -> String {
        format!("refs/tags/gherrit/{id}/v")
    }

    pub(super) fn version_tag(id: &str, version: usize) -> String {
        format!("{}{version}", version_tag_prefix(id))
    }
}
mod remote;
mod sync;

use collect::collect_commits;
use identity::ChangeId;
use push::push_to_origin;
use reconcile::ensure_pull_requests_open;
use sync::{batch_fetch_prs, sync_prs};

#[derive(Eq, PartialEq)]
pub(crate) enum GithubEndpoint {
    Production,
    #[cfg(feature = "test-driver")]
    Custom(String),
    #[cfg(feature = "test-driver")]
    Disabled,
}

impl GithubEndpoint {
    fn is_disabled(&self) -> bool {
        #[cfg(feature = "test-driver")]
        {
            *self == Self::Disabled
        }
        #[cfg(not(feature = "test-driver"))]
        {
            false
        }
    }

    fn custom_url(&self) -> Option<&str> {
        #[cfg(feature = "test-driver")]
        if let Self::Custom(url) = self {
            return Some(url);
        }
        None
    }
}

pub async fn run(repo: &util::Repo, github_endpoint: &GithubEndpoint) -> Result<()> {
    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Pending(bn) => bn,
        HeadState::Detached => {
            bail!("Cannot push from detached HEAD");
        }
    };

    match repo.is_managed(branch_name)? {
        false => {
            log::info!("Branch {} is UNMANAGED. Allowing standard push.", branch_name.yellow());
            return Ok(());
        }
        true => log::info!("Branch {} is MANAGED. Syncing stack...", branch_name.yellow()),
    }

    let commits = collect_commits(repo).wrap_err("Failed to collect commits")?;

    if commits.is_empty() {
        log::info!("No commits to sync.");
        return Ok(());
    }

    let public_branch =
        public_branch_name(repo, branch_name).wrap_err("Failed to read stack visibility")?;

    if github_endpoint.is_disabled() {
        bail!("The GHerrit test driver cannot sync PRs without a configured GitHub endpoint");
    }

    let token = util::get_github_token()?;
    let mut builder = Octocrab::builder().personal_token(token);

    // A custom endpoint is an explicit dependency supplied by the caller. The
    // production binary always selects `Production`, so an environment
    // variable cannot redirect a user's token.
    if let Some(api_url) = github_endpoint.custom_url() {
        log::warn!("Using custom GitHub API URL: {}", api_url);
        builder = builder.base_uri(api_url)?;
    }

    let octocrab = builder.build()?;

    let gherrit_ids: Vec<ChangeId> = commits.iter().map(|c| c.gherrit_id().clone()).collect();
    // Reject a PR known to be closed or merged before pushing Git refs. This
    // read does not lock the PR; another actor may change its state later.
    let prs = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;
    ensure_pull_requests_open(prs.iter().map(|pr| (pr.number, pr.state)))?;

    let commits = push_to_origin(repo, commits)?;
    let default_branch = repo.find_default_branch_on_default_remote()?;

    let num_commits = commits.len();
    sync_prs(repo, &octocrab, public_branch.as_deref(), &default_branch, commits, prs).await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

fn public_branch_name(repo: &util::Repo, branch: &str) -> Result<Option<String>> {
    if repo.config_string(&format!("branch.{branch}.pushRemote"))?.as_deref() == Some(".") {
        return Ok(None);
    }
    Ok(repo.head()?.try_into_referent().and_then(|head_ref| {
        let (category, short_name) = head_ref.inner.name.category_and_short_name()?;
        (category == Category::LocalBranch).then(|| short_name.to_string())
    }))
}

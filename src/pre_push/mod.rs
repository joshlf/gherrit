use color_eyre::eyre::{Context, Result, bail};
use owo_colors::OwoColorize;

use crate::{
    manage::State,
    util::{self, HeadState},
};

mod autosquash;
mod body;
mod destination;
mod github;
mod history;
mod local;
mod plan;
mod publication;
mod pull_request;
mod remote;
mod subprocess;
#[cfg(test)]
mod test_effect;
mod version;

const MAX_EXTERNAL_DIAGNOSTIC_BYTES: usize = 256;

/// Renders an untrusted external value as one short terminal-safe line.
fn bounded_diagnostic_detail(detail: &str) -> String {
    const CONTENT_BYTES: usize = MAX_EXTERNAL_DIAGNOSTIC_BYTES - 3;

    let mut characters = detail.chars();
    let mut bounded = String::with_capacity(MAX_EXTERNAL_DIAGNOSTIC_BYTES);
    for _ in 0..CONTENT_BYTES {
        let Some(character) = characters.next() else {
            return bounded;
        };
        bounded.push(if character == ' ' || character.is_ascii_graphic() {
            character
        } else {
            ' '
        });
    }
    if characters.next().is_some() {
        bounded.push_str("...");
    }
    bounded
}

use body::BodyLinkContext;
use destination::PushDestination;
use github::Github;
use history::CommitGraphEvidence;
use local::{GherritPrId, LocalStack};
use plan::plan_local_publication;
use remote::{DestinationObservation, RemoteDefault, complete_graph_wave, observe_remote_default};

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

    let public_branch = match State::read_required_from(repo, branch_name)? {
        State::Unmanaged => {
            log::info!("Branch {} is UNMANAGED. Allowing standard push.", branch_name.yellow());
            return Ok(());
        }
        State::Private => None,
        State::Public => Some(branch_name.clone()),
    };
    log::info!("Branch {} is MANAGED. Syncing stack...", branch_name.yellow());

    let configured_remote =
        repo.default_remote_name().wrap_err("Failed to read the configured GHerrit remote")?;
    let destination = PushDestination::resolve(configured_remote).await?;
    let remote_default = observe_remote_default(&destination).await?;
    let git_default_branch = remote_default.default_branch().clone();
    let stack = LocalStack::collect(repo, &git_default_branch, destination.configured_remote())
        .wrap_err("Failed to collect commits")?;
    if stack.is_empty() {
        log::info!("No commits to sync.");
        return Ok(());
    }

    if github_endpoint.is_disabled() {
        bail!("The GHerrit test driver cannot sync PRs without a configured GitHub endpoint");
    }

    // A custom endpoint is an explicit dependency supplied by the caller. The
    // production binary always selects `Production`, so an environment
    // variable cannot redirect a user's token.
    if let Some(api_url) = github_endpoint.custom_url() {
        log::warn!("Using custom GitHub API URL: {}", api_url);
    }
    let github =
        Github::new(util::get_github_token()?, github_endpoint.custom_url(), &destination)?;

    let local_ids = stack.iter().map(|change| change.id().clone()).collect::<Box<[_]>>();
    let graph_roots = std::iter::once(git_default_branch.tip())
        .chain(stack.iter().map(|change| change.head()))
        .collect::<Box<[_]>>();
    let ((observation, graph), pull_requests) = tokio::try_join!(
        observe_local_wave(repo, remote_default, &local_ids, &graph_roots),
        github.observe_local_pull_requests(local_ids.clone()),
    )?;
    let correlated = pull_requests.correlate(observation.default_branch())?;
    let remote = observation.into_active(&local_ids)?;
    let body_context = BodyLinkContext::from_destination(&destination, public_branch)?;
    let plan = plan_local_publication(body_context, stack, correlated, remote, &graph)?;
    plan.execute(&github).await?;

    log::info!("Successfully synced {} commits.", local_ids.len());
    Ok(())
}

async fn observe_local_wave<'destination>(
    repo: &util::Repo,
    remote_default: RemoteDefault<'destination>,
    local_ids: &[GherritPrId],
    graph_roots: &[gix::ObjectId],
) -> Result<(DestinationObservation<'destination>, CommitGraphEvidence)> {
    let observation = remote_default.observe_local_state(local_ids).await?;
    let graph = complete_graph_wave(repo, &observation, local_ids, graph_roots).await?;
    Ok((observation, graph))
}

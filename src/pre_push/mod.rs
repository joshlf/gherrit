use std::collections::HashMap;

use color_eyre::eyre::{Context, Result, bail};
use gix::reference::Category;
use owo_colors::OwoColorize;

use crate::util::{self, HeadState};

mod autosquash;
mod body;
mod destination;
mod github;
// Removed when the activation orchestration consumes the history domain.
#[allow(dead_code)]
mod history;
mod local;
// Activated by the owned-base orchestration cutover after its pure planning
// contract has landed independently.
#[allow(dead_code)]
mod plan;
mod publication;
mod pull_request;
mod reconcile;
mod remote;
// This production boundary is wired into destination commands by the owned-base
// activation change. Keep it independently testable while that cutover is built.
#[allow(dead_code)]
mod subprocess;
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

use body::PrBody;
use destination::{DefaultBranch, PushDestination};
use github::{
    CreatePullRequest, CreatedPullRequest, Github, LegacyGithubObservation,
    OpenPullRequest as PrState, PreparedCreates, PreparedUpdates, UpdatePullRequest,
};
use local::{GherritPrId, LocalStack};
use publication::{PlannedChanges, plan_git_publication};
use pull_request::{InitialPullRequestIdentities, PullRequestIdentity, TerminalHistories};
use reconcile::{CurrentPr, DesiredPr, PrUpdate, link_stack, plan_update};
use remote::{ObservedStack, observe_active_managed_tags, observe_remote_heads};

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

    let configured_remote =
        repo.default_remote_name().wrap_err("Failed to read the configured GHerrit remote")?;
    let destination = PushDestination::resolve(configured_remote).await?;
    let remote_heads = observe_remote_heads(&destination).await?;
    let git_default_branch = remote_heads.default_branch().clone();
    let commits = LocalStack::collect(repo, &git_default_branch, destination.configured_remote())
        .wrap_err("Failed to collect commits")?;

    if commits.is_empty() {
        log::info!("No commits to sync.");
        return Ok(());
    }

    if github_endpoint.is_disabled() {
        bail!("The GHerrit test driver cannot sync PRs without a configured GitHub endpoint");
    }

    // Missing heads are meaningful because the global observation covered the
    // complete namespace. Missing managed-tag namespaces remain an error
    // because only these active IDs were queried. Couple both domains before
    // planning so the complete stack is validated before any write can be
    // exposed.
    let managed_tags =
        observe_active_managed_tags(&destination, commits.iter().map(|change| change.id())).await?;
    let observed = ObservedStack::couple(&commits, &remote_heads, managed_tags)?;
    let publication = plan_git_publication(&observed)?;

    // A custom endpoint is an explicit dependency supplied by the caller. The
    // production binary always selects `Production`, so an environment
    // variable cannot redirect a user's token.
    if let Some(api_url) = github_endpoint.custom_url() {
        log::warn!("Using custom GitHub API URL: {}", api_url);
    }

    let github =
        Github::new(util::get_github_token()?, github_endpoint.custom_url(), &destination)?;
    let gherrit_ids = commits.iter().map(|commit| commit.id().clone()).collect::<Vec<_>>();
    let LegacyGithubObservation {
        repository,
        local_pull_requests,
        initial_identities,
        terminal_histories,
    } = github.observe_legacy_pull_requests(&gherrit_ids).await?;
    let (repository_id, github_default_branch) = repository.into_parts();
    let default_branch = DefaultBranch::agree(git_default_branch, github_default_branch)?;
    let planned_changes = publication.publish().await?;
    let public_branch = public_branch(repo, branch_name);
    let pr_repository = PrRepository {
        destination: &destination,
        node_id: &repository_id,
        default_branch: default_branch.name(),
    };

    let num_commits = commits.len();
    sync_prs(
        &github,
        pr_repository,
        public_branch.as_deref(),
        planned_changes,
        local_pull_requests,
        initial_identities,
        terminal_histories,
    )
    .await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

/// Syncs the local stack of commits with GitHub Pull Requests.
///
/// This function:
/// 1. Finds existing PRs or creates new ones for new commits.
/// 2. Updates PR metadata (title, body, base branch) to match the local stack.
/// 3. Updates are queued and executed in batches to optimize performance.
struct PrRepository<'a> {
    destination: &'a PushDestination,
    node_id: &'a str,
    default_branch: &'a str,
}

async fn sync_prs(
    github: &Github,
    repository: PrRepository<'_>,
    public_branch: Option<&str>,
    planned_changes: PlannedChanges<'_>,
    local_pull_requests: Vec<Option<PrState>>,
    initial_identities: InitialPullRequestIdentities,
    terminal_histories: TerminalHistories,
) -> Result<()> {
    let commits = link_stack(repository.default_branch, planned_changes, |change| {
        change.change().id().as_str().to_owned()
    });
    if commits.len() != local_pull_requests.len() {
        bail!("GitHub observation no longer aligns with the planned local stack");
    }

    enum PrResolution {
        Existing(PrState),
        ToCreate(BatchCreate),
    }

    struct PrProjectionState {
        number: u64,
        node_id: String,
        title: String,
        body: String,
        base_branch: String,
    }

    // 1. Identify existing PRs or queue for creation
    let resolutions: Vec<_> = commits
        .iter()
        .zip(local_pull_requests)
        .map(|(entry, pr)| {
            let c = entry.item.change();

            if let Some(pr) = pr {
                debug_assert_eq!(pr.head_branch, c.id().as_str());
                log::debug!(
                    "Found existing PR #{} for {}",
                    pr.number.green().bold(),
                    c.id().as_str()
                );
                PrResolution::Existing(pr)
            } else {
                log::debug!("No GitHub PR exists for {}; queuing creation...", c.id().as_str());
                PrResolution::ToCreate(BatchCreate {
                    title: c.title().to_owned(),
                    body: c.body().to_owned(),
                    base_branch: entry.base_branch.clone(),
                    id: c.id().clone(),
                })
            }
        })
        .collect();

    // 2. Batch create missing PRs
    let creations = resolutions
        .iter()
        .filter_map(|resolution| match resolution {
            PrResolution::ToCreate(create) => Some(create),
            PrResolution::Existing(_) => None,
        })
        .cloned()
        .collect::<Vec<_>>();
    let num_creations = creations.len();
    let new_prs: HashMap<String, CreatedPullRequest> = {
        let created = if creations.is_empty() {
            if !terminal_histories.is_empty() {
                bail!("GitHub returned terminal history for an already-open pull request");
            }
            Vec::new()
        } else {
            log::info!("Creating {num_creations} PRs...");
            batch_create_prs(
                github,
                repository.node_id,
                creations,
                initial_identities,
                terminal_histories,
            )
            .await?
        };
        assert_eq!(created.len(), num_creations);
        if !created.is_empty() {
            log::info!("Created {num_creations} PRs.");
        }
        created.into_iter().map(|created| (created.head_branch.clone(), created)).collect()
    };

    // 3. Resolve final PR states
    //
    // We zip commits with resolutions. Since resolutions were built in order,
    // they match perfectly.
    let commit_pr_states = commits
        .iter()
        .zip(resolutions)
        .map(|(entry, resolution)| {
            let pr_state = match resolution {
                PrResolution::Existing(state) => PrProjectionState {
                    number: state.number,
                    node_id: state.node_id,
                    title: state.title,
                    body: state.body,
                    base_branch: state.base_branch,
                },
                PrResolution::ToCreate(create) => {
                    let created = new_prs.get(create.id.as_str()).ok_or_else(|| {
                        eyre::eyre!("Failed to resolve created PR for {}", create.id.as_str())
                    })?;
                    log::info!(
                        "Created PR #{}: {}",
                        created.number.green().bold(),
                        repository.destination.pr_url(created.number).blue().underline()
                    );
                    PrProjectionState {
                        number: created.number,
                        node_id: created.node_id.clone(),
                        title: create.title,
                        body: create.body,
                        base_branch: create.base_branch,
                    }
                }
            };
            Ok((entry, pr_state))
        })
        .collect::<Result<Vec<_>>>()?;

    let repo_url = repository.destination.repo_url_relative();
    let stack_pr_numbers =
        commit_pr_states.iter().map(|(_, state)| state.number).collect::<Vec<_>>();
    let updates: Vec<(u64, PrUpdate)> = commit_pr_states
        .iter()
        .filter_map(|(entry, pr_state)| {
            let c = entry.item.change();
            let latest_version = entry.item.version();

            let body = PrBody {
                commit_body: c.body(),
                repo_url: &repo_url,
                public_branch,
                stack_pr_numbers: &stack_pr_numbers,
                current_pr_number: pr_state.number,
                latest_version,
                base_branch: &entry.base_branch,
                gherrit_id: c.id().as_str(),
                parent_id: entry.parent_id.as_deref(),
                child_id: entry.child_id.as_deref(),
            }
            .render();

            let pr_num = pr_state.number.green().bold().to_string();
            let pr_url =
                repository.destination.pr_url(pr_state.number).blue().underline().to_string();

            let update = plan_update(
                CurrentPr {
                    node_id: &pr_state.node_id,
                    title: &pr_state.title,
                    body: &pr_state.body,
                    base_branch: &pr_state.base_branch,
                },
                DesiredPr { title: c.title(), body: &body, base_branch: &entry.base_branch },
            );

            if update.is_some() {
                log::debug!("Queuing update for PR #{}", pr_num);
                log::info!("Queued update for PR #{}: {}", pr_num, pr_url);
            } else {
                log::info!("PR #{} is up to date: {}", pr_num, pr_url);
            }

            update.map(|update| (pr_state.number, update))
        })
        .collect();

    if !updates.is_empty() {
        log::info!("Updating batch of {} PRs...", updates.len());
        batch_update_prs(github, updates).await?;
        log::info!("Batch update complete.");
    }

    Ok(())
}

fn is_private_stack(repo: &util::Repo, branch: &str) -> bool {
    // If pushRemote is set to ".", it is a private loopback stack.
    // If it is unset or anything else (e.g. 'origin'), it is public.
    repo.config_string(&format!("branch.{}.pushRemote", branch))
        .map(|val| val.as_deref() == Some("."))
        .unwrap_or(false)
}

fn public_branch(repo: &util::Repo, branch: &str) -> Option<String> {
    (!is_private_stack(repo, branch)).then(|| {
        let head_ref = repo.head().ok()?.try_into_referent()?;
        let (category, short_name) = head_ref.inner.name.category_and_short_name()?;
        (category == Category::LocalBranch).then(|| short_name.to_string())
    })?
}

/// A request to create a new PR in a batch.
#[derive(Clone)]
struct BatchCreate {
    title: String,
    body: String,
    base_branch: String,
    id: GherritPrId,
}

/// Performs batched updates of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping updates into
/// bounded batches and sending each batch as one GraphQL operation.
async fn batch_update_prs(github: &Github, updates: Vec<(u64, PrUpdate)>) -> Result<()> {
    let updates = updates
        .into_iter()
        .map(|(number, update)| {
            UpdatePullRequest::new(
                PullRequestIdentity::new(number, update.node_id)?,
                update.title,
                update.body,
                update.base_branch,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    github.update_pull_requests(PreparedUpdates::new(updates)?).await.into_result()
}

/// Performs batched creation of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping creations into
/// bounded batches and sending each batch as one GraphQL operation.
///
/// Returns acknowledged created PRs in request order.
async fn batch_create_prs(
    github: &Github,
    repo_id: &str,
    creations: impl IntoIterator<Item = BatchCreate>,
    initial_identities: InitialPullRequestIdentities,
    terminal_histories: TerminalHistories,
) -> Result<Vec<CreatedPullRequest>> {
    let creations = creations
        .into_iter()
        .map(|create| {
            CreatePullRequest::new(
                create.id,
                repo_id.to_string(),
                create.base_branch,
                create.title,
                create.body,
            )
        })
        .collect::<Vec<_>>();
    let expected = terminal_histories.into_legacy_empty_ids()?;
    let prepared = PreparedCreates::new(initial_identities, expected, creations)?;
    Ok(github.create_pull_requests(prepared).await.into_result()?.into_legacy_created())
}

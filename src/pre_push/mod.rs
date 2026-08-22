use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::reference::Category;
use octocrab::Octocrab;
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
mod publication;
mod pull_request;
mod reconcile;
mod remote;
// This production boundary is wired into destination commands by the owned-base
// activation change. Keep it independently testable while that cutover is built.
#[allow(dead_code)]
mod subprocess;
mod version;

use body::PrBody;
use destination::{DefaultBranch, PushDestination};
use github::{
    CreatePullRequest, CreatedPullRequest, FirstOpenPullRequests, FirstOpenPullRequestsPage,
    MutationOperation, NextOpenPullRequests, OpenPullRequest as PrState,
    Repository as GithubRepository, TerminalPullRequestQuery, TerminalPullRequestState,
    TerminalPullRequests, UpdatePullRequest, decode_mutation_batch_response,
    prepare_mutation_batches,
};
use local::{GherritPrId, LocalStack};
use publication::{GitPublicationPlan, PlannedChanges, PushOutcome, plan_git_publication};
use reconcile::{
    CurrentPr, DesiredPr, PrUpdate, RetiredPullRequest, ensure_pull_request_ids_available,
    link_stack, plan_update,
};
use remote::{ObservedStack, observe_active_version_tags, observe_remote_heads};

const INDETERMINATE_GRAPHQL_MUTATION: &str = "GraphQL mutation acknowledgement is indeterminate; stop this publication attempt and retry the push to reobserve GitHub state";

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
    git_default_branch.ensure_local(repo)?;
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
    // complete namespace. Missing version histories remain an error because
    // only these active IDs were queried. Couple both domains before planning
    // so the complete stack is validated before any write can be exposed.
    let versions =
        observe_active_version_tags(&destination, commits.iter().map(|change| change.id())).await?;
    let observed = ObservedStack::couple(&commits, &remote_heads, versions)?;
    let publication = plan_git_publication(&observed)?;

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
    let gherrit_ids = commits.iter().map(|commit| commit.id().clone()).collect::<Vec<_>>();
    let GithubObservation { repository, local_pull_requests, known_pull_request_identities } =
        batch_fetch_prs(&octocrab, &destination, &gherrit_ids).await?;
    let GithubRepository { node_id: repository_id, default_branch: github_default_branch } =
        repository;
    let default_branch = DefaultBranch::agree(git_default_branch, github_default_branch)?;
    let planned_changes = push_to_origin(&destination, publication)?;
    let public_branch = public_branch(repo, branch_name);
    let pr_repository = PrRepository {
        destination: &destination,
        node_id: &repository_id,
        default_branch: default_branch.name(),
    };

    let num_commits = commits.len();
    sync_prs(
        &octocrab,
        pr_repository,
        public_branch.as_deref(),
        planned_changes,
        local_pull_requests,
        known_pull_request_identities,
    )
    .await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

fn push_to_origin<'stack>(
    destination: &PushDestination,
    publication: GitPublicationPlan<'stack>,
) -> Result<PlannedChanges<'stack>> {
    let (pushes, changes) = publication.into_parts();
    // Render and validate every request before the first push. A duplicate
    // planned destination in a later batch must not be discovered after an
    // earlier batch has already changed the remote.
    let requests =
        pushes.into_iter().map(publication::PushPlan::into_request).collect::<Result<Vec<_>>>()?;
    for request in requests {
        log::info!("Pushing chunk to remote...");
        let output = destination
            .push(request.options(), request.refspecs())
            .output()
            .map_err(|error| {
                eyre!(
                    "Could not execute or acknowledge `git push` for GHerrit remote '{}'; remote refs may or may not have changed. Run GHerrit again to observe them before continuing: {error}",
                    destination.configured_remote()
                )
            })?;
        if request.outcome(&output.status, &output.stdout) == PushOutcome::Indeterminate {
            bail!(
                "Could not acknowledge `git push` for GHerrit remote '{}'; remote refs may or may not have changed. Run GHerrit again to observe them before continuing.",
                destination.configured_remote()
            );
        }
    }

    Ok(changes)
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

struct GithubObservation {
    repository: GithubRepository,
    local_pull_requests: Vec<Option<PrState>>,
    known_pull_request_identities: KnownPullRequestIdentities,
}

async fn sync_prs(
    octocrab: &Octocrab,
    repository: PrRepository<'_>,
    public_branch: Option<&str>,
    planned_changes: PlannedChanges<'_>,
    local_pull_requests: Vec<Option<PrState>>,
    known_pull_request_identities: KnownPullRequestIdentities,
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
                    head_branch: c.id().as_str().to_owned(),
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
    let new_prs = if !creations.is_empty() {
        log::info!("Creating {num_creations} PRs...");
        let created = batch_create_prs(
            octocrab,
            repository.node_id,
            creations,
            known_pull_request_identities,
        )
        .await?;
        assert_eq!(created.len(), num_creations);
        log::info!("Created {num_creations} PRs.");
        created.into_iter().map(|created| (created.head_branch.clone(), created)).collect()
    } else {
        HashMap::new()
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
                    let created = new_prs.get(&create.head_branch).ok_or_else(|| {
                        eyre::eyre!("Failed to resolve created PR for {}", create.head_branch)
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
    let updates: Vec<PrUpdate> = commit_pr_states
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

            update
        })
        .collect();

    if !updates.is_empty() {
        log::info!("Updating batch of {} PRs...", updates.len());
        batch_update_prs(octocrab, updates).await?;
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
    head_branch: String,
}

/// Pull request numbers and node IDs which are already occupied.
///
/// The two namespaces are independent: publication needs only to prove that
/// each value is new, not to recover a number-to-node pairing from these sets.
#[derive(Debug, Default)]
struct KnownPullRequestIdentities {
    numbers: HashSet<u64>,
    node_ids: HashSet<String>,
}

impl KnownPullRequestIdentities {
    fn insert_open(&mut self, number: u64, node_id: String) -> Result<()> {
        if !self.numbers.insert(number) {
            bail!("GitHub returned duplicate open pull request number {number}");
        }
        if !self.node_ids.insert(node_id.clone()) {
            bail!("GitHub returned duplicate open pull request node ID '{node_id}'");
        }
        Ok(())
    }

    fn insert_created(&mut self, receipts: &[CreatedPullRequest]) -> Result<()> {
        for receipt in receipts {
            if !self.numbers.insert(receipt.number) {
                bail!(
                    "createPullRequest receipt for '{}' repeats known pull request number {}",
                    receipt.head_branch,
                    receipt.number
                );
            }
            if !self.node_ids.insert(receipt.node_id.clone()) {
                bail!(
                    "createPullRequest receipt for '{}' repeats known pull request node ID '{}'",
                    receipt.head_branch,
                    receipt.node_id
                );
            }
        }
        Ok(())
    }
}

/// Performs batched updates of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping updates into
/// bounded batches and sending each batch as one GraphQL operation.
async fn batch_update_prs(octocrab: &Octocrab, updates: Vec<PrUpdate>) -> Result<()> {
    let updates = updates
        .into_iter()
        .map(|update| {
            UpdatePullRequest::new(update.node_id, update.title, update.body, update.base_branch)
        })
        .collect::<Result<Vec<_>>>()?;
    run_graphql_mutations(octocrab, updates).await?;
    Ok(())
}

/// Performs batched creation of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping creations into
/// bounded batches and sending each batch as one GraphQL operation.
///
/// Returns acknowledged created PRs in request order.
async fn batch_create_prs(
    octocrab: &Octocrab,
    repo_id: &str,
    creations: impl IntoIterator<Item = BatchCreate>,
    mut known_identities: KnownPullRequestIdentities,
) -> Result<Vec<CreatedPullRequest>> {
    let creations = creations.into_iter().map(|create| {
        CreatePullRequest::new(
            repo_id.to_string(),
            create.base_branch,
            create.head_branch,
            create.title,
            create.body,
        )
    });
    run_graphql_mutations_with_receipt_validation(octocrab, creations, move |receipts| {
        known_identities.insert_created(receipts)
    })
    .await
}

async fn batch_fetch_prs(
    octocrab: &Octocrab,
    destination: &PushDestination,
    head_refs: &[GherritPrId],
) -> Result<GithubObservation> {
    let owner = destination.owner();
    let repo_name = destination.repository();
    let (repository, pull_requests, known_pull_request_identities) =
        observe_open_pull_requests(octocrab, owner, repo_name).await?;
    let local_ids = head_refs.iter().map(GherritPrId::as_str).collect::<HashSet<_>>();
    let mut candidates = HashMap::<String, Vec<PrState>>::new();

    for pull_request in pull_requests.into_iter().filter(|pr| !pr.is_cross_repository) {
        if local_ids.contains(pull_request.head_branch.as_str()) {
            candidates.entry(pull_request.head_branch.clone()).or_default().push(pull_request);
        }
    }

    let mut selected = Vec::with_capacity(head_refs.len());
    let mut missing = Vec::new();
    for head_ref in head_refs {
        match candidates.remove(head_ref.as_str()) {
            None => {
                missing.push(head_ref.as_str().to_owned());
                selected.push(None);
            }
            Some(mut candidates) if candidates.len() == 1 => {
                selected.push(Some(candidates.pop().expect("one candidate is present")));
            }
            Some(candidates) => {
                let mut numbers = candidates.iter().map(|pr| pr.number).collect::<Vec<_>>();
                numbers.sort_unstable();
                let numbers = numbers
                    .into_iter()
                    .map(|number| format!("#{number}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "Found multiple open pull requests for GHerrit ID '{}': {numbers}. GHerrit cannot safely choose one.",
                    head_ref.as_str()
                );
            }
        }
    }

    let retired = observe_terminal_pull_requests(octocrab, owner, repo_name, missing).await?;
    ensure_pull_request_ids_available(retired)?;
    Ok(GithubObservation {
        repository,
        local_pull_requests: selected,
        known_pull_request_identities,
    })
}

/// Observes every page in GitHub's repository-wide open PR connection.
///
/// The compatibility selection in `batch_fetch_prs` deliberately knows only
/// legacy head names. This repository-wide observation stays separate so it
/// can retain every managed identity without inheriting that selection rule.
async fn observe_open_pull_requests(
    octocrab: &Octocrab,
    owner: &str,
    repository: &str,
) -> Result<(GithubRepository, Vec<PrState>, KnownPullRequestIdentities)> {
    let mut page_len = 100;
    let first_page = loop {
        let operation =
            FirstOpenPullRequests::new(owner.to_owned(), repository.to_owned(), page_len);
        let Some(response) = run_graphql_observation_query(octocrab, &operation.document()).await?
        else {
            if page_len == 1 {
                bail!("The repository-wide open pull request query exceeds GitHub resource limits");
            }
            let retry_page_len = page_len / 2;
            log::warn!(
                "Backing off the repository-wide open pull request page size from {page_len} to {retry_page_len}."
            );
            page_len = retry_page_len;
            continue;
        };
        break operation.decode(response)?;
    };
    let FirstOpenPullRequestsPage {
        repository: observed_repository,
        pull_requests: first_pull_requests,
        next_cursor,
    } = first_page;
    let mut seen_cursors = next_cursor.iter().cloned().collect::<HashSet<_>>();
    let mut known_pull_request_identities = KnownPullRequestIdentities::default();
    let mut pull_requests = Vec::new();
    record_open_pull_requests(
        first_pull_requests,
        &mut known_pull_request_identities,
        &mut pull_requests,
    )?;

    let mut cursor = next_cursor;
    while let Some(current_cursor) = cursor {
        let operation = NextOpenPullRequests::new(
            owner.to_owned(),
            repository.to_owned(),
            current_cursor.clone(),
            page_len,
        );
        let Some(response) = run_graphql_observation_query(octocrab, &operation.document()).await?
        else {
            if page_len == 1 {
                bail!("The repository-wide open pull request query exceeds GitHub resource limits");
            }
            let retry_page_len = page_len / 2;
            log::warn!(
                "Backing off the repository-wide open pull request page size from {page_len} to {retry_page_len}."
            );
            page_len = retry_page_len;
            cursor = Some(current_cursor);
            continue;
        };
        let page = operation.decode(response)?;
        record_open_pull_requests(
            page.pull_requests,
            &mut known_pull_request_identities,
            &mut pull_requests,
        )?;
        if let Some(next_cursor) = &page.next_cursor
            && !seen_cursors.insert(next_cursor.clone())
        {
            bail!("GitHub repeated an open pull request pagination cursor");
        }
        cursor = page.next_cursor;
    }

    Ok((observed_repository, pull_requests, known_pull_request_identities))
}

fn record_open_pull_requests(
    page_pull_requests: Vec<PrState>,
    known_identities: &mut KnownPullRequestIdentities,
    pull_requests: &mut Vec<PrState>,
) -> Result<()> {
    for pull_request in page_pull_requests {
        known_identities.insert_open(pull_request.number, pull_request.node_id.clone())?;
        pull_requests.push(pull_request);
    }
    Ok(())
}

/// Returns terminal lifecycle evidence for local IDs absent from the open scan.
async fn observe_terminal_pull_requests(
    octocrab: &Octocrab,
    owner: &str,
    repository: &str,
    ids: Vec<String>,
) -> Result<Vec<RetiredPullRequest>> {
    #[derive(Debug)]
    struct Pending {
        id: String,
        cursor: Option<String>,
        seen_cursors: HashSet<String>,
        terminal_pull_request: Option<github::TerminalPullRequest>,
    }

    let mut pending = ids
        .into_iter()
        .map(|id| Pending {
            id,
            cursor: None,
            seen_cursors: HashSet::new(),
            terminal_pull_request: None,
        })
        .collect::<VecDeque<_>>();
    let mut batch_len = TerminalPullRequests::MAX_ALIASES;
    let mut page_len = 100;
    let mut retired = Vec::new();
    let mut numbers = HashSet::new();
    let mut node_ids = HashSet::new();

    while !pending.is_empty() {
        let count = pending.len().min(batch_len);
        let batch = pending.drain(..count).collect::<Vec<_>>();
        let operations = batch
            .iter()
            .map(|pending| {
                TerminalPullRequestQuery::new(pending.id.clone(), pending.cursor.clone(), page_len)
            })
            .collect::<Result<Vec<_>>>()?;
        let operation =
            TerminalPullRequests::new(owner.to_owned(), repository.to_owned(), operations)?;
        let response = run_graphql_observation_query(octocrab, &operation.document()).await?;
        let Some(response) = response else {
            if batch.len() == 1 {
                if page_len == 1 {
                    bail!(
                        "GitHub terminal pull request query for '{}' exceeds resource limits",
                        batch[0].id
                    );
                }
                let retry_page_len = page_len / 2;
                log::warn!(
                    "Backing off terminal pull request page size from {page_len} to {retry_page_len}."
                );
                page_len = retry_page_len;
                pending.push_front(batch.into_iter().next().expect("one terminal query"));
                continue;
            }
            let retry_batch_len = batch.len() / 2;
            log::warn!("Hit GitHub resource limit with GraphQL batch of size {}", batch.len());
            log::warn!("Backing off GraphQL batch size from {} to {retry_batch_len}.", batch.len());
            batch_len = retry_batch_len;
            for pending_item in batch.into_iter().rev() {
                pending.push_front(pending_item);
            }
            continue;
        };
        let data =
            response.get("data").and_then(|data| data.get("repository")).cloned().ok_or_else(
                || eyre!("GitHub terminal pull request response is missing repository data"),
            )?;
        for (mut pending_item, page) in batch.into_iter().zip(operation.decode(data)?) {
            for pull_request in page.pull_requests {
                if !numbers.insert(pull_request.number) {
                    bail!(
                        "GitHub returned duplicate same-repository terminal pull request number {}",
                        pull_request.number
                    );
                }
                if !node_ids.insert(pull_request.node_id.clone()) {
                    bail!(
                        "GitHub returned duplicate same-repository terminal pull request node ID '{}'",
                        pull_request.node_id
                    );
                }
                record_terminal_pull_request(
                    &pending_item.id,
                    &mut pending_item.terminal_pull_request,
                    pull_request,
                )?;
            }
            if let Some(cursor) = page.next_cursor {
                if !pending_item.seen_cursors.insert(cursor.clone()) {
                    bail!(
                        "GitHub repeated a terminal pull request pagination cursor for '{}'",
                        pending_item.id
                    );
                }
                pending_item.cursor = Some(cursor);
                pending.push_back(pending_item);
            } else if let Some(pull_request) = pending_item.terminal_pull_request {
                let pull_request = match pull_request.state {
                    TerminalPullRequestState::Closed => {
                        RetiredPullRequest::Closed { number: pull_request.number }
                    }
                    TerminalPullRequestState::Merged => {
                        RetiredPullRequest::Merged { number: pull_request.number }
                    }
                };
                retired.push(pull_request);
            }
        }
    }
    Ok(retired)
}

fn record_terminal_pull_request(
    change_id: &str,
    observed: &mut Option<github::TerminalPullRequest>,
    pull_request: github::TerminalPullRequest,
) -> Result<()> {
    if let Some(previous) = observed {
        bail!(
            "Found multiple historical pull requests for GHerrit ID '{change_id}': #{}, #{}. GHerrit cannot safely choose one.",
            previous.number,
            pull_request.number
        );
    }
    *observed = Some(pull_request);
    Ok(())
}

/// Executes mutation batches without retrying after transmission.
///
/// Every request is prepared before the first write. Once a request has been
/// sent, any failure to validate its complete receipt is indeterminate, so the
/// caller stops and a later pre-push attempt must start from fresh observation.
async fn run_graphql_mutations<O>(
    octocrab: &Octocrab,
    operations: impl IntoIterator<Item = O>,
) -> Result<Vec<O::Output>>
where
    O: MutationOperation,
{
    run_graphql_mutations_with_receipt_validation(octocrab, operations, |_| Ok(())).await
}

/// Executes mutation batches and validates each receipt batch before another
/// batch can be transmitted.
///
/// `decode_mutation_batch_response` validates the receipt fields and any
/// identities shared within one response. The callback can retain additional
/// state to validate facts shared with prior observation or prior batches.
async fn run_graphql_mutations_with_receipt_validation<O>(
    octocrab: &Octocrab,
    operations: impl IntoIterator<Item = O>,
    mut validate_receipts: impl FnMut(&[O::Output]) -> Result<()>,
) -> Result<Vec<O::Output>>
where
    O: MutationOperation,
{
    let operations = operations.into_iter().collect::<Vec<_>>();
    let batches = prepare_mutation_batches(&operations)?;
    let mut outputs = Vec::with_capacity(operations.len());

    for batch in batches {
        let operations = &operations[batch.operation_range];
        log::trace!(
            "Sending GraphQL mutation batch ({} operations, {} bytes)",
            operations.len(),
            batch.serialized_bytes
        );
        let response =
            octocrab.graphql(&batch.request).await.wrap_err(INDETERMINATE_GRAPHQL_MUTATION)?;
        let receipts = decode_mutation_batch_response(operations, response)
            .wrap_err(INDETERMINATE_GRAPHQL_MUTATION)?;
        validate_receipts(&receipts).wrap_err(INDETERMINATE_GRAPHQL_MUTATION)?;
        outputs.extend(receipts);
    }

    Ok(outputs)
}

// These delays pace only transient transport and HTTP response retries. They
// are deliberately small because a pre-push hook is interactive. Adaptive
// query-size reduction changes the request instead of waiting, and mutations
// do not call this policy and remain at-most-once.
const GRAPHQL_QUERY_RETRY_DELAYS: [Duration; 3] =
    [Duration::from_millis(100), Duration::from_millis(200), Duration::from_millis(400)];

fn graphql_query_retry_delay(completed_retries: usize) -> Option<Duration> {
    GRAPHQL_QUERY_RETRY_DELAYS.get(completed_retries).copied()
}

/// The asynchronous delay boundary for transient read-request retries.
async fn wait_before_graphql_query_retry(completed_retries: &mut usize, failure: &str) -> bool {
    let Some(delay) = graphql_query_retry_delay(*completed_retries) else {
        return false;
    };
    *completed_retries += 1;
    log::warn!(
        "Retrying read-only GraphQL request after {failure} ({}/{}) in {} ms",
        *completed_retries,
        GRAPHQL_QUERY_RETRY_DELAYS.len(),
        delay.as_millis()
    );
    tokio::time::sleep(delay).await;
    true
}

fn is_retryable_query_transport_error(error: &octocrab::Error) -> bool {
    matches!(error, octocrab::Error::Service { .. } | octocrab::Error::Hyper { .. })
}

/// Executes one read-only GraphQL request with bounded transport retries.
///
/// Octocrab's method-agnostic retry middleware is disabled because it can
/// replay mutation POSTs. Keeping retries here makes read-only intent explicit:
/// connection failures, response-body transport failures, HTTP 429, and HTTP
/// 5xx responses get three paced retries. Redirects are never followed.
async fn run_graphql_query(
    octocrab: &Octocrab,
    request: &serde_json::Value,
) -> octocrab::Result<serde_json::Value> {
    let mut retries = 0;

    loop {
        let response = match octocrab._post("/graphql", Some(request)).await {
            Ok(response) => response,
            Err(error) if is_retryable_query_transport_error(&error) => {
                if wait_before_graphql_query_retry(&mut retries, "a transport failure").await {
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };

        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            let failure = format!("HTTP {status}");
            if wait_before_graphql_query_retry(&mut retries, &failure).await {
                continue;
            }
        }

        let response = octocrab::map_github_error(response).await;
        let response = match response {
            Ok(response) => {
                <serde_json::Value as octocrab::FromResponse>::from_response(response).await
            }
            Err(error) => Err(error),
        };
        match response {
            Err(error) if is_retryable_query_transport_error(&error) => {
                if !wait_before_graphql_query_retry(
                    &mut retries,
                    "a response-body transport failure",
                )
                .await
                {
                    return Err(error);
                }
            }
            response => return response,
        }
    }
}

/// Sends one read-only document under the shared bounded retry policy.
///
/// `None` is an explicit resource-limit result so callers that can split a
/// logical observation do so before retrying it. Pagination itself is never
/// retried as a different cursor.
const MAX_GRAPHQL_QUERY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseDisposition {
    Success,
    RetryLimit,
    Fatal,
}

fn query_exceeds_limit(query: &str) -> bool {
    query.len() > MAX_GRAPHQL_QUERY_BYTES
}

fn classify_response(response: &serde_json::Value) -> ResponseDisposition {
    let Some(errors) = response.get("errors") else {
        return ResponseDisposition::Success;
    };
    let has_no_data = response.get("data").is_none_or(serde_json::Value::is_null);
    let has_only_resource_errors = errors
        .as_array()
        .is_some_and(|errors| !errors.is_empty() && errors.iter().all(is_resource_limit_error));

    if has_no_data && has_only_resource_errors {
        ResponseDisposition::RetryLimit
    } else {
        ResponseDisposition::Fatal
    }
}

fn is_resource_limit_error(error: &serde_json::Value) -> bool {
    let is_typed_resource_error = matches!(
        error.get("type").and_then(serde_json::Value::as_str),
        Some("RESOURCE_LIMITS_EXCEEDED" | "MAX_NODE_LIMIT_EXCEEDED")
    );
    // GitHub middleware has also returned this parse error after silently
    // dropping or truncating an oversized request.
    let is_oversized_request_error = matches!(
        error.get("message").and_then(serde_json::Value::as_str),
        Some("A query attribute must be specified and must be a string.")
    );

    is_typed_resource_error || is_oversized_request_error
}

async fn run_graphql_observation_query(
    octocrab: &Octocrab,
    query: &str,
) -> Result<Option<serde_json::Value>> {
    if query_exceeds_limit(query) {
        return Ok(None);
    }
    let request = serde_json::json!({ "query": query });
    let response = run_graphql_query(octocrab, &request)
        .await
        .wrap_err("GraphQL read-only observation failed")?;
    match classify_response(&response) {
        ResponseDisposition::Success => Ok(Some(response)),
        ResponseDisposition::RetryLimit => Ok(None),
        ResponseDisposition::Fatal => {
            let errors = response.get("errors").expect("fatal response has errors");
            bail!("GraphQL errors: {errors:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        time::Instant,
    };

    use super::*;
    use crate::pre_push::github::MAX_MUTATION_ALIASES;

    const ADAPTER_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_TEST_REQUEST_BYTES: usize = 1024 * 1024;

    async fn read_json_request(stream: &mut TcpStream) -> Value {
        let mut request = Vec::new();
        let (body_start, content_length) = loop {
            let mut chunk = [0; 4096];
            let read = stream.read(&mut chunk).await.expect("read HTTP request");
            assert_ne!(read, 0, "connection closed before request headers completed");
            request.extend_from_slice(&chunk[..read]);
            assert!(
                request.len() <= MAX_TEST_REQUEST_BYTES,
                "HTTP request exceeded the test server limit"
            );

            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).expect("ASCII HTTP headers");
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().expect("numeric Content-Length"))
                .expect("Octocrab request has Content-Length");
            break (header_end + 4, content_length);
        };

        while request.len() < body_start + content_length {
            let mut chunk = [0; 4096];
            let read = stream.read(&mut chunk).await.expect("read HTTP request body");
            assert_ne!(read, 0, "connection closed before request body completed");
            request.extend_from_slice(&chunk[..read]);
            assert!(
                request.len() <= MAX_TEST_REQUEST_BYTES,
                "HTTP request exceeded the test server limit"
            );
        }

        serde_json::from_slice(&request[body_start..body_start + content_length])
            .expect("JSON request body")
    }

    fn http_response_bytes(status: &str, body: &[u8], content_length: usize) -> Vec<u8> {
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
        );
        [headers.as_bytes(), body].concat()
    }

    async fn write_http_response_bytes(
        stream: &mut TcpStream,
        status: &str,
        body: &[u8],
        content_length: usize,
    ) {
        stream
            .write_all(&http_response_bytes(status, body, content_length))
            .await
            .expect("write HTTP response");
    }

    async fn write_http_response(
        stream: &mut TcpStream,
        status: &str,
        body: &[u8],
        content_length: usize,
    ) {
        write_http_response_bytes(stream, status, body, content_length).await;
        stream.shutdown().await.expect("finish HTTP response");
    }

    async fn write_json_response(stream: &mut TcpStream, response: &Value) {
        let body = serde_json::to_vec(response).expect("serialize JSON response");
        write_http_response(stream, "200 OK", &body, body.len()).await;
    }

    fn test_octocrab(listener: &TcpListener) -> Octocrab {
        Octocrab::builder()
            .base_uri(format!("http://{}", listener.local_addr().expect("listener address")))
            .expect("valid test endpoint")
            .build()
            .expect("build test client")
    }

    async fn scripted_graphql(
        responses: Vec<Value>,
    ) -> (Octocrab, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let octocrab = test_octocrab(&listener);
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let (mut stream, _) = listener.accept().await.expect("accept GraphQL request");
                requests.push(read_json_request(&mut stream).await);
                write_json_response(&mut stream, &response).await;
            }
            requests
        });
        (octocrab, server)
    }

    async fn finish_scripted_graphql(server: tokio::task::JoinHandle<Vec<Value>>) -> Vec<Value> {
        tokio::time::timeout(ADAPTER_TIMEOUT, server)
            .await
            .expect("scripted GraphQL server stopped")
            .expect("scripted GraphQL server completed")
    }

    fn request_query(request: &Value) -> &str {
        request.get("query").and_then(Value::as_str).expect("GraphQL request query")
    }

    fn open_observation_node(number: u64, head: &str) -> Value {
        json!({
            "number": number,
            "id": format!("PR_{number}"),
            "title": format!("Title {number}"),
            "body": format!("Body {number}"),
            "baseRefName": "main",
            "baseRefOid": "1".repeat(40),
            "headRefName": head,
            "headRefOid": "2".repeat(40),
            "state": "OPEN",
            "isCrossRepository": false,
            "autoMergeRequest": null,
            "isInMergeQueue": false,
        })
    }

    fn open_observation_page(
        nodes: Vec<Value>,
        next_cursor: Option<&str>,
        include_repository: bool,
    ) -> Value {
        let mut repository = serde_json::Map::from_iter([(
            "pullRequests".to_string(),
            json!({
                "nodes": nodes,
                "pageInfo": {
                    "hasNextPage": next_cursor.is_some(),
                    "endCursor": next_cursor,
                },
            }),
        )]);
        if include_repository {
            repository.insert("id".to_string(), json!("R_1"));
            repository.insert(
                "defaultBranchRef".to_string(),
                json!({ "name": "main", "target": { "oid": "3".repeat(40) } }),
            );
        }
        json!({ "data": { "repository": Value::Object(repository) } })
    }

    fn terminal_observation_node(number: u64, head: &str, is_cross_repository: bool) -> Value {
        json!({
            "number": number,
            "id": format!("PR_{number}"),
            "headRefName": head,
            "state": "CLOSED",
            "isCrossRepository": is_cross_repository,
        })
    }

    fn terminal_observation_page(pages: Vec<(Vec<Value>, Option<&str>)>) -> Value {
        let repository = pages
            .into_iter()
            .enumerate()
            .map(|(index, (nodes, next_cursor))| {
                (
                    format!("op{index}"),
                    json!({
                        "nodes": nodes,
                        "pageInfo": {
                            "hasNextPage": next_cursor.is_some(),
                            "endCursor": next_cursor,
                        },
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        json!({ "data": { "repository": Value::Object(repository) } })
    }

    fn resource_limit_response() -> Value {
        json!({
            "errors": [{
                "type": "RESOURCE_LIMITS_EXCEEDED",
                "message": "scripted resource limit",
            }],
        })
    }

    #[test]
    fn query_retry_delay_policy_is_exact_nonzero_and_bounded() {
        assert_eq!(
            (0..=GRAPHQL_QUERY_RETRY_DELAYS.len())
                .map(graphql_query_retry_delay)
                .collect::<Vec<_>>(),
            [
                Some(Duration::from_millis(100)),
                Some(Duration::from_millis(200)),
                Some(Duration::from_millis(400)),
                None,
            ]
        );
        assert_eq!(graphql_query_retry_delay(usize::MAX), None);
        assert!(GRAPHQL_QUERY_RETRY_DELAYS.into_iter().all(|delay| !delay.is_zero()));
    }

    #[test]
    fn query_limit_is_inclusive() {
        assert!(!query_exceeds_limit(&"x".repeat(MAX_GRAPHQL_QUERY_BYTES)));
        assert!(query_exceeds_limit(&"x".repeat(MAX_GRAPHQL_QUERY_BYTES + 1)));
    }

    #[test]
    fn classifies_resource_limit_responses() {
        for error in [
            json!({ "type": "RESOURCE_LIMITS_EXCEEDED" }),
            json!({ "type": "MAX_NODE_LIMIT_EXCEEDED" }),
            json!({
                "message": "A query attribute must be specified and must be a string."
            }),
        ] {
            assert_eq!(
                classify_response(&json!({ "errors": [error] })),
                ResponseDisposition::RetryLimit
            );
            assert_eq!(
                classify_response(&json!({ "data": null, "errors": [error] })),
                ResponseDisposition::RetryLimit
            );
        }
    }

    #[test]
    fn treats_partial_or_mixed_query_errors_as_fatal() {
        let resource_error = json!({ "type": "RESOURCE_LIMITS_EXCEEDED" });
        let fatal_error = json!({ "type": "FORBIDDEN" });

        for response in [
            json!({ "errors": [fatal_error.clone()] }),
            json!({ "errors": [resource_error.clone(), fatal_error] }),
            json!({ "errors": [] }),
            json!({ "errors": "not an array" }),
            json!({ "data": {}, "errors": [resource_error] }),
        ] {
            assert_eq!(classify_response(&response), ResponseDisposition::Fatal);
        }
    }

    #[tokio::test]
    async fn open_observation_pages_with_the_exact_cursor_and_rejects_a_loop() {
        let (octocrab, server) = scripted_graphql(vec![
            open_observation_page(vec![open_observation_node(1, "G1")], Some("cursor-1"), true),
            open_observation_page(vec![open_observation_node(2, "G2")], Some("cursor-1"), false),
        ])
        .await;

        let error = observe_open_pull_requests(&octocrab, "owner", "repo")
            .await
            .expect_err("a repeated cursor must stop observation");
        let requests = finish_scripted_graphql(server).await;

        assert!(error.to_string().contains("repeated an open pull request pagination cursor"));
        assert_eq!(requests.len(), 2);
        assert!(!request_query(&requests[0]).contains("after:"));
        assert!(request_query(&requests[1]).contains("after: \"cursor-1\""));
        assert!(!request_query(&requests[1]).contains("defaultBranchRef"));
    }

    #[tokio::test]
    async fn open_observation_rejects_duplicate_identity_across_pages() {
        for collision in [CreateReceiptCollision::Number, CreateReceiptCollision::NodeId] {
            for duplicate_has_fork_head in [false, true] {
                let mut duplicate = open_observation_node(2, "G2");
                duplicate["isCrossRepository"] = json!(duplicate_has_fork_head);
                let expected = match collision {
                    CreateReceiptCollision::Number => {
                        duplicate["number"] = json!(1);
                        "duplicate open pull request number 1"
                    }
                    CreateReceiptCollision::NodeId => {
                        duplicate["id"] = json!("PR_1");
                        "duplicate open pull request node ID 'PR_1'"
                    }
                };
                let (octocrab, server) = scripted_graphql(vec![
                    open_observation_page(
                        vec![open_observation_node(1, "G1")],
                        Some("cursor-1"),
                        true,
                    ),
                    open_observation_page(vec![duplicate], None, false),
                ])
                .await;

                let error = observe_open_pull_requests(&octocrab, "owner", "repo")
                    .await
                    .expect_err("one PR identity cannot occur on two pages");
                finish_scripted_graphql(server).await;

                assert!(error.to_string().contains(expected), "error={error:?}");
            }
        }
    }

    #[tokio::test]
    async fn terminal_observation_rejects_ambiguity_discovered_on_a_later_page() {
        let (octocrab, server) = scripted_graphql(vec![
            terminal_observation_page(vec![(
                vec![terminal_observation_node(7, "G42", false)],
                Some("cursor-1"),
            )]),
            terminal_observation_page(vec![(
                vec![terminal_observation_node(9, "G42", false)],
                None,
            )]),
        ])
        .await;

        let error =
            observe_terminal_pull_requests(&octocrab, "owner", "repo", vec!["G42".to_string()])
                .await
                .expect_err("the first historical PR must remain known across pages");
        let requests = finish_scripted_graphql(server).await;

        assert_eq!(
            error.to_string(),
            "Found multiple historical pull requests for GHerrit ID 'G42': #7, #9. GHerrit cannot safely choose one."
        );
        assert!(request_query(&requests[1]).contains("after: \"cursor-1\""));
    }

    #[tokio::test]
    async fn terminal_observation_ignores_forks_and_resumes_only_unfinished_ids() {
        let (octocrab, server) = scripted_graphql(vec![
            terminal_observation_page(vec![
                (vec![terminal_observation_node(7, "G1", true)], Some("cursor-1")),
                (vec![terminal_observation_node(8, "G2", true)], Some("cursor-2")),
                (vec![], None),
            ]),
            terminal_observation_page(vec![
                (vec![], None),
                (vec![terminal_observation_node(9, "G2", false)], None),
            ]),
        ])
        .await;

        let retired = observe_terminal_pull_requests(
            &octocrab,
            "owner",
            "repo",
            vec!["G1".to_string(), "G2".to_string(), "G3".to_string()],
        )
        .await
        .unwrap();
        let requests = finish_scripted_graphql(server).await;

        assert_eq!(retired, [RetiredPullRequest::Closed { number: 9 }]);
        assert!(request_query(&requests[0]).contains("G1"));
        assert!(request_query(&requests[0]).contains("G2"));
        assert!(request_query(&requests[0]).contains("G3"));
        assert!(request_query(&requests[1]).contains("G1"));
        assert!(request_query(&requests[1]).contains("G2"));
        assert!(!request_query(&requests[1]).contains("G3"));
        assert!(request_query(&requests[1]).contains("after: \"cursor-1\""));
        assert!(request_query(&requests[1]).contains("after: \"cursor-2\""));
    }

    #[tokio::test]
    async fn terminal_observation_rejects_a_repeated_per_id_cursor() {
        let (octocrab, server) = scripted_graphql(vec![
            terminal_observation_page(vec![(vec![], Some("cursor-1"))]),
            terminal_observation_page(vec![(vec![], Some("cursor-1"))]),
        ])
        .await;

        let error =
            observe_terminal_pull_requests(&octocrab, "owner", "repo", vec!["G1".to_string()])
                .await
                .expect_err("a repeated terminal cursor must stop observation");
        finish_scripted_graphql(server).await;

        assert!(error.to_string().contains("repeated a terminal pull request pagination cursor"));
    }

    #[tokio::test]
    async fn observation_reduces_connection_page_sizes_after_resource_limits() {
        let (open_octocrab, open_server) = scripted_graphql(vec![
            open_observation_page(vec![], Some("cursor-1"), true),
            resource_limit_response(),
            open_observation_page(vec![], None, false),
        ])
        .await;
        observe_open_pull_requests(&open_octocrab, "owner", "repo").await.unwrap();
        let open_requests = finish_scripted_graphql(open_server).await;
        assert!(request_query(&open_requests[0]).contains("first: 100"));
        assert!(request_query(&open_requests[1]).contains("first: 100"));
        assert!(request_query(&open_requests[1]).contains("after: \"cursor-1\""));
        assert!(request_query(&open_requests[2]).contains("first: 50"));
        assert!(request_query(&open_requests[2]).contains("after: \"cursor-1\""));

        let (terminal_octocrab, terminal_server) = scripted_graphql(vec![
            terminal_observation_page(vec![(
                vec![terminal_observation_node(7, "G42", false)],
                Some("cursor-1"),
            )]),
            resource_limit_response(),
            terminal_observation_page(vec![(vec![], None)]),
        ])
        .await;
        let retired = observe_terminal_pull_requests(
            &terminal_octocrab,
            "owner",
            "repo",
            vec!["G42".to_string()],
        )
        .await
        .unwrap();
        let terminal_requests = finish_scripted_graphql(terminal_server).await;
        assert_eq!(retired, [RetiredPullRequest::Closed { number: 7 }]);
        assert!(request_query(&terminal_requests[0]).contains("first: 100"));
        assert!(request_query(&terminal_requests[1]).contains("first: 100"));
        assert!(request_query(&terminal_requests[1]).contains("after: \"cursor-1\""));
        assert!(request_query(&terminal_requests[2]).contains("first: 50"));
        assert!(request_query(&terminal_requests[2]).contains("after: \"cursor-1\""));
    }

    #[test]
    fn terminal_history_rejects_a_second_candidate_from_any_page() {
        let pull_request = |number| github::TerminalPullRequest {
            number,
            node_id: format!("PR_{number}"),
            state: TerminalPullRequestState::Closed,
        };
        let mut observed = None;

        record_terminal_pull_request("G42", &mut observed, pull_request(7)).unwrap();
        let error = record_terminal_pull_request("G42", &mut observed, pull_request(9))
            .expect_err("a later page may not add another same-repository candidate");

        assert_eq!(
            error.to_string(),
            "Found multiple historical pull requests for GHerrit ID 'G42': #7, #9. GHerrit cannot safely choose one."
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum RetryableQueryFailure {
        BeforeHeaders,
        DuringBody,
        TooManyRequests,
        ServerError,
    }

    impl RetryableQueryFailure {
        async fn respond(self, stream: TcpStream) -> Instant {
            let response = match self {
                Self::BeforeHeaders => None,
                Self::DuringBody => Some(http_response_bytes("200 OK", b"{", 100)),
                Self::TooManyRequests => Some(http_response_bytes("429 Too Many Requests", b"", 0)),
                Self::ServerError => Some(http_response_bytes("503 Service Unavailable", b"", 0)),
            };
            if let Some(response) = response {
                stream.writable().await.expect("failure response socket became writable");
                assert_eq!(
                    stream.try_write(&response).expect("write failure response without yielding"),
                    response.len(),
                    "small local failure response was written atomically"
                );
            }

            // Writing, closing, and observing this instant do not yield. The
            // single-threaded test runtime therefore cannot begin the retry
            // before the recorded failure completion.
            drop(stream);
            Instant::now()
        }
    }

    struct RetryObservation {
        requests: Vec<Value>,
        inter_attempt_gaps: Vec<Duration>,
    }

    async fn assert_query_failure_is_paced(failure: RetryableQueryFailure) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let octocrab = test_octocrab(&listener);
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut inter_attempt_gaps = Vec::new();
            let mut previous_failure_completed_at = None;
            for _ in 0..=GRAPHQL_QUERY_RETRY_DELAYS.len() {
                let (mut stream, _) = listener.accept().await.expect("accept query request");
                let request_arrived_at = Instant::now();
                if let Some(completed_at) = previous_failure_completed_at {
                    inter_attempt_gaps.push(request_arrived_at.duration_since(completed_at));
                }
                requests.push(read_json_request(&mut stream).await);
                previous_failure_completed_at = Some(failure.respond(stream).await);
            }
            RetryObservation { requests, inter_attempt_gaps }
        });
        let request = json!({ "query": "query { viewer { login } }" });

        let (result, observation) = tokio::time::timeout(ADAPTER_TIMEOUT, async {
            tokio::join!(run_graphql_query(&octocrab, &request), server)
        })
        .await
        .expect("persistent query failure completed before the real timeout");
        result.expect_err("the fourth persistent failure exhausts the retry policy");
        let observation = observation.expect("test server completed");

        assert_eq!(
            observation.requests,
            vec![request; GRAPHQL_QUERY_RETRY_DELAYS.len() + 1],
            "{failure:?}"
        );
        assert_eq!(
            observation.inter_attempt_gaps.len(),
            GRAPHQL_QUERY_RETRY_DELAYS.len(),
            "{failure:?}"
        );
        for (retry, (gap, expected)) in
            observation.inter_attempt_gaps.iter().zip(GRAPHQL_QUERY_RETRY_DELAYS).enumerate()
        {
            assert!(
                *gap >= expected,
                "{failure:?} retry {} arrived after {gap:?}, before the required {expected:?}",
                retry + 1
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_retryable_query_failure_waits_before_the_next_attempt() {
        tokio::join!(
            assert_query_failure_is_paced(RetryableQueryFailure::BeforeHeaders),
            assert_query_failure_is_paced(RetryableQueryFailure::DuringBody),
            assert_query_failure_is_paced(RetryableQueryFailure::TooManyRequests),
            assert_query_failure_is_paced(RetryableQueryFailure::ServerError),
        );
    }

    #[derive(Debug)]
    struct TestMutation {
        index: usize,
        client_mutation_id: String,
    }

    impl TestMutation {
        fn new(index: usize) -> Self {
            Self { index, client_mutation_id: format!("test-{index}") }
        }
    }

    impl MutationOperation for TestMutation {
        type Output = usize;

        fn client_mutation_id(&self) -> &str {
            &self.client_mutation_id
        }

        fn document(&self) -> String {
            format!(
                "testMutation(input: {{ clientMutationId: {} }}) {{ clientMutationId }}",
                json!(self.client_mutation_id)
            )
        }

        fn decode_receipt(&self, response: Value) -> Result<Self::Output> {
            let receipt = response
                .get("clientMutationId")
                .and_then(Value::as_str)
                .ok_or_else(|| eyre!("missing test mutation receipt"))?;
            if receipt != self.client_mutation_id {
                bail!(
                    "test mutation echoed clientMutationId '{receipt}', expected '{}'",
                    self.client_mutation_id
                );
            }
            Ok(self.index)
        }
    }

    fn mutation_ids(request: &Value) -> Vec<usize> {
        const PREFIX: &str = "clientMutationId: \"test-";

        let query = request.get("query").and_then(Value::as_str).expect("mutation request query");
        query
            .split(PREFIX)
            .skip(1)
            .map(|suffix| {
                suffix
                    .split_once('"')
                    .expect("clientMutationId closing quote")
                    .0
                    .parse()
                    .expect("numeric test mutation ID")
            })
            .collect()
    }

    fn mutation_response(ids: &[usize]) -> Value {
        let data = ids
            .iter()
            .enumerate()
            .map(|(alias, index)| {
                (format!("op{alias}"), json!({ "clientMutationId": format!("test-{index}") }))
            })
            .collect();
        json!({ "data": Value::Object(data) })
    }

    #[derive(Debug)]
    struct MutationObservation {
        requests: Vec<Vec<usize>>,
        effects: Vec<usize>,
    }

    #[derive(Debug, Clone, Copy)]
    enum CreateReceiptCollision {
        Number,
        NodeId,
    }

    fn create_mutation_ids(request: &Value) -> Vec<usize> {
        const PREFIX: &str = "clientMutationId: \"gherrit:create:G";

        let query = request.get("query").and_then(Value::as_str).expect("mutation request query");
        query
            .split(PREFIX)
            .skip(1)
            .map(|suffix| {
                suffix
                    .split_once('"')
                    .expect("clientMutationId closing quote")
                    .0
                    .parse()
                    .expect("numeric create mutation ID")
            })
            .collect()
    }

    fn create_mutation_response(ids: &[usize], collision: Option<CreateReceiptCollision>) -> Value {
        let data = ids
            .iter()
            .enumerate()
            .map(|(alias, index)| {
                let mut number = index + 1;
                let mut node_id = format!("PR_{number}");
                if alias == 0 {
                    match collision {
                        Some(CreateReceiptCollision::Number) => number = 1,
                        Some(CreateReceiptCollision::NodeId) => node_id = "PR_1".to_string(),
                        None => {}
                    }
                }
                (
                    format!("op{alias}"),
                    json!({
                        "clientMutationId": format!("gherrit:create:G{index}"),
                        "pullRequest": {
                            "number": number,
                            "id": node_id,
                            "headRefName": format!("G{index}"),
                        },
                    }),
                )
            })
            .collect();
        json!({ "data": Value::Object(data) })
    }

    async fn assert_cross_batch_create_receipt_collision(collision: CreateReceiptCollision) {
        const OPERATION_COUNT: usize = MAX_MUTATION_ALIASES * 2 + 1;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let octocrab = test_octocrab(&listener);
        let (finished_tx, finished_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut effects = Vec::new();

            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept mutation request");
                let ids = create_mutation_ids(&read_json_request(&mut stream).await);
                effects.extend(ids.iter().copied());
                requests.push(ids.clone());
                let collision = (request_index == 1).then_some(collision);
                write_json_response(&mut stream, &create_mutation_response(&ids, collision)).await;
            }

            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.expect("accept unexpected third request");
                    let ids = create_mutation_ids(&read_json_request(&mut stream).await);
                    effects.extend(ids.iter().copied());
                    requests.push(ids.clone());
                    write_json_response(&mut stream, &create_mutation_response(&ids, None)).await;
                }
                _ = finished_rx => {}
            }

            MutationObservation { requests, effects }
        });
        let creations = (0..OPERATION_COUNT).map(|index| BatchCreate {
            title: format!("Title {index}"),
            body: format!("Body {index}"),
            base_branch: "main".to_owned(),
            head_branch: format!("G{index}"),
        });

        let result = tokio::time::timeout(
            ADAPTER_TIMEOUT,
            batch_create_prs(
                &octocrab,
                "REPO_NODE_ID",
                creations,
                KnownPullRequestIdentities::default(),
            ),
        )
        .await
        .expect("mutation attempt completed");
        let _ = finished_tx.send(());
        let observation = tokio::time::timeout(ADAPTER_TIMEOUT, server)
            .await
            .expect("test server stopped")
            .expect("test server completed");

        let error = result.expect_err("duplicate receipt identity must end the attempt");
        assert!(error.to_string().contains("indeterminate"), "error={error:?}");
        let expected = match collision {
            CreateReceiptCollision::Number => "repeats known pull request number 1",
            CreateReceiptCollision::NodeId => "repeats known pull request node ID 'PR_1'",
        };
        assert!(format!("{error:?}").contains(expected), "error={error:?}");
        assert_eq!(
            observation.requests,
            [
                (0..MAX_MUTATION_ALIASES).collect::<Vec<_>>(),
                (MAX_MUTATION_ALIASES..MAX_MUTATION_ALIASES * 2).collect::<Vec<_>>(),
            ],
            "the third mutation batch must not be transmitted"
        );
        assert_eq!(
            observation.effects,
            (0..MAX_MUTATION_ALIASES * 2).collect::<Vec<_>>(),
            "the peer committed distinct effects before corrupting the receipt"
        );
    }

    #[tokio::test]
    async fn duplicate_create_identities_across_batches_are_indeterminate() {
        for collision in [CreateReceiptCollision::Number, CreateReceiptCollision::NodeId] {
            assert_cross_batch_create_receipt_collision(collision).await;
        }
    }

    #[tokio::test]
    async fn fork_open_identity_collision_stops_before_the_next_create_batch() {
        const OPERATION_COUNT: usize = MAX_MUTATION_ALIASES + 1;

        for collision in [CreateReceiptCollision::Number, CreateReceiptCollision::NodeId] {
            let observed_number = match collision {
                CreateReceiptCollision::Number => 1,
                CreateReceiptCollision::NodeId => 999,
            };
            let mut fork = open_observation_node(observed_number, "fork-head");
            fork["isCrossRepository"] = json!(true);
            if matches!(collision, CreateReceiptCollision::NodeId) {
                fork["id"] = json!("PR_1");
            }
            let (observation_octocrab, observation_server) =
                scripted_graphql(vec![open_observation_page(vec![fork], None, true)]).await;
            let (_, observed, known_identities) =
                observe_open_pull_requests(&observation_octocrab, "owner", "repo")
                    .await
                    .expect("observe the fork-headed pull request");
            finish_scripted_graphql(observation_server).await;
            assert_eq!(observed.len(), 1);
            assert!(observed[0].is_cross_repository);

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
            let octocrab = test_octocrab(&listener);
            let (finished_tx, finished_rx) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept mutation request");
                let first = create_mutation_ids(&read_json_request(&mut stream).await);
                write_json_response(&mut stream, &create_mutation_response(&first, None)).await;

                let mut requests = vec![first.clone()];
                let mut effects = first;
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.expect("accept unexpected second request");
                        let ids = create_mutation_ids(&read_json_request(&mut stream).await);
                        effects.extend(ids.iter().copied());
                        requests.push(ids.clone());
                        write_json_response(
                            &mut stream,
                            &create_mutation_response(&ids, None),
                        )
                        .await;
                    }
                    _ = finished_rx => {}
                }
                MutationObservation { requests, effects }
            });
            let creations = (0..OPERATION_COUNT).map(|index| BatchCreate {
                title: format!("Title {index}"),
                body: format!("Body {index}"),
                base_branch: "main".to_owned(),
                head_branch: format!("G{index}"),
            });

            let result = tokio::time::timeout(
                ADAPTER_TIMEOUT,
                batch_create_prs(&octocrab, "REPO_NODE_ID", creations, known_identities),
            )
            .await
            .expect("mutation attempt completed");
            let _ = finished_tx.send(());
            let observation = tokio::time::timeout(ADAPTER_TIMEOUT, server)
                .await
                .expect("test server stopped")
                .expect("test server completed");

            let error = result.expect_err("an observed identity collision must end the attempt");
            assert!(error.to_string().contains("indeterminate"), "error={error:?}");
            assert_eq!(observation.requests, [(0..MAX_MUTATION_ALIASES).collect::<Vec<_>>()]);
            assert_eq!(observation.effects, (0..MAX_MUTATION_ALIASES).collect::<Vec<_>>());
        }
    }

    #[test]
    fn create_receipts_cannot_reuse_observed_pull_request_identities() {
        for (number, node_id, expected) in [
            (17, "PR_created", "repeats known pull request number 17"),
            (18, "PR_observed", "repeats known pull request node ID 'PR_observed'"),
        ] {
            let created = [CreatedPullRequest {
                head_branch: "Gnew".to_owned(),
                number,
                node_id: node_id.to_owned(),
            }];
            let mut known = KnownPullRequestIdentities::default();
            known
                .insert_open(17, "PR_observed".to_owned())
                .expect("observed identities are distinct");
            let error = known
                .insert_created(&created)
                .expect_err("a create receipt must not reuse an observed identity");
            assert!(error.to_string().contains(expected), "error={error:?}");
        }

        let mut known = KnownPullRequestIdentities::default();
        known.insert_open(17, "PR_observed".to_owned()).expect("observed identities are distinct");
        known
            .insert_created(&[CreatedPullRequest {
                head_branch: "Gnew".to_owned(),
                number: 18,
                node_id: "PR_created".to_owned(),
            }])
            .expect("distinct observed and created identities are valid");
    }

    #[tokio::test]
    async fn partial_second_batch_effects_with_a_lost_acknowledgement_stop_the_attempt() {
        const OPERATION_COUNT: usize = MAX_MUTATION_ALIASES * 2 + 1;
        const PARTIAL_SECOND_BATCH_EFFECTS: usize = 7;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let octocrab = test_octocrab(&listener);
        let (finished_tx, finished_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut effects = Vec::new();

            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept mutation request");
                let ids = mutation_ids(&read_json_request(&mut stream).await);
                requests.push(ids.clone());
                if request_index == 0 {
                    effects.extend(ids.iter().copied());
                    write_json_response(&mut stream, &mutation_response(&ids)).await;
                } else {
                    effects.extend(ids.iter().copied().take(PARTIAL_SECOND_BATCH_EFFECTS));
                    // Some mutation fields committed, but their acknowledgement
                    // was lost before any response headers reached the client.
                    drop(stream);
                }
            }

            tokio::select! {
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.expect("accept unexpected third request");
                    let ids = mutation_ids(&read_json_request(&mut stream).await);
                    effects.extend(ids.iter().copied());
                    requests.push(ids.clone());
                    write_json_response(&mut stream, &mutation_response(&ids)).await;
                }
                _ = finished_rx => {}
            }

            MutationObservation { requests, effects }
        });
        let mutations = (0..OPERATION_COUNT).map(TestMutation::new).collect::<Vec<_>>();

        let result =
            tokio::time::timeout(ADAPTER_TIMEOUT, run_graphql_mutations(&octocrab, mutations))
                .await
                .expect("mutation attempt completed");
        let _ = finished_tx.send(());
        let observation = tokio::time::timeout(ADAPTER_TIMEOUT, server)
            .await
            .expect("test server stopped")
            .expect("test server completed");

        let error = result.expect_err("lost acknowledgement must end the attempt");
        assert!(error.to_string().contains("indeterminate"), "error={error:?}");
        assert_eq!(
            observation.requests,
            [
                (0..MAX_MUTATION_ALIASES).collect::<Vec<_>>(),
                (MAX_MUTATION_ALIASES..MAX_MUTATION_ALIASES * 2).collect::<Vec<_>>(),
            ],
            "the third mutation batch must not be transmitted"
        );
        assert_eq!(
            observation.effects,
            (0..MAX_MUTATION_ALIASES + PARTIAL_SECOND_BATCH_EFFECTS).collect::<Vec<_>>(),
            "acknowledged and ambiguous partial effects remain committed"
        );
    }
}

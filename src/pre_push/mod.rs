use std::{collections::HashMap, env, ffi::OsStr, time::Duration};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::reference::Category;
use octocrab::Octocrab;
use owo_colors::OwoColorize;

use crate::util::{self, HeadState};

mod autosquash;
mod batching;
mod body;
mod destination;
mod legacy_github;
mod legacy_publication;
mod legacy_remote;
mod local;
// The complete exact-local workflow is compiled behind one private boundary.
// It becomes reachable only when observation and publication switch together.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "exact publication activates as one complete workflow")
)]
mod publication_attempt;
mod reconcile;
mod subprocess;

use batching::{
    BatchPlan, INITIAL_QUERY_BATCH_LEN, MAX_GRAPHQL_QUERY_BYTES, ResponseDisposition,
    classify_response, query_exceeds_limit,
};
use body::PrBody;
use destination::{DefaultBranch, PushDestination};
use legacy_github::{
    CreatePullRequest, CreatedPullRequest, FindPullRequest, MutationOperation,
    PullRequest as PrState, PullRequestNodeId, PullRequestNumber, QueryOperation,
    Repository as GithubRepository, UpdatePullRequest, decode_mutation_batch_response,
    decode_query_batch_response, prepare_mutation_batches, query_batch_document,
};
use legacy_publication::{plan_change, plan_push, push_batches};
use legacy_remote::observe_publications;
use local::LocalStack;
use reconcile::{
    CurrentPr, DesiredPr, PullRequestState, ensure_pull_requests_open, link_stack, plan_update,
};

const INDETERMINATE_GRAPHQL_MUTATION: &str = "GraphQL mutation acknowledgement is indeterminate; stop this publication attempt and retry the push to reobserve GitHub state";
const INTERNAL_PRE_PUSH_GIT_DIR_ENV: &str = "GHERRIT_INTERNAL_PRE_PUSH_GIT_DIR";
const INTERNAL_PRE_PUSH_REMOTE_ENV: &str = "GHERRIT_INTERNAL_PRE_PUSH_REMOTE";

/// Returns whether Git invoked this hook for a push started by GHerrit.
///
/// This marker is cooperative recursion control, not a private value or a
/// security boundary. Binding it to the exact per-worktree Git directory and
/// Git's remote-name argument prevents an inherited marker from disabling
/// GHerrit for an unrelated nested push.
pub(crate) fn is_internal_publication_push(
    repository: &util::Repo,
    remote_name: Option<&str>,
    remote_location: Option<&str>,
) -> bool {
    internal_publication_push_matches(
        repository.git_dir_identity().as_os_str(),
        remote_name,
        remote_location,
        env::var_os(INTERNAL_PRE_PUSH_REMOTE_ENV).as_deref(),
        env::var_os(INTERNAL_PRE_PUSH_GIT_DIR_ENV).as_deref(),
    )
}

fn internal_publication_push_matches(
    git_dir: &OsStr,
    remote_name: Option<&str>,
    remote_location: Option<&str>,
    remote_marker: Option<&OsStr>,
    git_dir_marker: Option<&OsStr>,
) -> bool {
    matches!(
        (remote_name, remote_location, remote_marker, git_dir_marker),
        (Some(remote_name), Some(remote_location), Some(remote_marker), Some(git_dir_marker))
            if !remote_name.is_empty()
                && !remote_location.is_empty()
                && remote_marker == OsStr::new(remote_name)
                && git_dir_marker == git_dir
    )
}

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
    let destination = PushDestination::resolve(repo, configured_remote)?;
    let git_default_branch = destination.observe_default_branch().await?;
    let commits =
        LocalStack::collect(repo, &git_default_branch).wrap_err("Failed to collect commits")?;

    if commits.is_empty() {
        log::info!("No commits to sync.");
        return Ok(());
    }

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

    let gherrit_ids =
        commits.iter().map(|commit| commit.id().as_str().to_owned()).collect::<Vec<_>>();
    let GithubRepositoryState { repository, pull_requests: prs } =
        batch_fetch_prs(&octocrab, &destination, &gherrit_ids).await?;
    let repository = GithubRepository {
        node_id: repository.node_id,
        default_branch: DefaultBranch::agree(
            commits.default_branch().clone(),
            repository.default_branch,
        )?,
    };
    ensure_pull_requests_open(prs.iter().map(|pr| (pr.number.get(), pr.state)))?;

    let latest_versions = push_to_origin(&destination, &commits).await?;
    let public_branch = public_stack_branch(repo, branch_name);

    let num_commits = commits.len();
    sync_prs(
        &octocrab,
        &destination,
        &repository,
        public_branch.as_deref(),
        &commits,
        latest_versions,
        prs,
    )
    .await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn push_to_origin(
    destination: &PushDestination,
    commits: &LocalStack,
) -> Result<HashMap<String, usize>> {
    let gherrit_ids =
        commits.iter().map(|commit| commit.id().as_str().to_owned()).collect::<Vec<_>>();
    let observed = observe_publications(destination, &gherrit_ids)?;

    // Plan every change before the first write. In particular, a later
    // exhausted version history must not be discovered after an earlier push.
    let mut latest_versions = HashMap::with_capacity(commits.len());
    let mut targets = Vec::with_capacity(commits.len());
    for (change, observed) in commits.iter().zip(observed) {
        let (version, target) =
            plan_change(change.head(), change.id().as_str(), observed)?.into_parts();
        latest_versions.insert(change.id().as_str().to_owned(), version);
        targets.extend(target);
    }

    for chunk in push_batches(&targets) {
        let plan = plan_push(chunk);
        let legacy_publication::PushPlan { options, refspecs } = plan;

        log::info!("Pushing chunk to remote...");
        let output = subprocess::output(
            destination.push(options, refspecs),
            subprocess::REMOTE_GIT_EXECUTION_TIMEOUT,
        )
        .await
        .wrap_err("Failed to run `git push`")?;
        if !output.status().success() {
            let message = format!(
                "`git push` failed for GHerrit remote '{}'. Its outcome is indeterminate; retry the push to reobserve remote state.",
                destination.configured_remote()
            );
            if let Some(diagnostic) = output.child_diagnostic(destination) {
                bail!(
                    "{message}\n\nInternal push diagnostic (untrusted and not publication evidence):\n{diagnostic}"
                );
            }
            bail!("{message}");
        }
    }

    Ok(latest_versions)
}

/// GitHub repository facts and pull requests read through the repository
/// identity validated by the active push destination.
struct GithubRepositoryState {
    repository: GithubRepository,
    pull_requests: Vec<PrState>,
}

/// Syncs the local stack of commits with GitHub Pull Requests.
///
/// This function:
/// 1. Finds existing PRs or creates new ones for new commits.
/// 2. Updates PR metadata (title, body, base branch) to match the local stack.
/// 3. Updates are queued and executed in batches to optimize performance.
async fn sync_prs(
    octocrab: &Octocrab,
    destination: &PushDestination,
    repository: &GithubRepository,
    public_branch: Option<&str>,
    commits: &LocalStack,
    latest_versions: HashMap<String, usize>,
    prs: Vec<PrState>,
) -> Result<()> {
    let commits = link_stack(repository.default_branch.name(), commits.iter(), |commit| {
        commit.id().as_str().to_owned()
    });

    enum PrResolution {
        Existing(PrState),
        ToCreate(BatchCreate),
    }

    // 1. Identify existing PRs or queue for creation
    let resolutions: Vec<_> = commits
        .iter()
        .map(|entry| {
            let c = &entry.item;

            if let Some(pr) = prs.iter().find(|pr| pr.head_branch == c.id().as_str()) {
                log::debug!(
                    "Found existing PR #{} for {}",
                    pr.number.green().bold(),
                    c.id().as_str()
                );
                PrResolution::Existing(pr.clone())
            } else {
                log::debug!("No GitHub PR exists for {}; queuing creation...", c.id().as_str());
                PrResolution::ToCreate(BatchCreate {
                    title: c.title().to_owned(),
                    body: c.body().to_owned(),
                    base_branch: entry.base_branch.clone(),
                    head_branch: c.id().as_str().to_owned(),
                    head_oid: c.head(),
                    base_oid: c.first_parent(),
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
        let created = batch_create_prs(octocrab, &repository.node_id, creations).await?;
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
                PrResolution::Existing(state) => state,
                PrResolution::ToCreate(create) => {
                    let created = new_prs.get(&create.head_branch).ok_or_else(|| {
                        eyre::eyre!("Failed to resolve created PR for {}", create.head_branch)
                    })?;
                    log::info!(
                        "Created PR #{}: {}",
                        created.number.green().bold(),
                        destination.pr_url(created.number.get()).blue().underline()
                    );
                    PrState {
                        number: created.number,
                        node_id: created.node_id.clone(),
                        title: Some(create.title),
                        body: Some(create.body),
                        base_branch: create.base_branch,
                        head_branch: create.head_branch,
                        // NOTE: We assume that newly-created PRs are in the
                        // OPEN state.
                        state: PullRequestState::Open,
                    }
                }
            };
            Ok((entry, pr_state))
        })
        .collect::<Result<Vec<_>>>()?;

    let repo_url = destination.repo_url_relative();
    let stack_pr_numbers =
        commit_pr_states.iter().map(|(_, state)| state.number.get()).collect::<Vec<_>>();
    let updates = commit_pr_states
        .iter()
        .filter_map(|(entry, pr_state)| {
            let c = &entry.item;
            let latest_version = latest_versions.get(c.id().as_str()).copied().unwrap_or(1);

            let body = PrBody {
                commit_body: c.body(),
                repo_url: &repo_url,
                public_branch,
                stack_pr_numbers: &stack_pr_numbers,
                current_pr_number: pr_state.number.get(),
                latest_version,
                base_branch: &entry.base_branch,
                gherrit_id: c.id().as_str(),
            }
            .render();

            let pr_num = pr_state.number.green().bold().to_string();
            let pr_url = destination.pr_url(pr_state.number.get()).blue().underline().to_string();

            let update = plan_update(
                CurrentPr {
                    title: pr_state.title.as_deref(),
                    body: pr_state.body.as_deref(),
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

            update.map(|update| {
                UpdatePullRequest::new(
                    pr_state.number,
                    pr_state.node_id.clone(),
                    update.title,
                    update.body,
                    update.base_branch,
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if !updates.is_empty() {
        log::info!("Updating batch of {} PRs...", updates.len());
        run_graphql_mutations(octocrab, updates).await?;
        log::info!("Batch update complete.");
    }

    Ok(())
}

fn public_stack_branch(repo: &util::Repo, branch_name: &str) -> Option<String> {
    (!is_private_stack(repo, branch_name))
        .then(|| {
            let head_ref = repo.head().ok()?.try_into_referent()?;
            let (cat, short_name) = head_ref.inner.name.category_and_short_name()?;
            (cat == Category::LocalBranch).then(|| short_name.to_string())
        })
        .flatten()
}

fn is_private_stack(repo: &util::Repo, branch: &str) -> bool {
    // If pushRemote is set to ".", it is a private loopback stack.
    // If it is unset or anything else (e.g. 'origin'), it is public.
    repo.config_string(&format!("branch.{}.pushRemote", branch))
        .map(|val| val.as_deref() == Some("."))
        .unwrap_or(false)
}

/// A request to create a new PR in a batch.
#[derive(Clone)]
struct BatchCreate {
    title: String,
    body: String,
    base_branch: String,
    head_branch: String,
    head_oid: gix::ObjectId,
    base_oid: gix::ObjectId,
}

/// Coupled pull request identities acknowledged by create batches in this
/// attempt.
///
/// Both maps are updated together after both uniqueness checks pass. Keeping
/// the inverse map makes either kind of identity reuse cheap to reject without
/// discarding the number-to-node pairing returned by GitHub.
#[derive(Default)]
struct CreatedPullRequestIdentitySet {
    node_by_number: HashMap<PullRequestNumber, PullRequestNodeId>,
    number_by_node: HashMap<PullRequestNodeId, PullRequestNumber>,
}

impl CreatedPullRequestIdentitySet {
    fn insert(&mut self, receipts: &[CreatedPullRequest]) -> Result<()> {
        for receipt in receipts {
            if self.node_by_number.contains_key(&receipt.number) {
                bail!(
                    "createPullRequest receipt for '{}' repeats acknowledged pull request number {}",
                    receipt.head_branch,
                    receipt.number
                );
            }
            if self.number_by_node.contains_key(&receipt.node_id) {
                bail!(
                    "createPullRequest receipt for '{}' repeats acknowledged pull request node ID '{}'",
                    receipt.head_branch,
                    receipt.node_id
                );
            }

            self.node_by_number.insert(receipt.number, receipt.node_id.clone());
            self.number_by_node.insert(receipt.node_id.clone(), receipt.number);
        }
        Ok(())
    }
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
) -> Result<Vec<CreatedPullRequest>> {
    let creations = creations.into_iter().map(|create| {
        CreatePullRequest::new(
            repo_id.to_string(),
            create.base_branch,
            create.head_branch,
            create.title,
            create.body,
            create.head_oid,
            create.base_oid,
        )
    });
    let mut identities = CreatedPullRequestIdentitySet::default();
    run_graphql_mutations_with_receipt_validation(octocrab, creations, move |receipts| {
        identities.insert(receipts)
    })
    .await
}

async fn batch_fetch_prs(
    octocrab: &Octocrab,
    destination: &PushDestination,
    head_refs: &[String],
) -> Result<GithubRepositoryState> {
    let coordinates = destination.coordinates();
    let queries = head_refs.iter().cloned().enumerate().map(|(index, head_ref)| {
        if index == 0 {
            FindPullRequest::with_repository(
                coordinates.owner().to_owned(),
                coordinates.repository().to_owned(),
                head_ref,
            )
        } else {
            FindPullRequest::new(
                coordinates.owner().to_owned(),
                coordinates.repository().to_owned(),
                head_ref,
            )
        }
    });

    let mut repository = None;
    let mut pull_requests = Vec::new();
    for lookup in run_batched_queries(octocrab, queries).await? {
        if let Some(observed) = lookup.repository
            && repository.replace(observed).is_some()
        {
            bail!("GitHub returned repository facts more than once");
        }
        pull_requests.extend(lookup.pull_request);
    }
    let repository = repository.ok_or_else(|| eyre!("GitHub omitted the repository facts"))?;
    Ok(GithubRepositoryState { repository, pull_requests })
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

/// Executes mutation batches and validates each acknowledged receipt batch
/// before another batch can be transmitted.
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

/// Executes adaptively sized, read-only GraphQL query batches.
///
/// Builds a combined query for each adaptive batch and decodes each operation
/// in a successful response.
async fn run_batched_queries<O>(
    octocrab: &Octocrab,
    operations: impl IntoIterator<Item = O>,
) -> Result<Vec<O::Output>>
where
    O: QueryOperation,
{
    let operations: Vec<O> = operations.into_iter().collect();
    if operations.is_empty() {
        return Ok(Vec::new());
    }

    let mut outputs = Vec::with_capacity(operations.len());

    // GitHub imposes a limit on the number of nodes that can be processed in a
    // single GraphQL query (500,000 as of this writing [1]), and also imposes
    // limits on the amount of computation resources required to process the
    // query [2]. In order to avoid hitting these limits while still processing
    // large batches in the optimistic case, we start with a large batch size
    // and perform exponential backoff if we hit the limits. This also ensures
    // that we are resilient in the face of GitHub changing these limits in the
    // future.
    //
    // [1] https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api#node-limit
    // [2] https://github.blog/changelog/2025-09-01-graphql-api-resource-limits/
    let mut batches = BatchPlan::new(operations.len(), INITIAL_QUERY_BATCH_LEN);
    while let Some(range) = batches.current() {
        let chunk = &operations[range];
        let query = query_batch_document(chunk);

        // Attempt to perform the query. Returns:
        // - Ok(Some(response)): Success
        // - Ok(None): Heuristic or API limit hit (needs backoff)
        // - Err(e): Fatal error (bail)
        let response = async {
            // HEURISTIC: Check query size before sending. GitHub's WAF/load
            // balancer/some other middleware seems to silently drop or truncate
            // requests larger than ~600KB, leading to confusing "missing query
            // attribute" errors. We preemptively backoff if we exceed a
            // conservative limit (256KB).
            if query_exceeds_limit(&query) {
                log::warn!(
                    "GraphQL query size ({} bytes) exceeds heuristic limit ({} bytes).",
                    query.len(),
                    MAX_GRAPHQL_QUERY_BYTES
                );
                return Ok(None);
            }

            log::trace!("Sending GraphQL Query (Length: {}): {}", query.len(), query);
            let request_payload = serde_json::json!({ "query": query });
            let response = run_graphql_query(octocrab, &request_payload)
                .await
                .wrap_err("GraphQL batched operation failed")?;

            match classify_response(&response) {
                ResponseDisposition::Success => {}
                ResponseDisposition::RetryLimit => {
                    log::warn!(
                        "Hit GitHub resource limit with GraphQL batch of size {}",
                        chunk.len()
                    );
                    return Ok(None);
                }
                ResponseDisposition::Fatal => {
                    let errors = response.get("errors").expect("fatal response has errors");
                    log::error!("GraphQL errors: {errors}");
                    bail!("GraphQL errors: {errors:?}");
                }
            }

            Ok(Some(response))
        }
        .await?;

        let Some(response) = response else {
            match batches.reject() {
                Ok(backoff) => log::warn!(
                    "Backing off GraphQL batch size from {} to {}.",
                    backoff.attempted,
                    backoff.retry
                ),
                Err(item) => bail!(
                    "GraphQL operation at item {} exceeds GitHub resource limits. Cannot sync.",
                    item.index
                ),
            }
            continue;
        };

        outputs.extend(decode_query_batch_response(chunk, response)?);

        batches.accept();
    }
    Ok(outputs)
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
    use crate::pre_push::batching::MAX_MUTATION_ALIASES;

    const ADAPTER_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_TEST_REQUEST_BYTES: usize = 1024 * 1024;

    fn test_object_id(hex_digit: u8) -> gix::ObjectId {
        gix::ObjectId::from_hex(&[hex_digit; 40]).unwrap()
    }

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
                            "state": "OPEN",
                            "headRefName": format!("G{index}"),
                            "headRefOid": test_object_id(b'1').to_string(),
                            "headRepository": { "id": "REPO_NODE_ID" },
                            "baseRefName": "main",
                            "baseRefOid": test_object_id(b'2').to_string(),
                            "baseRepository": { "id": "REPO_NODE_ID" },
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
            head_oid: test_object_id(b'1'),
            base_oid: test_object_id(b'2'),
        });

        let result = tokio::time::timeout(
            ADAPTER_TIMEOUT,
            batch_create_prs(&octocrab, "REPO_NODE_ID", creations),
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
            CreateReceiptCollision::Number => "repeats acknowledged pull request number 1",
            CreateReceiptCollision::NodeId => "repeats acknowledged pull request node ID 'PR_1'",
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

    #[test]
    fn internal_push_marker_must_match_complete_hook_arguments() {
        let git_dir = OsStr::new("/repository/.git/worktrees/current");
        let remote_marker = OsStr::new("gherrit-publication-2");
        let git_dir_marker = OsStr::new("/repository/.git/worktrees/current");

        assert!(internal_publication_push_matches(
            git_dir,
            Some("gherrit-publication-2"),
            Some("private-destination"),
            Some(remote_marker),
            Some(git_dir_marker),
        ));
        for (remote_name, remote_location, remote_marker, git_dir_marker) in [
            (
                Some("origin"),
                Some("private-destination"),
                Some(remote_marker),
                Some(git_dir_marker),
            ),
            (Some("gherrit-publication-2"), Some(""), Some(remote_marker), Some(git_dir_marker)),
            (Some(""), Some("private-destination"), Some(remote_marker), Some(git_dir_marker)),
            (Some("gherrit-publication-2"), None, Some(remote_marker), Some(git_dir_marker)),
            (None, Some("private-destination"), Some(remote_marker), Some(git_dir_marker)),
            (
                Some("gherrit-publication-2"),
                Some("private-destination"),
                None,
                Some(git_dir_marker),
            ),
            (Some("gherrit-publication-2"), Some("private-destination"), Some(remote_marker), None),
            (
                Some("gherrit-publication-2"),
                Some("private-destination"),
                Some(remote_marker),
                Some(OsStr::new("/repository/.git/worktrees/other")),
            ),
        ] {
            assert!(!internal_publication_push_matches(
                git_dir,
                remote_name,
                remote_location,
                remote_marker,
                git_dir_marker,
            ));
        }
    }
}

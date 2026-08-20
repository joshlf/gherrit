use std::{collections::HashSet, num::NonZeroUsize, ops::Range, process::Stdio};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::{ObjectId, reference::Category, refs::transaction::PreviousValue};
use octocrab::Octocrab;
use owo_colors::OwoColorize;

use crate::{
    re,
    util::{self, HeadState},
};

mod autosquash;
mod batching;
mod body;
mod github;
mod publication;
mod reconcile;
mod remote;

use batching::{
    BatchPlan, INITIAL_GRAPHQL_BATCH_LEN, MAX_GRAPHQL_QUERY_BYTES, ResponseDisposition,
    classify_response, query_exceeds_limit,
};
use body::gherrit_pr_id_re;
use github::{
    BatchedMutation, BatchedOperation, BatchedQuery, CreatePullRequest, FindPullRequest,
    OperationType, PullRequest as PrState, RepositoryIdQuery, UpdatePullRequest, batch_document,
    decode_batch_response,
};
use publication::{
    Checkpoint, CheckpointTarget, PublicationAction, PushTarget, parse_version, plan_publication,
    plan_push, push_batches,
};
use reconcile::{
    CreatePr, KnownPr, ProjectionCommit, ProjectionContext, ProjectionEntry, ProjectionStep,
    UpdatePr, ensure_pull_requests_open, plan_projection,
};
use remote::observe_publications;

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

    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let prs = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;
    ensure_pull_requests_open(prs.iter().flatten().map(|pr| (pr.number, pr.state)))?;

    let num_commits = commits.len();
    let published_commits = push_to_origin(repo, commits)?;
    let default_branch = repo.find_default_branch_on_default_remote();

    sync_prs(repo, &octocrab, branch_name, &default_branch, published_commits, prs).await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

fn collect_commits(repo: &util::Repo) -> Result<Vec<Commit>> {
    let head = repo.rev_parse_single("HEAD")?;
    let default_branch = repo.find_default_branch_on_default_remote();
    let default_ref = repo.rev_parse_single(format!("refs/heads/{}", default_branch).as_str())?;

    let commits = repo.commits_between(default_ref, head).map_err(|err| match err {
        util::CommitsBetweenError::NotAncestor => {
            let branch_name = repo.current_branch().name().unwrap_or("current branch");
            eyre!(
                "The branch '{branch_name}' is not based on '{default_branch}'.\n\
                 GHerrit only supports stacked branches that share history with the default branch.\n\
                 Maybe you want to 'git rebase' on '{default_branch}' before pushing?"
            )
        }
        util::CommitsBetweenError::Eyre(e) => e,
    })?;

    let commits = commits
        .into_iter()
        .map(|commit| -> Result<_> {
            let title = core::str::from_utf8(commit.message()?.title)?.to_owned();
            Ok((commit, title))
        })
        .collect::<Result<Vec<_>>>()?;

    autosquash::ensure_publishable(
        commits.iter().map(|(_, title)| title.as_str()),
        &repo.default_remote_name(),
        &default_branch,
    )?;

    let commits: Vec<Commit> =
        commits.into_iter().map(|(commit, _)| commit.try_into()).collect::<Result<Vec<_>>>()?;
    ensure_unique_gherrit_ids(commits.iter().map(|commit| commit.gherrit_id.as_str()))?;
    Ok(commits)
}

fn ensure_unique_gherrit_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<()> {
    ids.into_iter().try_fold(HashSet::new(), |mut seen, id| {
        if !seen.insert(id) {
            bail!("Stack contains multiple commits with gherrit-pr-id '{id}'");
        }
        Ok(seen)
    })?;
    Ok(())
}

struct PublishedCommit {
    commit: Commit,
    version: NonZeroUsize,
}

#[allow(clippy::too_many_lines)]
fn push_to_origin(repo: &util::Repo, commits: Vec<Commit>) -> Result<Vec<PublishedCommit>> {
    let local_checkpoints = commits
        .iter()
        .map(|commit| get_local_checkpoint(repo, &commit.gherrit_id))
        .collect::<Result<Vec<_>>>()?;
    let gherrit_ids = commits.iter().map(|commit| commit.gherrit_id.as_str()).collect::<Vec<_>>();
    let remote_publications = observe_publications(repo, &gherrit_ids)?;
    if remote_publications.len() != commits.len() {
        bail!(
            "Git returned {} publication observations for {} commits",
            remote_publications.len(),
            commits.len()
        );
    }

    let remote_name = repo.default_remote_name();
    let actions = commits
        .iter()
        .zip(local_checkpoints)
        .zip(remote_publications)
        .map(|((commit, local), remote)| {
            plan_publication(&remote_name, &commit.gherrit_id, commit.id, local, remote)
        })
        .collect::<Result<Vec<_>>>()?;

    let recoveries = actions
        .iter()
        .filter_map(|action| match action {
            PublicationAction::Recover(target) => Some(*target),
            PublicationAction::Current(_) | PublicationAction::Push(_) => None,
        })
        .collect::<Vec<_>>();
    if !recoveries.is_empty() {
        log::info!("Recovering {} remote publication checkpoints...", recoveries.len());
        recoveries.iter().try_for_each(|target| persist_local_checkpoint(repo, *target))?;
    }

    let pending = actions
        .iter()
        .filter_map(|action| match action {
            PublicationAction::Push(target) => Some(*target),
            PublicationAction::Current(_) | PublicationAction::Recover(_) => None,
        })
        .collect::<Vec<PushTarget<'_>>>();
    for chunk in push_batches(&pending) {
        let arguments = plan_push(&remote_name, chunk);

        log::info!("Pushing chunk to remote...");
        let mut child = util::cmd("git", arguments)
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err("Failed to run `git push`")?;

        {
            use std::io::{BufRead, BufReader};
            let stderr = child.stderr.take().unwrap();
            let reader = BufReader::new(stderr);
            let mut remote_buffer: Vec<String> = Vec::new();
            let flush_buffer = |buf: &mut Vec<String>| {
                if buf.is_empty() {
                    return;
                }
                let block = buf.join("\n");
                let re = re!(
                    r"(?m)\n?^remote:\s*\nremote: Create a pull request for '.*' on GitHub by visiting:\s*\nremote:\s*https://github\.com/.*\nremote:\s*$"
                );
                let cleaned = re.replace(&block, "");
                if !cleaned.is_empty() {
                    eprintln!("{}", cleaned);
                }
                buf.clear();
            };
            for line in reader.lines() {
                let line = line.unwrap();
                if line.trim_start().starts_with("remote:") {
                    remote_buffer.push(line);
                } else {
                    flush_buffer(&mut remote_buffer);
                    eprintln!("{}", line);
                }
            }
            flush_buffer(&mut remote_buffer);
        }

        let status = child.wait().unwrap();
        if !status.success() {
            bail!(
                "`git push` failed. The remote might be ahead or changed. Run `git fetch {remote_name}` to sync."
            );
        }

        chunk.iter().try_for_each(|target| persist_local_checkpoint(repo, target.checkpoint))?;
    }

    let versions = actions.iter().map(PublicationAction::version).collect::<Vec<_>>();
    drop(recoveries);
    drop(pending);
    drop(actions);
    drop(gherrit_ids);
    Ok(commits
        .into_iter()
        .zip(versions)
        .map(|(commit, version)| PublishedCommit { commit, version })
        .collect())
}

fn persist_local_checkpoint(repo: &util::Repo, target: CheckpointTarget<'_>) -> Result<()> {
    let ref_name =
        format!("refs/tags/gherrit/{}/v{}", target.gherrit_id, target.checkpoint.version.get());
    repo.reference(
        ref_name.clone(),
        target.checkpoint.object_id,
        PreviousValue::MustNotExist,
        "gherrit: record remote publication checkpoint",
    )
    .wrap_err_with(|| {
        format!(
            "Remote publication {ref_name} already exists, but GHerrit could not record the matching local checkpoint. Retry is safe and will recover any remaining checkpoints."
        )
    })?;
    Ok(())
}

fn get_local_checkpoint(repo: &util::Repo, gherrit_id: &str) -> Result<Option<Checkpoint>> {
    let prefix = format!("refs/tags/gherrit/{gherrit_id}/v").into_bytes();
    let mut latest = None;
    let references = repo.references().map_err(|error| eyre!(error))?;

    for reference in references.all().map_err(|error| eyre!(error))? {
        let reference = reference.map_err(|error| eyre!(error))?;
        let name: &[u8] = reference.name().as_bstr().as_ref();
        let Some(version) = name.strip_prefix(prefix.as_slice()) else {
            continue;
        };
        let version = core::str::from_utf8(version).wrap_err_with(|| {
            format!("Malformed local GHerrit version tag: {}", String::from_utf8_lossy(name))
        })?;
        let version = parse_version(version).ok_or_else(|| {
            eyre!("Malformed local GHerrit version tag: {}", String::from_utf8_lossy(name))
        })?;
        let object_id = reference.try_id().ok_or_else(|| {
            eyre!("Local GHerrit version tag is symbolic: {}", String::from_utf8_lossy(name))
        })?;
        let checkpoint = Checkpoint { object_id: object_id.detach(), version };
        if latest.is_none_or(|latest: Checkpoint| checkpoint.version > latest.version) {
            latest = Some(checkpoint);
        }
    }

    Ok(latest)
}

/// Syncs the local stack of commits with GitHub Pull Requests.
///
/// This function:
/// 1. Finds existing PRs or creates new ones for new commits.
/// 2. Updates PR metadata (title, body, base branch) to match the local stack.
/// 3. Updates are queued and executed in batches to optimize performance.
async fn sync_prs(
    repo: &util::Repo,
    octocrab: &Octocrab,
    branch_name: &str,
    base_branch: &str,
    commits: Vec<PublishedCommit>,
    prs: Vec<Option<PrState>>,
) -> Result<()> {
    let remote = repo.default_remote()?;
    let public_branch = (!is_private_stack(repo, branch_name))
        .then(|| {
            let head_ref = repo.head().ok()?.try_into_referent()?;
            let (cat, short_name) = head_ref.inner.name.category_and_short_name()?;
            (cat == Category::LocalBranch).then(|| short_name.to_string())
        })
        .flatten();
    let repo_url = remote.repo_url_relative();

    let mut entries = commits
        .into_iter()
        .map(|published| ProjectionEntry {
            pull_request: None,
            commit: ProjectionCommit {
                latest_version: published.version,
                gherrit_id: published.commit.gherrit_id,
                title: published.commit.message_title,
                commit_body: published.commit.message_body,
            },
        })
        .collect::<Vec<_>>();
    replace_pull_requests(&mut entries, prs)?;
    let mut mutation_batch_len = INITIAL_GRAPHQL_BATCH_LEN;
    entries.iter().for_each(|entry| match &entry.pull_request {
        Some(pr) => {
            log::debug!(
                "Found existing PR #{} for {}",
                pr.identity.number().green().bold(),
                entry.commit.gherrit_id
            );
        }
        None => {
            log::debug!("No GitHub PR exists for {}; queuing creation...", entry.commit.gherrit_id);
        }
    });

    let context = ProjectionContext {
        base_branch,
        repo_url: &repo_url,
        public_branch: public_branch.as_deref(),
    };
    loop {
        match plan_projection(context, &entries) {
            ProjectionStep::Create(creations) => {
                let num_creations = creations.len();
                log::info!("Creating {num_creations} PRs...");
                let repo_id = fetch_repo_id(octocrab, &remote).await?;
                let created = match batch_create_prs(
                    octocrab,
                    &repo_id,
                    creations.iter().cloned(),
                    mutation_batch_len,
                )
                .await?
                {
                    MutationExecution::Complete(created) => created,
                    MutationExecution::Ambiguous(ambiguity) => {
                        let singleton_head = ambiguity
                            .singleton(&creations)
                            .map(|creation| creation.head_branch.clone());
                        mutation_batch_len = ambiguity.retry_batch_len();
                        log::warn!(
                            "GitHub may have applied part of the PR creation batch; reobserving before continuing."
                        );
                        reobserve_pull_requests(repo, octocrab, &mut entries).await?;
                        if let Some(head) = singleton_head.filter(|head| {
                            entries.iter().any(|entry| {
                                entry.commit.gherrit_id == *head && entry.pull_request.is_none()
                            })
                        }) {
                            bail!(
                                "GitHub reported a resource limit while creating the PR for head branch '{head}', and the PR was still absent after reobservation. GHerrit cannot safely retry the ambiguous operation in this invocation."
                            );
                        }
                        continue;
                    }
                };
                if created.len() != num_creations {
                    bail!(
                        "GitHub returned {} PRs for {num_creations} creation actions",
                        created.len()
                    );
                }
                ensure_pull_requests_open(
                    created.iter().map(|pull_request| (pull_request.number, pull_request.state)),
                )?;
                log::info!("Created {num_creations} PRs.");

                entries
                    .iter_mut()
                    .filter(|entry| entry.pull_request.is_none())
                    .zip(creations)
                    .zip(created)
                    .for_each(|((entry, create), created)| {
                        debug_assert_eq!(entry.commit.gherrit_id, create.head_branch);
                        log::info!(
                            "Created PR #{}: {}",
                            created.number.green().bold(),
                            remote.pr_url(created.number).blue().underline()
                        );
                        entry.pull_request = Some(KnownPr::new(
                            created.number,
                            created.node_id,
                            created.title,
                            created.body,
                            created.base_branch,
                        ));
                    });
            }
            ProjectionStep::Update(updates) => {
                log_projection_updates(&remote, &entries, &updates);
                log::info!("Updating batch of {} PRs...", updates.len());
                match batch_update_prs(octocrab, &updates, mutation_batch_len).await? {
                    MutationExecution::Complete(_) => {
                        log::info!("Batch update complete.");
                        return Ok(());
                    }
                    MutationExecution::Ambiguous(ambiguity) => {
                        let attempted = ambiguity.singleton(&updates).cloned();
                        mutation_batch_len = ambiguity.retry_batch_len();
                        log::warn!(
                            "GitHub may have applied part of the PR update batch; reobserving before continuing."
                        );
                        reobserve_pull_requests(repo, octocrab, &mut entries).await?;
                        if let Some(attempted) = attempted {
                            let replanned = plan_projection(context, &entries);
                            if matches!(
                                replanned,
                                ProjectionStep::Update(ref updates)
                                    if updates.contains(&attempted)
                            ) {
                                bail!(
                                    "GitHub reported a resource limit while updating PR #{}, and the requested update was still unchanged after reobservation. GHerrit cannot safely retry the ambiguous operation in this invocation.",
                                    attempted.number()
                                );
                            }
                        }
                    }
                }
            }
            ProjectionStep::Done => {
                log_projection_updates(&remote, &entries, &[]);
                return Ok(());
            }
        }
    }
}

async fn reobserve_pull_requests(
    repo: &util::Repo,
    octocrab: &Octocrab,
    entries: &mut [ProjectionEntry],
) -> Result<()> {
    let head_refs = entries.iter().map(|entry| entry.commit.gherrit_id.clone()).collect::<Vec<_>>();
    let pull_requests = batch_fetch_prs(repo, octocrab, &head_refs).await?;
    ensure_pull_requests_open(pull_requests.iter().flatten().map(|pr| (pr.number, pr.state)))?;
    replace_pull_requests(entries, pull_requests)
}

fn replace_pull_requests(
    entries: &mut [ProjectionEntry],
    pull_requests: Vec<Option<PrState>>,
) -> Result<()> {
    if entries.len() != pull_requests.len() {
        bail!(
            "GitHub returned {} PR observations for {} stack commits",
            pull_requests.len(),
            entries.len()
        );
    }
    entries.iter_mut().zip(pull_requests).for_each(|(entry, pull_request)| {
        entry.pull_request = pull_request
            .map(|pr| KnownPr::new(pr.number, pr.node_id, pr.title, pr.body, pr.base_branch));
    });
    Ok(())
}

fn log_projection_updates(
    remote: &util::Remote,
    entries: &[ProjectionEntry],
    updates: &[UpdatePr],
) {
    entries.iter().for_each(|entry| {
        let pull_request = entry
            .pull_request
            .as_ref()
            .expect("projection cannot update before every commit has a pull request");
        let number = pull_request.identity.number();
        let pr_num = number.green().bold().to_string();
        let pr_url = remote.pr_url(number).blue().underline().to_string();

        if updates.iter().any(|update| update.number() == number) {
            log::debug!("Queuing update for PR #{}", pr_num);
            log::info!("Queued update for PR #{}: {}", pr_num, pr_url);
        } else {
            log::info!("PR #{} is up to date: {}", pr_num, pr_url);
        }
    });
}

fn is_private_stack(repo: &util::Repo, branch: &str) -> bool {
    // If pushRemote is set to ".", it is a private loopback stack.
    // If it is unset or anything else (e.g. 'origin'), it is public.
    repo.config_string(&format!("branch.{}.pushRemote", branch))
        .map(|val| val.as_deref() == Some("."))
        .unwrap_or(false)
}

struct Commit {
    id: ObjectId,
    gherrit_id: String,
    message_title: String,
    message_body: String,
}

impl TryFrom<gix::Commit<'_>> for Commit {
    type Error = eyre::Report;

    fn try_from(c: gix::Commit) -> Result<Self> {
        let message = c.message()?;
        let message_title = core::str::from_utf8(message.title)?.to_string();
        let message_body =
            message.body.map(|body| core::str::from_utf8(body).unwrap()).unwrap_or("").to_string();
        let mut gherrit_ids = gherrit_pr_id_re()
            .captures_iter(&message_body)
            .map(|captures| captures.get(1).unwrap().as_str());
        let gherrit_id = gherrit_ids
            .next()
            .ok_or_else(|| eyre!("Commit {} missing gherrit-pr-id trailer", c.id))?;
        if gherrit_ids.next().is_some() {
            bail!("Commit {} has multiple gherrit-pr-id trailers", c.id);
        }
        let gherrit_id = gherrit_id.to_string();

        Ok(Commit { id: c.id, gherrit_id, message_title, message_body })
    }
}

/// Fetches the global Repository Node ID for the given owner and repo.
///
/// This ID (e.g., "R_kgDOL...") is required for creating PRs via the GraphQL
/// API, as the `createPullRequest` mutation accepts a `repositoryId` argument,
/// not owner/name.
async fn fetch_repo_id(octocrab: &Octocrab, remote: &util::Remote) -> Result<String> {
    let query = RepositoryIdQuery::new(remote.owner.clone(), remote.repo_name.clone());
    let request = query.request();
    let response: serde_json::Value =
        octocrab.graphql(&request).await.wrap_err("Failed to fetch repository ID")?;
    query.decode(response)
}

/// Performs batched updates of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping updates into
/// adaptive batches and sending each batch as one GraphQL operation.
async fn batch_update_prs(
    octocrab: &Octocrab,
    updates: &[UpdatePr],
    max_batch_len: NonZeroUsize,
) -> Result<MutationExecution<()>> {
    let updates = updates.iter().cloned().map(|update| {
        let (node_id, title, body, base_branch) = update.into_parts();
        UpdatePullRequest::new(node_id, title, body, base_branch)
    });
    run_batched_mutations(octocrab, updates, max_batch_len).await
}

/// Performs batched creation of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping creations into
/// adaptive batches and sending each batch as one GraphQL operation.
///
/// Returns the newly-created PRs in creation-action order.
async fn batch_create_prs(
    octocrab: &Octocrab,
    repo_id: &str,
    creations: impl IntoIterator<Item = CreatePr>,
    max_batch_len: NonZeroUsize,
) -> Result<MutationExecution<PrState>> {
    let creations = creations.into_iter().map(|create| {
        CreatePullRequest::new(
            repo_id.to_string(),
            create.base_branch,
            create.head_branch,
            create.title,
            create.body,
        )
    });
    run_batched_mutations(octocrab, creations, max_batch_len).await
}

async fn batch_fetch_prs(
    repo: &util::Repo,
    octocrab: &Octocrab,
    head_refs: &[String],
) -> Result<Vec<Option<PrState>>> {
    let remote = repo.default_remote()?;
    let owner = remote.owner;
    let repo_name = remote.repo_name;
    let queries = head_refs
        .iter()
        .cloned()
        .map(|head_ref| FindPullRequest::new(owner.clone(), repo_name.clone(), head_ref));

    run_batched_queries(octocrab, queries).await
}

enum BatchResponse {
    Success(serde_json::Value),
    ResourceLimit,
    TooLarge,
}

enum MutationExecution<T> {
    Complete(Vec<T>),
    Ambiguous(AmbiguousMutation),
}

struct AmbiguousMutation {
    attempted: Range<usize>,
    retry_batch_len: NonZeroUsize,
}

impl AmbiguousMutation {
    fn retry_batch_len(&self) -> NonZeroUsize {
        self.retry_batch_len
    }

    fn singleton<'a, T>(&self, actions: &'a [T]) -> Option<&'a T> {
        (self.attempted.len() == 1).then(|| {
            actions
                .get(self.attempted.start)
                .expect("ambiguous mutation range identifies its input action")
        })
    }
}

async fn send_batch<O: BatchedOperation>(
    octocrab: &Octocrab,
    operation_type: OperationType,
    operations: &[O],
) -> Result<BatchResponse> {
    let document = batch_document(operation_type, operations);

    // GitHub's WAF or load balancer can silently drop or truncate very large
    // requests. A local size rejection is known not to have executed.
    if query_exceeds_limit(&document) {
        log::warn!(
            "GraphQL document size ({} bytes) exceeds heuristic limit ({} bytes).",
            document.len(),
            MAX_GRAPHQL_QUERY_BYTES
        );
        return Ok(BatchResponse::TooLarge);
    }

    log::trace!("Sending GraphQL document (length: {}): {}", document.len(), document);
    let request_payload = serde_json::json!({ "query": document });
    let response: serde_json::Value =
        octocrab.graphql(&request_payload).await.wrap_err("GraphQL batched operation failed")?;

    match classify_response(&response) {
        ResponseDisposition::Success => Ok(BatchResponse::Success(response)),
        ResponseDisposition::ResourceLimit => Ok(BatchResponse::ResourceLimit),
        ResponseDisposition::Fatal => {
            let errors = response.get("errors").expect("fatal response has errors");
            log::error!("GraphQL errors: {errors}");
            bail!("GraphQL errors: {errors:?}");
        }
    }
}

fn back_off_retryable_batch(batches: &mut BatchPlan) -> Result<()> {
    match batches.reject() {
        Ok(backoff) => {
            log::warn!(
                "Backing off GraphQL batch size from {} to {}.",
                backoff.attempted,
                backoff.retry
            );
            Ok(())
        }
        Err(item) => bail!(
            "GraphQL operation at item {} exceeds GitHub resource limits. Cannot sync.",
            item.index
        ),
    }
}

/// Executes observations, which are safe to replay after a resource limit.
async fn run_batched_queries<O>(
    octocrab: &Octocrab,
    operations: impl IntoIterator<Item = O>,
) -> Result<Vec<O::Output>>
where
    O: BatchedQuery,
{
    let operations = operations.into_iter().collect::<Vec<_>>();
    let mut outputs = Vec::with_capacity(operations.len());
    let mut batches = BatchPlan::new(operations.len(), INITIAL_GRAPHQL_BATCH_LEN);

    while let Some(range) = batches.current() {
        let chunk = &operations[range];
        match send_batch(octocrab, OperationType::Query, chunk).await? {
            BatchResponse::Success(response) => {
                outputs.extend(decode_batch_response(chunk, response)?);
                batches.accept();
            }
            BatchResponse::ResourceLimit | BatchResponse::TooLarge => {
                log::warn!("Hit GitHub resource limit with query batch of size {}", chunk.len());
                back_off_retryable_batch(&mut batches)?;
            }
        }
    }
    Ok(outputs)
}

/// Executes mutations without replaying an ambiguously acknowledged batch.
async fn run_batched_mutations<O>(
    octocrab: &Octocrab,
    operations: impl IntoIterator<Item = O>,
    max_batch_len: NonZeroUsize,
) -> Result<MutationExecution<O::Output>>
where
    O: BatchedMutation,
{
    let operations = operations.into_iter().collect::<Vec<_>>();
    let mut outputs = Vec::with_capacity(operations.len());
    let mut batches = BatchPlan::new(operations.len(), max_batch_len);

    while let Some(range) = batches.current() {
        let chunk = &operations[range.clone()];
        match send_batch(octocrab, OperationType::Mutation, chunk).await? {
            BatchResponse::Success(response) => {
                outputs.extend(decode_batch_response(chunk, response)?);
                batches.accept();
            }
            BatchResponse::TooLarge => back_off_retryable_batch(&mut batches)?,
            BatchResponse::ResourceLimit => {
                log::warn!("Hit GitHub resource limit with mutation batch of size {}", chunk.len());
                let retry_batch_len = match batches.reject() {
                    Ok(backoff) => {
                        log::warn!(
                            "Backing off GraphQL batch size from {} to {}.",
                            backoff.attempted,
                            backoff.retry
                        );
                        NonZeroUsize::new(backoff.retry).expect("backoff is nonzero")
                    }
                    Err(_) => NonZeroUsize::MIN,
                };
                return Ok(MutationExecution::Ambiguous(AmbiguousMutation {
                    attempted: range,
                    retry_batch_len,
                }));
            }
        }
    }
    Ok(MutationExecution::Complete(outputs))
}

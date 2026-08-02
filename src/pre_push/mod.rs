use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
    str,
};

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
    BatchedOperation, CreatePullRequest, FindPullRequest, PullRequest as PrState,
    RepositoryIdQuery, UpdatePullRequest, batch_document, decode_batch_response,
};
use publication::{PushTarget, plan_push, push_batches};
use reconcile::{
    CreatePr, KnownPr, ProjectionCommit, ProjectionContext, ProjectionEntry, ProjectionStep,
    UpdatePr, ensure_pull_requests_open, plan_projection,
};
use remote::observe_managed_branches;

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

    let latest_versions = push_to_origin(repo, &commits)?;
    let default_branch = repo.find_default_branch_on_default_remote();

    let num_commits = commits.len();
    sync_prs(repo, &octocrab, branch_name, &default_branch, commits, latest_versions, prs).await?;

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

#[allow(clippy::too_many_lines)]
fn push_to_origin(repo: &util::Repo, commits: &[Commit]) -> Result<HashMap<String, usize>> {
    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();

    // Fetch remote branch states to ensure we don't act on stale information.
    let remote_branch_states = observe_managed_branches(repo, &gherrit_ids)?;

    let mut next_versions = HashMap::new();

    for chunk in push_batches(commits) {
        let mut targets = Vec::with_capacity(chunk.len());

        for c in chunk {
            // Determine the next version based on local tags (Optimistic
            // Locking).
            let local_max = get_local_version(repo, &c.gherrit_id).unwrap_or(0);
            let next_ver = local_max + 1;
            next_versions.insert(c.gherrit_id.clone(), next_ver);

            // Lease the branch to ensure it hasn't changed since our fetch.
            // If we know the remote SHA, we expect it. If we don't (None), we
            // expect "" (creation).
            let expected_sha =
                remote_branch_states.get(&c.gherrit_id).map(String::as_str).unwrap_or("");

            targets.push(PushTarget {
                object_id: c.id,
                gherrit_id: &c.gherrit_id,
                version: next_ver,
                expected_remote_sha: expected_sha,
            });
        }

        let plan = plan_push(&repo.default_remote_name(), &targets);

        log::info!("Pushing chunk to remote...");
        let mut child = util::cmd("git", plan.arguments)
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err("Failed to run `git push`")?;

        // Filter output logic (elided for brevity, same as before)
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
            // If the push failed, it's likely due to a lease failure
            // (concurrent modification). If failed, it might be due to the tag
            // lock or branch lease.
            let r = repo.default_remote_name();
            bail!(
                "`git push` failed. The remote might be ahead or changed. Run `git fetch {r}` to sync."
            );
        }

        // Persist the local tags now that the push succeeded.
        for tag in plan.persisted_tags {
            let _ = repo.reference(
                format!("refs/tags/gherrit/{}/v{}", tag.gherrit_id, tag.version),
                tag.object_id,
                PreviousValue::Any,
                "gherrit: persist local version state",
            );
        }
    }

    Ok(next_versions)
}

fn get_local_version(repo: &util::Repo, gherrit_id: &str) -> Result<usize> {
    let prefix = format!("refs/tags/gherrit/{}/v", gherrit_id);
    let mut max_ver = 0;

    // Use .all() and manual filtering to avoid `prefixed` API type issues.
    let references = repo.references().map_err(|e| eyre!(e))?;

    for reference in references.all().map_err(|e| eyre!(e))? {
        let reference = reference.map_err(|e| eyre!(e))?;
        let name = reference.name().as_bstr().to_string();

        if name.starts_with(&prefix) {
            // Parse "refs/tags/gherrit/<id>/v<ver>"
            if let Some(ver_str) = name.rsplit('v').next()
                && let Ok(ver) = ver_str.parse::<usize>()
                && ver > max_ver
            {
                max_ver = ver;
            }
        }
    }

    Ok(max_ver)
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
    commits: Vec<Commit>,
    latest_versions: HashMap<String, usize>,
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

    if commits.len() != prs.len() {
        bail!("GitHub returned {} PR observations for {} stack commits", prs.len(), commits.len());
    }
    let mut entries = commits
        .into_iter()
        .zip(prs)
        .map(|(commit, pull_request)| ProjectionEntry {
            pull_request: pull_request
                .map(|pr| KnownPr::new(pr.number, pr.node_id, pr.title, pr.body, pr.base_branch)),
            commit: ProjectionCommit {
                latest_version: latest_versions.get(&commit.gherrit_id).copied().unwrap_or(1),
                gherrit_id: commit.gherrit_id,
                title: commit.message_title,
                commit_body: commit.message_body,
            },
        })
        .collect::<Vec<_>>();
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
                let created =
                    batch_create_prs(octocrab, &repo_id, creations.iter().cloned()).await?;
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
                batch_update_prs(octocrab, updates).await?;
                log::info!("Batch update complete.");
                return Ok(());
            }
            ProjectionStep::Done => {
                log_projection_updates(&remote, &entries, &[]);
                return Ok(());
            }
        }
    }
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
    updates: impl IntoIterator<Item = UpdatePr>,
) -> Result<()> {
    let updates = updates.into_iter().map(|update| {
        let (node_id, title, body, base_branch) = update.into_parts();
        UpdatePullRequest::new(node_id, title, body, base_branch)
    });
    run_batched_graphql(octocrab, updates).await?;
    Ok(())
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
) -> Result<Vec<PrState>> {
    let creations = creations.into_iter().map(|create| {
        CreatePullRequest::new(
            repo_id.to_string(),
            create.base_branch,
            create.head_branch,
            create.title,
            create.body,
        )
    });
    run_batched_graphql(octocrab, creations).await
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

    run_batched_graphql(octocrab, queries).await
}

/// Executes batched GraphQL operations (queries or mutations).
///
/// Builds a combined query for each adaptive batch and decodes each operation
/// in a successful response.
async fn run_batched_graphql<O>(
    octocrab: &Octocrab,
    operations: impl IntoIterator<Item = O>,
) -> Result<Vec<O::Output>>
where
    O: BatchedOperation,
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
    let mut batches = BatchPlan::new(operations.len(), INITIAL_GRAPHQL_BATCH_LEN);
    while let Some(range) = batches.current() {
        let chunk = &operations[range];
        let query = batch_document(chunk);

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
            let response: serde_json::Value = octocrab
                .graphql(&request_payload)
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

        outputs.extend(decode_batch_response(chunk, response)?);

        batches.accept();
    }
    Ok(outputs)
}

use std::{collections::HashMap, process::Stdio};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::{reference::Category, refs::transaction::PreviousValue};
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
mod local;
mod publication;
mod reconcile;
mod remote;

use batching::{
    BatchPlan, INITIAL_QUERY_BATCH_LEN, MAX_GRAPHQL_QUERY_BYTES, ResponseDisposition,
    classify_response, query_exceeds_limit,
};
use body::PrBody;
use github::{
    CreatePullRequest, CreatedPullRequest, FindPullRequest, MutationOperation,
    PullRequest as PrState, QueryOperation, RepositoryIdQuery, UpdatePullRequest,
    decode_mutation_batch_response, decode_query_batch_response, prepare_mutation_batches,
    query_batch_document,
};
use local::LocalStack;
use publication::{PushTarget, plan_push, push_batches};
use reconcile::{
    CurrentPr, DesiredPr, PrUpdate, PullRequestState, ensure_pull_requests_open, link_stack,
    plan_update,
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

    let commits = LocalStack::collect(repo).wrap_err("Failed to collect commits")?;

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
    let prs = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;
    ensure_pull_requests_open(prs.iter().map(|pr| (pr.number, pr.state)))?;

    let latest_versions = push_to_origin(repo, &commits)?;
    let default_branch = repo.find_default_branch_on_default_remote();

    let num_commits = commits.len();
    sync_prs(repo, &octocrab, branch_name, &default_branch, &commits, latest_versions, prs).await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn push_to_origin(repo: &util::Repo, commits: &LocalStack) -> Result<HashMap<String, usize>> {
    let gherrit_ids =
        commits.iter().map(|commit| commit.id().as_str().to_owned()).collect::<Vec<_>>();

    // Fetch remote branch states to ensure we don't act on stale information.
    let remote_branch_states = observe_managed_branches(repo, &gherrit_ids)?;

    let mut next_versions = HashMap::new();

    for chunk in push_batches(commits.as_slice()) {
        let mut targets = Vec::with_capacity(chunk.len());

        for c in chunk {
            // Determine the next version based on local tags (Optimistic
            // Locking).
            let local_max = get_local_version(repo, c.id().as_str()).unwrap_or(0);
            let next_ver = local_max + 1;
            next_versions.insert(c.id().as_str().to_owned(), next_ver);

            // Lease the branch to ensure it hasn't changed since our fetch.
            // If we know the remote SHA, we expect it. If we don't (None), we
            // expect "" (creation).
            let expected_sha =
                remote_branch_states.get(c.id().as_str()).map(String::as_str).unwrap_or("");

            targets.push(PushTarget {
                object_id: c.head(),
                gherrit_id: c.id().as_str(),
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
    commits: &LocalStack,
    latest_versions: HashMap<String, usize>,
    prs: Vec<PrState>,
) -> Result<()> {
    let remote = repo.default_remote()?;

    let commits = link_stack(base_branch, commits.iter(), |commit| commit.id().as_str().to_owned());

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
        let repo_id = fetch_repo_id(octocrab, &remote).await?;
        let created = batch_create_prs(octocrab, &repo_id, creations).await?;
        assert_eq!(created.len(), num_creations);
        log::info!("Created {num_creations} PRs.");
        created
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
                        remote.pr_url(created.number).blue().underline()
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

    let public_branch = (!is_private_stack(repo, branch_name))
        .then(|| {
            let head_ref = repo.head().ok()?.try_into_referent()?;
            let (cat, short_name) = head_ref.inner.name.category_and_short_name()?;
            (cat == Category::LocalBranch).then(|| short_name.to_string())
        })
        .flatten();

    let repo_url = remote.repo_url_relative();
    let stack_pr_numbers =
        commit_pr_states.iter().map(|(_, state)| state.number).collect::<Vec<_>>();
    let updates: Vec<PrUpdate> = commit_pr_states
        .iter()
        .filter_map(|(entry, pr_state)| {
            let c = &entry.item;
            let latest_version = latest_versions.get(c.id().as_str()).copied().unwrap_or(1);

            let body = PrBody {
                commit_body: c.body(),
                repo_url: &repo_url,
                public_branch: public_branch.as_deref(),
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
            let pr_url = remote.pr_url(pr_state.number).blue().underline().to_string();

            let update = plan_update(
                CurrentPr {
                    node_id: &pr_state.node_id,
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

/// A request to create a new PR in a batch.
#[derive(Clone)]
struct BatchCreate {
    title: String,
    body: String,
    base_branch: String,
    head_branch: String,
}

/// Fetches the global Repository Node ID for the given owner and repo.
///
/// This ID (e.g., "R_kgDOL...") is required for creating PRs via the GraphQL
/// API, as the `createPullRequest` mutation accepts a `repositoryId` argument,
/// not owner/name.
async fn fetch_repo_id(octocrab: &Octocrab, remote: &util::Remote) -> Result<String> {
    let query = RepositoryIdQuery::new(remote.owner.clone(), remote.repo_name.clone());
    let request = query.request();
    let response =
        run_graphql_query(octocrab, &request).await.wrap_err("Failed to fetch repository ID")?;
    query.decode(response)
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
/// Returns the newly-created PRs keyed by their head branches.
async fn batch_create_prs(
    octocrab: &Octocrab,
    repo_id: &str,
    creations: impl IntoIterator<Item = BatchCreate>,
) -> Result<HashMap<String, CreatedPullRequest>> {
    let creations = creations.into_iter().map(|create| {
        CreatePullRequest::new(
            repo_id.to_string(),
            create.base_branch,
            create.head_branch,
            create.title,
            create.body,
        )
    });
    Ok(run_graphql_mutations(octocrab, creations)
        .await?
        .into_iter()
        .map(|created| (created.head_branch.clone(), created))
        .collect())
}

async fn batch_fetch_prs(
    repo: &util::Repo,
    octocrab: &Octocrab,
    head_refs: &[String],
) -> Result<Vec<PrState>> {
    let remote = repo.default_remote()?;
    let owner = remote.owner;
    let repo_name = remote.repo_name;
    let queries = head_refs
        .iter()
        .cloned()
        .map(|head_ref| FindPullRequest::new(owner.clone(), repo_name.clone(), head_ref));

    Ok(run_batched_queries(octocrab, queries).await?.into_iter().flatten().collect())
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
    const INDETERMINATE: &str = "GraphQL mutation acknowledgement is indeterminate; stop this publication attempt and retry the push to reobserve GitHub state";

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
        let response = octocrab.graphql(&batch.request).await.wrap_err(INDETERMINATE)?;
        outputs
            .extend(decode_mutation_batch_response(operations, response).wrap_err(INDETERMINATE)?);
        O::validate_receipts(&outputs).wrap_err(INDETERMINATE)?;
    }

    Ok(outputs)
}

const MAX_GRAPHQL_QUERY_RETRIES: usize = 3;

fn is_retryable_query_transport_error(error: &octocrab::Error) -> bool {
    matches!(error, octocrab::Error::Service { .. } | octocrab::Error::Hyper { .. })
}

/// Executes one read-only GraphQL request with bounded transport retries.
///
/// Octocrab's method-agnostic retry middleware is disabled because it can
/// replay mutation POSTs. Keeping retries here makes read-only intent explicit:
/// connection failures, response-body transport failures, HTTP 429, and HTTP
/// 5xx responses get at most three retries. Redirects are never followed.
async fn run_graphql_query(
    octocrab: &Octocrab,
    request: &serde_json::Value,
) -> octocrab::Result<serde_json::Value> {
    let mut retries = 0;

    loop {
        let response = match octocrab._post("/graphql", Some(request)).await {
            Ok(response) => response,
            Err(error)
                if retries < MAX_GRAPHQL_QUERY_RETRIES
                    && is_retryable_query_transport_error(&error) =>
            {
                retries += 1;
                log::warn!(
                    "Retrying read-only GraphQL request after a transport failure ({retries}/{MAX_GRAPHQL_QUERY_RETRIES})"
                );
                continue;
            }
            Err(error) => return Err(error),
        };

        let status = response.status();
        if retries < MAX_GRAPHQL_QUERY_RETRIES
            && (status.is_server_error() || status.as_u16() == 429)
        {
            retries += 1;
            log::warn!(
                "Retrying read-only GraphQL request after HTTP {status} ({retries}/{MAX_GRAPHQL_QUERY_RETRIES})"
            );
            continue;
        }

        let response = octocrab::map_github_error(response).await;
        let response = match response {
            Ok(response) => {
                <serde_json::Value as octocrab::FromResponse>::from_response(response).await
            }
            Err(error) => Err(error),
        };
        match response {
            Err(error)
                if retries < MAX_GRAPHQL_QUERY_RETRIES
                    && is_retryable_query_transport_error(&error) =>
            {
                retries += 1;
                log::warn!(
                    "Retrying read-only GraphQL request after a response transport failure ({retries}/{MAX_GRAPHQL_QUERY_RETRIES})"
                );
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
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        sync::oneshot,
    };

    use super::*;
    use crate::pre_push::batching::MAX_MUTATION_ALIASES;

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

    async fn write_json_response(stream: &mut TcpStream, response: &Value) {
        let body = serde_json::to_vec(response).expect("serialize JSON response");
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.expect("write HTTP response headers");
        stream.write_all(&body).await.expect("write HTTP response body");
        stream.shutdown().await.expect("finish HTTP response");
    }

    fn test_octocrab(listener: &TcpListener) -> Octocrab {
        Octocrab::builder()
            .base_uri(format!("http://{}", listener.local_addr().expect("listener address")))
            .expect("valid test endpoint")
            .build()
            .expect("build test client")
    }

    #[tokio::test]
    async fn query_retries_when_the_connection_closes_before_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test server");
        let octocrab = test_octocrab(&listener);
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept query request");
                requests.push(read_json_request(&mut stream).await);
                if attempt == 0 {
                    // The request reached the peer, but it closed the
                    // connection without sending any response headers.
                    drop(stream);
                } else {
                    write_json_response(&mut stream, &json!({ "data": { "ok": true } })).await;
                }
            }
            requests
        });
        let request = json!({ "query": "query { viewer { login } }" });

        let response =
            tokio::time::timeout(ADAPTER_TIMEOUT, run_graphql_query(&octocrab, &request))
                .await
                .expect("query retry completed")
                .expect("second query attempt succeeded");
        let requests = tokio::time::timeout(ADAPTER_TIMEOUT, server)
            .await
            .expect("test server stopped")
            .expect("test server completed");

        assert_eq!(response, json!({ "data": { "ok": true } }));
        assert_eq!(requests, [request.clone(), request]);
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

    fn create_mutation(index: usize) -> CreatePullRequest {
        CreatePullRequest::new(
            "REPO_NODE_ID".to_string(),
            "main".to_string(),
            format!("G{index}"),
            format!("Title {index}"),
            format!("Body {index}"),
        )
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
        let mutations = (0..OPERATION_COUNT).map(create_mutation).collect::<Vec<_>>();

        let result =
            tokio::time::timeout(ADAPTER_TIMEOUT, run_graphql_mutations(&octocrab, mutations))
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
            CreateReceiptCollision::Number => "repeats pull request number 1",
            CreateReceiptCollision::NodeId => "repeats pull request node ID 'PR_1'",
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
}

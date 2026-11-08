//! Publish the current local stack as Git refs and GitHub pull requests.
//!
//! The order is: collect and validate commits, look up PRs and reject terminal
//! ones, publish Git refs, then create missing PRs and update their metadata.
//! Publishing Git first ensures that PR branches exist before creation. PR
//! numbers are learned before rendering navigation links for the whole stack.
//!
//! These stages are not one transaction. Git pushes are atomic within each
//! batch, and later Git or GraphQL batches can fail after earlier ones succeed.
//! Errors stop the command; they do not roll back remote changes. A nonempty
//! managed publication creates version tags even when objects are unchanged.
//!
//! Small modules own local decisions and codecs: `autosquash` checks whether
//! history is ready, `remote` observes branch tips, `publication` spells Git
//! updates, `github` encodes API operations, `batching` chooses request sizes,
//! `reconcile` derives PR relationships and changed fields, and `body` renders
//! descriptions. This module owns their effect order and shared inputs.

use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::{ObjectId, reference::Category, refs::transaction::PreviousValue};
use octocrab::Octocrab;
use owo_colors::OwoColorize;

use crate::{
    re,
    util::{self, CommandExt as _, HeadState},
};

mod autosquash;
mod batching;
mod body;
mod github;
mod identity;
mod publication;
mod reconcile;
mod refs {
    //! Managed ref names shared by publication, observation and local
    //! persistence. Parsing and admission stay with the caller; these functions
    //! only spell names.

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

use batching::{
    BatchPlan, INITIAL_GRAPHQL_BATCH_LEN, MAX_GRAPHQL_QUERY_BYTES, ResponseDisposition,
    classify_response, query_exceeds_limit,
};
use body::PrBody;
use github::{
    BatchedOperation, CreatePullRequest, CreatedPullRequest, FindPullRequest,
    PullRequest as PrState, RepositoryIdQuery, UpdatePullRequest, batch_document,
    decode_batch_response,
};
use identity::ChangeId;
use publication::{PushTarget, Version, plan_push, push_batches};
use reconcile::{
    CurrentPr, DesiredPr, PrUpdate, ensure_pull_requests_open, link_stack, plan_update,
};
use remote::observe_managed_branches;

use crate::commit_message::strip_gherrit_id;

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

    let gherrit_ids: Vec<ChangeId> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    // Reject a known closed/merged PR before publishing Git refs. This is an
    // observation, not a lock: another actor can change PR lifecycle afterward.
    let prs = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;
    ensure_pull_requests_open(prs.iter().map(|pr| (pr.number, pr.state)))?;

    let commits = push_to_origin(repo, commits)?;
    let default_branch = repo.find_default_branch_on_default_remote()?;

    let num_commits = commits.len();
    sync_prs(repo, &octocrab, public_branch.as_deref(), &default_branch, commits, prs).await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

fn collect_commits(repo: &util::Repo) -> Result<Vec<Commit>> {
    // Stack membership is based on the selected local default-branch ref, not
    // a freshly fetched remote tip. The user must update/rebase local history
    // when that ref no longer describes the intended publication base.
    let head = repo.rev_parse_single("HEAD")?;
    let default_branch = repo.find_default_branch_on_default_remote()?;
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
        &repo.default_remote_name()?,
        &default_branch,
    )?;

    let trailers = read_commit_trailers(&commits)?;
    let commits = commits
        .into_iter()
        .zip(trailers)
        .map(|((commit, _), trailers)| Commit::from_git(commit, &trailers))
        .collect::<Result<Vec<_>>>()?;
    ensure_unique_gherrit_ids(commits.iter().map(|commit| commit.gherrit_id.as_str()))?;
    Ok(commits)
}

fn read_commit_trailers(commits: &[(gix::Commit<'_>, String)]) -> Result<Vec<Vec<u8>>> {
    // Let native Git recognize trailers in each complete committed message.
    // Query several objects per child process, but retain their requested
    // order so a trailer can never silently migrate to a different commit.
    const QUERY_BATCH_LEN: usize = 120;
    const FORMAT: &str = "--format=tformat:%H%x00%(trailers:only,unfold)";

    commits.chunks(QUERY_BATCH_LEN).try_fold(
        Vec::with_capacity(commits.len()),
        |mut parsed, chunk| {
            let arguments = ["log", "--no-walk=unsorted", "-z", FORMAT]
                .into_iter()
                .map(ToString::to_string)
                .chain(chunk.iter().map(|(commit, _)| commit.id.to_string()));
            let output = util::cmd("git", arguments)
                .checked_output()
                .wrap_err("Failed to parse commit trailers")?;
            let object_ids = chunk.iter().map(|(commit, _)| commit.id).collect::<Vec<_>>();
            parsed.extend(decode_commit_trailers(&output.stdout, &object_ids)?);
            Ok(parsed)
        },
    )
}

fn decode_commit_trailers(output: &[u8], object_ids: &[ObjectId]) -> Result<Vec<Vec<u8>>> {
    // Each record is OID NUL trailers NUL, with one final empty split field.
    // Newlines belong to trailer content; NUL framing separates the records.
    // Validate counts and OIDs before exposing any values to ID validation.
    let mut parsed = Vec::with_capacity(object_ids.len());
    let mut fields = output.split(|byte| *byte == 0);
    for object_id in object_ids {
        let returned_id = fields
            .next()
            .ok_or_else(|| eyre!("Git omitted trailer data for commit {object_id}"))?;
        if returned_id != object_id.to_string().as_bytes() {
            bail!("Git returned commit trailers out of order");
        }
        let trailers = fields
            .next()
            .ok_or_else(|| eyre!("Git omitted trailer data for commit {object_id}"))?;
        parsed.push(trailers.to_vec());
    }
    if fields.next() != Some(&[][..]) || fields.next().is_some() {
        bail!("Git returned malformed commit trailer data");
    }
    Ok(parsed)
}

fn ensure_unique_gherrit_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<()> {
    // Two objects with one logical ID would both target the same branch and
    // PR. Reject the complete stack before any publication effect is issued.
    ids.into_iter().try_fold(HashSet::new(), |mut seen, id| {
        if !seen.insert(id) {
            bail!("Stack contains multiple commits with gherrit-pr-id '{id}'");
        }
        Ok(seen)
    })?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn push_to_origin(repo: &util::Repo, commits: Vec<Commit>) -> Result<Vec<VersionedCommit>> {
    let gherrit_ids: Vec<ChangeId> = commits.iter().map(|c| c.gherrit_id.clone()).collect();

    // Read branch tips for explicit lease expectations. ls-remote does not
    // fetch objects or version tags, and these observations can become stale;
    // Git must check each branch's expected value when applying its update.
    let remote_branch_states = observe_managed_branches(repo, &gherrit_ids)?;

    // One local observation for the entire attempt. Failed reads and counter
    // exhaustion must be discovered before any publication batch is pushed.
    let local_versions = get_local_versions(repo, &gherrit_ids)
        .wrap_err("Failed to read local version refs; no publication batch was pushed")?;
    let commits = commits
        .into_iter()
        .map(|commit| {
            let id = &commit.gherrit_id;
            let version = Version::next_after(local_versions[id]).ok_or_else(|| {
                eyre!("Local version counter exhausted for '{id}'; no publication batch was pushed")
            })?;
            Ok(VersionedCommit { commit, version })
        })
        .collect::<Result<Vec<_>>>()?;

    // Each chunk is a separate Git transaction. Local version refs are saved
    // after its success, so a later chunk failure can leave a published prefix.
    // Version numbers come from local tags; stale/missing local history is not
    // repaired by the remote branch-tip observation above.
    for chunk in push_batches(&commits) {
        let mut targets = Vec::with_capacity(chunk.len());

        for entry in chunk {
            let c = &entry.commit;

            // Absence remains distinct from a parsed object ID until the
            // command builder spells Git's empty creation-lease expectation.
            let expected_remote_tip = remote_branch_states.get(&c.gherrit_id).copied();

            targets.push(PushTarget {
                object_id: c.id,
                gherrit_id: &c.gherrit_id,
                version: entry.version,
                expected_remote_tip,
            });
        }

        let plan = plan_push(&repo.default_remote_name()?, &targets);

        log::info!("Pushing chunk to remote...");
        let mut child = util::cmd("git", plan.arguments)
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err("Failed to run `git push`")?;

        // Preserve Git's diagnostics except the generated "create a PR" invite,
        // since this command creates the PR itself. Buffer consecutive remote:
        // lines so the invitation can be recognized as a multiline block.
        let status = finish_push(&mut child, std::io::stderr().lock())?;
        if !status.success() {
            // If the push failed, it's likely due to a lease failure
            // (concurrent modification). If failed, it might be due to the tag
            // lock or branch lease.
            let r = repo.default_remote_name()?;
            bail!(
                "`git push` failed. The remote might be ahead or changed. Run `git fetch {r}` to sync."
            );
        }

        // The remote batch has already committed. Try every local ref and
        // distinguish local persistence failure from an unapplied push.
        let mut persistence_errors = Vec::new();
        for tag in plan.persisted_tags {
            let name = refs::version_tag(tag.gherrit_id.as_str(), tag.version.get());
            if let Err(error) = repo.reference(
                name.clone(),
                tag.object_id,
                PreviousValue::Any,
                "gherrit: persist local version state",
            ) {
                persistence_errors.push(format!("{name}: {error}"));
            }
        }
        if !persistence_errors.is_empty() {
            bail!(
                "Remote publication batch succeeded, but local version refs were not all persisted: {}. Remote changes are already applied; inspect and fetch version refs before retrying.",
                persistence_errors.join("; ")
            );
        }
    }

    Ok(commits)
}

fn without_pr_invitation(block: &str) -> std::borrow::Cow<'_, str> {
    re!(r"(?m)\n?^remote:\s*\nremote: Create a pull request for '.*' on GitHub by visiting:\s*\nremote:\s*https://github\.com/.*\nremote:\s*$").replace(block, "")
}

/// Own the pipe until it is consumed or dropped, then reap even if relaying
/// diagnostics failed. Once dispatched, an I/O error cannot prove no effect.
fn finish_push(
    child: &mut std::process::Child,
    output: impl std::io::Write,
) -> Result<std::process::ExitStatus> {
    let diagnostics = match child.stderr.take() {
        Some(stderr) => relay_push_diagnostics(std::io::BufReader::new(stderr), output),
        None => Err(std::io::Error::other("Git push stderr pipe is unavailable")),
    };
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            // Best-effort cleanup after wait itself failed. The remote outcome
            // is unknown even if termination and the second wait succeed.
            let termination = child.kill();
            let reaping = child.wait();
            bail!(
                "Failed to wait for git push: {error}; termination: {termination:?}; reaping: {reaping:?}. Remote changes may have applied; inspect remote refs before retrying."
            );
        }
    };
    diagnostics.wrap_err_with(|| format!("Failed to relay git push diagnostics (child status {status}). Remote changes may have applied; inspect remote refs before retrying."))?;
    Ok(status)
}

fn relay_push_diagnostics(
    mut reader: impl std::io::BufRead,
    mut output: impl std::io::Write,
) -> std::io::Result<()> {
    fn flush(buffer: &mut Vec<String>, output: &mut impl std::io::Write) -> std::io::Result<()> {
        if !buffer.is_empty() {
            let block = buffer.join("\n");
            let cleaned = without_pr_invitation(&block);
            if !cleaned.is_empty() {
                writeln!(output, "{cleaned}")?;
            }
            buffer.clear();
        }
        Ok(())
    }
    let result = (|| {
        let mut buffer = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            // Match BufRead::lines for valid text, including CRLF and a final
            // unterminated line. Non-UTF-8 diagnostics are relayed as bytes.
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            match std::str::from_utf8(&line) {
                Ok(text) if text.trim_start().starts_with("remote:") => {
                    buffer.push(text.to_owned())
                }
                _ => {
                    flush(&mut buffer, &mut output)?;
                    output.write_all(&line)?;
                    output.write_all(b"\n")?;
                }
            }
        }
        flush(&mut buffer, &mut output)
    })();
    if result.is_err() {
        // A broken output stream must not leave Git blocked on its stderr
        // pipe. If reading also fails, dropping the owned pipe closes it.
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    }
    result
}

fn local_version_entry(name: &str) -> Option<(&str, usize)> {
    let tail = name.strip_prefix("refs/tags/gherrit/")?;
    let (id, version) = tail.split_once("/v")?;
    // Preserve the legacy last-'v' numeric suffix, including noncanonical
    // spellings. Changing archive admission is a separate policy decision.
    let version = version.rsplit('v').next()?.parse().ok()?;
    Some((id, version))
}

fn get_local_versions(repo: &util::Repo, ids: &[ChangeId]) -> Result<HashMap<ChangeId, usize>> {
    // An ID with no recognized local tag starts at zero. Scan all refs once
    // for the whole stack; decoding/iteration errors must stop publication
    // rather than turn incomplete history into a lower version number.
    let mut versions = ids.iter().map(|id| (id.clone(), 0)).collect::<HashMap<_, _>>();
    let references = repo.references().map_err(|error| eyre!(error))?;
    for reference in references.all().map_err(|error| eyre!(error))? {
        let reference = reference.map_err(|error| eyre!(error))?;
        let name = reference.name().as_bstr().to_string();
        if let Some((id, version)) = local_version_entry(&name)
            && let Some(current) = versions.get_mut(id)
        {
            *current = (*current).max(version);
        }
    }
    Ok(versions)
}

fn pull_requests_by_head(prs: &[PrState]) -> HashMap<&str, &PrState> {
    let mut by_head = HashMap::with_capacity(prs.len());
    for pr in prs {
        // Preserve the previous linear search's first-match behavior.
        by_head.entry(pr.head_branch.as_str()).or_insert(pr);
    }
    by_head
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
    public_branch: Option<&str>,
    base_branch: &str,
    commits: Vec<VersionedCommit>,
    prs: Vec<PrState>,
) -> Result<()> {
    let remote = repo.default_remote()?;

    let commits = link_stack(base_branch, commits, |entry| entry.commit.gherrit_id.to_string());

    let prs_by_head = pull_requests_by_head(&prs);

    // Preserve one slot per commit. Only missing slots become create requests;
    // existing PRs keep the index's established first-match policy.
    let resolutions: Vec<_> = commits
        .iter()
        .map(|entry| {
            let c = &entry.item.commit;
            if let Some(&pr) = prs_by_head.get(c.gherrit_id.as_str()) {
                log::debug!("Found existing PR #{} for {}", pr.number.green().bold(), c.gherrit_id);
                Some(pr.clone())
            } else {
                log::debug!("No GitHub PR exists for {}; queuing creation...", c.gherrit_id);
                None
            }
        })
        .collect();

    // Creation uses the plain commit body first. Full descriptions depend on
    // every PR number, so final projection remains a separate write phase.
    let num_creations = resolutions.iter().filter(|state| state.is_none()).count();
    let created = if num_creations != 0 {
        log::info!("Creating {num_creations} PRs...");
        let repo_id = fetch_repo_id(octocrab, &remote).await?;
        let requests = commits.iter().zip(&resolutions).filter(|(_, state)| state.is_none()).map(
            |(entry, _)| {
                let c = &entry.item.commit;
                CreatePullRequest::new(
                    repo_id.clone(),
                    entry.base_branch.clone(),
                    c.gherrit_id.to_string(),
                    c.message_title.clone(),
                    c.message_body.clone(),
                )
            },
        );
        let created = run_batched_graphql(octocrab, requests).await?;
        if created.len() != num_creations {
            bail!(
                "GitHub creation results did not match the requested PR count; PRs may have been created"
            );
        }
        log::info!("Created {num_creations} PRs.");
        created
    } else {
        Vec::new()
    };

    // The batch decoder correlates aliases to requests and returns their order.
    // Consume each creation once, in the order of the missing slots. Its codec
    // already attached the request metadata, so no head-keyed map or second
    // copy of the creation payload is needed to reconstruct a resolved PR.
    let mut created = created.into_iter();
    let mut commit_pr_states = Vec::with_capacity(commits.len());
    for (entry, existing) in commits.iter().zip(resolutions) {
        let state = match existing {
            Some(state) => state,
            None => {
                let id = &entry.item.commit.gherrit_id;
                let CreatedPullRequest { pull_request, url } =
                    created.next().ok_or_else(|| eyre!("Failed to resolve created PR for {id}"))?;
                if pull_request.head_branch != id.as_str() {
                    bail!(
                        "Created PR response did not match requested head '{id}'; PRs may have been created"
                    );
                }
                log::info!(
                    "Created PR #{}: {}",
                    pull_request.number.green().bold(),
                    url.blue().underline()
                );
                pull_request
            }
        };
        commit_pr_states.push((entry, state));
    }

    let repo_url = remote.repo_url_relative();
    let stack_pr_numbers =
        commit_pr_states.iter().map(|(_, state)| state.number).collect::<Vec<_>>();
    // Use one resolved base-to-head order for both navigation and adjacency.
    // plan_update omits unchanged fields, especially an unchanged base: GitHub
    // can reject a redundant base edit for a PR in the merge queue.
    let updates: Vec<PrUpdate> = commit_pr_states
        .iter()
        .filter_map(|(entry, pr_state)| {
            let c = &entry.item.commit;
            let latest_version = entry.item.version.get();

            let body = PrBody {
                commit_body: &c.message_body,
                repo_url: &repo_url,
                public_branch,
                stack_pr_numbers: &stack_pr_numbers,
                current_pr_number: pr_state.number,
                latest_version,
                base_branch: &entry.base_branch,
                gherrit_id: c.gherrit_id.as_str(),
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
                DesiredPr { title: &c.message_title, body: &body, base_branch: &entry.base_branch },
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

fn public_branch_name(repo: &util::Repo, branch: &str) -> Result<Option<String>> {
    if repo.config_string(&format!("branch.{branch}.pushRemote"))?.as_deref() == Some(".") {
        return Ok(None);
    }
    Ok(repo.head()?.try_into_referent().and_then(|head_ref| {
        let (category, short_name) = head_ref.inner.name.category_and_short_name()?;
        (category == Category::LocalBranch).then(|| short_name.to_string())
    }))
}

/// One revision paired with the version selected for it. Keep that relationship
/// through publication and PR rendering rather than joining again by ID.
struct VersionedCommit {
    commit: Commit,
    version: Version,
}

struct Commit {
    // These identities have different lifetimes: id changes on a rewrite;
    // gherrit_id is retained so the rewritten change updates the same PR.
    id: ObjectId,
    gherrit_id: ChangeId,
    message_title: String,
    message_body: String,
}

impl Commit {
    fn from_git(c: gix::Commit<'_>, trailers: &[u8]) -> Result<Self> {
        let message = c.message()?;
        let message_title = core::str::from_utf8(message.title)?.to_string();
        let message_body = message
            .body
            .map(|body| core::str::from_utf8(body))
            .transpose()
            .wrap_err_with(|| format!("Commit {} body is not UTF-8", c.id))?
            .unwrap_or("")
            .to_string();
        let gherrit_id = committed_gherrit_id(trailers, c.id)?;
        let message_body = strip_gherrit_id(&message_body, gherrit_id.as_str());

        Ok(Commit { id: c.id, gherrit_id, message_title, message_body })
    }
}

fn committed_gherrit_id(trailers: &[u8], object_id: ObjectId) -> Result<ChangeId> {
    // The commit hook only preserves/inserts an identity. Publication is the
    // admission boundary: ambiguous or unsafe IDs must not become ref names.
    let mut gherrit_ids = crate::commit_message::gherrit_id_values(trailers);
    let gherrit_id = gherrit_ids
        .next()
        .ok_or_else(|| eyre!("Commit {} missing gherrit-pr-id trailer", object_id))?;
    if gherrit_ids.next().is_some() {
        bail!("Commit {} has multiple gherrit-pr-id trailers", object_id);
    }
    if gherrit_id.is_empty() {
        bail!("Commit {} missing gherrit-pr-id trailer", object_id);
    }
    ChangeId::parse(gherrit_id)
        .ok_or_else(|| eyre!("Commit {} has invalid gherrit-pr-id trailer", object_id))
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
/// Group updates to reduce request count, shrinking batches on selected
/// resource-limit responses. This is not a general rate-limit retry policy.
async fn batch_update_prs(octocrab: &Octocrab, updates: Vec<PrUpdate>) -> Result<()> {
    let updates = updates.into_iter().map(|update| {
        UpdatePullRequest::new(update.node_id, update.title, update.body, update.base_branch)
    });
    run_batched_graphql(octocrab, updates).await?;
    Ok(())
}

async fn batch_fetch_prs(
    repo: &util::Repo,
    octocrab: &Octocrab,
    head_refs: &[ChangeId],
) -> Result<Vec<PrState>> {
    let remote = repo.default_remote()?;
    let owner = remote.owner;
    let repo_name = remote.repo_name;
    let queries = head_refs.iter().cloned().map(|head_ref| {
        FindPullRequest::new(owner.clone(), repo_name.clone(), head_ref.to_string())
    });

    Ok(run_batched_graphql(octocrab, queries).await?.into_iter().flatten().collect())
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

    // GraphQL resource use depends on both item count and document size. Start
    // with a fixed batch cap, then reduce the unaccepted range on the resource
    // responses recognized by classify_response. This changes batch size,
    // not timing; transport failures do not enter this retry path.
    //
    // [1] https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api#node-limit
    // [2] https://github.blog/changelog/2025-09-01-graphql-api-resource-limits/
    let mut batches = BatchPlan::new(operations.len(), INITIAL_GRAPHQL_BATCH_LEN);
    while let Some(attempt) = batches.current() {
        let chunk = &operations[attempt.range()];
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
            match attempt.reject() {
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

        // Advance only after every alias decodes. A malformed acknowledgement
        // still may follow an applied mutation; returning an error here does
        // not establish that GitHub left the remote state unchanged.
        outputs.extend(decode_batch_response(chunk, response)?);

        attempt.accept();
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed_pr(number: u64, head: &str) -> PrState {
        PrState {
            number,
            node_id: format!("PR_{number}"),
            title: None,
            body: None,
            base_branch: "main".to_string(),
            head_branch: head.to_string(),
            state: reconcile::PullRequestState::Open,
        }
    }

    #[test]
    fn push_relay_preserves_text_and_non_utf8_diagnostics() {
        let input = b"remote: server warning\r\nlocal diagnostic\r\nremote: \xff\nlast";
        let mut output = Vec::new();
        relay_push_diagnostics(std::io::Cursor::new(input), &mut output).unwrap();
        assert_eq!(output, b"remote: server warning\nlocal diagnostic\nremote: \xff\nlast\n");
    }

    struct FailedOutput;
    impl std::io::Write for FailedOutput {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn push_relay_drains_after_output_or_read_failure() {
        let mut reader = std::io::Cursor::new(b"first\nsecond\nlast\n");
        assert!(relay_push_diagnostics(&mut reader, FailedOutput).is_err());
        assert_eq!(reader.position(), reader.get_ref().len() as u64);

        struct ReadFailure {
            reader: std::io::Cursor<Vec<u8>>,
            fail: bool,
        }
        impl std::io::Read for ReadFailure {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.reader.read(bytes)
            }
        }
        impl std::io::BufRead for ReadFailure {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                if std::mem::take(&mut self.fail) {
                    return Err(std::io::Error::other("fixture read failure"));
                }
                self.reader.fill_buf()
            }
            fn consume(&mut self, n: usize) {
                self.reader.consume(n);
            }
        }
        let mut reader =
            ReadFailure { reader: std::io::Cursor::new(b"remaining bytes".to_vec()), fail: true };
        let error = relay_push_diagnostics(&mut reader, Vec::new()).unwrap_err();
        assert_eq!(error.to_string(), "fixture read failure");
        assert_eq!(reader.reader.position(), reader.reader.get_ref().len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn push_child_is_drained_and_reaped_after_output_failure() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "i=0; while [ $i -lt 10000 ]; do echo diagnostic >&2; i=$((i+1)); done"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let error = finish_push(&mut child, FailedOutput).unwrap_err();
        assert!(format!("{error:#}").contains("Remote changes may have applied"));
        assert!(child.try_wait().unwrap().unwrap().success());

        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(finish_push(&mut child, Vec::new()).unwrap().code(), Some(3));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn local_version_index_retains_legacy_numeric_suffixes() {
        for (name, version) in [("v2", 2), ("v0002", 2), ("v+2", 2), ("vjunkv3", 3)] {
            let full = format!("refs/tags/gherrit/Gcase/{name}");
            assert_eq!(local_version_entry(&full), Some(("Gcase", version)));
        }
        for name in ["refs/heads/Gcase", "refs/tags/gherrit/Gcase/vno", "refs/tags/other/v2"] {
            assert!(local_version_entry(name).is_none());
        }
    }

    #[test]
    fn batched_mutation_http_contracts_use_literal_requests_and_raw_responses() {
        use testutil::scripted_http::{Exchange, HttpRequest, ScriptedHttp};
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _entered = runtime.enter();
        for (status, response, success) in [
            (200, br#"{"data":{"op0":{"clientMutationId":null}}}"#.as_slice(), true),
            (200, b"not JSON".as_slice(), false),
            (200, br#"{"data":{}}"#.as_slice(), false),
            (
                400,
                br#"{"message":"Bad request","documentation_url":"https://example.invalid/docs"}"#
                    .as_slice(),
                false,
            ),
        ] {
            let server = ScriptedHttp::start([Exchange {
                request: HttpRequest { method: "POST".to_string(), path: "/graphql".to_string(),
                    body: br#"{"query":"mutation { op0: updatePullRequest(input: { pullRequestId: \"PR_1\" }) { clientMutationId } }"}"#.to_vec() },
                status, response: response.to_vec(),
            }]);
            let client = Octocrab::builder()
                .personal_token("fixture-token".to_string())
                .base_uri(&server.url)
                .unwrap()
                .build()
                .unwrap();
            let result = runtime.block_on(run_batched_graphql(
                &client,
                [UpdatePullRequest::new("PR_1".to_string(), None, None, None)],
            ));
            assert_eq!(result.is_ok(), success, "status={status}, result={result:?}");
            assert_eq!(
                server.arrivals().len(),
                1,
                "a malformed acknowledgement must not be replayed"
            );
        }
    }

    #[test]
    fn transient_pr_index_preserves_first_match() {
        let prs = [observed_pr(1, "Ga"), observed_pr(2, "Gb"), observed_pr(3, "Ga")];
        let index = pull_requests_by_head(&prs);
        assert_eq!(index["Ga"].number, 1);
        assert_eq!(index["Gb"].number, 2);
        assert!(!index.contains_key("Gc"));
    }

    #[test]
    fn push_diagnostics_only_remove_the_complete_github_invitation() {
        let invitation = "remote: \nremote: Create a pull request for 'G1' on GitHub by visiting:\nremote: https://github.com/owner/repo/pull/new/G1\nremote: ";
        assert_eq!(without_pr_invitation(invitation), "");
        assert_eq!(
            without_pr_invitation(&format!("remote: warning\n{invitation}")),
            "remote: warning"
        );
        for ordinary in
            ["remote: warning", "fatal: push failed", "remote: https://example.com/path"]
        {
            assert_eq!(without_pr_invitation(ordinary), ordinary);
        }
    }

    #[test]
    fn committed_identity_corpus_preserves_admission_and_error_order() {
        let object_id = ObjectId::from_bytes_or_panic(&[0x11; 20]);
        for case in crate::commit_message::trailer_cases() {
            let actual = committed_gherrit_id(case.trailers.as_bytes(), object_id);
            if let Some(expected_id) = case.publication.strip_prefix("ok:") {
                assert_eq!(actual.unwrap().as_str(), expected_id, "{}", case.name);
            } else {
                let expected = case.publication.strip_prefix("error:").unwrap();
                assert_eq!(
                    actual.unwrap_err().to_string(),
                    format!("Commit {object_id} {expected}"),
                    "{}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn native_trailer_records_require_exact_ids_order_and_terminator() {
        let a = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let b = ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        let valid = format!("{a}\0gherrit-pr-id: GA\n\0{b}\0\0");
        assert_eq!(
            decode_commit_trailers(valid.as_bytes(), &[a, b]).unwrap(),
            vec![b"gherrit-pr-id: GA\n".to_vec(), vec![]]
        );
        for malformed in [
            format!("{a}\0"),
            format!("{a}\0value"),
            format!("{b}\0value\0"),
            format!("{a}\0value\0extra\0"),
        ] {
            assert!(decode_commit_trailers(malformed.as_bytes(), &[a]).is_err(), "{malformed:?}");
        }
        assert!(decode_commit_trailers(valid.as_bytes(), &[b, a]).is_err());
    }
}

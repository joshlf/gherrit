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
//! Small modules handle local decisions and formats: [`autosquash`] checks
//! whether history is ready, [`remote`] reads branch tips, [`publication`]
//! builds Git updates, [`github`] encodes API requests, [`batching`] chooses
//! request sizes, [`reconcile`] links PRs and chooses changed fields, and
//! [`body`] renders descriptions. This module controls write order and shared
//! inputs.

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

#[derive(Debug, Eq, PartialEq)]
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

fn collect_commits(repo: &util::Repo) -> Result<Vec<Commit>> {
    // Choose stack members from the local default-branch ref, without fetching
    // the remote tip. The user must update or rebase local history when that
    // ref no longer names the intended publication base.
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
    // Let Git find trailers in each complete commit message. Query several
    // commits per child process, keeping the requested order so trailers cannot
    // be assigned to the wrong commit.
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
    // Each record is OID NUL trailers NUL. Splitting the whole response leaves
    // one final empty field. Newlines are trailer content; NUL separates
    // records. Check record counts and OIDs before checking the trailer IDs.
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
    // Two commits with the same change ID would target the same branch and PR.
    // Reject the whole stack before making any remote changes.
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

    // Read branch tips for the leases. ls-remote fetches neither objects nor
    // version tags, and its results may become stale. Git must check each
    // branch's expected value when applying the update.
    let remote_branch_states = observe_managed_branches(repo, &gherrit_ids)?;

    // Read local versions once for this attempt. Detect read failures or
    // exhausted version numbers before pushing any batch.
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

    // Each chunk is a separate Git transaction. Save local version refs after
    // it succeeds. A later chunk may fail while earlier chunks stay published.
    // Version numbers come from local tags; reading remote branch tips above
    // does not repair missing or stale local history.
    for chunk in push_batches(&commits) {
        let mut targets = Vec::with_capacity(chunk.len());

        for entry in chunk {
            let c = &entry.commit;

            // Keep a missing tip distinct from a parsed object ID until
            // building the command, where Git uses an empty lease value for a
            // new branch.
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

        // Keep Git's diagnostics except the invitation to create a PR, since
        // this command creates the PR itself. Buffer consecutive remote: lines
        // so the filter can recognize an invitation that spans several lines.
        let status = finish_push(&mut child, std::io::stderr().lock())?;
        if !status.success() {
            // A lease failure is a likely cause: another writer may have
            // changed the branch or version tag.
            let r = repo.default_remote_name()?;
            bail!(
                "`git push` failed. The remote might be ahead or changed. Run `git fetch {r}` to sync."
            );
        }

        // The remote batch has already succeeded. Try every local ref and
        // report failed local writes separately from a push that never took
        // effect.
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

/// Waits for Git after consuming or dropping its diagnostics pipe.
///
/// The function waits even if forwarding diagnostics fails. A nonzero exit
/// status is returned to the caller rather than treated as an error here.
///
/// # Errors
///
/// Returns an error if forwarding diagnostics or waiting for Git fails. Once
/// Git starts, an I/O error does not mean the push made no remote changes.
fn finish_push(
    child: &mut std::process::Child,
    output: impl std::io::Write,
) -> Result<std::process::ExitStatus> {
    let diagnostics = match child.stderr.take() {
        Some(stderr) => relay_push_diagnostics(std::io::BufReader::new(stderr), output),
        None => Err(std::io::Error::other("Git push stderr pipe is unavailable")),
    };
    complete_push(child, diagnostics)
}

// Only process completion varies here. Pipe ownership and forwarding stay in
// finish_push, so cleanup cannot accidentally keep a diagnostics reader alive.
trait PushProcess {
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus>;
    fn kill(&mut self) -> std::io::Result<()>;
}

impl PushProcess for std::process::Child {
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.wait()
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.kill()
    }
}

fn complete_push(
    child: &mut impl PushProcess,
    diagnostics: std::io::Result<()>,
) -> Result<std::process::ExitStatus> {
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            // Try to clean up after waiting fails. The remote result is still
            // unknown even if killing Git and waiting again both succeed.
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
            // Match BufRead::lines for text, including CRLF and a final line
            // with no newline. Forward non-UTF-8 diagnostics as bytes.
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
        // Do not leave Git blocked on stderr when the output stream fails. If
        // reading fails too, dropping our pipe closes it.
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    }
    result
}

fn local_version_entry(name: &str) -> Option<(&str, usize)> {
    let tail = name.strip_prefix("refs/tags/gherrit/")?;
    let (id, version) = tail.split_once("/v")?;
    // Keep parsing the number after the last 'v', including unusual existing
    // spellings. Changing which archive names are accepted is a separate
    // decision.
    let version = version.rsplit('v').next()?.parse().ok()?;
    Some((id, version))
}

fn get_local_versions(repo: &util::Repo, ids: &[ChangeId]) -> Result<HashMap<ChangeId, usize>> {
    // An ID with no recognized local tag starts at zero. Scan refs once for the
    // whole stack. Stop on decoding or iteration errors so incomplete history
    // cannot silently produce a lower version number.
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
        // Use the first PR found for each head, as a linear search would.
        by_head.entry(pr.head_branch.as_str()).or_insert(pr);
    }
    by_head
}

// The codec returns creations in request order. Fill only empty slots in that
// order and keep existing states intact. Validate the whole count before using
// any result; a rejection cannot undo PRs already created by GitHub.
fn resolve_pr_slots(
    slots: Vec<(&ChangeId, Option<PrState>)>,
    created: Vec<CreatedPullRequest>,
) -> Result<Vec<PrState>> {
    let expected = slots.iter().filter(|(_, state)| state.is_none()).count();
    if created.len() != expected {
        bail!(
            "GitHub creation results did not match the requested PR count; PRs may have been created"
        );
    }
    if expected != 0 {
        log::info!("Created {expected} PRs.");
    }
    let mut created = created.into_iter();
    let mut states = Vec::with_capacity(slots.len());
    for (id, existing) in slots {
        let state = match existing {
            Some(state) => state,
            None => {
                // The count check provides one result for each empty slot.
                let CreatedPullRequest { pull_request, url } =
                    created.next().expect("one creation result per empty slot");
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
        states.push(state);
    }
    Ok(states)
}

/// Syncs the local commit stack with GitHub PRs.
///
/// The function finds existing PRs and creates any missing ones. It then queues
/// changes to titles, descriptions, and base branches to match the local stack.
/// Updates are sent in batches to reduce the number of requests.
///
/// # Errors
///
/// Returns an error if a GitHub request or response decoding fails, or a new PR
/// cannot be matched to its commit. Completed remote writes are not undone.
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

    // Keep one slot per commit. Request a new PR only for an empty slot. For
    // existing PRs, keep using the first match in the index.
    let resolutions: Vec<_> = commits
        .iter()
        .map(|entry| {
            let c = &entry.item.commit;
            let state = if let Some(&pr) = prs_by_head.get(c.gherrit_id.as_str()) {
                log::debug!("Found existing PR #{} for {}", pr.number.green().bold(), c.gherrit_id);
                Some(pr.clone())
            } else {
                log::debug!("No GitHub PR exists for {}; queuing creation...", c.gherrit_id);
                None
            };
            (&c.gherrit_id, state)
        })
        .collect();

    // Create PRs with the plain commit body first. Full descriptions need every
    // PR number, so writing them remains a separate step.
    let num_creations = resolutions.iter().filter(|(_, state)| state.is_none()).count();
    let created = if num_creations != 0 {
        log::info!("Creating {num_creations} PRs...");
        let repo_id = fetch_repo_id(octocrab, &remote).await?;
        let requests =
            commits.iter().zip(&resolutions).filter(|(_, (_, state))| state.is_none()).map(
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
        run_batched_graphql(octocrab, requests).await?
    } else {
        Vec::new()
    };

    let states = resolve_pr_slots(resolutions, created)?;
    let repo_url = remote.repo_url_relative();
    let stack_pr_numbers = states.iter().map(|state| state.number).collect::<Vec<_>>();
    // Use the same bottom-to-top order for navigation and parent/child links.
    // plan_update leaves out unchanged fields, especially the base: GitHub may
    // reject an unchanged base edit for a PR in the merge queue.
    let updates: Vec<PrUpdate> = commits
        .iter()
        .zip(&states)
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

/// A commit paired with its selected version.
///
/// The pair stays together through the push and PR rendering, so the version
/// does not need to be looked up again by ID.
struct VersionedCommit {
    commit: Commit,
    version: Version,
}

struct Commit {
    // id changes when a commit is rewritten. Keep gherrit_id unchanged so the
    // rewritten change updates the same PR.
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
    // The commit hook only keeps or adds an ID. Check it here before
    // publishing, so ambiguous or unsafe IDs never become ref names.
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

/// Fetches the repository's global node ID.
///
/// GraphQL `createPullRequest` needs this ID, such as `R_kgDOL...`, in its
/// `repositoryId` argument. It does not accept an owner/name pair instead.
///
/// # Errors
///
/// Returns an error if the request fails, GitHub reports an error, or the reply
/// does not contain the expected repository ID.
async fn fetch_repo_id(octocrab: &Octocrab, remote: &util::Remote) -> Result<String> {
    let query = RepositoryIdQuery::new(remote.owner.clone(), remote.repo_name.clone());
    let request = query.request();
    let response: serde_json::Value =
        octocrab.graphql(&request).await.wrap_err("Failed to fetch repository ID")?;
    query.decode(response)
}

/// Updates PRs in batches through GitHub's GraphQL API.
///
/// Updates are grouped to reduce requests. Batches shrink on the resource-limit
/// responses recognized here; this does not retry rate-limit errors in general.
///
/// # Errors
///
/// Returns an error if a request or response decoding fails, or a single
/// operation still exceeds the limits. Earlier batches may have taken effect.
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
    let queries = head_refs.iter().map(|head_ref| {
        FindPullRequest::new(owner.clone(), repo_name.clone(), head_ref.to_string())
    });

    Ok(run_batched_graphql(octocrab, queries).await?.into_iter().flatten().collect())
}

/// Runs GraphQL queries or mutations in batches.
///
/// A combined request is built for each batch, with its size adjusted as
/// needed. Each operation in a successful reply is decoded in request order.
///
/// # Errors
///
/// Returns an error on transport failures, unexpected API errors or response
/// shapes, decoding failures, or when a single operation still exceeds the
/// limits. Earlier mutation batches may already have taken effect.
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
    // with a fixed batch cap, then shrink the pending range on resource
    // responses recognized by classify_response. Change the batch size, not the
    // timing. HTTP transport failures do not use this retry path.
    //
    // [1] https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api#node-limit
    // [2] https://github.blog/changelog/2025-09-01-graphql-api-resource-limits/
    let mut batches = BatchPlan::new(operations.len(), INITIAL_GRAPHQL_BATCH_LEN);
    while let Some(attempt) = batches.current() {
        let chunk = &operations[attempt.range()];
        let query = batch_document(chunk);

        // Try the query and return:
        //
        // - Ok(Some(response)) on success.
        // - Ok(None) when the size check or an API resource limit calls for
        //   backoff.
        // - Err(e) for an error that stops the operation.
        let response = async {
            // Check query size before sending. A firewall, load balancer, or
            // other intermediary may be dropping or truncating requests above
            // about 600KB, causing confusing "missing query attribute" errors.
            // Use the conservative 256KiB limit below to shrink the batch
            // before sending it.
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
                Err(item) => {
                    bail!(
                        "GraphQL operation at item {} exceeds GitHub resource limits. Cannot sync.",
                        item.index
                    );
                }
            }
            continue;
        };

        // Advance only after decoding every alias. A malformed reply may follow
        // a mutation that already took effect. Returning an error here does not
        // mean GitHub left the remote state unchanged.
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
            .env_clear()
            .args(["-c", "i=0; while [ $i -lt 10000 ]; do echo diagnostic >&2; i=$((i+1)); done"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let error = finish_push(&mut child, FailedOutput).unwrap_err();
        assert!(format!("{error:#}").contains("Remote changes may have applied"));
        assert!(child.try_wait().unwrap().unwrap().success());

        let mut child = std::process::Command::new("sh")
            .env_clear()
            .args(["-c", "exit 3"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(finish_push(&mut child, Vec::new()).unwrap().code(), Some(3));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn push_completion_preserves_status_and_cleans_up_every_wait_failure() {
        use std::os::unix::process::ExitStatusExt as _;

        struct Process {
            events: Vec<&'static str>,
            first_wait_fails: bool,
            kill_fails: bool,
            reap_fails: bool,
            code: i32,
        }
        impl PushProcess for Process {
            fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
                let first = self.events.is_empty();
                self.events.push("wait");
                if (first && self.first_wait_fails) || (!first && self.reap_fails) {
                    Err(std::io::Error::other(if first { "initial wait" } else { "cleanup wait" }))
                } else {
                    Ok(std::process::ExitStatus::from_raw(self.code << 8))
                }
            }
            fn kill(&mut self) -> std::io::Result<()> {
                self.events.push("kill");
                if self.kill_fails { Err(std::io::Error::other("cleanup kill")) } else { Ok(()) }
            }
        }
        for code in [0, 3] {
            for diagnostics_fail in [false, true] {
                for first_wait_fails in [false, true] {
                    for kill_fails in [false, true] {
                        for reap_fails in [false, true] {
                            let mut process = Process {
                                events: Vec::new(),
                                first_wait_fails,
                                kill_fails,
                                reap_fails,
                                code,
                            };
                            let diagnostics = if diagnostics_fail {
                                Err(std::io::Error::other("relay error"))
                            } else {
                                Ok(())
                            };
                            let result = complete_push(&mut process, diagnostics);
                            if first_wait_fails {
                                assert_eq!(process.events, ["wait", "kill", "wait"]);
                                let error = format!("{:#}", result.unwrap_err());
                                assert!(
                                    error.contains("Failed to wait for git push: initial wait")
                                );
                                assert_eq!(error.contains("cleanup kill"), kill_fails);
                                assert_eq!(error.contains("cleanup wait"), reap_fails);
                                assert!(error.contains("Remote changes may have applied"));
                            } else {
                                assert_eq!(process.events, ["wait"]);
                                match result {
                                    Ok(status) => {
                                        assert!(!diagnostics_fail);
                                        assert_eq!(status.code(), Some(code));
                                    }
                                    Err(error) => {
                                        assert!(diagnostics_fail);
                                        let text = format!("{error:#}");
                                        assert!(text.contains("relay error"));
                                        assert!(text.contains("child status"));
                                        assert!(text.contains("Remote changes may have applied"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn missing_push_pipe_still_reaps_the_child() {
        let mut child = std::process::Command::new("sh")
            .env_clear()
            .args(["-c", "exit 3"])
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let error = format!("{:#}", finish_push(&mut child, Vec::new()).unwrap_err());
        assert!(error.contains("Git push stderr pipe is unavailable"));
        assert!(error.contains("Remote changes may have applied"));
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(3));
    }

    #[test]
    fn push_relay_covers_buffer_flush_and_newline_write_failures() {
        struct Output {
            writes_left: usize,
        }
        impl std::io::Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.writes_left == 0 {
                    return Err(std::io::Error::other("output failure"));
                }
                self.writes_left -= 1;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for input in [
            b"local\nremaining\n".as_slice(),
            b"remote: warning\nlocal\nremaining\n",
            b"remote: warning\n",
        ] {
            for writes_left in [0, 1] {
                let mut reader = std::io::Cursor::new(input);
                let result = relay_push_diagnostics(&mut reader, Output { writes_left });
                assert_eq!(result.unwrap_err().to_string(), "output failure");
                assert_eq!(reader.position(), input.len() as u64);
            }
        }
        let mut output = Vec::new();
        relay_push_diagnostics(std::io::Cursor::new(b"\nremote: \nremote: Create a pull request for 'Ga' on GitHub by visiting:\nremote: https://github.com/o/r/pull/new/Ga\nremote: \nlast\r"), &mut output).unwrap();
        assert_eq!(output, b"\nlast\r\n");
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
    fn resolves_every_mixture_of_existing_and_created_prs_in_slot_order() {
        let ids = ["Gzeta", "Galpha", "Gmiddle"].map(|id| ChangeId::parse(id.as_bytes()).unwrap());
        let expected = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut state = observed_pr(10 + index as u64, id.as_str());
                state.title = Some(format!("Title {index}"));
                state.body = Some(format!("Body {index}"));
                state.base_branch = format!("base{index}");
                state
            })
            .collect::<Vec<_>>();
        for mask in 0..8 {
            let slots = ids
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    (id, (mask & (1 << index) != 0).then(|| expected[index].clone()))
                })
                .collect();
            let created = expected
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) == 0)
                .map(|(_, state)| CreatedPullRequest {
                    pull_request: state.clone(),
                    url: format!("https://github.test/pull/{}", state.number),
                })
                .collect();
            assert_eq!(resolve_pr_slots(slots, created).unwrap(), expected, "mask {mask}");
        }
        assert!(resolve_pr_slots(Vec::new(), Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn rejects_creation_count_and_head_mismatches_before_rendering() {
        let ids = ["Gone", "Gtwo"].map(|id| ChangeId::parse(id.as_bytes()).unwrap());
        let creation = |head: &str| CreatedPullRequest {
            pull_request: observed_pr(10, head),
            url: "https://github.test/pull/10".to_string(),
        };
        for count in [0, 1, 3] {
            let error = resolve_pr_slots(
                ids.iter().map(|id| (id, None)).collect(),
                (0..count).map(|_| creation("Gone")).collect(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("requested PR count; PRs may have been created"));
        }
        for wrong_index in 0..2 {
            let created = ids
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    creation(if index == wrong_index { "Gother" } else { id.as_str() })
                })
                .collect();
            let error =
                resolve_pr_slots(ids.iter().map(|id| (id, None)).collect(), created).unwrap_err();
            assert!(error.to_string().contains(&format!(
                "requested head '{}'; PRs may have been created",
                ids[wrong_index]
            )));
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

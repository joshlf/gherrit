use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::{self, Write},
    process::Stdio,
    str,
};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::{ObjectId, reference::Category, refs::transaction::PreviousValue};
use octocrab::Octocrab;
use owo_colors::OwoColorize;
use serde_json::json;

use crate::{
    gherrit_id, gherrit_metadata, re,
    util::{self, CommandExt as _, HeadState, Remote},
};

mod batching;
mod publication;
mod reconcile;
mod safety;

use batching::{
    BatchPlan, INITIAL_GRAPHQL_BATCH_LEN, MAX_GRAPHQL_QUERY_BYTES, ResponseDisposition,
    classify_response, query_exceeds_limit,
};
use publication::{
    PersistedTag, PushPlan, PushTarget, plan_push, push_batches, remote_query_batches,
};
use reconcile::{CurrentPr, DesiredPr, PrUpdate, link_stack, plan_update};
use safety::{PrSafetyInput, StagingBase, plan_staging_bases};

pub async fn run(repo: &util::Repo) -> Result<()> {
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

    validate_repository_dag_authority(repo)?;
    let remote = repo.default_remote()?;
    let remote_default = observe_remote_default(repo, &remote)?;
    fetch_remote_default(repo, &remote, &remote_default)?;
    let commits = collect_commits(repo, &remote_default).wrap_err("Failed to collect commits")?;

    if commits.is_empty() {
        log::info!("No commits to sync.");
        return Ok(());
    }

    let token = util::get_github_token()?;
    let mut builder = Octocrab::builder().personal_token(token);

    // NOTE: It would be very dangerous to support this in production, as an
    // attacker could use it to steal a user's GitHub API token. Thus, we only
    // support it in testing.
    if util::__TESTING
        && let Ok(api_url) = std::env::var("GHERRIT_GITHUB_API_URL")
    {
        log::warn!("Using custom GitHub API URL: {}", api_url);
        builder = builder.base_uri(api_url)?;
    }

    let octocrab = builder.build()?;
    let repository =
        fetch_repository_identity(&octocrab, &remote.owner, &remote.repo_name, "fetch URL").await?;
    let push_repository = fetch_repository_identity(
        &octocrab,
        &remote.push_owner,
        &remote.push_repo_name,
        "push URL",
    )
    .await?;
    if push_repository.node_id != repository.node_id {
        bail!(
            "Remote `{}` fetch repository `{}` (node {}) differs from push repository `{}` (node {}). GHerrit refuses to observe one repository and publish to another.",
            remote.name,
            repository.name_with_owner,
            repository.node_id,
            push_repository.name_with_owner,
            push_repository.node_id,
        );
    }

    let publication = plan_publication(repo, &remote, &commits)?;
    validate_ids_against_remote_default(&commits, &remote_default)?;
    validate_existing_branch_ownership(repo, &remote, &publication, &remote_default)?;
    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let candidates = batch_fetch_prs(&remote, &octocrab, &gherrit_ids).await?;
    let prs = select_canonical_prs(&repository, &gherrit_ids, candidates)?;
    validate_prs_for_publication(&prs, &publication)?;
    preflight_pr_projection(
        repo,
        &remote,
        branch_name,
        &remote_default.name,
        &commits,
        &publication.latest_versions,
        &prs,
    )?;

    let rewritten_branches = rewritten_existing_branches(&publication);
    let base_consumers =
        batch_fetch_base_consumers(&remote, &octocrab, &rewritten_branches).await?;
    validate_base_consumers(&repository, &prs, &rewritten_branches, &base_consumers)?;

    let staging_bases =
        plan_pr_staging(repo, &remote, &commits, &prs, &publication, &remote_default)?;
    validate_operational_state(&prs)?;
    for staging in &staging_bases {
        log::debug!(
            "PR #{} ({}) stages {} -> {} before final base {} ({:?}, node {})",
            staging.number,
            staging.head_branch,
            staging.current_base,
            staging.staging_base,
            staging.desired_base,
            staging.reason,
            staging.node_id,
        );
    }

    apply_staging_bases(
        &remote,
        &repository,
        &octocrab,
        &gherrit_ids,
        &remote_default.name,
        &staging_bases,
    )
    .await?;
    let prepared_candidates = batch_fetch_prs(&remote, &octocrab, &gherrit_ids).await?;
    let prepared_prs = select_canonical_prs(&repository, &gherrit_ids, prepared_candidates)?;
    validate_prs_for_publication(&prepared_prs, &publication)?;
    validate_operational_state(&prepared_prs)?;
    verify_staging_bases(
        repo,
        &remote,
        &commits,
        &prepared_prs,
        &publication,
        &remote_default,
        &staging_bases,
    )?;
    verify_publication_inputs(repo, &remote, &publication, &remote_default)?;

    execute_publication(repo, &remote, &publication)?;
    let latest_versions = publication.latest_versions.clone();
    let default_branch = remote_default.name;

    let projected_candidates = batch_fetch_prs(&remote, &octocrab, &gherrit_ids).await?;
    let projected_prs = select_canonical_prs(&repository, &gherrit_ids, projected_candidates)?;
    validate_prs_after_publication(&projected_prs, &publication)?;

    let num_commits = commits.len();
    sync_prs(
        SyncPrContext {
            repo,
            octocrab: &octocrab,
            remote: &remote,
            repository: &repository,
            branch_name,
            base_branch: &default_branch,
        },
        commits,
        latest_versions,
        projected_prs,
    )
    .await?;
    verify_final_projection(
        &remote,
        &repository,
        &octocrab,
        &gherrit_ids,
        &default_branch,
        &publication,
    )
    .await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

fn validate_repository_dag_authority(repo: &util::Repo) -> Result<()> {
    let mut shallow = util::cmd("git", ["rev-parse", "--is-shallow-repository"]);
    shallow.current_dir(repo.workdir().unwrap_or(repo.path()));
    let output = shallow.checked_output()?;
    if core::str::from_utf8(&output.stdout)?.trim() == "true" {
        bail!(
            "GHerrit cannot prove PR reachability from a shallow repository. Fetch complete history (for example, `git fetch --unshallow`) before pushing."
        );
    }

    if std::env::var_os("GIT_REPLACE_REF_BASE").is_some() {
        bail!(
            "GHerrit cannot prove remote reachability while GIT_REPLACE_REF_BASE is set; unset it before pushing"
        );
    }

    let mut replacements =
        util::cmd("git", ["for-each-ref", "--format=%(refname)", "refs/replace"]);
    replacements.current_dir(repo.workdir().unwrap_or(repo.path()));
    let output = replacements.checked_output()?;
    if !output.stdout.is_empty() {
        bail!(
            "GHerrit cannot prove remote reachability while local replace refs exist; delete refs/replace/* before pushing"
        );
    }

    let grafts = repo.common_dir().join("info/grafts");
    if std::fs::read(&grafts)
        .is_ok_and(|contents| contents.iter().any(|byte| !byte.is_ascii_whitespace()))
    {
        bail!(
            "GHerrit cannot prove remote reachability while .git/info/grafts is nonempty; remove the graft configuration before pushing"
        );
    }

    Ok(())
}

fn fetch_remote_default(
    repo: &util::Repo,
    remote: &Remote,
    remote_default: &RemoteDefault,
) -> Result<()> {
    fetch_remote_branch_objects(repo, remote, std::slice::from_ref(&remote_default.name))
        .wrap_err("Failed to fetch the publication remote's default branch")?;
    repo.rev_parse_single(remote_default.oid.as_str()).wrap_err_with(|| {
        format!(
            "Fetched remote default branch `{}`, but object {} is unavailable",
            remote_default.name, remote_default.oid
        )
    })?;

    let observed =
        get_remote_branch_states(repo, remote, std::slice::from_ref(&remote_default.name))?;
    if observed.get(&remote_default.name).and_then(Option::as_deref)
        != Some(remote_default.oid.as_str())
    {
        bail!("The remote default branch changed while its history was being fetched");
    }
    Ok(())
}

fn collect_commits(repo: &util::Repo, remote_default: &RemoteDefault) -> Result<Vec<Commit>> {
    let head = repo.rev_parse_single("HEAD")?;
    let default_ref = repo.rev_parse_single(remote_default.oid.as_str())?;

    let commits = repo.commits_between(default_ref, head).map_err(|err| match err {
        util::CommitsBetweenError::NotAncestor => {
            let branch_name = repo.current_branch().name().unwrap_or("current branch");
            eyre!(
                "The publication remote's default branch `{}` at {} is not an ancestor of `{branch_name}`.\n\
                 GHerrit refuses to substitute a local or stale default-branch ref.\n\
                 Fetch and rebase the stack onto `{}/{}` before pushing.",
                remote_default.name,
                remote_default.oid,
                repo.default_remote_name(),
                remote_default.name,
            )
        }
        util::CommitsBetweenError::Eyre(e) => e,
    })?;
    validate_linear_history(default_ref.detach(), &commits)?;

    let remote = repo.default_remote_name();
    let commits = commits
        .into_iter()
        .map(|c| -> Result<Commit> {
            let msg = c.message()?;
            let title = core::str::from_utf8(msg.title)?;

            if ["fixup!", "squash!", "amend!"].iter().any(|p| title.starts_with(p)) {
                bail!(
                    "Stack contains pending fixup/squash/amend commits.\n\
                    Please squash your history before syncing:\n\
                        git rebase -i --autosquash {remote}/{}",
                    remote_default.name,
                );
            }

            c.try_into()
        })
        .collect::<Result<Vec<_>>>()?;

    let mut seen = HashSet::new();
    for commit in &commits {
        if !seen.insert(commit.gherrit_id.as_str()) {
            bail!(
                "Stack contains duplicate gherrit-pr-id `{}`; every managed commit must have a unique ID",
                commit.gherrit_id
            );
        }
    }
    Ok(commits)
}

fn validate_ids_against_remote_default(
    commits: &[Commit],
    remote_default: &RemoteDefault,
) -> Result<()> {
    if let Some(commit) = commits.iter().find(|commit| commit.gherrit_id == remote_default.name) {
        bail!(
            "Commit {} uses gherrit-pr-id `{}`, which collides with the publication remote's default branch",
            commit.id,
            commit.gherrit_id
        );
    }
    Ok(())
}

/// Proves that every existing branch which GHerrit is about to rewrite is an
/// already-managed GHerrit branch rather than an unrelated user branch that
/// happens to have an ID-shaped name.
fn validate_existing_branch_ownership(
    repo: &util::Repo,
    remote: &Remote,
    publication: &PublicationPlan,
    remote_default: &RemoteDefault,
) -> Result<()> {
    let existing = publication
        .expected_heads
        .iter()
        .filter_map(|(branch, oid)| oid.as_ref().map(|oid| (branch, oid)))
        .collect::<Vec<_>>();
    if existing.is_empty() {
        return Ok(());
    }

    let branches = existing.iter().map(|(branch, _)| (*branch).clone()).collect::<Vec<_>>();
    fetch_remote_branch_objects(repo, remote, &branches)
        .wrap_err("Failed to fetch existing managed branches for ownership validation")?;

    let default_oid = repo.rev_parse_single(remote_default.oid.as_str())?;
    for (branch, oid) in existing {
        if branch == &remote_default.name {
            bail!(
                "Refusing to treat the publication remote's default branch `{branch}` as a GHerrit-managed branch"
            );
        }

        let tip = repo.rev_parse_single(oid.as_str()).wrap_err_with(|| {
            format!("Existing branch `{branch}` points to unavailable object {oid}")
        })?;
        let merge_base = repo.merge_base(default_oid, tip).map_err(|error| {
            eyre!(
                "Existing branch `{branch}` at {oid} has no provable common ancestor with the observed remote default {} at {}: {error}",
                remote_default.name,
                remote_default.oid
            )
        })?;
        if merge_base.detach() == tip.detach() {
            bail!(
                "Existing branch `{branch}` at {oid} is already reachable from the observed remote default {} at {}; it cannot represent an active GHerrit change",
                remote_default.name,
                remote_default.oid
            );
        }
        let commits = repo.commits_between(merge_base, tip).map_err(|error| match error {
            util::CommitsBetweenError::NotAncestor => eyre!(
                "Existing branch `{branch}` at {oid} is not descended from its merge base with the observed remote default; GHerrit ownership cannot be proven"
            ),
            util::CommitsBetweenError::Eyre(error) => error,
        })?;
        if commits.is_empty() {
            bail!("Existing branch `{branch}` has no managed suffix beyond its merge base");
        }
        validate_linear_history(merge_base.detach(), &commits)?;

        let mut seen = HashSet::new();
        for (index, commit) in commits.iter().enumerate() {
            let id = gherrit_id::from_message(commit.message_raw()?.as_ref())?.ok_or_else(|| {
                eyre!(
                    "Existing branch `{branch}` is not provably GHerrit-owned: commit {} lacks a canonical gherrit-pr-id trailer",
                    commit.id
                )
            })?;
            if !seen.insert(id.clone()) {
                bail!(
                    "Existing branch `{branch}` contains repeated gherrit-pr-id `{id}` in its managed ancestry"
                );
            }
            if index + 1 == commits.len() && id != *branch {
                bail!(
                    "Existing branch `{branch}` is not provably GHerrit-owned: its tip commit {} carries gherrit-pr-id `{id}`",
                    commit.id
                );
            }
        }
    }
    Ok(())
}

fn validate_linear_history(base: ObjectId, commits: &[gix::Commit<'_>]) -> Result<()> {
    let mut expected_parent = base;
    for commit in commits {
        let parents = commit.parent_ids().map(gix::Id::detach).collect::<Vec<_>>();
        if parents.len() != 1 {
            bail!(
                "Commit {} has {} parents; GHerrit only supports a linear first-parent stack",
                commit.id,
                parents.len()
            );
        }
        if parents[0] != expected_parent {
            bail!(
                "Commit {} does not directly follow {}; GHerrit only supports a linear first-parent stack",
                commit.id,
                expected_parent
            );
        }
        expected_parent = commit.id;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone)]
struct PrState {
    number: u64,
    node_id: String,
    title: Option<String>,
    body: Option<String>,
    base_branch: String,
    base_oid: String,
    head_branch: String,
    head_oid: String,
    head_repository: Option<RepositoryIdentity>,
    base_repository: Option<RepositoryIdentity>,
    is_cross_repository: bool,
    state: PullRequestState,
    is_in_merge_queue: bool,
    auto_merge_enabled: bool,
    native_stack: bool,
}

#[derive(Debug, Clone)]
struct PrBaseConsumer {
    number: u64,
    node_id: String,
    head_branch: String,
    head_repository: Option<RepositoryIdentity>,
    base_repository: Option<RepositoryIdentity>,
    is_cross_repository: bool,
    base_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepositoryIdentity {
    node_id: String,
    name_with_owner: String,
}

struct PublicationPlan {
    batches: Vec<PushPlan>,
    latest_versions: HashMap<String, usize>,
    expected_heads: HashMap<String, Option<String>>,
    desired_heads: HashMap<String, ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteVersion {
    version: usize,
    target_oid: String,
}

fn plan_publication(
    repo: &util::Repo,
    remote: &Remote,
    commits: &[Commit],
) -> Result<PublicationPlan> {
    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let expected_heads = get_remote_branch_states(repo, remote, &gherrit_ids)
        .wrap_err("Failed to observe remote GHerrit branches")?;
    let remote_versions = get_remote_versions(repo, remote, &gherrit_ids)
        .wrap_err("Failed to observe remote GHerrit patch versions")?;

    let mut latest_versions = HashMap::new();
    let mut desired_heads = HashMap::new();
    let mut batches = Vec::new();

    for chunk in push_batches(commits) {
        let mut targets = Vec::with_capacity(chunk.len());

        for commit in chunk {
            let desired_oid = commit.id.to_string();
            let expected_remote_sha =
                expected_heads.get(&commit.gherrit_id).and_then(|sha| sha.as_deref()).unwrap_or("");
            let remote_version = remote_versions.get(&commit.gherrit_id);
            desired_heads.insert(commit.gherrit_id.clone(), commit.id);

            match (expected_remote_sha.is_empty(), remote_version) {
                (true, Some(remote_version)) => bail!(
                    "Remote patch tag gherrit/{}/v{} exists, but managed branch {} does not. Refusing to publish into an inconsistent remote history.",
                    commit.gherrit_id,
                    remote_version.version,
                    commit.gherrit_id
                ),
                (false, None) => bail!(
                    "Managed branch {} points to {}, but it has no authoritative GHerrit patch-version tag. Refusing to overwrite an unauthenticated remote history.",
                    commit.gherrit_id,
                    expected_remote_sha
                ),
                (false, Some(remote_version))
                    if remote_version.target_oid != expected_remote_sha =>
                {
                    bail!(
                        "Remote patch tag gherrit/{}/v{} points to {}, but managed branch {} points to {}. Refusing to extend this inconsistent remote history.",
                        commit.gherrit_id,
                        remote_version.version,
                        remote_version.target_oid,
                        commit.gherrit_id,
                        expected_remote_sha
                    );
                }
                _ => {}
            }

            // If remote Git already contains this exact head and its latest
            // authoritative version tag targets that head, this invocation is
            // projection-only. This lets a retry repair GitHub projection
            // without manufacturing another patch version.
            if expected_remote_sha == desired_oid
                && let Some(remote_version) = remote_version
            {
                latest_versions.insert(commit.gherrit_id.clone(), remote_version.version);
                continue;
            }

            let next_version = remote_version
                .map(|version| version.version)
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| {
                    eyre!("Patch version overflow for GHerrit ID `{}`", commit.gherrit_id)
                })?;
            latest_versions.insert(commit.gherrit_id.clone(), next_version);
            targets.push(PushTarget {
                object_id: commit.id,
                gherrit_id: &commit.gherrit_id,
                version: next_version,
                expected_remote_sha,
            });
        }

        if !targets.is_empty() {
            batches.push(plan_push(remote.git_url(), &targets));
        }
    }

    Ok(PublicationPlan { batches, latest_versions, expected_heads, desired_heads })
}

fn rewritten_existing_branches(publication: &PublicationPlan) -> Vec<String> {
    publication
        .expected_heads
        .iter()
        .filter_map(|(branch, expected)| {
            let expected = expected.as_deref()?;
            let desired = publication.desired_heads.get(branch)?.to_string();
            (expected != desired).then(|| branch.clone())
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn execute_publication(
    repo: &util::Repo,
    remote: &Remote,
    publication: &PublicationPlan,
) -> Result<()> {
    for plan in &publication.batches {
        log::info!("Pushing chunk to remote...");
        let mut child = util::cmd("git", &plan.arguments)
            .current_dir(repo.workdir().unwrap_or(repo.path()))
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
                    eprintln!("{cleaned}");
                }
                buf.clear();
            };
            for line in reader.lines() {
                let line = line.unwrap();
                if line.trim_start().starts_with("remote:") {
                    remote_buffer.push(line);
                } else {
                    flush_buffer(&mut remote_buffer);
                    eprintln!("{line}");
                }
            }
            flush_buffer(&mut remote_buffer);
        }

        let status = child.wait().unwrap();
        if !status.success() {
            match classify_push_outcome(repo, remote, plan).wrap_err(
                "`git push` failed and GHerrit could not determine the resulting remote state",
            )? {
                PushOutcome::Applied => {
                    log::warn!(
                        "`git push` reported failure, but every branch and version tag in the atomic transaction is present at the planned object IDs; continuing with PR reconciliation"
                    );
                }
                PushOutcome::NotApplied => {
                    let remote = repo.default_remote_name();
                    bail!(
                        "`git push` failed and the planned remote refs were not applied. Run `git fetch {remote}` to inspect the remote before retrying."
                    );
                }
                PushOutcome::Inconsistent(details) => bail!(
                    "`git push` failed and left a ref state that is neither the complete planned atomic transaction nor the complete pre-push state:\n{}",
                    details.join("\n")
                ),
            }
        }

        persist_local_tags(repo, &plan.persisted_tags);
    }

    Ok(())
}

enum PushOutcome {
    Applied,
    NotApplied,
    Inconsistent(Vec<String>),
}

fn classify_push_outcome(
    repo: &util::Repo,
    remote: &Remote,
    plan: &PushPlan,
) -> Result<PushOutcome> {
    let refs = plan.ref_updates.iter().map(|update| update.ref_name.clone()).collect::<Vec<_>>();
    let observed = get_remote_ref_states(repo, remote, &refs)?;
    let all_after = plan.ref_updates.iter().all(|update| {
        observed.get(&update.ref_name).and_then(Option::as_deref)
            == Some(update.desired_after.as_str())
    });
    if all_after {
        return Ok(PushOutcome::Applied);
    }
    let all_before = plan
        .ref_updates
        .iter()
        .all(|update| observed.get(&update.ref_name).cloned().flatten() == update.expected_before);
    if all_before {
        return Ok(PushOutcome::NotApplied);
    }

    let details = plan
        .ref_updates
        .iter()
        .map(|update| {
            let observed =
                observed.get(&update.ref_name).and_then(Option::as_deref).unwrap_or("<missing>");
            format!(
                "{}: observed {observed}, expected before {}, desired after {}",
                update.ref_name,
                update.expected_before.as_deref().unwrap_or("<missing>"),
                update.desired_after
            )
        })
        .collect();
    Ok(PushOutcome::Inconsistent(details))
}

fn get_remote_ref_states(
    repo: &util::Repo,
    remote: &Remote,
    refs: &[String],
) -> Result<HashMap<String, Option<String>>> {
    let mut states =
        refs.iter().cloned().map(|reference| (reference, None)).collect::<HashMap<_, _>>();
    for chunk in remote_query_batches(refs) {
        let mut args = vec!["ls-remote".to_string(), remote.git_url().to_string()];
        args.extend(chunk.iter().cloned());
        let mut command = util::cmd("git", args);
        command.current_dir(repo.workdir().unwrap_or(repo.path()));
        let output = command.checked_output()?;
        for line in core::str::from_utf8(&output.stdout)?.lines() {
            let Some((oid, reference)) = line.split_once('\t') else {
                continue;
            };
            if let Some(state) = states.get_mut(reference) {
                if state.as_deref().is_some_and(|existing| existing != oid) {
                    bail!("Remote ref `{reference}` was reported with conflicting object IDs");
                }
                *state = Some(oid.to_string());
            }
        }
    }
    Ok(states)
}

fn persist_local_tags(repo: &util::Repo, tags: &[PersistedTag]) {
    for tag in tags {
        let _ = repo.reference(
            format!("refs/tags/gherrit/{}/v{}", tag.gherrit_id, tag.version),
            tag.object_id,
            PreviousValue::Any,
            "gherrit: persist local version state",
        );
    }
}

#[allow(clippy::type_complexity)]
fn get_remote_branch_states(
    repo: &util::Repo,
    remote: &Remote,
    gherrit_ids: &[String],
) -> Result<HashMap<String, Option<String>>> {
    let mut states: HashMap<String, Option<String>> =
        gherrit_ids.iter().cloned().map(|id| (id, None)).collect();
    for chunk in remote_query_batches(gherrit_ids) {
        let mut args = vec!["ls-remote".to_string(), remote.git_url().to_string()];
        args.extend(chunk.iter().map(|id| format!("refs/heads/{id}")));

        let mut command = util::cmd("git", args);
        command.current_dir(repo.workdir().unwrap_or(repo.path()));
        let output = command.checked_output()?;
        let output = core::str::from_utf8(&output.stdout)?;

        for line in output.lines() {
            // Output format: "<SHA>\t<refname>"
            let Some((sha, ref_name)) = line.split_once('\t') else {
                continue;
            };

            let Some(branch) = ref_name.strip_prefix("refs/heads/") else {
                continue;
            };
            if let Some(state) = states.get_mut(branch) {
                *state = Some(sha.to_string());
            }
        }
    }

    Ok(states)
}

struct RemoteDefault {
    name: String,
    oid: String,
}

fn get_remote_versions(
    repo: &util::Repo,
    remote: &Remote,
    gherrit_ids: &[String],
) -> Result<HashMap<String, RemoteVersion>> {
    let mut observations: HashMap<(String, usize), TagObservation> = HashMap::new();
    for chunk in remote_query_batches(gherrit_ids) {
        let mut args =
            vec!["ls-remote".to_string(), "--tags".to_string(), remote.git_url().to_string()];
        args.extend(chunk.iter().map(|id| format!("refs/tags/gherrit/{id}/v*")));
        let mut command = util::cmd("git", args);
        command.current_dir(repo.workdir().unwrap_or(repo.path()));
        let output = command.checked_output()?;
        parse_remote_version_lines(
            core::str::from_utf8(&output.stdout)?,
            gherrit_ids,
            &mut observations,
        )?;
    }

    let requested = gherrit_ids.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut observed_versions: HashMap<&str, BTreeSet<usize>> = HashMap::new();
    for (id, version) in observations.keys() {
        if requested.contains(id.as_str()) {
            observed_versions.entry(id.as_str()).or_default().insert(*version);
        }
    }
    for (id, observed) in observed_versions {
        let latest = *observed.last().expect("observed version set is nonempty");
        if let Some(missing) = (1..=latest).find(|version| !observed.contains(version)) {
            bail!(
                "Remote patch history for GHerrit ID `{id}` is missing authoritative version v{missing} before v{latest}"
            );
        }
    }

    let mut versions: HashMap<String, RemoteVersion> = HashMap::new();
    for ((id, version), observation) in observations {
        if !requested.contains(id.as_str()) {
            continue;
        }
        let target_oid = observation.peeled_oid.unwrap_or(observation.direct_oid);
        match versions.get(&id) {
            Some(existing) if existing.version >= version => {}
            _ => {
                versions.insert(id, RemoteVersion { version, target_oid });
            }
        }
    }
    Ok(versions)
}

#[derive(Debug, Default)]
struct TagObservation {
    direct_oid: String,
    peeled_oid: Option<String>,
}

fn parse_remote_version_lines(
    output: &str,
    gherrit_ids: &[String],
    observations: &mut HashMap<(String, usize), TagObservation>,
) -> Result<()> {
    let requested = gherrit_ids.iter().map(String::as_str).collect::<HashSet<_>>();
    for line in output.lines() {
        let Some((oid, ref_name)) = line.split_once('\t') else {
            continue;
        };
        let (ref_name, peeled) = match ref_name.strip_suffix("^{}") {
            Some(ref_name) => (ref_name, true),
            None => (ref_name, false),
        };
        let Some(rest) = ref_name.strip_prefix("refs/tags/gherrit/") else {
            continue;
        };
        let Some((id, version)) = rest.rsplit_once("/v") else {
            continue;
        };
        if !requested.contains(id) {
            continue;
        }
        if version.is_empty() || !version.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("Remote patch tag `{ref_name}` has a noncanonical version number");
        }
        let parsed_version = version
            .parse::<usize>()
            .map_err(|_| eyre!("Remote patch tag `{ref_name}` has an invalid version number"))?;
        if parsed_version == 0 || version != parsed_version.to_string() {
            bail!("Remote patch tag `{ref_name}` has a noncanonical version number");
        }
        let version = parsed_version;
        let observation = observations.entry((id.to_string(), version)).or_default();
        let slot = if peeled {
            &mut observation.peeled_oid
        } else {
            if observation.direct_oid.is_empty() {
                observation.direct_oid = oid.to_string();
                continue;
            }
            if observation.direct_oid != oid {
                bail!(
                    "Remote patch tag `{ref_name}` was reported with conflicting object IDs {} and {oid}",
                    observation.direct_oid
                );
            }
            continue;
        };
        match slot {
            Some(existing) if existing != oid => bail!(
                "Remote patch tag `{ref_name}` was reported with conflicting peeled object IDs {existing} and {oid}"
            ),
            Some(_) => {}
            None => *slot = Some(oid.to_string()),
        }
    }

    for ((id, version), observation) in observations.iter() {
        if observation.direct_oid.is_empty() {
            bail!(
                "Remote patch tag `gherrit/{id}/v{version}` has a peeled target but no tag ref object"
            );
        }
    }
    Ok(())
}

fn observe_remote_default(repo: &util::Repo, remote: &Remote) -> Result<RemoteDefault> {
    let mut command = util::cmd("git", ["ls-remote", "--symref", remote.git_url(), "HEAD"]);
    command.current_dir(repo.workdir().unwrap_or(repo.path()));
    let output = command
        .checked_output()
        .wrap_err("Failed to observe the publication remote's default branch")?;
    let output = core::str::from_utf8(&output.stdout)?;

    let mut name = None;
    let mut oid = None;
    for line in output.lines() {
        if let Some(reference) =
            line.strip_prefix("ref: ").and_then(|line| line.strip_suffix("\tHEAD"))
        {
            name = reference.strip_prefix("refs/heads/").map(ToString::to_string);
        } else if let Some((sha, "HEAD")) = line.split_once('\t') {
            oid = Some(sha.to_string());
        }
    }

    Ok(RemoteDefault {
        name: name.ok_or_else(|| eyre!("Remote HEAD is not a symbolic branch"))?,
        oid: oid.ok_or_else(|| eyre!("Remote HEAD is missing an object ID"))?,
    })
}

fn plan_pr_staging(
    repo: &util::Repo,
    remote: &Remote,
    commits: &[Commit],
    prs: &[PrState],
    publication: &PublicationPlan,
    remote_default: &RemoteDefault,
) -> Result<Vec<StagingBase>> {
    let mut relevant_branches = BTreeSet::from([remote_default.name.clone()]);
    relevant_branches.extend(
        publication
            .expected_heads
            .iter()
            .filter_map(|(branch, oid)| oid.as_ref().map(|_| branch.clone())),
    );
    relevant_branches.extend(prs.iter().map(|pr| pr.base_branch.clone()));

    let relevant_branches = relevant_branches.into_iter().collect::<Vec<_>>();
    let observed = get_remote_branch_states(repo, remote, &relevant_branches)
        .wrap_err("Failed to re-observe candidate PR base branches")?;
    if observed.get(&remote_default.name).and_then(Option::as_deref)
        != Some(remote_default.oid.as_str())
    {
        bail!("The remote default branch changed while planning the publication");
    }
    for pr in prs {
        let observed_base = observed.get(&pr.base_branch).and_then(Option::as_deref);
        if observed_base != Some(pr.base_oid.as_str()) {
            bail!(
                "PR #{} reports base {} at {}, but remote Git was observed at {}",
                pr.number,
                pr.base_oid,
                pr.base_branch,
                observed_base.unwrap_or("<missing>")
            );
        }
    }

    fetch_remote_branch_objects(repo, remote, &relevant_branches)?;

    let mut trajectories = HashMap::<String, Vec<String>>::new();
    insert_oid(trajectories.entry(remote_default.name.clone()).or_default(), &remote_default.oid);
    for (branch, desired) in &publication.desired_heads {
        let Some(old) = publication.expected_heads.get(branch).and_then(Option::as_deref) else {
            continue;
        };
        let trajectory = trajectories.entry(branch.clone()).or_default();
        insert_oid(trajectory, old);
        insert_oid(trajectory, &desired.to_string());
    }
    for pr in prs {
        insert_oid(trajectories.entry(pr.base_branch.clone()).or_default(), &pr.base_oid);
    }

    let desired_order = commits.iter().map(|commit| commit.gherrit_id.clone()).collect::<Vec<_>>();
    let inputs = prs
        .iter()
        .map(|pr| {
            let desired = publication.desired_heads.get(&pr.head_branch).ok_or_else(|| {
                eyre!("Missing desired head for GHerrit branch `{}`", pr.head_branch)
            })?;
            let mut head_oids = vec![pr.head_oid.clone()];
            insert_oid(&mut head_oids, &desired.to_string());
            Ok(PrSafetyInput {
                number: pr.number,
                node_id: pr.node_id.clone(),
                head_branch: pr.head_branch.clone(),
                current_base: pr.base_branch.clone(),
                head_oids,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    plan_staging_bases(
        &remote_default.name,
        &desired_order,
        &inputs,
        &trajectories,
        |ancestor, descendant| git_is_ancestor(repo, ancestor, descendant),
    )
}

fn insert_oid(oids: &mut Vec<String>, oid: &str) {
    if oids.iter().all(|existing| existing != oid) {
        oids.push(oid.to_string());
    }
}

fn fetch_remote_branch_objects(
    repo: &util::Repo,
    remote: &Remote,
    branches: &[String],
) -> Result<()> {
    for chunk in remote_query_batches(branches) {
        let mut args = vec![
            "fetch".to_string(),
            "--quiet".to_string(),
            "--no-tags".to_string(),
            "--no-write-fetch-head".to_string(),
            remote.git_url().to_string(),
        ];
        args.extend(chunk.iter().map(|branch| format!("refs/heads/{branch}")));
        let mut command = util::cmd("git", args);
        command.current_dir(repo.workdir().unwrap_or(repo.path()));
        command
            .success()
            .wrap_err("Failed to fetch remote objects required for reachability checks")?;
    }
    Ok(())
}

fn git_is_ancestor(repo: &util::Repo, ancestor: &str, descendant: &str) -> Result<bool> {
    let mut command = util::cmd(
        "git",
        ["--no-replace-objects", "merge-base", "--is-ancestor", ancestor, descendant],
    );
    command.current_dir(repo.workdir().unwrap_or(repo.path()));
    let status = command
        .status()
        .wrap_err_with(|| format!("Failed to compare Git objects {ancestor} and {descendant}"))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => bail!(
            "Could not determine whether {ancestor} is an ancestor of {descendant}: exit {code:?}"
        ),
    }
}

fn validate_operational_state(prs: &[PrState]) -> Result<()> {
    let mut errors = Vec::new();
    for pr in prs {
        if pr.is_in_merge_queue {
            errors.push(format!(
                "PR #{} is in the merge queue; remove it before GHerrit publishes this stack",
                pr.number
            ));
        }
        if pr.auto_merge_enabled {
            errors.push(format!(
                "PR #{} has auto-merge enabled; disable it before GHerrit publishes this stack",
                pr.number
            ));
        }
        if pr.native_stack {
            errors.push(format!(
                "PR #{} belongs to a native GitHub stack; unstack it before GHerrit publishes this stack",
                pr.number
            ));
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(())
}

async fn apply_staging_bases(
    remote: &Remote,
    repository: &RepositoryIdentity,
    octocrab: &Octocrab,
    gherrit_ids: &[String],
    default_branch: &str,
    staging_bases: &[StagingBase],
) -> Result<()> {
    let mut updates = staging_bases
        .iter()
        .filter(|staging| staging.current_base != staging.staging_base)
        .map(|staging| PrUpdate {
            node_id: staging.node_id.clone(),
            title: None,
            body: None,
            base_branch: Some(staging.staging_base.clone()),
        })
        .collect::<Vec<_>>();

    // Move PRs to non-default staging bases first and expose temporary roots
    // only for the shortest possible portion of the preparation phase.
    updates.sort_by_key(|update| update.base_branch.as_deref() == Some(default_branch));
    if updates.is_empty() {
        return Ok(());
    }

    log::info!("Preparing {} PR bases for a safe ref publication...", updates.len());
    if let Err(mutation_error) = batch_update_prs(octocrab, updates).await {
        let candidates = batch_fetch_prs(remote, octocrab, gherrit_ids)
            .await
            .wrap_err("Failed to re-observe PRs after a staging mutation error")?;
        let prs = select_canonical_prs(repository, gherrit_ids, candidates)?;
        match classify_staging_mutation_outcome(&prs, staging_bases)? {
            StagingMutationOutcome::Applied => log::warn!(
                "The staging GraphQL mutation reported failure, but every affected PR is on its planned safety base; continuing after re-observation"
            ),
            StagingMutationOutcome::NotApplied => {
                return Err(mutation_error).wrap_err(
                    "The staging GraphQL mutation failed and re-observation confirmed that none of its base changes were applied",
                );
            }
            StagingMutationOutcome::Inconsistent(details) => bail!(
                "The staging GraphQL mutation failed and re-observation found a partial or unexpected result:\n{}\nOriginal mutation error: {mutation_error:#}",
                details.join("\n")
            ),
        }
    }
    log::info!("Prepared PR bases for ref publication.");
    Ok(())
}

enum StagingMutationOutcome {
    Applied,
    NotApplied,
    Inconsistent(Vec<String>),
}

fn classify_staging_mutation_outcome(
    prs: &[PrState],
    staging_bases: &[StagingBase],
) -> Result<StagingMutationOutcome> {
    let affected = staging_bases
        .iter()
        .filter(|staging| staging.current_base != staging.staging_base)
        .collect::<Vec<_>>();
    let mut applied = 0;
    let mut not_applied = 0;
    let mut details = Vec::new();

    for staging in affected {
        let Some(pr) = prs.iter().find(|pr| pr.node_id == staging.node_id) else {
            details.push(format!(
                "PR node {} ({}) was not found after staging",
                staging.node_id, staging.head_branch
            ));
            continue;
        };
        if pr.base_branch == staging.staging_base {
            applied += 1;
        } else if pr.base_branch == staging.current_base {
            not_applied += 1;
        } else {
            details.push(format!(
                "PR #{} ({}) targets `{}`, expected pre-mutation `{}` or staged `{}`",
                pr.number,
                pr.head_branch,
                pr.base_branch,
                staging.current_base,
                staging.staging_base
            ));
        }
    }

    if details.is_empty() && not_applied == 0 {
        Ok(StagingMutationOutcome::Applied)
    } else if details.is_empty() && applied == 0 {
        Ok(StagingMutationOutcome::NotApplied)
    } else {
        if details.is_empty() {
            details.push(format!(
                "{applied} staging base changes were applied and {not_applied} were not applied"
            ));
        }
        Ok(StagingMutationOutcome::Inconsistent(details))
    }
}

fn verify_staging_bases(
    repo: &util::Repo,
    remote: &Remote,
    commits: &[Commit],
    prs: &[PrState],
    publication: &PublicationPlan,
    remote_default: &RemoteDefault,
    expected: &[StagingBase],
) -> Result<()> {
    let actual = plan_pr_staging(repo, remote, commits, prs, publication, remote_default)?;
    for expected in expected {
        let actual = actual
            .iter()
            .find(|actual| actual.head_branch == expected.head_branch)
            .ok_or_else(|| eyre!("Prepared PR #{} disappeared", expected.number))?;
        if actual.current_base != expected.staging_base
            || actual.staging_base != expected.staging_base
        {
            bail!(
                "PR #{} was expected on safe staging base `{}`, but GitHub reports `{}`",
                expected.number,
                expected.staging_base,
                actual.current_base
            );
        }
    }
    Ok(())
}

fn verify_publication_inputs(
    repo: &util::Repo,
    remote: &Remote,
    publication: &PublicationPlan,
    remote_default: &RemoteDefault,
) -> Result<()> {
    let mut branches = publication.expected_heads.keys().cloned().collect::<Vec<_>>();
    branches.push(remote_default.name.clone());
    let observed = get_remote_branch_states(repo, remote, &branches)
        .wrap_err("Failed to verify remote refs after preparing PR bases")?;

    for (branch, expected) in &publication.expected_heads {
        if observed.get(branch) != Some(expected) {
            bail!("Remote branch `{branch}` changed after PR base preparation");
        }
    }
    if observed.get(&remote_default.name).and_then(Option::as_deref)
        != Some(remote_default.oid.as_str())
    {
        bail!("The remote default branch changed after PR base preparation");
    }
    Ok(())
}

fn validate_prs_after_publication(prs: &[PrState], publication: &PublicationPlan) -> Result<()> {
    let expected_heads = publication
        .desired_heads
        .iter()
        .map(|(branch, oid)| (branch.clone(), oid.to_string()))
        .collect::<HashMap<_, _>>();
    validate_prs_against_heads(prs, &expected_heads, "after Git publication")
}

fn validate_prs_against_heads(
    prs: &[PrState],
    expected_heads: &HashMap<String, String>,
    phase: &str,
) -> Result<()> {
    let mut errors = Vec::new();
    for pr in prs {
        if pr.state != PullRequestState::Open {
            errors.push(format!("PR #{} became {:?} {phase}", pr.number, pr.state));
        }
        let expected = expected_heads.get(&pr.head_branch).map(String::as_str);
        if expected != Some(pr.head_oid.as_str()) {
            errors.push(format!(
                "PR #{} reports head {}, expected {} {phase}",
                pr.number,
                pr.head_oid,
                expected.unwrap_or("<missing>")
            ));
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(())
}

async fn verify_final_projection(
    remote: &Remote,
    repository: &RepositoryIdentity,
    octocrab: &Octocrab,
    gherrit_ids: &[String],
    default_branch: &str,
    publication: &PublicationPlan,
) -> Result<()> {
    let candidates = batch_fetch_prs(remote, octocrab, gherrit_ids).await?;
    let prs = select_canonical_prs(repository, gherrit_ids, candidates)?;
    validate_prs_after_publication(&prs, publication)?;
    if prs.len() != gherrit_ids.len() {
        bail!(
            "Expected {} canonical PRs after reconciliation, found {}",
            gherrit_ids.len(),
            prs.len()
        );
    }

    let desired = link_stack(default_branch, gherrit_ids.iter().cloned(), Clone::clone)
        .into_iter()
        .map(|entry| (entry.item, entry.base_branch))
        .collect::<HashMap<_, _>>();
    let mut errors = Vec::new();
    for pr in prs {
        let expected = desired.get(&pr.head_branch).map(String::as_str);
        if expected != Some(pr.base_branch.as_str()) {
            errors.push(format!(
                "PR #{} targets `{}`, expected `{}`",
                pr.number,
                pr.base_branch,
                expected.unwrap_or("<missing>")
            ));
        }
    }
    if !errors.is_empty() {
        bail!("Final PR projection is incomplete:\n{}", errors.join("\n"));
    }
    Ok(())
}

const MAX_PR_BODY_BYTES: usize = 60_000;
const MAX_PR_TITLE_CHARS: usize = 256;
const FULL_HISTORY_MAX_VERSION: usize = 8;
const MAX_HISTORY_ROWS: usize = 32;

struct PrBodyBuilder<'a> {
    c: &'a Commit,
    repo_url: &'a str,
    head_branch_markdown: Option<&'a str>,
    gh_pr_ids_markdown: &'a str,
    latest_version: usize,
    base_branch: &'a str,
    parent_id: Option<&'a str>,
    child_id: Option<&'a str>,
}

fn gherrit_metadata_comment(id: &str, parent: Option<&str>, child: Option<&str>) -> String {
    gherrit_metadata::render(id, parent, child)
}

impl PrBodyBuilder<'_> {
    fn build(self) -> Result<String> {
        #[derive(Clone, Copy)]
        enum HistoryTableFormat {
            Full,
            Bounded,
        }

        fn write_body(
            slf: &PrBodyBuilder<'_>,
            mut w: impl Write,
            format: HistoryTableFormat,
        ) -> fmt::Result {
            let current_gherrit_id = &slf.c.gherrit_id;
            w.write_str("<!-- WARNING: This PR description is automatically generated by GHerrit. Any manual edits will be overwritten on the next push. -->\n\n")?;
            w.write_str(&slf.c.message_body)?;
            w.write_str("\n\n---\n\n")?;
            writeln!(w, "{}{}", slf.head_branch_markdown.unwrap_or(""), slf.gh_pr_ids_markdown)?;
            write_history_table(slf, &mut w, format)?;
            write_download_section(slf, &mut w)?;
            w.write_str("\n\n")?;
            w.write_str(
                "*Stacked PRs enabled by [GHerrit](https://github.com/joshlf/gherrit).*\n\n",
            )?;
            w.write_str("<!-- WARNING: GHerrit relies on the following metadata to work properly. DO NOT EDIT OR REMOVE. -->")?;
            w.write_str(&gherrit_metadata_comment(current_gherrit_id, slf.parent_id, slf.child_id))
        }

        fn write_history_table(
            slf: &PrBodyBuilder<'_>,
            mut w: impl Write,
            format: HistoryTableFormat,
        ) -> fmt::Result {
            if slf.latest_version <= 1 || slf.repo_url.is_empty() {
                return Ok(());
            }

            write!(
                w,
                "\n\n**Latest Update:** v{} — [Compare vs v{}]({}/compare/gherrit/{}/v{}..gherrit/{}/v{})\n\n",
                slf.latest_version,
                slf.latest_version - 1,
                slf.repo_url,
                slf.c.gherrit_id,
                slf.latest_version - 1,
                slf.c.gherrit_id,
                slf.latest_version
            )?;
            w.write_str(
                "<details>\n<summary><strong>📚 Full Patch History</strong></summary>\n\n",
            )?;

            match format {
                HistoryTableFormat::Full => {
                    w.write_str(
                        "*Links show the diff between the row version and the column version.*\n\n",
                    )?;

                    w.write_str("|Version|")?;
                    for version in (1..slf.latest_version).rev() {
                        write!(w, " v{version} |")?;
                    }
                    w.write_str("Base|")?;

                    w.write_str("\n|:---|")?;
                    for _ in 1..slf.latest_version {
                        w.write_str(":---|")?;
                    }
                    w.write_str(":---|\n")?;

                    for row in (1..=slf.latest_version).rev() {
                        write!(w, "|v{row}|")?;
                        for column in (1..slf.latest_version).rev() {
                            if column < row {
                                write!(
                                    w,
                                    "[vs v{column}]({}/compare/gherrit/{}/v{column}..gherrit/{}/v{row})|",
                                    slf.repo_url, slf.c.gherrit_id, slf.c.gherrit_id
                                )?;
                            } else {
                                w.write_str("|")?;
                            }
                        }
                        writeln!(
                            w,
                            "[vs Base]({}/compare/{}..gherrit/{}/v{row})|",
                            slf.repo_url, slf.base_branch, slf.c.gherrit_id
                        )?;
                    }
                }
                HistoryTableFormat::Bounded => {
                    w.write_str(
                        "*Each row compares that version with its predecessor and the current base.*\n\n",
                    )?;
                    w.write_str("|Version|Previous version|Base|\n")?;
                    w.write_str("|:---|:---|:---|\n")?;

                    let oldest = slf.latest_version.saturating_sub(MAX_HISTORY_ROWS - 1).max(1);
                    for version in (oldest..=slf.latest_version).rev() {
                        write!(w, "|v{version}|")?;
                        if version > 1 {
                            write!(
                                w,
                                "[v{}]({}/compare/gherrit/{}/v{}..gherrit/{}/v{})|",
                                version - 1,
                                slf.repo_url,
                                slf.c.gherrit_id,
                                version - 1,
                                slf.c.gherrit_id,
                                version
                            )?;
                        } else {
                            w.write_str("—|")?;
                        }
                        writeln!(
                            w,
                            "[Base]({}/compare/{}..gherrit/{}/v{version})|",
                            slf.repo_url, slf.base_branch, slf.c.gherrit_id
                        )?;
                    }
                    if oldest > 1 {
                        write!(
                            w,
                            "\n*Showing the latest {MAX_HISTORY_ROWS} of {} patch versions; older version tags remain available in Git.*\n",
                            slf.latest_version
                        )?;
                    }
                }
            }

            w.write_str("\n</details>")
        }

        fn write_download_section(slf: &PrBodyBuilder<'_>, mut w: impl Write) -> fmt::Result {
            let id = &slf.c.gherrit_id;

            w.write_str(
                "\n<details>\n<summary><strong>⬇️ Download this PR</strong></summary>\n\n",
            )?;
            w.write_str("######\n\n")?;

            // While `git fetch origin {id}` would work most of the time, we use
            // the full `refs/heads/` syntax to avoid ambiguity with tags of the
            // same name.
            let fetch_cmd = format!("git fetch origin refs/heads/{id}");
            let commands = [
                ("Branch", format!("{fetch_cmd} && git checkout -b pr-{id} FETCH_HEAD")),
                ("Checkout", format!("{fetch_cmd} && git checkout FETCH_HEAD")),
                ("Cherry Pick", format!("{fetch_cmd} && git cherry-pick FETCH_HEAD")),
                ("Pull", format!("git pull origin refs/heads/{id}")),
            ];

            for (title, command) in commands {
                writeln!(w, "**{title}**\n```bash\n{command}\n```\n")?;
            }

            w.write_str("</details>")
        }

        let format = if self.latest_version <= FULL_HISTORY_MAX_VERSION {
            HistoryTableFormat::Full
        } else {
            HistoryTableFormat::Bounded
        };
        let mut body = String::new();
        write_body(&self, &mut body, format).expect("writing to String cannot fail");
        if body.len() > MAX_PR_BODY_BYTES {
            bail!(
                "Generated PR body for GHerrit ID `{}` is {} UTF-8 bytes, exceeding GHerrit's conservative {}-byte limit. Shorten the commit message or reduce stack metadata before publishing.",
                self.c.gherrit_id,
                body.len(),
                MAX_PR_BODY_BYTES
            );
        }
        Ok(body)
    }
}

/// Syncs the local stack of commits with GitHub Pull Requests.
///
/// This function:
/// 1. Finds existing PRs or creates new ones for new commits.
/// 2. Updates PR metadata (title, body, base branch) to match the local stack.
/// 3. Updates are queued and executed in batches to optimize performance.
struct SyncPrContext<'a> {
    repo: &'a util::Repo,
    octocrab: &'a Octocrab,
    remote: &'a Remote,
    repository: &'a RepositoryIdentity,
    branch_name: &'a str,
    base_branch: &'a str,
}

async fn sync_prs(
    context: SyncPrContext<'_>,
    commits: Vec<Commit>,
    latest_versions: HashMap<String, usize>,
    prs: Vec<PrState>,
) -> Result<()> {
    let SyncPrContext { repo, octocrab, remote, repository, branch_name, base_branch } = context;
    let commits = link_stack(base_branch, commits, |commit| commit.gherrit_id.clone());

    enum PrResolution {
        Existing(PrState),
        ToCreate(BatchCreate),
    }

    // 1. Identify existing PRs or queue for creation
    let resolutions: Vec<_> = commits
        .iter()
        .map(|entry| {
            let c = &entry.item;

            if let Some(pr) = prs.iter().find(|pr| pr.head_branch == c.gherrit_id) {
                log::debug!("Found existing PR #{} for {}", pr.number.green().bold(), c.gherrit_id);
                PrResolution::Existing(pr.clone())
            } else {
                log::debug!("No GitHub PR exists for {}; queuing creation...", c.gherrit_id);
                PrResolution::ToCreate(BatchCreate {
                    title: c.message_title.clone(),
                    body: provisional_pr_body(
                        c,
                        entry.parent_id.as_deref(),
                        entry.child_id.as_deref(),
                    ),
                    base_branch: entry.base_branch.clone(),
                    head_branch: c.gherrit_id.clone(),
                    head_oid: c.id.to_string(),
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
        let created = create_prs_recoverably(remote, repository, octocrab, creations).await?;
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
                    let (number, url, node_id) =
                        new_prs.get(&create.head_branch).ok_or_else(|| {
                            eyre::eyre!("Failed to resolve created PR for {}", create.head_branch)
                        })?;
                    log::info!("Created PR #{}: {}", number.green().bold(), url.blue().underline());
                    PrState {
                        number: *number,
                        node_id: node_id.clone(),
                        title: Some(create.title),
                        body: Some(create.body),
                        base_branch: create.base_branch,
                        base_oid: String::new(),
                        head_branch: create.head_branch,
                        head_oid: create.head_oid,
                        head_repository: Some(repository.clone()),
                        base_repository: Some(repository.clone()),
                        is_cross_repository: false,
                        // Newly-created PRs are open and cannot already be in a
                        // queue, auto-merge request, or native stack.
                        state: PullRequestState::Open,
                        is_in_merge_queue: false,
                        auto_merge_enabled: false,
                        native_stack: false,
                    }
                }
            };
            Ok((entry, pr_state))
        })
        .collect::<Result<Vec<_>>>()?;

    let head_branch_markdown = head_branch_markdown(repo, branch_name);
    let repo_url = remote.repo_url_relative();
    let pr_numbers = commit_pr_states.iter().map(|(_, state)| state.number).collect::<Vec<_>>();
    let mut updates = commit_pr_states
        .iter()
        .enumerate()
        .map(|(index, (entry, pr_state))| -> Result<Option<PrUpdate>> {
            let c = &entry.item;
            let navigation = pr_navigation_markdown(&pr_numbers, index);
            let latest_version = latest_versions.get(&c.gherrit_id).copied().unwrap_or(1);

            let body = (PrBodyBuilder {
                c,
                repo_url: &repo_url,
                head_branch_markdown: head_branch_markdown.as_deref(),
                gh_pr_ids_markdown: &navigation,
                latest_version,
                base_branch: &entry.base_branch,
                parent_id: entry.parent_id.as_deref(),
                child_id: entry.child_id.as_deref(),
            })
            .build()?;

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

            Ok(update)
        })
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>>>()?;

    // A stale root is the most dangerous final intermediate state: it is
    // temporarily landing-eligible. Move non-root PRs to their final bases
    // before assigning the final default-branch root.
    updates.sort_by_key(|update| update.base_branch.as_deref() == Some(base_branch));

    if !updates.is_empty() {
        log::info!("Updating batch of {} PRs...", updates.len());
        batch_update_prs(octocrab, updates).await?;
        log::info!("Batch update complete.");
    }

    Ok(())
}

fn head_branch_markdown(repo: &util::Repo, branch_name: &str) -> Option<String> {
    if is_private_stack(repo, branch_name) {
        return None;
    }
    let head_ref = repo.head().ok()?.try_into_referent()?;
    let (category, short_name) = head_ref.inner.name.category_and_short_name()?;
    (category == Category::LocalBranch)
        .then(|| format!("This PR is on branch [{short_name}](../tree/{short_name}).\n\n"))
}

fn pr_navigation_markdown(numbers: &[u64], current_index: usize) -> String {
    numbers
        .iter()
        .enumerate()
        .rev()
        .map(|(index, number)| {
            let prefix = if index == current_index { "👉" } else { "\u{3000}\u{2009}" };
            format!("- {prefix} #{number}")
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    type Error = color_eyre::eyre::Error;

    fn try_from(c: gix::Commit) -> Result<Self> {
        let message = c.message()?;
        let message_title = core::str::from_utf8(message.title)?.to_string();
        let gherrit_id = gherrit_id::from_message(c.message_raw()?.as_ref())?
            .ok_or_else(|| eyre!("Commit {} missing a non-empty gherrit-pr-id trailer", c.id))?;
        let message_body = message
            .body
            .map(|body| remove_terminal_id_trailer(core::str::from_utf8(body)?, &gherrit_id))
            .transpose()?
            .unwrap_or_default();

        Ok(Commit { id: c.id, gherrit_id, message_title, message_body })
    }
}

fn remove_terminal_id_trailer(body: &str, id: &str) -> Result<String> {
    let mut lines = body.split_inclusive('\n').collect::<Vec<_>>();
    let index = lines
        .iter()
        .rposition(|line| {
            let line = line.trim_end_matches(['\r', '\n']);
            let Some((token, value)) = line.split_once(':') else {
                return false;
            };
            token.as_bytes().eq_ignore_ascii_case(gherrit_id::TRAILER_TOKEN) && value.trim() == id
        })
        .ok_or_else(|| {
            eyre!("Parsed gherrit-pr-id trailer `{id}` was not found in the commit body")
        })?;
    lines.remove(index);
    Ok(lines.concat().trim_end().to_string())
}

/// A request to create a new PR in a batch.
#[derive(Clone)]
struct BatchCreate {
    title: String,
    body: String,
    base_branch: String,
    head_branch: String,
    head_oid: String,
}

fn provisional_pr_body(commit: &Commit, parent_id: Option<&str>, child_id: Option<&str>) -> String {
    format!(
        "<!-- WARNING: This PR description is automatically generated by GHerrit. Any manual edits will be overwritten on the next push. -->\n\n{}\n\n---\n\n*GHerrit is completing the initial projection for this PR.*\n\n<!-- WARNING: GHerrit relies on the following metadata to work properly. DO NOT EDIT OR REMOVE. -->{}",
        commit.message_body,
        gherrit_metadata::render(&commit.gherrit_id, parent_id, child_id),
    )
}

/// Formats a string with JSON values, safely avoiding variable capture.
///
/// This macro is used to format strings with JSON values, which are then passed
/// to the GraphQL API. It intentionally doesn't support normal string
/// interpolation, which would present injection vulnerabilities. Instead, all
/// values are formatted as JSON before being interpolated.
macro_rules! safe_json_format {
    // Handle optional fields (?: operator)
    (@inner $parts:ident, $key:literal ? $value:expr) => {
        if let Some(ref v) = $value {
            // Make sure `$key` is a `&str` so that it's formatted correctly
            // using `{:?}`.
            let key: &str = $key;
            $parts.push(format!("{}: {}", key, serde_json::json!(v)));
        }
    };

    // Handle mandatory fields (: operator)
    (@inner $parts:ident, $key:literal : $value:expr) => {{
        // Make sure `$key` is a `&str` so that it's formatted correctly using
        // `{:?}`.
        let key: &str = $key;
        $parts.push(format!("{}: {}", key, serde_json::json!($value)));
    }};

    ($fmt:literal $(, $k:ident = $v:expr)* $(, ($target:ident = { $($key:literal $op:tt $value:expr),* $(,)? }))? $(,)?) => {{
        #[allow(unused_mut)]
        let mut parts: Vec<String> = Vec::new();
        $($(
            safe_json_format!(@inner parts, $key $op $value);
        )*)?

        // Inner function to avoid capturing environment variables.
        fn inner($($k: serde_json::Value,)* _target: String) -> String {
            format!($fmt $(, $target = _target)?)
        }

        inner($(serde_json::json!($v),)* parts.join(", "))
    }};
}

fn update_pull_request_operation(update: &PrUpdate) -> String {
    safe_json_format!(
        "updatePullRequest(input: {{ {fields} }}) {{ clientMutationId }}",
        (fields = {
            "pullRequestId" : update.node_id,
            "baseRefName" ? update.base_branch,
            "title" ? update.title,
            "body" ? update.body,
        })
    )
}

fn preflight_pr_projection(
    repo: &util::Repo,
    remote: &Remote,
    branch_name: &str,
    base_branch: &str,
    commits: &[Commit],
    latest_versions: &HashMap<String, usize>,
    prs: &[PrState],
) -> Result<()> {
    const MAX_NODE_ID_BYTES: usize = 256;

    let stack = link_stack(base_branch, commits.iter(), |commit| commit.gherrit_id.clone());
    let existing = prs.iter().map(|pr| (pr.head_branch.as_str(), pr)).collect::<HashMap<_, _>>();
    // A missing PR's final number is not known until after its head ref exists.
    // u64::MAX is a conservative upper bound on the decimal width GitHub can
    // return, so an accepted preflight body cannot grow after PR creation.
    let numbers = stack
        .iter()
        .map(|entry| existing.get(entry.item.gherrit_id.as_str()).map_or(u64::MAX, |pr| pr.number))
        .collect::<Vec<_>>();
    let head_branch_markdown = head_branch_markdown(repo, branch_name);
    let repo_url = remote.repo_url_relative();

    for (index, entry) in stack.iter().enumerate() {
        let commit = entry.item;
        let title_chars = commit.message_title.chars().count();
        if title_chars == 0 {
            bail!(
                "Prospective PR title for GHerrit ID `{}` is empty; GitHub requires a nonempty pull-request title",
                commit.gherrit_id
            );
        }
        if title_chars > MAX_PR_TITLE_CHARS {
            bail!(
                "Prospective PR title for GHerrit ID `{}` is {title_chars} characters, exceeding GitHub's {MAX_PR_TITLE_CHARS}-character limit. Shorten the commit subject before publishing.",
                commit.gherrit_id
            );
        }
        let navigation = pr_navigation_markdown(&numbers, index);
        let latest_version = latest_versions.get(&commit.gherrit_id).copied().unwrap_or(1);
        let body = (PrBodyBuilder {
            c: commit,
            repo_url: &repo_url,
            head_branch_markdown: head_branch_markdown.as_deref(),
            gh_pr_ids_markdown: &navigation,
            latest_version,
            base_branch: &entry.base_branch,
            parent_id: entry.parent_id.as_deref(),
            child_id: entry.child_id.as_deref(),
        })
        .build()?;

        let node_id = existing
            .get(commit.gherrit_id.as_str())
            .map_or_else(|| "N".repeat(MAX_NODE_ID_BYTES), |pr| pr.node_id.clone());
        let probe = PrUpdate {
            node_id,
            title: Some(commit.message_title.clone()),
            body: Some(body),
            base_branch: Some(entry.base_branch.clone()),
        };
        let operation = update_pull_request_operation(&probe);
        let query = format!("mutation {{ op0: {operation} }}");
        if query_exceeds_limit(&query) {
            bail!(
                "The final GraphQL projection for GHerrit ID `{}` would require a {}-byte single-PR mutation, exceeding GHerrit's {}-byte safety limit. Shorten the commit title/body before publishing.",
                commit.gherrit_id,
                query.len(),
                MAX_GRAPHQL_QUERY_BYTES
            );
        }
    }

    Ok(())
}

/// Recursively looks up nested values from a JSON object, converting lookup
/// failures to `Result::Err` values.
macro_rules! json_get {
    ($val:ident [$key:expr] $(.$as:ident())? $([$rest_key:expr] $(.$rest_as:ident())?)*) => {
        $val
            .get($key)$(.and_then(|v| v.$as()))?.ok_or_else(|| eyre!("Missing JSON field in GraphQL response: `{}`", stringify!($key)))
            $(.and_then(|v| v.get($rest_key)$(.and_then(|v| v.$rest_as()))?.ok_or_else(|| eyre!("Missing JSON field in GraphQL response: `{}`", stringify!($rest_key)))))*
    };
}

fn parse_repository_identity(value: &serde_json::Value) -> Result<Option<RepositoryIdentity>> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(RepositoryIdentity {
        node_id: json_get!(value["id"].as_str())?.to_string(),
        name_with_owner: json_get!(value["nameWithOwner"].as_str())?.to_string(),
    }))
}

/// Fetches the immutable repository identity for the publication repository.
///
/// The node ID, rather than owner/name spelling, is the authority used to
/// decide whether a PR belongs to this repository. `nameWithOwner` is retained
/// only for diagnostics and generated links.
async fn fetch_repository_identity(
    octocrab: &Octocrab,
    owner: &str,
    repo_name: &str,
    source: &str,
) -> Result<RepositoryIdentity> {
    // NOTE: It's important that we pass `remote.*` as GraphQL variables, not
    // using string interpolation, as the variables are escaped. Using string
    // interpolation would risk injection attacks.
    let query = r#"query RepositoryIdentity($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id, nameWithOwner } }"#;
    let query_body = json!({
        "query": query,
        "variables": {
            "owner": owner,
            "name": repo_name,
        }
    });
    let response: serde_json::Value = octocrab
        .graphql(&query_body)
        .await
        .wrap_err_with(|| format!("Failed to fetch repository ID for remote {source}"))?;

    if let Some(errors) = response.get("errors") {
        log::error!("GraphQL errors: {}", errors);
        bail!("Failed to fetch repository ID for remote {source}: {errors:?}");
    }

    let repository = json_get!(response["data"]["repository"])?;
    Ok(RepositoryIdentity {
        node_id: json_get!(repository["id"].as_str())?.to_string(),
        name_with_owner: json_get!(repository["nameWithOwner"].as_str())?.to_string(),
    })
}

/// Performs batched updates of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping updates into
/// adaptive batches and sending each batch as one GraphQL operation.
async fn batch_update_prs(octocrab: &Octocrab, updates: Vec<PrUpdate>) -> Result<()> {
    run_batched_graphql(
        octocrab,
        GraphQlOp::Mutation,
        updates,
        update_pull_request_operation,
        |update, op_data| {
            if op_data.is_null() {
                bail!(
                    "The batched GraphQL mutation failed to update PR with node ID '{}'. The response for this operation was null.",
                    update.node_id
                );
            }
            Ok(())
        },
    )
    .await
}

/// Performs batched creation of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping creations into
/// adaptive batches and sending each batch as one GraphQL operation.
///
/// Returns a map of head branch name -> (number, url, node_id) for the newly
/// created PRs.
async fn batch_create_prs(
    octocrab: &Octocrab,
    repo_id: &str,
    creations: impl IntoIterator<Item = BatchCreate>,
) -> Result<HashMap<String, (u64, String, String)>> {
    let creations_list: Vec<BatchCreate> = creations.into_iter().collect();
    let mut created_prs = HashMap::new();

    run_batched_graphql(
        octocrab,
        GraphQlOp::Mutation,
        creations_list,
        |create| {
            safe_json_format!(
                "createPullRequest(input: {{ {fields} }}) {{ pullRequest {{ number, url, id }} }}",
                (fields = {
                    "repositoryId" : repo_id,
                    "baseRefName" : create.base_branch,
                    "headRefName" : create.head_branch,
                    "title" : create.title,
                    "body" : create.body,
                })
            )
        },
        |create, val| {
            let pr = json_get!(val["pullRequest"])?;
            let node_id = json_get!(pr["id"].as_str())?.to_string();
            let number = json_get!(pr["number"].as_u64())?;
            let url = json_get!(pr["url"].as_str())?.to_string();

            created_prs.insert(create.head_branch.clone(), (number, url, node_id));
            Ok(())
        },
    )
    .await?;

    Ok(created_prs)
}

async fn create_prs_recoverably(
    remote: &Remote,
    repository: &RepositoryIdentity,
    octocrab: &Octocrab,
    creations: Vec<BatchCreate>,
) -> Result<HashMap<String, (u64, String, String)>> {
    let mut pending = creations;
    let mut recovered = HashMap::new();

    loop {
        match batch_create_prs(octocrab, &repository.node_id, pending.clone()).await {
            Ok(created) => {
                recovered.extend(created);
                return Ok(recovered);
            }
            Err(error) => {
                let heads =
                    pending.iter().map(|create| create.head_branch.clone()).collect::<Vec<_>>();
                let candidates = batch_fetch_prs(remote, octocrab, &heads)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "PR creation failed, and GHerrit could not re-observe the affected heads to determine the outcome: {error}"
                        )
                    })?;
                let observed = select_canonical_prs(repository, &heads, candidates)?;
                let mut still_missing = Vec::new();
                let mut observed_count = 0;

                for create in pending {
                    let Some(pr) = observed.iter().find(|pr| pr.head_branch == create.head_branch)
                    else {
                        still_missing.push(create);
                        continue;
                    };
                    observed_count += 1;
                    validate_recovered_creation(pr, &create)?;
                    recovered.insert(
                        create.head_branch.clone(),
                        (pr.number, remote.pr_url(pr.number), pr.node_id.clone()),
                    );
                }

                if still_missing.is_empty() {
                    log::warn!(
                        "GitHub reported a PR creation failure, but every requested PR was observed in the exact planned state; continuing reconciliation"
                    );
                    return Ok(recovered);
                }
                if observed_count == 0 {
                    return Err(error.wrap_err(
                        "GitHub reported a PR creation failure and none of the requested PRs were created",
                    ));
                }

                log::warn!(
                    "GitHub reported a partial PR creation result; {} PRs were observed and {} remain. Retrying only the missing creations.",
                    observed_count,
                    still_missing.len()
                );
                pending = still_missing;
            }
        }
    }
}

fn validate_recovered_creation(pr: &PrState, create: &BatchCreate) -> Result<()> {
    let metadata = pr
        .body
        .as_deref()
        .ok_or_else(|| eyre!("Recovered PR #{} has no body", pr.number))
        .and_then(|body| {
            gherrit_metadata::parse_terminal(body)?.ok_or_else(|| {
                eyre!("Recovered PR #{} has no terminal GHerrit metadata", pr.number)
            })
        })?;
    if pr.state != PullRequestState::Open
        || pr.head_oid != create.head_oid
        || pr.base_branch != create.base_branch
        || pr.title.as_deref() != Some(create.title.as_str())
        || pr.body.as_deref() != Some(create.body.as_str())
        || metadata.id != create.head_branch
    {
        bail!(
            "PR creation for `{}` had an inconsistent outcome: observed PR #{} in state {:?}, head {}@{}, base `{}`, title {:?}, exact provisional body {}, metadata ID `{}`",
            create.head_branch,
            pr.number,
            pr.state,
            pr.head_branch,
            pr.head_oid,
            pr.base_branch,
            pr.title,
            pr.body.as_deref() == Some(create.body.as_str()),
            metadata.id,
        );
    }
    Ok(())
}

async fn batch_fetch_prs(
    remote: &Remote,
    octocrab: &Octocrab,
    head_refs: &[String],
) -> Result<Vec<PrState>> {
    let owner = &remote.owner;
    let repo_name = &remote.repo_name;

    let mut all_prs = Vec::new();

    run_batched_graphql(
        octocrab,
        GraphQlOp::Query,
        head_refs,
        |head_ref| {
            safe_json_format!(
                "repository(owner: {owner}, name: {repo_name}) {{ pullRequests(headRefName: {head_ref}, first: 100, states: [OPEN, CLOSED, MERGED]) {{ totalCount, nodes {{ number, id, title, body, baseRefName, baseRefOid, headRefName, headRefOid, headRepository {{ id, nameWithOwner }}, baseRepository {{ id, nameWithOwner }}, isCrossRepository, state, isInMergeQueue, autoMergeRequest {{ enabledAt }}, stackEntry {{ id }} }} }} }}",
                owner = owner,
                repo_name = repo_name,
                head_ref = head_ref,
            )
        },
        |head_ref, val| {
            let pull_requests = json_get!(val["pullRequests"])?;
            let total_count = json_get!(pull_requests["totalCount"].as_u64())?;
            let nodes = json_get!(pull_requests["nodes"].as_array())?;
            if total_count > nodes.len() as u64 {
                bail!(
                    "More than 100 PRs exist for head branch `{head_ref}`; cannot select a canonical PR safely"
                );
            }

            for node in nodes {
                let number = json_get!(node["number"].as_u64())?;
                let id = json_get!(node["id"].as_str())?;
                let state: PullRequestState =
                    serde_json::from_value(json_get!(node["state"])?.clone())
                        .wrap_err("Failed to parse PR state")?;

                all_prs.push(PrState {
                    number,
                    node_id: id.to_string(),
                    title: node
                        .get("title")
                        .and_then(|title| title.as_str())
                        .map(ToString::to_string),
                    body: node
                        .get("body")
                        .and_then(|body| body.as_str())
                        .map(ToString::to_string),
                    base_branch: json_get!(node["baseRefName"].as_str())?.to_string(),
                    base_oid: json_get!(node["baseRefOid"].as_str())?.to_string(),
                    head_branch: json_get!(node["headRefName"].as_str())?.to_string(),
                    head_oid: json_get!(node["headRefOid"].as_str())?.to_string(),
                    head_repository: parse_repository_identity(
                        node.get("headRepository").unwrap_or(&serde_json::Value::Null),
                    )?,
                    base_repository: parse_repository_identity(
                        node.get("baseRepository").unwrap_or(&serde_json::Value::Null),
                    )?,
                    is_cross_repository: json_get!(node["isCrossRepository"].as_bool())?,
                    state,
                    is_in_merge_queue: json_get!(node["isInMergeQueue"].as_bool())?,
                    auto_merge_enabled: node
                        .get("autoMergeRequest")
                        .is_some_and(|request| !request.is_null()),
                    native_stack: node
                        .get("stackEntry")
                        .is_some_and(|entry| !entry.is_null()),
                });
            }
            Ok(())
        },
    )
    .await?;

    Ok(all_prs)
}

async fn batch_fetch_base_consumers(
    remote: &Remote,
    octocrab: &Octocrab,
    base_refs: &[String],
) -> Result<Vec<PrBaseConsumer>> {
    let owner = &remote.owner;
    let repo_name = &remote.repo_name;
    let mut consumers = Vec::new();

    run_batched_graphql(
        octocrab,
        GraphQlOp::Query,
        base_refs,
        |base_ref| {
            safe_json_format!(
                "repository(owner: {owner}, name: {repo_name}) {{ pullRequests(baseRefName: {base_ref}, first: 100, states: [OPEN]) {{ totalCount, nodes {{ number, id, headRefName, headRepository {{ id, nameWithOwner }}, baseRepository {{ id, nameWithOwner }}, isCrossRepository, baseRefName }} }} }}",
                owner = owner,
                repo_name = repo_name,
                base_ref = base_ref,
            )
        },
        |base_ref, val| {
            let pull_requests = json_get!(val["pullRequests"])?;
            let total_count = json_get!(pull_requests["totalCount"].as_u64())?;
            let nodes = json_get!(pull_requests["nodes"].as_array())?;
            if total_count > nodes.len() as u64 {
                bail!(
                    "More than 100 open PRs target managed branch `{base_ref}`; cannot prove publication safety"
                );
            }

            for node in nodes {
                consumers.push(PrBaseConsumer {
                    number: json_get!(node["number"].as_u64())?,
                    node_id: json_get!(node["id"].as_str())?.to_string(),
                    head_branch: json_get!(node["headRefName"].as_str())?.to_string(),
                    head_repository: parse_repository_identity(
                        node.get("headRepository").unwrap_or(&serde_json::Value::Null),
                    )?,
                    base_repository: parse_repository_identity(
                        node.get("baseRepository").unwrap_or(&serde_json::Value::Null),
                    )?,
                    is_cross_repository: json_get!(node["isCrossRepository"].as_bool())?,
                    base_branch: json_get!(node["baseRefName"].as_str())?.to_string(),
                });
            }
            Ok(())
        },
    )
    .await?;

    Ok(consumers)
}

fn validate_base_consumers(
    repository: &RepositoryIdentity,
    canonical_prs: &[PrState],
    rewritten_branches: &[String],
    consumers: &[PrBaseConsumer],
) -> Result<()> {
    if rewritten_branches.is_empty() {
        return Ok(());
    }

    let canonical_nodes =
        canonical_prs.iter().map(|pr| pr.node_id.as_str()).collect::<HashSet<_>>();
    let rewritten = rewritten_branches.iter().map(String::as_str).collect::<HashSet<_>>();
    let external = consumers
        .iter()
        .filter(|consumer| {
            rewritten.contains(consumer.base_branch.as_str())
                && !canonical_nodes.contains(consumer.node_id.as_str())
        })
        .collect::<Vec<_>>();

    if external.is_empty() {
        return Ok(());
    }

    let details = external
        .iter()
        .map(|consumer| {
            let head_repository = consumer
                .head_repository
                .as_ref()
                .map(|identity| identity.name_with_owner.as_str())
                .unwrap_or("<deleted or unavailable repository>");
            let ownership = match (&consumer.head_repository, &consumer.base_repository) {
                (Some(head), Some(base))
                    if head.node_id == repository.node_id
                        && base.node_id == repository.node_id
                        && !consumer.is_cross_repository =>
                {
                    "same-repository"
                }
                _ => "external or unknown repository",
            };
            format!(
                "PR #{} ({head_repository}/{} -> {}, {ownership})",
                consumer.number, consumer.head_branch, consumer.base_branch
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "Cannot rewrite managed branches while unrelated open PRs target them: {details}. Retarget or close those PRs first."
    )
}

fn select_canonical_prs(
    repository: &RepositoryIdentity,
    head_refs: &[String],
    candidates: Vec<PrState>,
) -> Result<Vec<PrState>> {
    let mut canonical = Vec::new();

    for head_ref in head_refs {
        let owned_head = candidates
            .iter()
            .filter(|pr| {
                pr.head_branch == *head_ref
                    && pr
                        .head_repository
                        .as_ref()
                        .is_some_and(|identity| identity.node_id == repository.node_id)
            })
            .collect::<Vec<_>>();
        if let Some(pr) = owned_head.iter().find(|pr| {
            pr.state == PullRequestState::Open
                && (pr.is_cross_repository
                    || pr
                        .base_repository
                        .as_ref()
                        .is_none_or(|identity| identity.node_id != repository.node_id))
        }) {
            bail!(
                "Open PR #{} uses managed head branch `{head_ref}` but its repository identity is inconsistent with publication repository {}",
                pr.number,
                repository.name_with_owner
            );
        }

        let same_repository = candidates
            .iter()
            .filter(|pr| {
                pr.head_branch == *head_ref
                    && !pr.is_cross_repository
                    && pr
                        .head_repository
                        .as_ref()
                        .is_some_and(|identity| identity.node_id == repository.node_id)
                    && pr
                        .base_repository
                        .as_ref()
                        .is_some_and(|identity| identity.node_id == repository.node_id)
            })
            .collect::<Vec<_>>();
        let open = same_repository
            .iter()
            .copied()
            .filter(|pr| pr.state == PullRequestState::Open)
            .collect::<Vec<_>>();

        match open.as_slice() {
            [] => {
                if let Some(pr) = same_repository.iter().find(|pr| {
                    matches!(pr.state, PullRequestState::Merged | PullRequestState::Closed)
                }) {
                    canonical.push((*pr).clone());
                }
            }
            [pr] => canonical.push((*pr).clone()),
            _ => {
                let numbers =
                    open.iter().map(|pr| format!("#{}", pr.number)).collect::<Vec<_>>().join(", ");
                bail!(
                    "Multiple open PRs ({numbers}) use GHerrit head branch `{head_ref}` in {}",
                    repository.name_with_owner
                );
            }
        }
    }

    Ok(canonical)
}

fn validate_prs_for_publication(prs: &[PrState], publication: &PublicationPlan) -> Result<()> {
    let mut errors = Vec::new();

    for pr in prs {
        match pr.state {
            PullRequestState::Closed => errors.push(format!(
                "Cannot push to closed PR #{}. Please reopen it or use a new GHerrit ID.",
                pr.number
            )),
            PullRequestState::Merged => errors.push(format!(
                "Cannot push to merged PR #{}. Merged PRs cannot be reopened.",
                pr.number
            )),
            PullRequestState::Open => {}
        }

        match pr.body.as_deref() {
            Some(body) => match gherrit_metadata::parse_terminal(body) {
                Ok(Some(metadata)) if metadata.id == pr.head_branch => {}
                Ok(Some(metadata)) => errors.push(format!(
                    "PR #{} head branch `{}` disagrees with terminal GHerrit metadata ID `{}`",
                    pr.number, pr.head_branch, metadata.id
                )),
                Ok(None) => errors.push(format!(
                    "PR #{} for managed branch `{}` has no terminal GHerrit metadata",
                    pr.number, pr.head_branch
                )),
                Err(error) => errors.push(format!(
                    "PR #{} for managed branch `{}` has invalid terminal GHerrit metadata: {error}",
                    pr.number, pr.head_branch
                )),
            },
            None => errors.push(format!(
                "PR #{} for managed branch `{}` has no body or terminal GHerrit metadata",
                pr.number, pr.head_branch
            )),
        }

        let expected_head =
            publication.expected_heads.get(&pr.head_branch).and_then(|head| head.as_deref());
        if expected_head != Some(pr.head_oid.as_str()) {
            errors.push(format!(
                "PR #{} reports head {} at {}, but remote Git was observed at {}",
                pr.number,
                pr.head_oid,
                pr.head_branch,
                expected_head.unwrap_or("<missing>")
            ));
        }

        let desired_head = publication
            .desired_heads
            .get(&pr.head_branch)
            .map(ToString::to_string)
            .unwrap_or_else(|| "<missing>".to_string());
        log::trace!(
            "Observed PR #{}: {}@{} (desired {}) -> {}@{}",
            pr.number,
            pr.head_branch,
            pr.head_oid,
            desired_head,
            pr.base_branch,
            pr.base_oid
        );
    }

    if !errors.is_empty() {
        bail!(
            "{}\nYou may want to fetch and reconcile the remote state before pushing.",
            errors.join("\n")
        );
    }

    Ok(())
}

enum GraphQlOp {
    Query,
    Mutation,
}

/// Executes batched GraphQL operations (queries or mutations).
///
/// Builds a combined query for each adaptive batch and processes each
/// operation in a successful response with `response_handler`.
async fn run_batched_graphql<T, M, H>(
    octocrab: &Octocrab,
    operation_type: GraphQlOp,
    items: impl IntoIterator<Item = T>,
    query_builder: M,
    mut response_handler: H,
) -> Result<()>
where
    M: Fn(&T) -> String,
    H: FnMut(&T, &serde_json::Value) -> Result<()>,
{
    let items: Vec<T> = items.into_iter().collect();
    if items.is_empty() {
        return Ok(());
    }

    let alias = |index| format!("op{index}");

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
    let mut batches = BatchPlan::new(items.len(), INITIAL_GRAPHQL_BATCH_LEN);
    while let Some(range) = batches.current() {
        let chunk = &items[range];

        let query_body: String = chunk
            .iter()
            .enumerate()
            .map(|(i, item)| format!("{}: {}", alias(i), query_builder(item)))
            .collect();

        let query = format!(
            "{} {{ {query_body} }}",
            match operation_type {
                GraphQlOp::Query => "query",
                GraphQlOp::Mutation => "mutation",
            }
        );

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
            let request_payload = json!({ "query": query });
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

        let data = json_get!(response["data"])?;

        for (i, item) in chunk.iter().enumerate() {
            let alias = alias(i);
            let op_data = graphql_operation(data, &alias)?;
            response_handler(item, op_data)?;
        }

        batches.accept();
    }
    Ok(())
}

fn graphql_operation<'a>(
    data: &'a serde_json::Value,
    alias: &str,
) -> Result<&'a serde_json::Value> {
    data.get(alias).ok_or_else(|| eyre!("GraphQL response is missing operation `{alias}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec!["Gabcdefghijklmnopqrstuvwxyz234567".to_string()]
    }

    #[test]
    fn remote_version_parser_prefers_peeled_targets_and_rejects_conflicts() {
        let mut observations = HashMap::new();
        parse_remote_version_lines(
            concat!(
                "1111111111111111111111111111111111111111\trefs/tags/gherrit/Gabcdefghijklmnopqrstuvwxyz234567/v1\n",
                "2222222222222222222222222222222222222222\trefs/tags/gherrit/Gabcdefghijklmnopqrstuvwxyz234567/v2\n",
                "3333333333333333333333333333333333333333\trefs/tags/gherrit/Gabcdefghijklmnopqrstuvwxyz234567/v2^{}\n",
            ),
            &ids(),
            &mut observations,
        )
        .unwrap();
        assert_eq!(
            observations
                .get(&("Gabcdefghijklmnopqrstuvwxyz234567".to_string(), 2))
                .unwrap()
                .peeled_oid
                .as_deref(),
            Some("3333333333333333333333333333333333333333")
        );

        let error = parse_remote_version_lines(
            "4444444444444444444444444444444444444444\trefs/tags/gherrit/Gabcdefghijklmnopqrstuvwxyz234567/v2^{}\n",
            &ids(),
            &mut observations,
        )
        .unwrap_err();
        assert!(error.to_string().contains("conflicting peeled object IDs"));
    }

    #[test]
    fn missing_graphql_operation_is_an_error() {
        let data = serde_json::json!({ "op0": { "clientMutationId": null } });

        assert!(graphql_operation(&data, "op0").is_ok());
        let error = graphql_operation(&data, "op1").unwrap_err();
        assert_eq!(error.to_string(), "GraphQL response is missing operation `op1`");
    }
}

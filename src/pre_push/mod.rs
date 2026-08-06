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
    re,
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

    let commits = collect_commits(repo).wrap_err("Failed to collect commits")?;

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

    let publication = plan_publication(repo, &commits)?;
    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let candidates = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;
    let prs = select_canonical_prs(repo, &gherrit_ids, candidates)?;
    validate_prs_for_publication(&prs, &publication)?;

    let rewritten_branches = rewritten_existing_branches(&publication);
    let base_consumers = batch_fetch_base_consumers(repo, &octocrab, &rewritten_branches).await?;
    validate_base_consumers(&prs, &rewritten_branches, &base_consumers)?;

    let remote_default = observe_remote_default(repo)?;
    let staging_bases = plan_pr_staging(repo, &commits, &prs, &publication, &remote_default)?;
    validate_topology_transition_state(&prs, &staging_bases)?;
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

    apply_staging_bases(&octocrab, &remote_default.name, &staging_bases).await?;
    let prepared_candidates = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;
    let prepared_prs = select_canonical_prs(repo, &gherrit_ids, prepared_candidates)?;
    validate_prs_for_publication(&prepared_prs, &publication)?;
    verify_staging_bases(
        repo,
        &commits,
        &prepared_prs,
        &publication,
        &remote_default,
        &staging_bases,
    )?;
    verify_publication_inputs(repo, &publication, &remote_default)?;

    execute_publication(repo, &publication)?;
    let latest_versions = publication.latest_versions.clone();
    let default_branch = remote_default.name;

    let projected_candidates = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;
    let projected_prs = select_canonical_prs(repo, &gherrit_ids, projected_candidates)?;
    validate_prs_after_publication(&projected_prs, &publication)?;

    let num_commits = commits.len();
    sync_prs(
        repo,
        &octocrab,
        branch_name,
        &default_branch,
        commits,
        latest_versions,
        projected_prs,
    )
    .await?;
    verify_final_projection(repo, &octocrab, &gherrit_ids, &default_branch, &publication).await?;

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
    validate_linear_history(default_ref.detach(), &commits)?;

    let remote = repo.default_remote_name();
    let commits = commits
        .into_iter()
        .map(|c| -> Result<Commit> {
            let msg = c.message()?;
            let title = core::str::from_utf8(msg.title)?;

            if ["fixup!", "squash!", "amend!"].iter().any(|p| title.starts_with(p)) {
                // FIXME: Currently, the indent before `git rebase` is not
                // preserved.
                bail!(
                    "Stack contains pending fixup/squash/amend commits.\n\
                    Please squash your history before syncing:\n\
                        git rebase -i --autosquash {remote}/{default_branch}",
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
    head_repository: String,
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
    head_repository: String,
    base_branch: String,
}

struct PublicationPlan {
    batches: Vec<PushPlan>,
    latest_versions: HashMap<String, usize>,
    expected_heads: HashMap<String, Option<String>>,
    desired_heads: HashMap<String, ObjectId>,
}

fn plan_publication(repo: &util::Repo, commits: &[Commit]) -> Result<PublicationPlan> {
    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let expected_heads = get_remote_branch_states(repo, &gherrit_ids)
        .wrap_err("Failed to observe remote GHerrit branches")?;
    let remote_versions = get_remote_versions(repo, &gherrit_ids)
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
            let remote_version = remote_versions.get(&commit.gherrit_id).copied().unwrap_or(0);
            desired_heads.insert(commit.gherrit_id.clone(), commit.id);

            // If remote Git already contains this exact head and a coherent
            // patch version, this invocation is projection-only. This is what
            // makes a retry after a successful push but failed GitHub update
            // repair the PRs without manufacturing another version.
            if expected_remote_sha == desired_oid && remote_version > 0 {
                latest_versions.insert(commit.gherrit_id.clone(), remote_version);
                continue;
            }

            let next_version = remote_version.checked_add(1).ok_or_else(|| {
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
            batches.push(plan_push(&repo.default_remote_name(), &targets));
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
fn execute_publication(repo: &util::Repo, publication: &PublicationPlan) -> Result<()> {
    for plan in &publication.batches {
        log::info!("Pushing chunk to remote...");
        let mut child = util::cmd("git", &plan.arguments)
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
            let remote = repo.default_remote_name();
            bail!(
                "`git push` failed. The remote might be ahead or changed. Run `git fetch {remote}` to sync."
            );
        }

        persist_local_tags(repo, &plan.persisted_tags);
    }

    Ok(())
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
    gherrit_ids: &[String],
) -> Result<HashMap<String, Option<String>>> {
    let mut states: HashMap<String, Option<String>> =
        gherrit_ids.iter().cloned().map(|id| (id, None)).collect();
    for chunk in remote_query_batches(gherrit_ids) {
        let mut args = vec!["ls-remote".to_string(), repo.default_remote_name()];
        args.extend(chunk.iter().map(|id| format!("refs/heads/{id}")));

        let output = util::cmd("git", args).checked_output()?;
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
    gherrit_ids: &[String],
) -> Result<HashMap<String, usize>> {
    let mut versions = gherrit_ids.iter().cloned().map(|id| (id, 0)).collect::<HashMap<_, _>>();
    for chunk in remote_query_batches(gherrit_ids) {
        let mut args =
            vec!["ls-remote".to_string(), "--tags".to_string(), repo.default_remote_name()];
        args.extend(chunk.iter().map(|id| format!("refs/tags/gherrit/{id}/v*")));
        let output = util::cmd("git", args).checked_output()?;
        let output = core::str::from_utf8(&output.stdout)?;

        for line in output.lines() {
            let Some((_, ref_name)) = line.split_once('\t') else {
                continue;
            };
            let ref_name = ref_name.strip_suffix("^{}").unwrap_or(ref_name);
            let Some(rest) = ref_name.strip_prefix("refs/tags/gherrit/") else {
                continue;
            };
            let Some((id, version)) = rest.rsplit_once("/v") else {
                continue;
            };
            let Some(current) = versions.get_mut(id) else {
                continue;
            };
            let version = version.parse::<usize>().map_err(|_| {
                eyre!("Remote patch tag `{ref_name}` has an invalid version number")
            })?;
            *current = (*current).max(version);
        }
    }
    Ok(versions)
}

fn observe_remote_default(repo: &util::Repo) -> Result<RemoteDefault> {
    let remote = repo.default_remote_name();
    let output = util::cmd("git", ["ls-remote", "--symref", &remote, "HEAD"])
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
    let observed = get_remote_branch_states(repo, &relevant_branches)
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

    fetch_remote_branch_objects(repo, &relevant_branches)?;

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

fn fetch_remote_branch_objects(repo: &util::Repo, branches: &[String]) -> Result<()> {
    for chunk in remote_query_batches(branches) {
        let mut args = vec![
            "fetch".to_string(),
            "--quiet".to_string(),
            "--no-tags".to_string(),
            "--no-write-fetch-head".to_string(),
            repo.default_remote_name(),
        ];
        args.extend(chunk.iter().map(|branch| format!("refs/heads/{branch}")));
        util::cmd("git", args)
            .success()
            .wrap_err("Failed to fetch remote objects required for reachability checks")?;
    }
    Ok(())
}

fn git_is_ancestor(repo: &util::Repo, ancestor: &str, descendant: &str) -> Result<bool> {
    let mut command = util::cmd("git", ["merge-base", "--is-ancestor", ancestor, descendant]);
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

fn validate_topology_transition_state(
    prs: &[PrState],
    staging_bases: &[StagingBase],
) -> Result<()> {
    let topology_changes =
        staging_bases.iter().any(|staging| staging.current_base != staging.desired_base);
    if !topology_changes {
        return Ok(());
    }

    let mut errors = Vec::new();
    for pr in prs {
        if pr.is_in_merge_queue {
            errors.push(format!(
                "PR #{} is in the merge queue; remove it before reordering this stack",
                pr.number
            ));
        }
        if pr.auto_merge_enabled {
            errors.push(format!(
                "PR #{} has auto-merge enabled; disable it before reordering this stack",
                pr.number
            ));
        }
        if pr.native_stack {
            errors.push(format!(
                "PR #{} belongs to a native GitHub stack; unstack it before GHerrit rewrites bases",
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
    octocrab: &Octocrab,
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
    batch_update_prs(octocrab, updates).await?;
    log::info!("Prepared PR bases for ref publication.");
    Ok(())
}

fn verify_staging_bases(
    repo: &util::Repo,
    commits: &[Commit],
    prs: &[PrState],
    publication: &PublicationPlan,
    remote_default: &RemoteDefault,
    expected: &[StagingBase],
) -> Result<()> {
    let actual = plan_pr_staging(repo, commits, prs, publication, remote_default)?;
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
    publication: &PublicationPlan,
    remote_default: &RemoteDefault,
) -> Result<()> {
    let mut branches = publication.expected_heads.keys().cloned().collect::<Vec<_>>();
    branches.push(remote_default.name.clone());
    let observed = get_remote_branch_states(repo, &branches)
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
    repo: &util::Repo,
    octocrab: &Octocrab,
    gherrit_ids: &[String],
    default_branch: &str,
    publication: &PublicationPlan,
) -> Result<()> {
    let candidates = batch_fetch_prs(repo, octocrab, gherrit_ids).await?;
    let prs = select_canonical_prs(repo, gherrit_ids, candidates)?;
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
    #[derive(serde::Serialize)]
    struct GherritMetadata<'a> {
        id: &'a str,
        parent: Option<&'a str>,
        child: Option<&'a str>,
    }

    let metadata = serde_json::to_string(&GherritMetadata { id, parent, child })
        .expect("serializing GHerrit metadata cannot fail");
    format!("<!-- gherrit-meta: {metadata} -->")
}

impl PrBodyBuilder<'_> {
    fn build(self) -> String {
        enum HistoryTableFormat {
            Full,
            Sparse,
        }

        fn write_body(
            slf: &PrBodyBuilder,
            mut w: impl Write,
            format: HistoryTableFormat,
        ) -> fmt::Result {
            let current_gherrit_id = &slf.c.gherrit_id;
            let re = gherrit_pr_id_re();
            let body_clean = re.replace(&slf.c.message_body, "");

            w.write_str("<!-- WARNING: This PR description is automatically generated by GHerrit. Any manual edits will be overwritten on the next push. -->\n\n")?;
            w.write_str(&body_clean)?;
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
            slf: &PrBodyBuilder,
            mut w: impl Write,
            format: HistoryTableFormat,
        ) -> fmt::Result {
            if slf.latest_version > 1 && !slf.repo_url.is_empty() {
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
                w.write_str(
                    "*Links show the diff between the row version and the column version.*\n\n",
                )?;

                // Header
                w.write_str("|Version|")?;
                for v in (1..slf.latest_version).rev() {
                    write!(w, " v{} |", v)?;
                }
                w.write_str("Base|")?;

                w.write_str("\n|:---|")?;
                for _ in 1..slf.latest_version {
                    w.write_str(":---|")?;
                }
                w.write_str(":---|\n")?;

                let prefix = if slf.latest_version <= 8 { "vs " } else { "" };

                // Rows
                for v_row in (1..=slf.latest_version).rev() {
                    write!(w, "|v{}|", v_row)?;

                    // Previous version columns
                    for v_col in (1..slf.latest_version).rev() {
                        if v_col < v_row {
                            use HistoryTableFormat::*;
                            // In sparse mode, only show:
                            // - Diffs between the current version and each previous version
                            // - Diffs between each version and:
                            //   - Its previous version
                            //   - The base branch
                            let show_link = match format {
                                Full => true,
                                Sparse => v_row == slf.latest_version || v_row == v_col + 1,
                            };

                            if show_link {
                                write!(
                                    w,
                                    "[{}v{}]({}/compare/gherrit/{}/v{}..gherrit/{}/v{})|",
                                    prefix,
                                    v_col,
                                    slf.repo_url,
                                    slf.c.gherrit_id,
                                    v_col,
                                    slf.c.gherrit_id,
                                    v_row
                                )?;
                            } else {
                                w.write_str("|")?;
                            }
                        } else {
                            w.write_str("|")?;
                        }
                    }

                    // Base column (v0) – compare base_branch..v_row.
                    write!(
                        w,
                        "[{}Base]({}/compare/{}..gherrit/{}/v{})|",
                        prefix, slf.repo_url, slf.base_branch, slf.c.gherrit_id, v_row
                    )?;

                    w.write_str("\n")?;
                }
                w.write_str("\n</details>")?;
            }

            Ok(())
        }

        fn write_download_section(slf: &PrBodyBuilder, mut w: impl Write) -> fmt::Result {
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

            w.write_str("</details>")?;
            Ok(())
        }

        struct ByteCounter(usize);
        impl Write for ByteCounter {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                self.0 += s.len();
                Ok(())
            }
        }

        // Per https://github.com/orgs/community/discussions/27190#discussioncomment-3254953:
        //
        //   PR body/Issue comments are still stored in MySQL as a mediumblob
        //   with a maximum value length of 262,144. This equals a limit of
        //   65,536 4-byte unicode characters.
        //
        // We use half of GitHub's limit to add a safety factor.
        const MAX_BODY_SIZE_BYTES: usize = 131_072;

        let history_table_format = {
            use HistoryTableFormat::*;

            let mut full_size = ByteCounter(0);
            write_body(&self, &mut full_size, Full).unwrap();
            if full_size.0 > MAX_BODY_SIZE_BYTES { Sparse } else { Full }
        };

        let mut body = String::new();
        write_body(&self, &mut body, history_table_format).unwrap();
        body
    }
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
    prs: Vec<PrState>,
) -> Result<()> {
    let remote = repo.default_remote()?;

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
                    body: c.message_body.clone(),
                    base_branch: entry.base_branch.clone(),
                    head_branch: c.gherrit_id.clone(),
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
                        head_oid: String::new(),
                        head_repository: format!("{}/{}", remote.owner, remote.repo_name),
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

    let head_branch_markdown = (!is_private_stack(repo, branch_name))
        .then(|| {
            let head_ref = repo.head().ok()?.try_into_referent()?;
            let (cat, short_name) = head_ref.inner.name.category_and_short_name()?;
            (cat == Category::LocalBranch)
                .then(|| format!("This PR is on branch [{short_name}](../tree/{short_name}).\n\n"))
        })
        .flatten();

    let repo_url = remote.repo_url_relative();
    let mut updates: Vec<PrUpdate> = commit_pr_states
        .iter()
        .filter_map(|(entry, pr_state)| {
            let c = &entry.item;
            let gh_pr_ids_markdown = commit_pr_states
                .iter()
                .rev()
                .map(|(_, state)| {
                    let prefix =
                        if state.number == pr_state.number { "👉" } else { "\u{3000}\u{2009}" };
                    format!("- {} #{}", prefix, state.number)
                })
                .collect::<Vec<_>>()
                .join("\n");

            let latest_version = latest_versions.get(&c.gherrit_id).copied().unwrap_or(1);

            let body = (PrBodyBuilder {
                c,
                repo_url: &repo_url,
                head_branch_markdown: head_branch_markdown.as_deref(),
                gh_pr_ids_markdown: &gh_pr_ids_markdown,
                latest_version,
                base_branch: &entry.base_branch,
                parent_id: entry.parent_id.as_deref(),
                child_id: entry.child_id.as_deref(),
            })
            .build();

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
        let gherrit_id = {
            let mut captures = gherrit_pr_id_re().captures_iter(&message_body);
            let first = captures.next().ok_or_else(|| {
                eyre!("Commit {} missing a non-empty gherrit-pr-id trailer", c.id)
            })?;
            if captures.next().is_some() {
                bail!("Commit {} contains multiple gherrit-pr-id trailers", c.id);
            }
            first.get(1).unwrap().as_str().to_string()
        };

        Ok(Commit { id: c.id, gherrit_id, message_title, message_body })
    }
}

re!(gherrit_pr_id_re, r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]+)$");

/// A request to create a new PR in a batch.
#[derive(Clone)]
struct BatchCreate {
    title: String,
    body: String,
    base_branch: String,
    head_branch: String,
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

/// Recursively looks up nested values from a JSON object, converting lookup
/// failures to `Result::Err` values.
macro_rules! json_get {
    ($val:ident [$key:expr] $(.$as:ident())? $([$rest_key:expr] $(.$rest_as:ident())?)*) => {
        $val
            .get($key)$(.and_then(|v| v.$as()))?.ok_or_else(|| eyre!("Missing JSON field in GraphQL response: `{}`", stringify!($key)))
            $(.and_then(|v| v.get($rest_key)$(.and_then(|v| v.$rest_as()))?.ok_or_else(|| eyre!("Missing JSON field in GraphQL response: `{}`", stringify!($rest_key)))))*
    };
}

/// Fetches the global Repository Node ID for the given owner and repo.
///
/// This ID (e.g., "R_kgDOL...") is required for creating PRs via the GraphQL
/// API, as the `createPullRequest` mutation accepts a `repositoryId` argument,
/// not owner/name.
async fn fetch_repo_id(octocrab: &Octocrab, remote: &Remote) -> Result<String> {
    // NOTE: It's important that we pass `remote.*` as GraphQL variables, not
    // using string interpolation, as the variables are escaped. Using string
    // interpolation would risk injection attacks.
    let query = r#"query RepositoryID($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { id } }"#;
    let query_body = json!({
        "query": query,
        "variables": {
            "owner": remote.owner,
            "name": remote.repo_name,
        }
    });
    let response: serde_json::Value =
        octocrab.graphql(&query_body).await.wrap_err("Failed to fetch repository ID")?;

    if let Some(errors) = response.get("errors") {
        log::error!("GraphQL errors: {}", errors);
        bail!("Failed to fetch repository ID: {:?}", errors);
    }

    let id = json_get!(response["data"]["repository"]["id"].as_str())?;
    Ok(id.to_string())
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
        |update| {
            safe_json_format!(
                "updatePullRequest(input: {{ {fields} }}) {{ clientMutationId }}",
                (fields = {
                    "pullRequestId" : update.node_id,
                    "baseRefName" ? update.base_branch,
                    "title" ? update.title,
                    "body" ? update.body,
                })
            )
        },
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

async fn batch_fetch_prs(
    repo: &util::Repo,
    octocrab: &Octocrab,
    head_refs: &[String],
) -> Result<Vec<PrState>> {
    let remote = repo.default_remote()?;
    let owner = remote.owner;
    let repo_name = remote.repo_name;

    let mut all_prs = Vec::new();

    run_batched_graphql(
        octocrab,
        GraphQlOp::Query,
        head_refs,
        |head_ref| {
            safe_json_format!(
                "repository(owner: {owner}, name: {repo_name}) {{ pullRequests(headRefName: {head_ref}, first: 100, states: [OPEN, CLOSED, MERGED]) {{ totalCount, nodes {{ number, id, title, body, baseRefName, baseRefOid, headRefName, headRefOid, headRepository {{ nameWithOwner }}, state, isInMergeQueue, autoMergeRequest {{ enabledAt }}, stackEntry {{ id }} }} }} }}",
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
                    head_repository: json_get!(node["headRepository"]["nameWithOwner"].as_str())?
                        .to_string(),
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
    repo: &util::Repo,
    octocrab: &Octocrab,
    base_refs: &[String],
) -> Result<Vec<PrBaseConsumer>> {
    let remote = repo.default_remote()?;
    let owner = remote.owner;
    let repo_name = remote.repo_name;
    let mut consumers = Vec::new();

    run_batched_graphql(
        octocrab,
        GraphQlOp::Query,
        base_refs,
        |base_ref| {
            safe_json_format!(
                "repository(owner: {owner}, name: {repo_name}) {{ pullRequests(baseRefName: {base_ref}, first: 100, states: [OPEN]) {{ totalCount, nodes {{ number, id, headRefName, headRepository {{ nameWithOwner }}, baseRefName }} }} }}",
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
                    head_repository: json_get!(
                        node["headRepository"]["nameWithOwner"].as_str()
                    )?
                    .to_string(),
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
            format!(
                "PR #{} ({}/{} -> {})",
                consumer.number,
                consumer.head_repository,
                consumer.head_branch,
                consumer.base_branch
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "Cannot rewrite managed branches while unrelated open PRs target them: {details}. Retarget or close those PRs first."
    )
}

fn select_canonical_prs(
    repo: &util::Repo,
    head_refs: &[String],
    candidates: Vec<PrState>,
) -> Result<Vec<PrState>> {
    let remote = repo.default_remote()?;
    let repository = format!("{}/{}", remote.owner, remote.repo_name);
    let mut canonical = Vec::new();

    for head_ref in head_refs {
        let same_repository = candidates
            .iter()
            .filter(|pr| pr.head_branch == *head_ref && pr.head_repository == repository)
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
                    "Multiple open PRs ({numbers}) use GHerrit head branch `{head_ref}` in {repository}"
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

    #[test]
    fn missing_graphql_operation_is_an_error() {
        let data = serde_json::json!({ "op0": { "clientMutationId": null } });

        assert!(graphql_operation(&data, "op0").is_ok());
        let error = graphql_operation(&data, "op1").unwrap_err();
        assert_eq!(error.to_string(), "GraphQL response is missing operation `op1`");
    }
}

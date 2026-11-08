//! Finds each commit's PR and updates it to match the submitted stack.
//!
//! Existing PRs and empty slots stay in stack order. Creation replies supply
//! the missing PR IDs before full descriptions are rendered. Queries and
//! mutations use the shared GraphQL transport. The command entry point still
//! checks PR states and chooses when to push Git refs.

use std::collections::HashMap;

use color_eyre::eyre::{Context as _, Result, bail, eyre};
use octocrab::Octocrab;
use owo_colors::OwoColorize;

use super::{
    body::PrBody,
    github::{
        CreatePullRequest, CreatedPullRequest, FindPullRequest, PullRequest as PrState,
        RepositoryIdQuery, UpdatePullRequest, run_batched_graphql,
    },
    identity::ChangeId,
    push::VersionedCommit,
    reconcile::{CurrentPr, DesiredPr, PrUpdate, link_stack, plan_update},
};
use crate::util;

fn pull_requests_by_head(prs: &[PrState]) -> HashMap<&str, &PrState> {
    let mut by_head = HashMap::with_capacity(prs.len());
    for pr in prs {
        // Use the first PR found for each head, as a linear search would.
        by_head.entry(pr.head_branch.as_str()).or_insert(pr);
    }
    by_head
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
pub(super) async fn sync_prs(
    repo: &util::Repo,
    octocrab: &Octocrab,
    public_branch: Option<&str>,
    base_branch: &str,
    commits: Vec<VersionedCommit>,
    prs: Vec<PrState>,
) -> Result<()> {
    let remote = repo.default_remote()?;

    let commits = link_stack(base_branch, commits, |entry| entry.commit().gherrit_id().to_string());

    let prs_by_head = pull_requests_by_head(&prs);

    // Keep one slot per commit. Request a new PR only for an empty slot. For
    // existing PRs, keep using the first match in the index.
    let resolutions: Vec<_> = commits
        .iter()
        .map(|entry| {
            let c = entry.item.commit();
            if let Some(&pr) = prs_by_head.get(c.gherrit_id().as_str()) {
                log::debug!(
                    "Found existing PR #{} for {}",
                    pr.number.green().bold(),
                    c.gherrit_id()
                );
                Some(pr.clone())
            } else {
                log::debug!("No GitHub PR exists for {}; queuing creation...", c.gherrit_id());
                None
            }
        })
        .collect();

    // Create PRs with the plain commit body first. Full descriptions need every
    // PR number, so writing them remains a separate step.
    let num_creations = resolutions.iter().filter(|state| state.is_none()).count();
    let created = if num_creations != 0 {
        log::info!("Creating {num_creations} PRs...");
        let repo_id = fetch_repo_id(octocrab, &remote).await?;
        let requests = commits.iter().zip(&resolutions).filter(|(_, state)| state.is_none()).map(
            |(entry, _)| {
                let c = entry.item.commit();
                CreatePullRequest::new(
                    repo_id.clone(),
                    entry.base_branch.clone(),
                    c.gherrit_id().to_string(),
                    c.message_title().to_owned(),
                    c.message_body().to_owned(),
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

    // The decoder matches aliases to requests and returns results in request
    // order. Use each new PR once, in the order of the empty slots. Its decoded
    // result already has the request details, so we need neither a head-keyed
    // map nor another copy of those details to fill the slot.
    let mut created = created.into_iter();
    let mut commit_pr_states = Vec::with_capacity(commits.len());
    for (entry, existing) in commits.iter().zip(resolutions) {
        let state = match existing {
            Some(state) => state,
            None => {
                let id = entry.item.commit().gherrit_id();
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
    // Use the same bottom-to-top order for navigation and parent/child links.
    // plan_update leaves out unchanged fields, especially the base: GitHub may
    // reject an unchanged base edit for a PR in the merge queue.
    let updates: Vec<PrUpdate> = commit_pr_states
        .iter()
        .filter_map(|(entry, pr_state)| {
            let c = entry.item.commit();
            let latest_version = entry.item.version().get();

            let body = PrBody {
                commit_body: c.message_body(),
                repo_url: &repo_url,
                public_branch,
                stack_pr_numbers: &stack_pr_numbers,
                current_pr_number: pr_state.number,
                latest_version,
                base_branch: &entry.base_branch,
                gherrit_id: c.gherrit_id().as_str(),
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
                DesiredPr {
                    title: c.message_title(),
                    body: &body,
                    base_branch: &entry.base_branch,
                },
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

pub(super) async fn batch_fetch_prs(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_push::reconcile;

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
    fn transient_pr_index_preserves_first_match() {
        let prs = [observed_pr(1, "Ga"), observed_pr(2, "Gb"), observed_pr(3, "Ga")];
        let index = pull_requests_by_head(&prs);
        assert_eq!(index["Ga"].number, 1);
        assert_eq!(index["Gb"].number, 2);
        assert!(!index.contains_key("Gc"));
    }
}

use core::str;
use std::{collections::HashMap, process::Stdio, time::Instant};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::{ObjectId, reference::Category, refs::transaction::PreviousValue};
use itertools::Itertools;
use octocrab::Octocrab;
use owo_colors::OwoColorize;
use serde_json::json;

use crate::{
    re,
    util::{self, CommandExt as _, HeadState, Remote},
};

pub async fn run(repo: &util::Repo) -> Result<()> {
    let t0 = Instant::now();

    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Pending(bn) => bn,
        HeadState::Detached => {
            bail!("Cannot push from detached HEAD");
        }
    };

    match repo.is_managed(branch_name)? {
        false => {
            log::info!(
                "Branch {} is UNMANAGED. Allowing standard push.",
                branch_name.yellow()
            );
            return Ok(());
        }
        true => log::info!(
            "Branch {} is MANAGED. Syncing stack...",
            branch_name.yellow()
        ),
    }

    let commits = collect_commits(repo).wrap_err("Failed to collect commits")?;

    let t1 = Instant::now();
    log::trace!("t0 -> t1: {:?}", t1 - t0);

    let commits = create_gherrit_refs(repo, commits).wrap_err("Failed to create refs")?;

    let t2 = Instant::now();
    log::trace!("t1 -> t2: {:?}", t2 - t1);

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

    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let prs = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;

    // Validate PR states early
    for pr in &prs {
        if matches!(
            pr.state,
            PullRequestState::Closed | PullRequestState::Merged
        ) {
            bail!(
                "Cannot push to a {} PR (PR #{}). Please open a new PR or reopen the existing one.",
                format!("{:?}", pr.state).to_ascii_uppercase(),
                pr.number
            );
        }
    }

    let latest_versions = push_to_origin(repo, &commits)?;
    let default_branch = repo.find_default_branch_on_default_remote();

    let num_commits = commits.len();
    sync_prs(
        repo,
        &octocrab,
        branch_name,
        &default_branch,
        commits,
        latest_versions,
        prs,
    )
    .await?;

    log::info!("Successfully synced {num_commits} commits.");
    Ok(())
}

fn collect_commits(repo: &util::Repo) -> Result<Vec<Commit>> {
    let head = repo.rev_parse_single("HEAD")?;
    let default_branch = repo.find_default_branch_on_default_remote();
    let default_ref_spec = format!("refs/heads/{}", default_branch);
    let default_ref = repo.rev_parse_single(default_ref_spec.as_str())?;

    // Verify ancestry to safely determine if we can walk back to default_ref
    // without traversing the entire history (e.g. if the branch is orphaned).
    if !repo.is_ancestor(default_ref.detach(), head.detach())? {
        let branch_name = repo.current_branch().name().unwrap_or("current branch");
        bail!(
            "The branch '{branch_name}' is not based on '{default_branch}'.\n\
             GHerrit only supports stacked branches that share history with the default branch.\n\
             Maybe you want to 'git rebase' on '{default_branch}' before pushing?"
        );
    }

    let mut commits = repo
        .rev_walk([head])
        .all()?
        .take_while(|res| {
            res.as_ref()
                .map(|info| info.id != default_ref)
                .unwrap_or(true)
        })
        .map(|res| -> Result<_> { Ok(res?.object()?) })
        .collect::<Result<Vec<_>>>()?;
    commits.reverse();

    let remote = repo.default_remote_name();
    commits
        .into_iter()
        .map(|c| -> Result<Commit> {
            let msg = c.message()?;
            let title = core::str::from_utf8(msg.title)?;

            if ["fixup!", "squash!", "amend!"]
                .iter()
                .any(|p| title.starts_with(p))
            {
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
        .collect()
}

fn create_gherrit_refs(repo: &util::Repo, commits: Vec<Commit>) -> Result<Vec<Commit>> {
    commits
        .into_iter()
        .map(|c| -> Result<_> {
            let rf = format!("refs/gherrit/{}", c.gherrit_id);
            let _ = repo.reference(rf, c.id, PreviousValue::Any, "")?;
            Ok(c)
        })
        .collect::<Result<Vec<_>>>()
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
    head_branch: String,
    state: PullRequestState,
}

#[allow(clippy::too_many_lines)]
fn push_to_origin(repo: &util::Repo, commits: &[Commit]) -> Result<HashMap<String, usize>> {
    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();

    // Fetch remote branch states to ensure we don't act on stale information.
    let remote_branch_states = get_remote_branch_states(repo, &gherrit_ids).unwrap_or_else(|e| {
        log::warn!("Failed to fetch remote branch states: {}", e);
        HashMap::new()
    });

    let mut next_versions = HashMap::new();

    // Windows command line limit is ~32k chars. Each commit generates ~200
    // chars of refspecs (1 branch ref + 1 tag ref).
    // 80 * 200 = 16,000 chars, leaving plenty of headroom.
    const BATCH_SIZE: usize = 80;

    for chunk in commits.chunks(BATCH_SIZE) {
        let mut refspecs = Vec::new();
        let mut refs_to_persist = Vec::new();

        for c in chunk {
            // Determine the next version based on local tags (Optimistic
            // Locking).
            let local_max = get_local_version(repo, &c.gherrit_id).unwrap_or(0);
            let next_ver = local_max + 1;
            next_versions.insert(c.gherrit_id.clone(), next_ver);

            // Lease the branch to ensure it hasn't changed since our fetch.
            // If we know the remote SHA, we expect it. If we don't (None), we
            // expect "" (creation).
            let expected_sha = remote_branch_states
                .get(&c.gherrit_id)
                .map(|s| s.as_deref().unwrap_or(""))
                .unwrap_or("");

            refspecs.push(format!("{}:refs/heads/{}", c.id, c.gherrit_id));
            refspecs.push(format!(
                "--force-with-lease=refs/heads/{}:{expected_sha}",
                c.gherrit_id
            ));

            // Lock the next version tag to prevent concurrent pushes of the
            // same version. `--force-with-lease=<ref>:` means "expect the ref
            // to NOT exist", and causes the server to fail the operation if it
            // does exist. This prevents overwriting if someone else pushed
            // next_ver already.
            refspecs.push(format!(
                "{}:refs/tags/gherrit/{}/v{}",
                c.id, c.gherrit_id, next_ver
            ));
            refspecs.push(format!(
                "--force-with-lease=refs/tags/gherrit/{}/v{next_ver}:",
                c.gherrit_id
            ));

            refs_to_persist.push((c.id, c.gherrit_id.clone(), next_ver));
        }

        if refspecs.is_empty() {
            continue;
        }

        let mut args = vec![
            "push".to_string(),
            "--quiet".to_string(),
            "--no-verify".to_string(),
            "--atomic".to_string(), // Critical for the lock to work
            repo.default_remote_name(),
        ];
        args.extend(refspecs);

        log::info!("Pushing chunk to remote...");
        let mut child = util::cmd("git", args)
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
        for (id, gherrit_id, ver) in refs_to_persist {
            let _ = repo.reference(
                format!("refs/tags/gherrit/{gherrit_id}/v{ver}"),
                id,
                PreviousValue::Any,
                "gherrit: persist local version state",
            );
        }
    }

    Ok(next_versions)
}

#[allow(clippy::type_complexity)]
fn get_remote_branch_states(
    repo: &util::Repo,
    gherrit_ids: &[String],
) -> Result<HashMap<String, Option<String>>> {
    if gherrit_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut states: HashMap<String, Option<String>> = HashMap::new();

    // Batch size is limited to avoid exceeding command line limits (e.g.,
    // Windows 32k chars). Each refspec is ~62 chars. 250 * 62 = 15,500
    // chars (safe).
    const BATCH_SIZE: usize = 250;

    for chunk in gherrit_ids.chunks(BATCH_SIZE) {
        let mut args = vec!["ls-remote".to_string(), repo.default_remote_name()];
        args.extend(chunk.iter().map(|id| format!("refs/heads/{id}")));

        let output = util::cmd("git", args).checked_output()?;
        let output = core::str::from_utf8(&output.stdout)?;

        for line in output.lines() {
            // Output format: "<SHA>\t<refname>"
            let Some((sha, ref_name)) = line.split_once('\t') else {
                continue;
            };

            // Match heads: refs/heads/<id>
            let head_re = re!(r"refs/heads/([a-zA-Z0-9]+)$");
            if let Some(caps) = head_re.captures(ref_name)
                && let Some(id_match) = caps.get(1)
            {
                let id = id_match.as_str().to_string();
                states.insert(id, Some(sha.to_string()));
            }
        }
    }

    Ok(states)
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

#[allow(clippy::too_many_arguments)]
fn generate_pr_body(
    c: &Commit,
    repo_url: &str,
    head_branch_markdown: &str,
    gh_pr_ids_markdown: &str,
    latest_version: usize,
    base_branch: &str,
    parent_id: Option<&str>,
    child_id: Option<&str>,
) -> String {
    let current_gherrit_id = &c.gherrit_id;
    let re = gherrit_pr_id_re();
    let body_clean = re.replace(&c.message_body, "");

    // Generate Patch History Table
    let mut history_table = String::new();
    if latest_version > 1 && !repo_url.is_empty() {
        history_table.push_str(&format!(
            "\n\n**Latest Update:** v{} — [Compare vs v{}]({}/compare/gherrit/{}/v{}..gherrit/{}/v{})\n\n",
            latest_version,
            latest_version - 1,
            repo_url,
            current_gherrit_id,
            latest_version - 1,
            current_gherrit_id,
            latest_version
        ));

        history_table
            .push_str("<details>\n<summary><strong>📚 Full Patch History</strong></summary>\n\n");
        history_table
            .push_str("*Links show the diff between the row version and the column version.*\n\n");

        // Header
        history_table.push_str("| Version | Base |");
        for v in 1..latest_version {
            history_table.push_str(&format!(" v{} |", v));
        }
        history_table.push_str("\n| :--- | :--- |");
        for _ in 1..latest_version {
            history_table.push_str(" :--- |");
        }
        history_table.push('\n');

        let prefix = if latest_version <= 8 { "vs " } else { "" };

        // Rows
        for v_row in (1..=latest_version).rev() {
            history_table.push_str(&format!("| v{} |", v_row));

            // Base column (v0)
            // Compare base_branch..v_row
            let base_link = format!(
                "[{}Base]({}/compare/{}..gherrit/{}/v{})",
                prefix, repo_url, base_branch, current_gherrit_id, v_row
            );
            history_table.push_str(&format!(" {} |", base_link));

            // Previous version columns
            for v_col in 1..latest_version {
                if v_col < v_row {
                    let link = format!(
                        "[{}v{}]({}/compare/gherrit/{}/v{}..gherrit/{}/v{})",
                        prefix,
                        v_col,
                        repo_url,
                        current_gherrit_id,
                        v_col,
                        current_gherrit_id,
                        v_row
                    );
                    history_table.push_str(&format!(" {} |", link));
                } else {
                    history_table.push_str(" |");
                }
            }
            history_table.push('\n');
        }
        history_table.push_str("\n</details>");
    }

    // Generate Metadata JSON
    let parent_val = parent_id
        .map(|s| format!("\"{}\"", s))
        .unwrap_or("null".to_string());
    let child_val = child_id
        .map(|s| format!("\"{}\"", s))
        .unwrap_or("null".to_string());

    let meta_json = format!(
        r#"{{"id": "{}", "parent": {}, "child": {}}}"#,
        current_gherrit_id, parent_val, child_val
    );

    let meta_footer = format!(
        "<!-- WARNING: GHerrit relies on the following metadata to work properly. DO NOT EDIT OR REMOVE. -->\n<!-- gherrit-meta: {meta_json} -->",
    );

    let warning = "<!-- WARNING: This PR description is automatically generated by GHerrit. Any manual edits will be overwritten on the next push. -->";

    // Combine into final body
    let gh_pr_body_trailer = format!("{head_branch_markdown}{gh_pr_ids_markdown}");

    format!(
        "{warning}\n\n{body_clean}\n\n---\n\n{gh_pr_body_trailer}\n{history_table}\n{meta_footer}"
    )
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

    let commits = commits
        .into_iter()
        .scan(base_branch.to_string(), |parent_branch, c| {
            let parent = parent_branch.clone();
            *parent_branch = c.gherrit_id.clone();
            Some((c, parent))
        })
        .collect::<Vec<_>>();

    enum PrResolution {
        Existing(PrState),
        ToCreate(BatchCreate),
    }

    // 1. Identify existing PRs or queue for creation
    let resolutions: Vec<_> = commits
        .iter()
        .map(|(c, parent_branch)| {
            let pr_info = prs.iter().find(|pr| pr.head_branch == c.gherrit_id);

            if let Some(pr) = pr_info {
                log::debug!(
                    "Found existing PR #{} for {}",
                    pr.number.green().bold(),
                    c.gherrit_id
                );
                Ok(PrResolution::Existing(pr.clone()))
            } else {
                log::debug!(
                    "No GitHub PR exists for {}; queuing creation...",
                    c.gherrit_id
                );
                Ok(PrResolution::ToCreate(BatchCreate {
                    title: c.message_title.clone(),
                    body: c.message_body.clone(),
                    base_branch: parent_branch.clone(),
                    head_branch: c.gherrit_id.clone(),
                }))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    // 2. Batch create missing PRs
    let creations = resolutions.iter().filter_map(|r| match r {
        PrResolution::ToCreate(c) => Some(c),
        _ => None,
    });
    let num_creations = creations.clone().count();
    let new_prs = if num_creations > 0 {
        log::info!("Creating {num_creations} PRs...");
        let repo_id = fetch_repo_id(octocrab, &remote).await?;
        let created = batch_create_prs(octocrab, &repo_id, creations.cloned()).await?;
        assert_eq!(created.len(), num_creations);
        log::info!("Created {num_creations} PRs.");
        created
    } else {
        HashMap::new()
    };

    // 3. Resolve final PR states
    let mut commit_pr_states = Vec::new();

    // We zip commits with resolutions. Since resolutions were built in order,
    // they match perfectly.
    for ((c, parent_branch), resolution) in commits.iter().zip(resolutions) {
        let pr_state = match resolution {
            PrResolution::Existing(state) => state,
            PrResolution::ToCreate(create) => {
                if let Some((number, url, node_id)) = new_prs.get(&create.head_branch) {
                    log::info!(
                        "Created PR #{}: {}",
                        number.green().bold(),
                        url.blue().underline()
                    );
                    PrState {
                        number: *number,
                        node_id: node_id.clone(),
                        title: Some(create.title),
                        body: Some(create.body),
                        base_branch: create.base_branch,
                        head_branch: create.head_branch, // Added this line
                        state: PullRequestState::Open,   // Assuming newly created PRs are open
                    }
                } else {
                    bail!("Failed to resolve created PR for {}", create.head_branch);
                }
            }
        };
        commit_pr_states.push((c, parent_branch, pr_state));
    }

    let is_private = is_private_stack(repo, branch_name);
    let head_branch_markdown = if !is_private {
        repo.head()
            .ok()
            .and_then(|head| head.try_into_referent())
            .and_then(|head_ref| {
                let (cat, short_name) = head_ref.inner.name.category_and_short_name()?;
                (cat == Category::LocalBranch).then(|| {
                    format!("This PR is on branch [{short_name}](../tree/{short_name}).\n\n")
                })
            })
            .unwrap_or("".to_string())
    } else {
        "".to_string()
    };

    let gh_pr_ids_markdown = commit_pr_states
        .iter()
        .rev()
        .map(|(_, _, pr_state)| format!("- #{}", pr_state.number))
        .collect::<Vec<_>>()
        .join("\n");

    let updates: Vec<BatchUpdate> = commit_pr_states
        .iter()
        .enumerate()
        .filter_map(|(i, (c, parent_branch, pr_state))| {
            let latest_version = latest_versions.get(&c.gherrit_id).copied().unwrap_or(1);

            let parent_gherrit_id = (i > 0).then(|| commit_pr_states[i - 1].0.gherrit_id.clone());
            let child_gherrit_id = (i < commit_pr_states.len() - 1)
                .then(|| commit_pr_states[i + 1].0.gherrit_id.clone());

            let body = generate_pr_body(
                c,
                &remote.pr_url(pr_state.number),
                &head_branch_markdown,
                &gh_pr_ids_markdown,
                latest_version,
                parent_branch,
                parent_gherrit_id.as_deref(),
                child_gherrit_id.as_deref(),
            );

            let pr_num = pr_state.number.green().bold().to_string();
            let pr_url = remote
                .pr_url(pr_state.number)
                .blue()
                .underline()
                .to_string();

            let body_changed = pr_state.body.as_ref().is_none_or(|b| {
                b.replace("\r\n", "\n").trim() != body.replace("\r\n", "\n").trim()
            });

            let changed = pr_state.base_branch != **parent_branch
                || pr_state.title != Some(c.message_title.clone())
                || body_changed;

            if changed {
                log::debug!("Queuing update for PR #{}", pr_num);
                log::info!("Queued update for PR #{}: {}", pr_num, pr_url);
                Some(BatchUpdate {
                    node_id: pr_state.node_id.clone(),
                    title: c.message_title.clone(),
                    body: body.clone(),
                    base_branch: parent_branch.to_string(),
                })
            } else {
                log::info!("PR #{} is up to date: {}", pr_num, pr_url);
                None
            }
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
        let message_body = message
            .body
            .map(|body| core::str::from_utf8(body).unwrap())
            .unwrap_or("")
            .to_string();
        let gherrit_id = {
            let re = gherrit_pr_id_re();
            let captures = re
                .captures(&message_body)
                .ok_or_else(|| eyre!("Commit {} missing gherrit-pr-id trailer", c.id))?;
            captures.get(1).unwrap().as_str().to_string()
        };

        Ok(Commit {
            id: c.id,
            gherrit_id,
            message_title,
            message_body,
        })
    }
}

re!(gherrit_pr_id_re, r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$");

/// A request to update an existing PR in a batch.
struct BatchUpdate {
    /// The global Node ID of the Pull Request (required for GraphQL mutations).
    node_id: String,
    title: String,
    body: String,
    base_branch: String,
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
    let response: serde_json::Value = octocrab
        .graphql(&query_body)
        .await
        .wrap_err("Failed to fetch repository ID")?;

    if let Some(errors) = response.get("errors") {
        log::error!("GraphQL errors: {}", errors);
        bail!("Failed to fetch repository ID: {:?}", errors);
    }

    let id = response
        .get("data")
        .and_then(|d| d.get("repository"))
        .and_then(|r| r.get("id"))
        .and_then(|id| id.as_str())
        .ok_or_else(|| eyre!("Required repository ID not found in response"))?;

    Ok(id.to_string())
}

/// Performs batched updates of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping updates into chunks
/// (default 50) and sending them as a single GraphQL operation.
async fn batch_update_prs(octocrab: &Octocrab, updates: Vec<BatchUpdate>) -> Result<()> {
    run_batched_graphql(
        octocrab,
        "mutation",
        updates,
        |update| {
            // TODO: In a follow-up PR, add a macro which does this
            // automatically (takes a format string and then json-ifies all
            // format arguments before formatting).
            let node_id = json!(update.node_id);
            let title = json!(update.title);
            let body = json!(update.body);
            let base = json!(update.base_branch);
            format!(
                "updatePullRequest(input: {{pullRequestId: {node_id}, baseRefName: {base}, title: {title}, body: {body}}}) {{ clientMutationId }}\n"
            )
        },
        |_, _| Ok(()),
    )
    .await
}

/// Performs batched creation of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping creations into chunks
/// (default 50) and sending them as a single GraphQL operation.
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
        "mutation",
        creations_list.clone(),
        |create| {
            let repo_id = json!(repo_id);
            let base = json!(create.base_branch);
            let head = json!(create.head_branch);
            let title = json!(create.title);
            let body = json!(create.body);
            format!(
                "createPullRequest(input: {{ repositoryId: {repo_id}, baseRefName: {base}, headRefName: {head}, title: {title}, body: {body} }}) {{ pullRequest {{ number, url, id }} }}"
            )
        },
        |create, val| {
             let node_id = val["pullRequest"]["id"].as_str().ok_or_else(|| eyre!("Missing id"))?.to_string();
             let number = val["pullRequest"]["number"].as_u64().ok_or_else(|| eyre!("Missing number"))?;
             let url = val["pullRequest"]["url"].as_str().ok_or_else(|| eyre!("Missing url"))?.to_string();

             created_prs.insert(create.head_branch.clone(), (number, url, node_id));
             Ok(())
        }
    ).await?;

    Ok(created_prs)
}

async fn batch_fetch_prs(
    repo: &util::Repo,
    octocrab: &Octocrab,
    head_refs: &[String],
) -> Result<Vec<PrState>> {
    let remote = repo.default_remote()?;
    let owner = serde_json::to_string(&remote.owner)?;
    let repo_name = serde_json::to_string(&remote.repo_name)?;

    let mut all_prs = Vec::new();

    run_batched_graphql(
        octocrab,
        "query",
        head_refs,
        |head_ref| {
             let safe_head = json!(head_ref);
             format!(
                "repository(owner: {owner}, name: {repo_name}) {{ pullRequests(headRefName: {safe_head}, first: 1, states: [OPEN, CLOSED, MERGED]) {{ nodes {{ number, id, title, body, baseRefName, state }} }} }}"
            )
        },
        |head_ref, val| {
             if let Some(nodes) = val["pullRequests"]["nodes"].as_array()
                && let Some(node) = nodes.first()
            {
                    let number = node["number"]
                        .as_u64()
                        .ok_or_else(|| eyre!("Missing number"))?;
                    let id = node["id"].as_str().ok_or_else(|| eyre!("Missing id"))?;

                    // Use serde_json::from_value for cleaner enum deserialization
                    let state: PullRequestState = serde_json::from_value(node["state"].clone())
                        .wrap_err("Failed to parse PR state")?;

                    all_prs.push(PrState {
                        number,
                        node_id: id.to_string(),
                        title: node["title"].as_str().map(ToString::to_string),
                        body: node["body"].as_str().map(ToString::to_string),
                        base_branch: node["baseRefName"].as_str().unwrap_or_default().to_string(),
                        head_branch: head_ref.to_string(),
                        state,
                    });
            }
            Ok(())
        }
    ).await?;

    Ok(all_prs)
}

/// Executes batched GraphQL operations (queries or mutations).
///
/// Iterates over items in chunks of 50, builds a combined query string using
/// `query_builder`, and processes the response using `response_handler`.
async fn run_batched_graphql<T, M, H>(
    octocrab: &Octocrab,
    operation_type: &str,
    items: impl IntoIterator<Item = T>,
    query_builder: M,
    mut response_handler: H,
) -> Result<()>
where
    M: Fn(&T) -> String,
    H: FnMut(&T, &serde_json::Value) -> Result<()>,
{
    let alias = |i| format!("op{i}");
    for chunk in items.into_iter().chunks(50).into_iter() {
        let chunk: Vec<_> = chunk.collect();
        let query_body: String = chunk
            .iter()
            .enumerate()
            .map(|(i, item)| format!("{}: {}", alias(i), query_builder(item)))
            .collect();

        let query = format!("{operation_type} {{ {query_body} }}");
        let query_body = json!({ "query": query });
        let response: serde_json::Value = octocrab
            .graphql(&query_body)
            .await
            .wrap_err("GraphQL batched operation failed")?;

        if let Some(errors) = response.get("errors") {
            log::error!("GraphQL errors: {}", errors);
            bail!("GraphQL errors: {:?}", errors);
        }

        let data = response
            .get("data")
            .ok_or_else(|| eyre!("No data in response"))?;

        for (i, item) in chunk.iter().enumerate() {
            if let Some(op_data) = data.get(alias(i)) {
                response_handler(item, op_data)?;
            }
        }
    }
    Ok(())
}

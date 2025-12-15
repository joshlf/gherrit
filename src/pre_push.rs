use core::str;
use std::{collections::HashMap, process::Stdio, time::Instant};

use eyre::{Context, Result, bail, eyre};
use gix::{ObjectId, reference::Category, refs::transaction::PreviousValue};
use owo_colors::OwoColorize;
use serde_json::json;

use crate::{
    re,
    util::{self, CommandExt as _, HeadState},
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

    let latest_versions = push_to_origin(repo, &commits)?;
    let token = util::get_github_token()?;
    let mut builder = octocrab::Octocrab::builder().personal_token(token);

    // TODO: Only support this in development so we don't introduce a security
    // risk for users in prod.
    if let Ok(api_url) = std::env::var("GHERRIT_GITHUB_API_URL") {
        log::warn!("Using custom GitHub API URL: {}", api_url);
        builder = builder.base_uri(api_url)?;
    }

    let octocrab = builder.build()?;

    sync_prs(repo, &octocrab, branch_name, commits, latest_versions).await
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
            "The branch '{}' is not based on '{}'.\n\
             GHerrit only supports stacked branches that share history with the default branch.",
            branch_name,
            default_branch
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
    octocrab: &octocrab::Octocrab,
    branch_name: &str,
    commits: Vec<Commit>,
    latest_versions: HashMap<String, usize>,
) -> Result<()> {
    // Determine owner/repo from remote URL
    // Determine owner/repo from remote URL
    let remote_name = repo.default_remote_name();
    let remote_url = repo
        .config_string(&format!("remote.{}.url", remote_name))?
        .ok_or_else(|| eyre!("Remote '{}' missing URL", remote_name))?;
    let (owner, repo_name) = util::get_repo_owner_name(&remote_url)?;

    let prs_page = octocrab
        .pulls(&owner, &repo_name)
        .list()
        .state(octocrab::params::State::Open)
        .per_page(100)
        .send()
        .await?;

    let prs = prs_page.items;

    let commits = commits
        .into_iter()
        .scan("main".to_string(), |parent_branch, c| {
            let parent = parent_branch.clone();
            *parent_branch = c.gherrit_id.clone();
            Some((c, parent))
        })
        .collect::<Vec<_>>();

    let mut commit_pr_states = Vec::new();
    for (c, parent_branch) in commits {
        let pr_info = prs.iter().find(|pr| pr.head.ref_field == c.gherrit_id);

        let pr_state = if let Some(pr) = pr_info {
            log::debug!(
                "Found existing PR #{} for {}",
                pr.number.green().bold(),
                c.gherrit_id
            );
            PrState {
                number: pr.number.try_into().unwrap(),
                url: pr
                    .html_url
                    .clone()
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
                node_id: pr.node_id.clone().unwrap_or_default(),
                title: pr.title.clone().unwrap_or_default(),
                body: pr.body.clone().unwrap_or_default(),
                base: pr.base.ref_field.clone(),
            }
        } else {
            log::debug!("No GitHub PR exists for {}; creating one...", c.gherrit_id);
            let (num, url, node_id) = create_gh_pr(
                octocrab,
                &owner,
                &repo_name,
                &parent_branch,
                &c.gherrit_id,
                &c.message_title,
                &c.message_body,
            )
            .await?;

            log::info!(
                "Created PR #{}: {}",
                num.green().bold(),
                url.blue().underline()
            );
            PrState {
                number: num,
                url,
                node_id,
                title: c.message_title.clone(),
                body: c.message_body.clone(),
                base: parent_branch.clone(),
            }
        };
        commit_pr_states.push((c, parent_branch, pr_state));
    }

    let is_private = is_private_stack(repo, branch_name);

    let repo_url = commit_pr_states
        .first()
        .map(|(_, _, pr_state)| pr_state.url.split("/pull/").next().unwrap_or(""))
        .unwrap_or("")
        .to_string();

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

    let mut updates = Vec::new();

    for (i, (c, parent_branch, pr_state)) in commit_pr_states.iter().enumerate() {
        let latest_version = latest_versions.get(&c.gherrit_id).copied().unwrap_or(1);

        let parent_gherrit_id = (i > 0).then(|| commit_pr_states[i - 1].0.gherrit_id.clone());
        let child_gherrit_id =
            (i < commit_pr_states.len() - 1).then(|| commit_pr_states[i + 1].0.gherrit_id.clone());

        let body = generate_pr_body(
            c,
            &repo_url,
            &head_branch_markdown,
            &gh_pr_ids_markdown,
            latest_version,
            parent_branch,
            parent_gherrit_id.as_deref(),
            child_gherrit_id.as_deref(),
        );

        let pr_num = pr_state.number.green().bold().to_string();
        let pr_url = pr_state.url.blue().underline().to_string();

        let mut changed = false;
        if pr_state.base != *parent_branch {
            changed = true;
        }
        if pr_state.title != c.message_title {
            changed = true;
        }
        if pr_state.body.replace("\r\n", "\n").trim() != body.replace("\r\n", "\n").trim() {
            changed = true;
        }

        if changed {
            log::debug!("Queuing update for PR #{}", pr_num);
            updates.push(BatchUpdate {
                node_id: pr_state.node_id.clone(),
                title: c.message_title.clone(),
                body: body.clone(),
                base: parent_branch.clone(),
            });
            log::info!("Queued update for PR #{}: {}", pr_num, pr_url);
        } else {
            log::info!("PR #{} is up to date: {}", pr_num, pr_url);
        }
    }

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

#[derive(Debug, Clone)]
struct PrState {
    number: usize,
    url: String,
    node_id: String,
    title: String,
    body: String,
    base: String,
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

async fn create_gh_pr(
    octocrab: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    base_branch: &str,
    head_branch: &str,
    title: &str,
    body: &str,
) -> Result<(usize, String, String)> {
    let pr = octocrab
        .pulls(owner, repo)
        .create(title, head_branch, base_branch)
        .body(body)
        .send()
        .await
        .wrap_err("Failed to create PR")?;

    let number = pr.number.try_into()?; // octocrab uses u64, we us usize
    let url = pr.html_url.map(|u| u.to_string()).unwrap_or_default();
    Ok((number, url, pr.node_id.unwrap_or_default()))
}

re!(gherrit_pr_id_re, r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$");

struct BatchUpdate {
    node_id: String,
    title: String,
    body: String,
    base: String,
}

/// Perform batched updates of Pull Requests using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping updates into chunks
/// (default 50) and sending them as a single aliased mutation.
async fn batch_update_prs(octocrab: &octocrab::Octocrab, updates: Vec<BatchUpdate>) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    // Chunking by 50 to avoid complexity limits
    for chunk in updates.chunks(50) {
        let mut mutation_body = String::new();
        for (i, update) in chunk.iter().enumerate() {
            let safe_title = json!(update.title);
            let safe_body = json!(update.body);
            let safe_base = json!(update.base);

            mutation_body.push_str(&format!(
                "update{i}: updatePullRequest(input: {{pullRequestId: \"{node_id}\", baseRefName: {safe_base}, title: {safe_title}, body: {safe_body}}}) {{ clientMutationId }}\n",
                node_id = update.node_id,
                safe_base = safe_base,
                safe_title = safe_title,
                safe_body = safe_body
            ));
        }

        let query = format!("mutation {{ {} }}", mutation_body);
        let query_body = json!({ "query": query });
        let response: serde_json::Value = octocrab
            .graphql(&query_body)
            .await
            .wrap_err("GraphQL batch update failed")?;

        if let Some(errors) = response.get("errors") {
            log::error!("Batch update errors: {}", errors);
            bail!("Batch update failed: {:?}", errors);
        }
    }
    Ok(())
}

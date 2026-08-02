use std::{
    collections::HashMap,
    fmt::{self, Write},
    process::Stdio,
    str,
};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::{ObjectId, reference::Category, refs::transaction::PreviousValue};
use octocrab::Octocrab;
use owo_colors::OwoColorize;

use crate::{
    re,
    util::{self, CommandExt as _, HeadState},
};

mod batching;
mod github;
mod publication;
mod reconcile;

use batching::{
    BatchPlan, INITIAL_GRAPHQL_BATCH_LEN, MAX_GRAPHQL_QUERY_BYTES, ResponseDisposition,
    classify_response, query_exceeds_limit,
};
use github::{
    BatchedOperation, CreatePullRequest, CreatedPullRequest, FindPullRequest,
    PullRequest as PrState, PullRequestState, RepositoryIdQuery, UpdatePullRequest, batch_document,
    decode_batch_response,
};
use publication::{PushTarget, plan_push, push_batches, remote_query_batches};
use reconcile::{CurrentPr, DesiredPr, PrUpdate, link_stack, plan_update};

pub async fn run(repo: &util::Repo, github_api_url: Option<&str>) -> Result<()> {
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

    // A custom endpoint is an explicit dependency supplied by the caller. The
    // production binary always passes `None`, so an environment variable
    // cannot redirect a user's token.
    if let Some(api_url) = github_api_url {
        log::warn!("Using custom GitHub API URL: {}", api_url);
        builder = builder.base_uri(api_url)?;
    }

    let octocrab = builder.build()?;

    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let prs = batch_fetch_prs(repo, &octocrab, &gherrit_ids).await?;

    let errors: Vec<String> = prs.iter().filter_map(|pr| match pr.state {
        PullRequestState::Open => None,
        PullRequestState::Closed => Some((pr.number, "closed")),
        PullRequestState::Merged => Some((pr.number, "merged")),
    }).map(|(number, state)| {
        format!(
            "Cannot push to {state} PR #{number}. Please open a new PR or reopen the existing one."
        )
    }).collect();

    if !errors.is_empty() {
        bail!(
            "{}\nYou may want to rebase on the latest changes before pushing.",
            errors.join("\n")
        );
    }

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

    let remote = repo.default_remote_name();
    commits
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
        .collect()
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
            let expected_sha = remote_branch_states
                .get(&c.gherrit_id)
                .map(|s| s.as_deref().unwrap_or(""))
                .unwrap_or("");

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

#[allow(clippy::type_complexity)]
fn get_remote_branch_states(
    repo: &util::Repo,
    gherrit_ids: &[String],
) -> Result<HashMap<String, Option<String>>> {
    let mut states: HashMap<String, Option<String>> = HashMap::new();
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
                    let created = new_prs.get(&create.head_branch).ok_or_else(|| {
                        eyre::eyre!("Failed to resolve created PR for {}", create.head_branch)
                    })?;
                    log::info!(
                        "Created PR #{}: {}",
                        created.number.green().bold(),
                        created.url.blue().underline()
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

    let head_branch_markdown = (!is_private_stack(repo, branch_name))
        .then(|| {
            let head_ref = repo.head().ok()?.try_into_referent()?;
            let (cat, short_name) = head_ref.inner.name.category_and_short_name()?;
            (cat == Category::LocalBranch)
                .then(|| format!("This PR is on branch [{short_name}](../tree/{short_name}).\n\n"))
        })
        .flatten();

    let repo_url = remote.repo_url_relative();
    let updates: Vec<PrUpdate> = commit_pr_states
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
            let re = gherrit_pr_id_re();
            let captures = re
                .captures(&message_body)
                .ok_or_else(|| eyre!("Commit {} missing gherrit-pr-id trailer", c.id))?;
            captures.get(1).unwrap().as_str().to_string()
        };

        Ok(Commit { id: c.id, gherrit_id, message_title, message_body })
    }
}

re!(gherrit_pr_id_re, r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$");

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
    let response: serde_json::Value =
        octocrab.graphql(&request).await.wrap_err("Failed to fetch repository ID")?;
    query.decode(response)
}

/// Performs batched updates of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping updates into
/// adaptive batches and sending each batch as one GraphQL operation.
async fn batch_update_prs(octocrab: &Octocrab, updates: Vec<PrUpdate>) -> Result<()> {
    let updates = updates.into_iter().map(|update| {
        UpdatePullRequest::new(update.node_id, update.title, update.body, update.base_branch)
    });
    run_batched_graphql(octocrab, updates).await?;
    Ok(())
}

/// Performs batched creation of PRs using GitHub's GraphQL API.
///
/// This avoids rate limits and network latency by grouping creations into
/// adaptive batches and sending each batch as one GraphQL operation.
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
    Ok(run_batched_graphql(octocrab, creations)
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

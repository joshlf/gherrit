use core::str;
use std::{collections::HashMap, process::Stdio, time::Instant};

use gix::{ObjectId, reference::Category, refs::transaction::PreviousValue};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    cmd, manage, re,
    util::{self, HeadState},
};
use eyre::{Context, Result, bail, eyre};
use owo_colors::OwoColorize;

pub fn run(repo: &util::Repo) -> Result<()> {
    let t0 = Instant::now();

    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Pending(bn) => bn,
        HeadState::Detached => {
            bail!("Cannot push from detached HEAD");
        }
    };

    check_managed_state(repo, branch_name)?;

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

    sync_prs(repo, branch_name, commits, latest_versions)
}

// Check if the branch is managed by GHerrit.
fn check_managed_state(repo: &util::Repo, branch_name: &str) -> Result<()> {
    let state = manage::get_state(repo, branch_name).wrap_err("Failed to parse gherritManaged")?;

    match state {
        Some(manage::State::Unmanaged) => {
            log::info!(
                "Branch '{}' is UNMANAGED. Allowing standard push.",
                branch_name
            );
            return Ok(()); // Allow standard push
        }
        Some(manage::State::Managed) => {
            log::info!("Branch '{}' is MANAGED. Syncing stack...", branch_name);
        } // Proceed
        None => {
            bail!(
                "It is unclear if branch '{branch_name}' should be a Stack.\n\
                Run 'gherrit manage' to sync it as a Stack.\n\
                Run 'gherrit unmanage' to push it as a standard Git branch."
            );
        }
    }
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

    commits.into_iter().map(|c| c.try_into()).collect()
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

            // [Clean Up Old Versions]
            // Iterate over all references to find and delete old tags for this specific ID.
            let prefix = format!("refs/tags/gherrit/{gherrit_id}/");

            // Note: We use manual filtering as established in get_local_version
            if let Ok(references) = repo.references().map_err(|e| eyre!(e)) {
                if let Ok(iter) = references.all().map_err(|e| eyre!(e)) {
                    for reference in iter.filter_map(Result::ok) {
                        let name = reference.name().as_bstr().to_string();

                        if let Some(suffix) = name.strip_prefix(&prefix) {
                            if let Some(ver_str) = suffix.strip_prefix("v") {
                                if let Ok(old_ver) = ver_str.parse::<usize>() {
                                    // CRITICAL: Only delete if strictly older than the current version.
                                    if old_ver < ver {
                                        log::debug!("Cleaning up obsolete tag: {name}");
                                        // Best effort deletion
                                        if let Ok(r) = repo.find_reference(&name) {
                                            let _ = r.delete();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
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

        let output = util::cmd("git", args).output()?;
        let output = core::str::from_utf8(&output.stdout)?;

        for line in output.lines() {
            // Output format: "<SHA>\t<refname>"
            let Some((sha, ref_name)) = line.split_once('\t') else {
                continue;
            };

            // Match heads: refs/heads/<id>
            let head_re = re!(r"refs/heads/([a-zA-Z0-9]+)$");
            if let Some(caps) = head_re.captures(ref_name) {
                if let Some(id_match) = caps.get(1) {
                    let id = id_match.as_str().to_string();
                    states.insert(id, Some(sha.to_string()));
                }
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
            if let Some(ver_str) = name.rsplit('v').next() {
                if let Ok(ver) = ver_str.parse::<usize>() {
                    if ver > max_ver {
                        max_ver = ver;
                    }
                }
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

fn sync_prs(
    repo: &util::Repo,
    branch_name: &str,
    commits: Vec<Commit>,
    latest_versions: HashMap<String, usize>,
) -> Result<()> {
    let pr_list =
        cmd!("gh pr list --json number,headRefName,url,title,body,baseRefName").output()?;

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(rename_all = "camelCase")]
    struct ListEntry {
        head_ref_name: String,
        number: usize,
        url: String,
        title: String,
        body: String,
        base_ref_name: String,
    }

    let prs: Vec<ListEntry> = if pr_list.stdout.is_empty() {
        vec![]
    } else {
        match serde_json::from_slice(&pr_list.stdout) {
            Ok(prs) => prs,
            Err(err) => {
                let mut error = format!("failed to parse `gh` command output: {err}");
                if let Ok(stdout) = str::from_utf8(&pr_list.stdout) {
                    error += &format!("\ncommand output (verbatim):\n{stdout}");
                }
                bail!(error);
            }
        }
    };

    let commits = commits
        .into_iter()
        .scan("main".to_string(), |parent_branch, c| {
            let parent = parent_branch.clone();
            *parent_branch = c.gherrit_id.clone();
            Some((c, parent))
        })
        .collect::<Vec<_>>();

    let commits = commits
        .into_par_iter()
        .map(move |(c, parent_branch)| -> Result<_> {
            let pr_info = prs.iter().find(|pr| pr.head_ref_name == c.gherrit_id);

            let pr_state = if let Some(pr) = pr_info {
                log::debug!(
                    "Found existing PR #{} for {}",
                    pr.number.green().bold(),
                    c.gherrit_id
                );
                PrState {
                    number: pr.number,
                    url: pr.url.clone(),
                    title: pr.title.clone(),
                    body: pr.body.clone(),
                    base: pr.base_ref_name.clone(),
                }
            } else {
                log::debug!("No GitHub PR exists for {}; creating one...", c.gherrit_id);
                let (num, url) = create_gh_pr(
                    &parent_branch,
                    &c.gherrit_id,
                    &c.message_title,
                    &c.message_body,
                )?;

                log::info!(
                    "Created PR #{}: {}",
                    num.green().bold(),
                    url.blue().underline()
                );
                PrState {
                    number: num,
                    url,
                    title: c.message_title.clone(),
                    body: c.message_body.clone(),
                    base: parent_branch.clone(),
                }
            };

            Ok((c, parent_branch, pr_state))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    let is_private = is_private_stack(repo, branch_name);

    // Derive base repo URL safely from the first commit's PR URL.
    // Since `commits` is not empty (checked at the start of `run`), and
    // `create_gh_pr` always returns a valid URL, this is safe.
    let repo_url = commits
        .first()
        .map(|(_, _, pr_state)| pr_state.url.split("/pull/").next().unwrap_or(""))
        .unwrap_or("")
        .to_string();

    // Attempt to resolve `HEAD` to a branch name so that we can refer to it
    // in PR bodies. If we can't, then silently fail and just don't include
    // that information in PR bodies.
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

    // A markdown bulleted list of links to each PR, with the "top" PR (the
    // furthest from `main`) at the top of the list.
    let gh_pr_ids_markdown = commits
        .iter()
        .rev()
        .map(|(_, _, pr_state)| format!("- #{}", pr_state.number))
        .collect::<Vec<_>>()
        .join("\n");

    commits.par_iter().enumerate().try_for_each(
        |(i, (c, parent_branch, pr_state))| -> Result<()> {
            let latest_version = latest_versions.get(&c.gherrit_id).copied().unwrap_or(1);

            // Determine parent and child IDs
            let parent_gherrit_id = (i > 0).then(|| commits[i - 1].0.gherrit_id.clone());
            let child_gherrit_id =
                (i < commits.len() - 1).then(|| commits[i + 1].0.gherrit_id.clone());

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

            edit_gh_pr(pr_state, parent_branch, &c.message_title, &body)?;
            Ok(())
        },
    )?;
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
    title: String,
    body: String,
    base: String,
}

impl<'a> TryFrom<gix::Commit<'a>> for Commit {
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

fn create_gh_pr(
    base_branch: &str,
    head_branch: &str,
    title: &str,
    body: &str,
) -> Result<(usize, String)> {
    let output = cmd!(
        "gh pr create --base",
        base_branch,
        "--head",
        head_branch,
        "--title",
        title,
        "--body",
        body,
    )
    .stderr(Stdio::inherit())
    .output()?;

    let output = core::str::from_utf8(&output.stdout)?;
    let re = re!("https://github.com/[a-zA-Z0-9]+/[a-zA-Z0-9]+/pull/([0-9]+)");
    let captures = re.captures(output).unwrap();
    let pr_id = captures.get(1).unwrap();
    let pr_url = output.trim().to_string();
    Ok((pr_id.as_str().parse()?, pr_url))
}

fn edit_gh_pr(state: &PrState, new_base: &str, new_title: &str, new_body: &str) -> Result<()> {
    let pr_num = state.number.to_string();
    let mut args = vec!["pr", "edit", &pr_num];
    let mut changed = false;

    if state.base != new_base {
        args.push("--base");
        args.push(new_base);
        changed = true;
    }

    if state.title != new_title {
        args.push("--title");
        args.push(new_title);
        changed = true;
    }

    if state.body.replace("\r\n", "\n").trim() != new_body.replace("\r\n", "\n").trim() {
        args.push("--body");
        args.push(new_body);
        changed = true;
    }

    let pr_num = state.number.green().bold().to_string();
    let pr_url = state.url.blue().underline().to_string();
    if !changed {
        log::info!("PR #{pr_num} is up to date: {pr_url}");
    } else {
        log::debug!("Updating PR #{pr_num}...");
        util::cmd("gh", args).stdout(Stdio::null()).status()?;
        log::info!("Updated PR #{pr_num}: {pr_url}");
    }

    Ok(())
}

re!(gherrit_pr_id_re, r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$");

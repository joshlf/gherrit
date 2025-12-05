use core::str;
use std::{
    collections::HashMap,
    error::Error,
    process::{ExitStatus, Stdio},
    time::Instant,
};

use gix::{reference::Category, refs::transaction::PreviousValue, ObjectId};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    cmd, manage, re,
    util::{self, CommandExt as _, HeadState, ResultExt as _},
};

pub fn run(repo: &util::Repo) {
    let t0 = Instant::now();

    let branch_name = repo.current_branch();
    let branch_name = match branch_name {
        HeadState::Attached(bn) | HeadState::Rebasing(bn) => bn,
        HeadState::Detached => {
            log::error!("Cannot push from detached HEAD");
            std::process::exit(1);
        }
    };

    check_managed_state(repo, branch_name);

    let commits = collect_commits(repo).unwrap_or_exit("Failed to collect commits");

    let t1 = Instant::now();
    log::trace!("t0 -> t1: {:?}", t1 - t0);

    let commits = create_gherrit_refs(repo, commits).unwrap_or_exit("Failed to create refs");

    let t2 = Instant::now();
    log::trace!("t1 -> t2: {:?}", t2 - t1);

    if commits.is_empty() {
        log::info!("No commits to sync.");
        return;
    }

    let latest_versions = push_to_origin(&commits);

    sync_prs(repo, branch_name, commits, latest_versions);
}

// TODO: Maybe this should return a Result instead of bailing from inside?
fn check_managed_state(repo: &util::Repo, branch_name: &str) {
    let state =
        manage::get_state(repo, branch_name).unwrap_or_exit("Failed to parse gherritManaged");

    match state {
        Some(manage::State::Unmanaged) => {
            log::info!(
                "Branch '{}' is UNMANAGED. Allowing standard push.",
                branch_name
            );
            std::process::exit(0); // Allow standard push
        }
        Some(manage::State::Managed) => {
            log::info!("Branch '{}' is MANAGED. Syncing stack...", branch_name);
        } // Proceed
        None => {
            log::error!(
                "It is unclear if branch '{}' should be a Stack.",
                branch_name
            );
            log::error!("Run 'gherrit manage' to sync it as a Stack.");
            log::error!("Run 'gherrit unmanage' to push it as a standard Git branch.");
            std::process::exit(1);
        }
    }
}

fn collect_commits(repo: &util::Repo) -> Result<Vec<Commit>, Box<dyn Error>> {
    let head = repo.rev_parse_single("HEAD")?;
    let main = repo.rev_parse_single("main")?;
    let mut commits = repo
        .rev_walk([head])
        .all()?
        .take_while(|res| res.as_ref().map(|info| info.id != main).unwrap_or(true))
        .map(|res| -> Result<_, Box<dyn Error>> {
            res.map_err::<Box<dyn Error>, _>(|e| Box::new(e))
                .and_then(|info| info.object().map_err::<Box<dyn Error>, _>(|e| Box::new(e)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    commits.reverse();

    commits
        .into_iter()
        .map(|c| c.try_into())
        .collect::<Result<Vec<_>, _>>()
}

fn create_gherrit_refs(
    repo: &util::Repo,
    commits: Vec<Commit>,
) -> Result<Vec<Commit>, Box<dyn Error>> {
    commits
        .into_iter()
        .map(|c| -> Result<_, Box<dyn Error>> {
            let rf = format!("refs/gherrit/{}", c.gherrit_id);
            let _ = repo.reference(rf, c.id, PreviousValue::Any, "")?;
            Ok(c)
        })
        .collect::<Result<Vec<_>, _>>()
}

fn push_to_origin(commits: &[Commit]) -> HashMap<String, usize> {
    let gherrit_ids: Vec<String> = commits.iter().map(|c| c.gherrit_id.clone()).collect();
    let remote_versions = get_remote_versions(&gherrit_ids).unwrap_or_else(|e| {
        log::warn!("Failed to fetch remote versions: {}", e);
        HashMap::new()
    });

    let mut args = vec![
        "push".to_string(),
        "--quiet".to_string(),
        "--no-verify".to_string(),
        // Use --force-with-lease to ensure we don't overwrite remote changes
        // that we haven't seen (i.e. if the remote ref has moved since we last fetched).
        "--force-with-lease".to_string(),
        // If any push fails, abort the entire push instead of leaving GitHub with refs
        // that don't correspond to any PR. Mostly commonly, this is caused by
        // --force-with-lease causing some refs to fail to push.
        "--atomic".to_string(),
        "origin".to_string(),
    ];

    let mut next_versions = HashMap::new();

    args.extend(commits.iter().flat_map(|c| {
        let versions = remote_versions
            .get(&c.gherrit_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        // Find the latest version
        let latest = versions.iter().max_by_key(|(v, _)| *v);

        if let Some((ver, sha)) = latest {
            if *sha == c.id.to_string() {
                log::info!("Commit {} already tagged as v{}", c.id, ver);
                next_versions.insert(c.gherrit_id.clone(), *ver);
                // Still push the branch ref
                return vec![format!("{}:refs/heads/{}", c.id, c.gherrit_id)];
            }
        }

        let next_version = latest.map(|(v, _)| *v).unwrap_or(0) + 1;
        next_versions.insert(c.gherrit_id.clone(), next_version);

        vec![
            format!("{}:refs/heads/{}", c.id, c.gherrit_id),
            format!(
                "{}:refs/tags/gherrit/{}/v{}",
                c.id, c.gherrit_id, next_version
            ),
        ]
    }));

    log::info!("Pushing {} commits and tags to origin...", commits.len());
    let mut child = util::cmd("git", args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Filter out the "Create a pull request" message from GitHub
    {
        use std::io::{BufRead, BufReader};
        let stderr = child.stderr.take().unwrap();
        let reader = BufReader::new(stderr);

        // Buffer for contiguous "remote:" lines
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
        log::error!("`git push` failed. You may need to `git pull --rebase`.");
        std::process::exit(1);
    }

    next_versions
}

#[allow(clippy::type_complexity)]
fn get_remote_versions(
    gherrit_ids: &[String],
) -> Result<HashMap<String, Vec<(usize, String)>>, Box<dyn Error>> {
    if gherrit_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut args = vec![
        "ls-remote".to_string(),
        "--tags".to_string(),
        "origin".to_string(),
    ];

    // Windows command line limit is ~32k chars. Each refspec is ~70 chars.
    // 50 * 70 = 3500 chars, which is ~10% of the limit, leaving plenty of room
    // for environment variables and other overhead.
    const MAX_SPECIFIC_REFSPECS: usize = 50;
    if gherrit_ids.len() > MAX_SPECIFIC_REFSPECS {
        // Fallback: Fetch ALL gherrit tags to avoid crashing the shell
        // with too many arguments.
        args.push("refs/tags/gherrit/*".to_string());
    } else {
        args.extend(
            gherrit_ids
                .iter()
                .map(|id| format!("refs/tags/gherrit/{id}/*")),
        );
    }

    let output = util::cmd("git", args).output()?;
    let output = core::str::from_utf8(&output.stdout)?;

    let mut versions: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    let re = re!(r"refs/tags/gherrit/([^/]+)/v(\d+)$");

    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let sha = parts[0].to_string();
        let ref_name = parts[1];
        if let Some(caps) = re.captures(ref_name) {
            if let (Some(id), Some(ver)) = (caps.get(1), caps.get(2)) {
                let id = id.as_str().to_string();
                if let Ok(ver) = ver.as_str().parse::<usize>() {
                    versions.entry(id).or_default().push((ver, sha));
                }
            }
        }
    }

    Ok(versions)
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

    // 1. Generate Patch History Table
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

    // 2. Generate Metadata JSON
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

    // 3. Combine
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
) {
    let pr_list = cmd!("gh pr list --json number,headRefName,url").unwrap_output();

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(rename_all = "camelCase")]
    struct ListEntry {
        head_ref_name: String,
        number: usize,
        url: String,
    }

    let prs: Vec<ListEntry> = if pr_list.stdout.is_empty() {
        vec![]
    } else {
        match serde_json::from_slice(&pr_list.stdout) {
            Ok(prs) => prs,
            Err(err) => {
                log::error!("failed to parse `gh` command output: {err}");
                if let Ok(stdout) = str::from_utf8(&pr_list.stdout) {
                    log::error!("command output (verbatim):\n{stdout}");
                }
                std::process::exit(2);
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
        .map(
            move |(c, parent_branch)| -> Result<_, Box<dyn Error + Send + Sync>> {
                let pr_info = prs.iter().find(|pr| pr.head_ref_name == c.gherrit_id);

                let (pr_num, pr_url) = if let Some(pr) = pr_info {
                    log::debug!("Found existing PR #{} for {}", pr.number, c.gherrit_id);
                    (pr.number, pr.url.clone())
                } else {
                    log::debug!("No GitHub PR exists for {}; creating one...", c.gherrit_id);
                    let (num, url) = create_gh_pr(
                        &parent_branch,
                        &c.gherrit_id,
                        &c.message_title,
                        &c.message_body,
                    )?;

                    log::info!("Created PR #{num}: {url}");
                    (num, url)
                };

                Ok((c, parent_branch, pr_num, pr_url))
            },
        )
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let is_private = is_private_stack(repo, branch_name);

    // Derive base repo URL safely from the first commit's PR URL.
    // Since `commits` is not empty (checked at the start of `run`), and `create_gh_pr`
    // always returns a valid URL, this is safe.
    let repo_url = commits
        .first()
        .map(|(_, _, _, url)| url.split("/pull/").next().unwrap_or(""))
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
        .map(|(_, _, pr_num, _)| format!("- #{pr_num}"))
        .collect::<Vec<_>>()
        .join("\n");

    commits
        .par_iter()
        .enumerate()
        .try_for_each(
            |(i, (c, parent_branch, pr_num, pr_url))| -> Result<(), Box<dyn Error + Send + Sync>> {
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

                log::debug!("Updating PR #{} description...", pr_num);
                log::info!("Updated PR #{}: {}", pr_num, pr_url);
                edit_gh_pr(*pr_num, parent_branch, &c.message_title, &body)?;
                Ok(())
            },
        )
        .unwrap();
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

impl<'a> TryFrom<gix::Commit<'a>> for Commit {
    type Error = Box<dyn Error>;

    fn try_from(c: gix::Commit) -> Result<Self, Self::Error> {
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
                .ok_or_else(|| format!("Commit {} missing gherrit-pr-id trailer", c.id))?;
            let gherrit_id = captures.get(1).unwrap().as_str().to_string();
            gherrit_id
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
) -> Result<(usize, String), Box<dyn Error + Send + Sync>> {
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

fn edit_gh_pr(
    pr_num: usize,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<ExitStatus, std::io::Error> {
    let pr_num = format!("{pr_num}");
    let mut c = cmd!(
        "gh pr edit",
        pr_num,
        "--base",
        base_branch,
        "--title",
        title,
        "--body",
        body
    );

    c.stdout(Stdio::null()).status()
}

re!(gherrit_pr_id_re, r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$");

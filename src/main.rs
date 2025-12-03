#![feature(iterator_try_collect, iter_intersperse)]

use core::str;
use std::{
    env,
    error::Error,
    ffi::OsStr,
    process::{Command, ExitStatus, Output, Stdio},
    sync::OnceLock,
    thread,
    time::Instant,
};

use gix::{reference::Category, refs::transaction::PreviousValue, ObjectId};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

static GHERRIT_PR_ID_RE: OnceLock<regex::Regex> = OnceLock::new();
static GH_PR_URL_RE: OnceLock<regex::Regex> = OnceLock::new();

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            let level = record.level();
            if level == log::Level::Info {
                writeln!(buf, "[gherrit] {}", record.args())
            } else {
                writeln!(buf, "[gherrit] [{}] {}", level, record.args())
            }
        })
        .init();

    let args: Vec<String> = env::args().collect();
    let args: Vec<_> = args.iter().map(|s| s.as_str()).collect();
    match args.as_slice() {
        [_, "pre-push"] => pre_push(),
        [_, "commit-msg"] => unimplemented!(),
        [_, "manage"] => manage(),
        [_, "unmanage"] => unmanage(),
        [_, "post-checkout", prev, new, flag] => post_checkout(prev, new, flag),
        _ => {
            eprintln!("Usage:");
            eprintln!("    {} pre-push", args[0]);
            eprintln!("    {} commit-msg", args[0]);
            eprintln!("    {} manage", args[0]);
            eprintln!("    {} unmanage", args[0]);
            eprintln!("    {} post-checkout <prev> <new> <flag>", args[0]);
            std::process::exit(1);
        }
    }
}

macro_rules! cmd {
    ($bin:expr $(, $($rest:tt)*)?) => {{
        let bin_str = $bin.to_string();
        let parts: Vec<&str> = bin_str.split_whitespace().collect();
        let (bin, pre_args) = match parts.as_slice() {
            [bin, args @ ..] => (bin, args),
            [] => panic!("Command cannot be empty"),
        };

        #[allow(unused_mut)]
        let mut args: Vec<String> = pre_args.iter().map(|s| s.to_string()).collect();
        cmd!(@inner args $(, $($rest)*)?);

        log::debug!("exec: {} {}", bin, args.iter().map(|s| if s.contains(" ") {
            format!("'{}'", s)
        } else {
            s.clone()
        }).collect::<Vec<_>>().join(" "));
        cmd(bin, &args)
    }};

    // 1. Parenthesized group: ($(...))
    (@inner $vec:ident, ($($fmt:tt)+) $(, $($rest:tt)*)?) => {
        $vec.push(format!($($fmt)+));
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    // 2. String literal
    (@inner $vec:ident, $l:literal $(, $($rest:tt)*)?) => {
        for s in $l.split_whitespace() {
            $vec.push(s.to_string());
        }
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    // 3. Expression
    (@inner $vec:ident, $e:expr $(, $($rest:tt)*)?) => {
        $vec.push($e.to_string());
        cmd!(@inner $vec $(, $($rest)*)?);
    };

    // Base cases
    (@inner $vec:ident $(,)?) => {};
}

fn manage() {
    let repo = gix::open(".").unwrap();
    let head_ref = repo.head().unwrap().try_into_referent().unwrap();
    let branch_name = head_ref.name().shorten();

    cmd!(
        "git config",
        ("branch.{}.gherritState", branch_name),
        "managed"
    )
    .unwrap_status();
    log::info!("Branch '{}' is now managed by GHerrit.", branch_name);
}

fn unmanage() {
    let repo = gix::open(".").unwrap();
    let head_ref = repo.head().unwrap().try_into_referent().unwrap();
    let branch_name = head_ref.name().shorten();

    cmd!(
        "git config",
        ("branch.{}.gherritState", branch_name),
        "unmanaged"
    )
    .unwrap_status();
    eprintln!("Branch '{}' is now unmanaged by GHerrit.", branch_name);
}

fn post_checkout(_prev: &str, _new: &str, flag: &str) {
    // Only run on branch switches (flag=1)
    if flag != "1" {
        return;
    }

    let repo = gix::open(".").unwrap();
    let head = repo.head().unwrap();

    let head_ref = match head.try_into_referent() {
        Some(referent) => referent,
        None => return, // We are in detached HEAD (e.g. during rebase); do nothing.
    };

    let branch_name = head_ref.name().shorten();

    // Idempotency check: Bail if the branch management state is already set.
    let config_output = cmd!("git config", ("branch.{}.gherritState", branch_name)).unwrap_output();
    if config_output.status.success() {
        log::debug!("Branch '{}' is already configured.", branch_name);
        return;
    }

    // Creation detection: Bail if we're just checking out an already-existing branch.
    let reflog_output = cmd!("git reflog show", branch_name, "-n1").unwrap_output();
    let reflog_stdout = String::from_utf8_lossy(&reflog_output.stdout);
    if !reflog_stdout.contains("branch: Created from") {
        log::debug!("Branch '{}' is not newly created.", branch_name);
        return;
    }

    let upstream_remote = cmd!("git config", ("branch.{}.remote", branch_name)).unwrap_output();

    let upstream_merge = cmd!("git config", ("branch.{}.merge", branch_name)).unwrap_output();

    let has_upstream = upstream_remote.status.success() && upstream_merge.status.success();
    let is_origin_main = if has_upstream {
        let remote = to_trimmed_string_lossy(&upstream_remote.stdout);
        let merge = to_trimmed_string_lossy(&upstream_merge.stdout);
        remote == "origin" && merge == "refs/heads/main"
    } else {
        false
    };

    if has_upstream && !is_origin_main {
        // Condition A: Shared Branch
        unmanage();
        log::info!("Branch initialized as UNMANAGED (Collaboration Mode).");
    } else {
        // Condition B: New Stack
        manage();
        log::info!("Branch initialized as MANAGED (Stack Mode).");
        log::info!("To opt-out, run: gherrit unmanage");
    }
}

fn pre_push() {
    let t0 = Instant::now();

    let repo = gix::open(".").unwrap();
    let head = repo.head().unwrap();
    let head_ref = head.try_into_referent().unwrap();
    let branch_name = head_ref.name().shorten();

    // Step 1: Resolve State
    let state = to_trimmed_string_lossy(
        &cmd!("git config --get", ("branch.{}.gherritState", branch_name),)
            .unwrap_output()
            .stdout,
    );

    match state.as_str() {
        "unmanaged" => {
            log::info!(
                "Branch '{}' is UNMANAGED. Allowing standard push.",
                branch_name
            );
            return; // Allow standard push
        }
        "managed" => {
            log::info!("Branch '{}' is MANAGED. Syncing stack...", branch_name);
        } // Proceed
        _ => {
            log::error!(
                "It is unclear if branch '{}' should be a Stack.",
                branch_name
            );
            log::error!("Run 'gherrit manage' to sync it as a Stack.");
            log::error!("Run 'gherrit unmanage' to push it as a standard Git branch.");
            std::process::exit(1);
        }
    }

    let head = repo.rev_parse_single("HEAD").unwrap();
    let main = repo.rev_parse_single("main").unwrap();
    let mut commits = repo
        .rev_walk([head])
        .all()
        .unwrap()
        .take_while(|res| res.as_ref().map(|info| info.id != main).unwrap_or(true))
        .map(|res| -> Result<_, Box<dyn Error>> {
            res.map_err::<Box<dyn Error>, _>(|e| Box::new(e))
                .and_then(|info| info.object().map_err::<Box<dyn Error>, _>(|e| Box::new(e)))
        })
        .try_collect::<Vec<_>>()
        .unwrap();
    commits.reverse();

    let t1 = Instant::now();
    log::trace!("t0 -> t1: {:?}", t1 - t0);

    let commits = commits
        .iter()
        .map(Commit::try_from_gix)
        .try_collect::<Vec<_>>()
        .unwrap();

    let t2 = Instant::now();
    log::trace!("t1 -> t2: {:?}", t2 - t1);

    let commits = commits
        .into_iter()
        .map(|c| -> Result<_, Box<dyn Error>> {
            let gherrit_id = c.gherrit_id;
            let rf = format!("refs/gherrit/{gherrit_id}");
            let _ = repo.reference(rf, c.id, PreviousValue::Any, "")?;
            Ok((c, gherrit_id))
        })
        .try_collect::<Vec<_>>()
        .unwrap();

    let t3 = Instant::now();
    log::trace!("t2 -> t3: {:?}", t3 - t2);

    if commits.is_empty() {
        log::info!("No commits to sync.");
        return;
    }

    let config_key = format!("branch.{}.gherritPrivate", branch_name);
    let config_output = cmd!("git config --get --bool", &config_key).unwrap_output();

    let is_private = if config_output.status.success() {
        // If config is set, respect it (true/false)
        to_trimmed_string_lossy(&config_output.stdout) == "true"
    } else {
        // If config is unset, DEFAULT TO TRUE (Private)
        true
    };

    let mut args = vec![
        "push".to_string(),
        "--quiet".to_string(),
        "--no-verify".to_string(),
        // Use --force-with-lease to ensure we don't overwrite remote changes
        // that we haven't seen (i.e. if the remote ref has moved since we last fetched).
        "--force-with-lease".to_string(),
        "origin".to_string(),
    ];
    args.extend(
        commits
            .iter()
            .map(|(c, gherrit_id)| format!("{}:refs/heads/{gherrit_id}", c.id)),
    );

    log::info!("Pushing {} commits to origin...", commits.len());
    let mut child = cmd("git", args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Filter out the "Create a pull request" message from GitHub:
    //
    //   remote:
    //   remote: Create a pull request for 'G7a4e64a53733779e8f32b7258d5083e5b15ea91d' on GitHub by visiting:
    //   remote:      https://github.com/joshlf/gherrit/pull/new/G7a4e64a53733779e8f32b7258d5083e5b15ea91d
    //   remote:
    //
    // We use a multi-line regex to match this block specifically.
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
            static RE: OnceLock<regex::Regex> = OnceLock::new();
            let re = RE.get_or_init(|| {
                regex::Regex::new(r"(?m)\n?^remote:\s*\nremote: Create a pull request for '.*' on GitHub by visiting:\s*\nremote:\s*https://github\.com/.*\nremote:\s*$").unwrap()
            });

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

    let pr_list = cmd!("gh pr list --json number,headRefName,url").unwrap_output();

    let t4 = Instant::now();
    log::trace!("t3 -> t4: {:?}", t4 - t3);

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
        .scan("main", |parent_branch, (c, gherrit_id)| {
            let parent = *parent_branch;
            *parent_branch = gherrit_id;
            Some((c, parent, gherrit_id))
        })
        .collect::<Vec<_>>();

    let commits = commits
        .into_par_iter()
        .map(
            move |(c, parent_branch, gherrit_id)| -> Result<_, Box<dyn Error + Send + Sync>> {
                let pr_info = prs.iter().find(|pr| pr.head_ref_name == gherrit_id);

                let (pr_num, pr_url) = if let Some(pr) = pr_info {
                    log::debug!("Found existing PR #{} for {}", pr.number, gherrit_id);
                    (pr.number, pr.url.clone())
                } else {
                    log::debug!("No GitHub PR exists for {gherrit_id}; creating one...");
                    // Note that the PR's body will soon be overwritten
                    // (when we add the Markdown links to other PRs).
                    // However, setting a reasonable default PR body makes
                    // sense here in case something crashes between here and
                    // there.
                    let (num, url) =
                        create_gh_pr(parent_branch, gherrit_id, c.message_title, c.message_body)?;

                    log::info!("Created PR #{num}: {url}");
                    (num, url)
                };

                Ok((c, parent_branch, pr_num, pr_url))
            },
        )
        .collect::<Vec<_>>()
        .into_iter()
        .try_collect::<Vec<_>>()
        .unwrap();

    let t5 = Instant::now();
    log::trace!("t4 -> t5: {:?}", t5 - t4);

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
        .intersperse("\n".to_string())
        .collect::<String>();

    let gh_pr_body_trailer = format!("{head_branch_markdown}{gh_pr_ids_markdown}");

    thread::scope(|s| -> Result<(), Box<dyn Error>> {
        let join_handles = commits
            .iter()
            .enumerate()
            .map(|(i, (c, parent_branch, pr_num, pr_url))| {
                let gh_pr_body_trailer = &gh_pr_body_trailer;

                // Determine parent and child IDs, which may be `None` at the
                // beginning or end of the list.
                let parent_gherrit_id = (i > 0).then(|| commits[i - 1].0.gherrit_id);
                let child_gherrit_id = (i < commits.len() - 1).then(|| commits[i + 1].0.gherrit_id);

                let current_gherrit_id = c.gherrit_id;
                let message_title = &c.message_title;
                let message_body = &c.message_body;

                s.spawn(move || -> Result<(), std::io::Error> {
                    let re = GHERRIT_PR_ID_RE.get_or_init(|| {
                        regex::Regex::new(r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$").unwrap()
                    });
                    let body = re.replace(message_body, "");

                    // 3. Generate Metadata JSON
                    // We generate a JSON object stored in an HTML comment.
                    let parent_val = parent_gherrit_id
                        .map(|s| format!("\"{}\"", s))
                        .unwrap_or("null".to_string());
                    let child_val = child_gherrit_id
                        .map(|s| format!("\"{}\"", s))
                        .unwrap_or("null".to_string());

                    let meta_json = format!(
                        r#"{{"id": "{}", "parent": {}, "child": {}}}"#,
                        current_gherrit_id, parent_val, child_val
                    );
                    // WARNING: Our "Rebase Stack" GitHub Action relies on the metadata
                    // footer being formatted exactly as-is. It is sensitive to whitespace
                    // and newlines. Do not change this format without also updating the
                    // action.
                    let meta_footer = format!(
                        "<!-- WARNING: GHerrit relies on the following metadata to work properly. DO NOT EDIT OR REMOVE. -->\n<!-- gherrit-meta: {meta_json} -->",
                    );

                    let body = format!("{body}\n\n---\n\n{gh_pr_body_trailer}\n{meta_footer}");

                    log::debug!("Updating PR #{} description...", pr_num);
                    log::info!("Updated PR #{}: {}", pr_num, pr_url);
                    edit_gh_pr(*pr_num, parent_branch, message_title, &body)?;
                    Ok(())
                })
            })
            .collect::<Vec<_>>();

        for handle in join_handles {
            let _: () = handle.join().unwrap()?;
        }

        let t6 = Instant::now();
        log::trace!("t5 -> t6: {:?}", t6 - t5);

        Ok(())
    })
    .unwrap();

    if is_private {
        log::info!("-------------------------------------------------------------------------");
        log::info!(" Stack successfully synchronized!");
        log::info!("");
        log::info!(" NOTE: Standard 'git push' was blocked to keep origin clean.");
        log::info!("       Your changes are already active on GitHub (via GHerrit refs).");
        log::info!("");
        log::info!(
            "       To enable pushing this branch to 'origin/{}':",
            branch_name
        );
        log::info!("       git config {} false", config_key);
        log::info!("-------------------------------------------------------------------------");

        // Exit with failure (1) to stop Git from proceeding with the standard push
        std::process::exit(1);
    }
}

struct Commit<'a> {
    id: ObjectId,
    gherrit_id: &'a str,
    message_title: &'a str,
    message_body: &'a str,
}

impl<'a> Commit<'a> {
    fn try_from_gix(c: &'a gix::Commit) -> Result<Commit<'a>, Box<dyn Error>> {
        let message = c.message()?;
        let message_title = core::str::from_utf8(message.title)?;
        let message_body = message
            .body
            .map(|body| core::str::from_utf8(body).unwrap())
            .unwrap_or("");
        let gherrit_id = {
            let re = GHERRIT_PR_ID_RE
                .get_or_init(|| regex::Regex::new(r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$").unwrap());
            let captures = re
                .captures(message_body)
                .ok_or_else(|| format!("Commit {} missing gherrit-pr-id trailer", c.id))?;
            let gherrit_id = captures.get(1).unwrap().as_str();
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

fn cmd<I: AsRef<OsStr>>(name: &str, args: impl IntoIterator<Item = I>) -> Command {
    let mut c = Command::new(name);
    c.args(args);
    c
}

trait CommandExt {
    fn unwrap_status(self) -> ExitStatus;

    fn unwrap_output(self) -> Output;
}

impl CommandExt for Command {
    fn unwrap_status(mut self) -> ExitStatus {
        self.status().unwrap()
    }

    fn unwrap_output(mut self) -> Output {
        self.output().unwrap()
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
    let re = GH_PR_URL_RE.get_or_init(|| {
        regex::Regex::new("https://github.com/[a-zA-Z0-9]+/[a-zA-Z0-9]+/pull/([0-9]+)").unwrap()
    });
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
        &pr_num,
        "--base",
        base_branch,
        "--title",
        title,
        "--body",
        body,
    );

    c.stdout(Stdio::null()).status()
}

fn to_trimmed_string_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

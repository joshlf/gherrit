#![feature(iterator_try_collect, iter_intersperse)]

use core::str;
use std::{
    env,
    error::Error,
    ffi::OsStr,
    process::{Command, ExitStatus, Stdio},
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

fn manage() {
    let repo = gix::open(".").unwrap();
    let head_ref = repo.head().unwrap().try_into_referent().unwrap();
    let branch_name = head_ref.name().shorten();

    cmd(
        "git",
        [
            "config",
            &format!("branch.{}.gherritState", branch_name),
            "managed",
        ],
    )
    .status()
    .unwrap();
    eprintln!("Branch '{}' is now managed by GHerrit.", branch_name);
}

fn unmanage() {
    let repo = gix::open(".").unwrap();
    let head_ref = repo.head().unwrap().try_into_referent().unwrap();
    let branch_name = head_ref.name().shorten();

    cmd(
        "git",
        [
            "config",
            &format!("branch.{}.gherritState", branch_name),
            "unmanaged",
        ],
    )
    .status()
    .unwrap();
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
    let config_output = cmd(
        "git",
        ["config", &format!("branch.{}.gherritState", branch_name)],
    )
    .output()
    .unwrap();
    if config_output.status.success() {
        return;
    }

    // Creation detection: Bail if we're just checking out an already-existing branch.
    let reflog_output = cmd(
        "git",
        ["reflog", "show", branch_name.to_string().as_str(), "-n1"],
    )
    .output()
    .unwrap();
    let reflog_stdout = String::from_utf8_lossy(&reflog_output.stdout);
    if !reflog_stdout.contains("branch: Created from") {
        return;
    }

    let upstream_remote = cmd(
        "git",
        ["config", "--get", &format!("branch.{}.remote", branch_name)],
    )
    .output()
    .unwrap();

    let upstream_merge = cmd(
        "git",
        ["config", "--get", &format!("branch.{}.merge", branch_name)],
    )
    .output()
    .unwrap();

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
        eprintln!("[gherrit] Branch initialized as UNMANAGED (Collaboration Mode).");
    } else {
        // Condition B: New Stack
        manage();
        eprintln!("[gherrit] Branch initialized as MANAGED (Stack Mode).");
        eprintln!("[gherrit] To opt-out, run: gherrit unmanage");
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
        &cmd(
            "git",
            [
                "config",
                "--get",
                &format!("branch.{}.gherritState", branch_name),
            ],
        )
        .output()
        .unwrap()
        .stdout,
    );

    match state.as_str() {
        "unmanaged" => return, // Allow standard push
        "managed" => {}        // Proceed
        _ => {
            eprintln!(
                "[gherrit] Error: It is unclear if branch '{}' should be a Stack.",
                branch_name
            );
            eprintln!("[gherrit] Run 'gherrit manage' to sync it as a Stack.");
            eprintln!("[gherrit] Run 'gherrit unmanage' to push it as a standard Git branch.");
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
    println!("t0 -> t1: {:?}", t1 - t0);

    let commits = commits
        .iter()
        .map(Commit::try_from_gix)
        .try_collect::<Vec<_>>()
        .unwrap();

    let t2 = Instant::now();
    println!("t1 -> t2: {:?}", t2 - t1);

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
    println!("t2 -> t3: {:?}", t3 - t2);

    let mut args = vec![
        "push".to_string(),
        "--quiet".to_string(),
        "--no-verify".to_string(),
        // Use --force-with-lease to ensure we don't overwrite remote changes
        // that we haven't seen (i.e. if the remote ref has moved since we last fetched).
        "--force-with-lease".to_string(),
        // Combined with --force-with-lease, --force-if-includes ensures that
        // we have locally integrated the remote changes we are overwriting.
        // This prevents overwriting work even if we have fetched the latest refs
        // but haven't actually merged/rebased them into our local branch.
        "--force-if-includes".to_string(),
        "origin".to_string(),
    ];
    args.extend(
        commits
            .iter()
            .map(|(c, gherrit_id)| format!("{}:refs/heads/{gherrit_id}", c.id)),
    );
    let status = cmd("git", args).status().unwrap();
    if !status.success() {
        eprintln!("Error: `git push` failed. You may need to `git pull --rebase`.");
        std::process::exit(1);
    }

    let pr_list = cmd("gh", ["pr", "list", "--json", "number,headRefName"])
        .output()
        .unwrap();

    let t4 = Instant::now();
    println!("t3 -> t4: {:?}", t4 - t3);

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(rename_all = "camelCase")]
    struct ListEntry {
        head_ref_name: String,
        number: usize,
    }

    // TODO: Be more robust: allow whitespace
    let prs: Vec<ListEntry> = if pr_list.stdout.is_empty() {
        vec![]
    } else {
        match serde_json::from_slice(&pr_list.stdout) {
            Ok(prs) => prs,
            Err(err) => {
                eprintln!("failed to parse `gh` command output: {err}");
                if let Ok(stdout) = str::from_utf8(&pr_list.stdout) {
                    eprintln!("command output (verbatim):\n{stdout}");
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
                let pr_num = prs
                    .iter()
                    .find_map(|pr| (pr.head_ref_name == gherrit_id).then_some(pr.number));
                let pr_num = if let Some(pr_num) = pr_num {
                    pr_num
                } else {
                    println!("No GitHub PR exists for {gherrit_id}; creating one...");
                    // Note that the PR's body will soon be overwritten
                    // (when we add the Markdown links to other PRs).
                    // However, setting a reasonable default PR body makes
                    // sense here in case something crashes between here and
                    // there.
                    let num =
                        create_gh_pr(parent_branch, gherrit_id, c.message_title, c.message_body)?;
                    // TODO: Print the full PR URL. Requires resolving the
                    // username/organization and repository name. Could also
                    // capture this from the `gh` command output - we
                    // already have a regex in `create_gh_pr` to do this in
                    // order to parse the PR number.
                    println!("Created PR #{num}");
                    num
                };

                Ok((c, parent_branch, pr_num))
            },
        )
        .collect::<Vec<_>>()
        .into_iter()
        .try_collect::<Vec<_>>()
        .unwrap();

    let t5 = Instant::now();
    println!("t4 -> t5: {:?}", t5 - t4);

    // Attempt to resolve `HEAD` to a branch name so that we can refer to it
    // in PR bodies. If we can't, then silently fail and just don't include
    // that information in PR bodies.
    let head_branch_markdown = repo
        .head()
        .ok()
        .and_then(|head| head.try_into_referent())
        .and_then(|head_ref| {
            let (cat, short_name) = head_ref.inner.name.category_and_short_name()?;
            (cat == Category::LocalBranch)
                .then(|| format!("This PR is on branch [{short_name}](../tree/{short_name}).\n\n"))
        })
        .unwrap_or("".to_string());

    // A markdown bulleted list of links to each PR, with the "top" PR (the
    // furthest from `main`) at the top of the list.
    let gh_pr_ids_markdown = commits
        .iter()
        .rev()
        .map(|(_, _, pr_num)| format!("- #{pr_num}"))
        .intersperse("\n".to_string())
        .collect::<String>();

    let gh_pr_body_trailer = format!("{head_branch_markdown}{gh_pr_ids_markdown}");

    thread::scope(|s| -> Result<(), Box<dyn Error>> {
        let join_handles = commits
            .into_iter()
            .map(|(c, parent_branch, pr_num)| {
                let gh_pr_body_trailer = &gh_pr_body_trailer;
                s.spawn(move || -> Result<(), std::io::Error> {
                    let body = c.message_body;
                    let re = GHERRIT_PR_ID_RE.get_or_init(|| {
                        regex::Regex::new(r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$").unwrap()
                    });
                    let body = re.replace(body, "");

                    let body = format!("{body}\n\n---\n\n{gh_pr_body_trailer}");
                    edit_gh_pr(pr_num, parent_branch, c.message_title, &body)?;
                    Ok(())
                })
            })
            .collect::<Vec<_>>();

        for handle in join_handles {
            let _: () = handle.join().unwrap()?;
        }

        let t6 = Instant::now();
        println!("t5 -> t6: {:?}", t6 - t5);

        Ok(())
    })
    .unwrap();

    let config_key = format!("branch.{}.gherritPrivate", branch_name);
    let config_output = cmd("git", ["config", "--get", "--bool", &config_key])
        .output()
        .unwrap();

    let is_private = if config_output.status.success() {
        // If config is set, respect it (true/false)
        to_trimmed_string_lossy(&config_output.stdout) == "true"
    } else {
        // If config is unset, DEFAULT TO TRUE (Private)
        true
    };

    if is_private {
        eprintln!("-------------------------------------------------------------------------");
        eprintln!(" [gherrit] Stack successfully synchronized!");
        eprintln!("");
        eprintln!(" [gherrit] NOTE: Standard 'git push' was blocked to keep origin clean.");
        eprintln!("           Your changes are already active on GitHub (via GHerrit refs).");
        eprintln!("");
        eprintln!("           To enable pushing this branch to 'origin/{}':", branch_name);
        eprintln!("           git config {} false", config_key);
        eprintln!("-------------------------------------------------------------------------");
        
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
            // TODO: Return error here instead of unwrapping.
            let captures = re.captures(message_body).unwrap();
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

fn create_gh_pr(
    base_branch: &str,
    head_branch: &str,
    title: &str,
    body: &str,
) -> Result<usize, Box<dyn Error + Send + Sync>> {
    let output = cmd(
        "gh",
        [
            "pr",
            "create",
            "--base",
            base_branch,
            "--head",
            head_branch,
            "--title",
            title,
            "--body",
            body,
        ],
    )
    .stderr(Stdio::inherit())
    .output()?;

    let output = core::str::from_utf8(&output.stdout)?;
    let re = GH_PR_URL_RE.get_or_init(|| {
        regex::Regex::new("https://github.com/[a-zA-Z0-9]+/[a-zA-Z0-9]+/pull/([0-9]+)").unwrap()
    });
    let captures = re.captures(output).unwrap();
    let pr_id = captures.get(1).unwrap();
    Ok(pr_id.as_str().parse()?)
}

fn edit_gh_pr(
    pr_num: usize,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<ExitStatus, std::io::Error> {
    let pr_num = format!("{pr_num}");
    let mut c = cmd(
        "gh",
        [
            "pr",
            "edit",
            &pr_num,
            "--base",
            base_branch,
            "--title",
            title,
            "--body",
            body,
        ],
    );

    c.stdout(Stdio::null()).status()
}

fn to_trimmed_string_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#![feature(iterator_try_collect, iter_intersperse)]

use std::{
    env,
    error::Error,
    ffi::OsStr,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::Instant,
};

use gix::{
    reference::Category,
    refs::{transaction::PreviousValue, Target},
    ObjectId, Repository,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

const KEEP_FULL_HISTORY: bool = false;

fn main() {
    let args: Vec<String> = env::args().collect();
    let args: Vec<_> = args.iter().map(|s| s.as_str()).collect();
    match args.as_slice() {
        [_, "pre-push"] => pre_push(),
        [_, "commit-msg"] => unimplemented!(),
        _ => {
            eprintln!("Usage:");
            eprintln!("    {} pre-push", args[0]);
            eprintln!("    {} commit-msg", args[0]);
            std::process::exit(1);
        }
    }
}

fn pre_push() {
    // Since we call `git push` from this hook, we need to detect recursion and
    // bail.
    const VAR_NAME: &str = "GHERRIT_PRE_PUSH_EXECUTING";
    if env::var_os(VAR_NAME).is_some() {
        return;
    }
    env::set_var(VAR_NAME, "1");

    let t0 = Instant::now();

    let repo = gix::open(".").unwrap();
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

    if !KEEP_FULL_HISTORY {
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
            "--force".to_string(),
            "--quiet".to_string(),
            "origin".to_string(),
        ];
        args.extend(
            commits
                .iter()
                .map(|(c, gherrit_id)| format!("{}:refs/heads/{gherrit_id}", c.id)),
        );
        cmd("git", args).status().unwrap();

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

        let prs: Vec<ListEntry> = serde_json::from_slice(&pr_list.stdout).unwrap();

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
                        .find_map(|pr| (&pr.head_ref_name == gherrit_id).then_some(pr.number));
                    let pr_num = if let Some(pr_num) = pr_num {
                        pr_num
                    } else {
                        println!("No GitHub PR exists for {gherrit_id}; creating one...");
                        // Note that the PR's body will soon be overwritten
                        // (when we add the Markdown links to other PRs).
                        // However, setting a reasonable default PR body makes
                        // sense here in case something crashes between here and
                        // there.
                        create_gh_pr(&parent_branch, &gherrit_id, c.message_title, c.message_body)?
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
                (cat == Category::LocalBranch).then(|| {
                    format!("This PR is on branch [{short_name}](../tree/{short_name}).\n\n")
                })
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
                        // TODO: Only compile this once.
                        let re = regex::Regex::new(r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$").unwrap();
                        let body = re.replace(body, "");

                        let body = format!("{body}\n\n---\n\n{gh_pr_body_trailer}");
                        edit_gh_pr(pr_num, &parent_branch, c.message_title, &body)?;
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
    } else {
        // let commits = commits
        //     .into_iter()
        //     .map(|c| -> Result<_, Box<dyn Error>> {
        //         let gherrit_id = c.gherrit_id;
        //         let head_commit = repo.gherrit_head_commit(gherrit_id)?;
        //         Ok((c, gherrit_id, head_commit))
        //     })
        //     .try_collect::<Vec<_>>()
        //     .unwrap();

        // let commits = commits
        //     .into_iter()
        //     .map(
        //         |(c, gherrit_id, head_commit)| -> Result<_, Box<dyn Error>> {
        //             let rf = format!("refs/gherrit/{gherrit_id}");
        //             if let Some(head_commit) = head_commit {
        //                 // We already have an existing history, so we need to
        //                 // add to it.
        //                 //
        //                 // TODO: If nothing has changed about the commit
        //                 // (contents, message, parents, etc), then we can
        //                 // short-circuit and not create a new commit in this
        //                 // Gherrit PR's history.
        //                 let tree = c.inner.tree()?;
        //                 let message = core::str::from_utf8(c.inner.message_raw()?)?;

        //                 // TODO: `head_commit.id()` can panic.
        //                 let new_head = repo.commit(rf, message, tree.id, [head_commit.id()])?;
        //                 Ok((new_head, gherrit_id))
        //             } else {
        //                 // We don't have an existing history, so initialize the
        //                 // history starting with this commit.
        //                 let _ = repo.reference(rf, c.id, PreviousValue::Any, "")?;
        //                 Ok((c.id, gherrit_id))
        //             }
        //         },
        //     )
        //     .try_collect::<Vec<_>>()
        //     .unwrap();

        // for (head, gherrit_id) in commits {
        //     println!("{head}:refs/heads/{gherrit_id}");
        // }
    }
}

struct Commit<'a> {
    id: ObjectId,
    gherrit_id: &'a str,
    message_title: &'a str,
    message_body: &'a str,
    // inner: &'a gix::Commit<'a>,
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
            // TODO: Only compile this regex once.
            let re = regex::Regex::new(r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$").unwrap();
            // TODO: Return error here instead of unwrapping.
            let captures = re.captures(&message_body).unwrap();
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

trait RepositoryExt {
    fn gherrit_head_commit(&self, id: &str) -> Result<Option<Target>, Box<dyn Error>>;
}

impl RepositoryExt for Repository {
    fn gherrit_head_commit(&self, id: &str) -> Result<Option<Target>, Box<dyn Error>> {
        let r = self.refs.try_find(&format!("refs/gherrit/{id}"))?;
        Ok(r.map(|r| r.target))
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
    let re =
        regex::Regex::new("https://github.com/[a-zA-Z0-9]+/[a-zA-Z0-9]+/pull/([0-9]+)").unwrap();
    let captures = re.captures(&output).unwrap();
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

    Ok(c.stdout(Stdio::null()).status()?)
}

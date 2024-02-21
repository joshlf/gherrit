#![feature(iterator_try_collect, iter_intersperse)]

use std::{
    error::Error,
    ffi::OsStr,
    process::{Command, Stdio},
};

use gix::{
    refs::{transaction::PreviousValue, Target},
    Repository,
};
use serde::{Deserialize, Serialize};

const KEEP_FULL_HISTORY: bool = false;

fn main() {
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

    let commits = commits
        .iter()
        .map(Commit::try_from_gix)
        .try_collect::<Vec<_>>()
        .unwrap();

    if !KEEP_FULL_HISTORY {
        let commits = commits
            .into_iter()
            .map(|c| -> Result<_, Box<dyn Error>> {
                let gherrit_id = c.gherrit_id;
                let rf = format!("refs/gherrit/{gherrit_id}");
                let _ = repo.reference(rf, c.inner.id, PreviousValue::Any, "")?;
                Ok((c, gherrit_id))
            })
            .try_collect::<Vec<_>>()
            .unwrap();

        let mut args = vec![
            "push".to_string(),
            "--force".to_string(),
            "--quiet".to_string(),
            "origin".to_string(),
        ];
        args.extend(
            commits
                .iter()
                .map(|(c, gherrit_id)| format!("{}:refs/heads/{gherrit_id}", c.inner.id())),
        );
        cmd("git", args).status().unwrap();

        let pr_list = cmd("gh", ["pr", "list", "--json", "number,headRefName"])
            .output()
            .unwrap();

        #[derive(Serialize, Deserialize, Debug)]
        #[serde(rename_all = "camelCase")]
        struct ListEntry {
            head_ref_name: String,
            number: usize,
        }

        let prs: Vec<ListEntry> = serde_json::from_slice(&pr_list.stdout).unwrap();

        println!("{prs:?}");

        let mut parent_branch = "main";
        let commits = commits
            .into_iter()
            .map(|(c, gherrit_id)| -> Result<_, Box<dyn Error>> {
                let pr_num = prs.iter().find_map(|pr| {
                    if &pr.head_ref_name == gherrit_id {
                        Some(pr.number)
                    } else {
                        None
                    }
                });

                let pr_num = if let Some(pr_num) = pr_num {
                    let pr_num_str = format!("{pr_num}");
                    let mut c = cmd(
                        "gh",
                        [
                            "pr",
                            "edit",
                            &pr_num_str,
                            "--base",
                            &parent_branch,
                            "--title",
                            c.message_title,
                            "--body",
                            c.message_body,
                        ],
                    );
                    c.stdout(Stdio::null()).status()?;

                    pr_num
                } else {
                    println!("No GitHub PR exists for {gherrit_id}; creating one...");
                    let mut c = cmd(
                        "gh",
                        [
                            "pr",
                            "create",
                            "--base",
                            &parent_branch,
                            "--head",
                            &gherrit_id,
                            "--title",
                            c.message_title,
                            "--body",
                            c.message_body,
                        ],
                    );
                    let output = c.stderr(Stdio::inherit()).output()?;

                    let output = core::str::from_utf8(&output.stdout)?;
                    let re = regex::Regex::new(
                        "https://github.com/[a-zA-Z0-9]+/[a-zA-Z0-9]+/pull/([0-9]+)",
                    )
                    .unwrap();
                    let captures = re.captures(&output).unwrap();
                    let pr_id = captures.get(1).unwrap();
                    pr_id.as_str().parse()?
                };

                parent_branch = gherrit_id;

                Ok((c, pr_num))
            })
            .try_collect::<Vec<_>>()
            .unwrap();

        // A markdown bulleted list of links to each PR, with the "top" PR (the
        // furthest from `main`) at the top of the list.
        let gh_pr_ids_markdown = commits
            .iter()
            .rev()
            .map(|(_, pr_num)| format!("- #{pr_num}"))
            .intersperse("\n".to_string())
            .collect::<String>();

        // TODO: These take a long time; run them in parallel.
        for (c, pr_num) in commits {
            let body = c.message_body;
            let body = format!("{body}\n\n---\n\n{gh_pr_ids_markdown}");
            let mut gh_pr_edit = Command::new("gh");
            let pr_num = format!("{pr_num}");
            gh_pr_edit.args(["pr", "edit", &pr_num, "--body", &body]);
            gh_pr_edit.status().unwrap();
        }
    } else {
        let commits = commits
            .into_iter()
            .map(|c| -> Result<_, Box<dyn Error>> {
                let gherrit_id = c.gherrit_id;
                let head_commit = repo.gherrit_head_commit(gherrit_id)?;
                Ok((c, gherrit_id, head_commit))
            })
            .try_collect::<Vec<_>>()
            .unwrap();

        let commits = commits
            .into_iter()
            .map(
                |(c, gherrit_id, head_commit)| -> Result<_, Box<dyn Error>> {
                    let rf = format!("refs/gherrit/{gherrit_id}");
                    if let Some(head_commit) = head_commit {
                        // We already have an existing history, so we need to
                        // add to it.
                        //
                        // TODO: If nothing has changed about the commit
                        // (contents, message, parents, etc), then we can
                        // short-circuit and not create a new commit in this
                        // Gherrit PR's history.
                        let tree = c.inner.tree()?;
                        let message = core::str::from_utf8(c.inner.message_raw()?)?;

                        // TODO: `head_commit.id()` can panic.
                        let new_head = repo.commit(rf, message, tree.id, [head_commit.id()])?;
                        Ok((new_head, gherrit_id))
                    } else {
                        // We don't have an existing history, so initialize the
                        // history starting with this commit.
                        let _ = repo.reference(rf, c.inner.id, PreviousValue::Any, "")?;
                        Ok((c.inner.id(), gherrit_id))
                    }
                },
            )
            .try_collect::<Vec<_>>()
            .unwrap();

        for (head, gherrit_id) in commits {
            println!("{head}:refs/heads/{gherrit_id}");
        }
    }
}

struct Commit<'a> {
    gherrit_id: &'a str,
    message_title: &'a str,
    message_body: &'a str,
    inner: &'a gix::Commit<'a>,
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
            gherrit_id,
            message_title,
            message_body,
            inner: c,
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

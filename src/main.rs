#![feature(iterator_try_collect)]

use std::error::Error;

use gix::{
    refs::{transaction::PreviousValue, Target},
    Commit, Repository,
};

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
        .into_iter()
        .map(|c| -> Result<_, Box<dyn Error>> {
            let gherrit_id = c.gherrit_pr_id()?;
            let head_commit = repo.gherrit_head_commit(gherrit_id.clone())?;
            println!("{c:?} => {gherrit_id} => {head_commit:?}");
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
                    // We already have an existing history, so we need to add to
                    // it.
                    //
                    // TODO: If nothing has changed about the commit (contents,
                    // message, parents, etc), then we can short-circuit and not
                    // create a new commit in this Gherrit PR's history.
                    let tree = c.tree()?;
                    let message = core::str::from_utf8(c.message_raw()?)?;

                    // TODO: `head_commit.id()` can panic.
                    let new_head = repo.commit(rf, message, tree.id, [head_commit.id()])?;
                    Ok((new_head, gherrit_id))
                } else {
                    // We don't have an existing history, so initialize the
                    // history starting with this commit.
                    let _ = repo.reference(rf, c.id, PreviousValue::Any, "")?;
                    Ok((c.id(), gherrit_id))
                }
            },
        )
        .try_collect::<Vec<_>>()
        .unwrap();

    for (head, gherrit_id) in commits {
        println!("{head}:refs/heads/{gherrit_id}");
    }
}

type GherritId = String;

trait CommitExt {
    fn gherrit_pr_id(&self) -> Result<GherritId, Box<dyn Error>>;
}

impl<'repo> CommitExt for Commit<'repo> {
    fn gherrit_pr_id(&self) -> Result<GherritId, Box<dyn Error>> {
        let msg = self.message()?;
        let Some(body) = msg.body else { todo!() };
        // TODO: Only compile this regex once.
        let re = regex::bytes::Regex::new(r"(?m)^gherrit-pr-id: ([a-zA-Z0-9]*)$").unwrap();
        // TODO: Return error here instead of unwrapping.
        let captures = re.captures(&body).unwrap();
        let gherrit_id = captures.get(1).unwrap();
        // This can't fail because the regex only matches UTF-8.
        let gherrit_id = core::str::from_utf8(gherrit_id.as_bytes()).unwrap();
        Ok(gherrit_id.to_string())
    }
}

trait RepositoryExt {
    fn gherrit_head_commit(&self, id: GherritId) -> Result<Option<Target>, Box<dyn Error>>;
}

impl RepositoryExt for Repository {
    fn gherrit_head_commit(&self, id: GherritId) -> Result<Option<Target>, Box<dyn Error>> {
        let r = self.refs.try_find(&format!("refs/gherrit/{id}"))?;
        Ok(r.map(|r| r.target))
    }
}

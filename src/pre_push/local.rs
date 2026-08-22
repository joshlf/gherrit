use std::{collections::HashSet, str};

use color_eyre::eyre::{Context, Result, bail, eyre};
use gix::ObjectId;

use super::{autosquash, body::gherrit_pr_id_re};
use crate::util::{self, CommandExt as _};

pub(super) fn collect_commits(repo: &util::Repo) -> Result<Vec<Commit>> {
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

    let commits = commits
        .into_iter()
        .map(|commit| -> Result<_> {
            let title = core::str::from_utf8(commit.message()?.title)?.to_owned();
            Ok((commit, title))
        })
        .collect::<Result<Vec<_>>>()?;

    autosquash::ensure_publishable(
        commits.iter().map(|(_, title)| title.as_str()),
        &repo.default_remote_name(),
        &default_branch,
    )?;

    let trailers = read_commit_trailers(&commits)?;
    let commits = commits
        .into_iter()
        .zip(trailers)
        .map(|((commit, _), trailers)| Commit::from_git(commit, &trailers))
        .collect::<Result<Vec<_>>>()?;
    ensure_unique_gherrit_ids(commits.iter().map(|commit| commit.gherrit_id.as_str()))?;
    Ok(commits)
}

fn read_commit_trailers(commits: &[(gix::Commit<'_>, String)]) -> Result<Vec<Vec<u8>>> {
    const QUERY_BATCH_LEN: usize = 120;
    const FORMAT: &str = "--format=tformat:%H%x00%(trailers:only,unfold)";

    commits.chunks(QUERY_BATCH_LEN).try_fold(
        Vec::with_capacity(commits.len()),
        |mut parsed, chunk| {
            let arguments = ["log", "--no-walk=unsorted", "-z", FORMAT]
                .into_iter()
                .map(ToString::to_string)
                .chain(chunk.iter().map(|(commit, _)| commit.id.to_string()));
            let output = util::cmd("git", arguments)
                .checked_output()
                .wrap_err("Failed to parse commit trailers")?;
            let mut fields = output.stdout.split(|byte| *byte == 0);

            chunk.iter().try_for_each(|(commit, _)| {
                let object_id = fields
                    .next()
                    .ok_or_else(|| eyre!("Git omitted trailer data for commit {}", commit.id))?;
                if object_id != commit.id.to_string().as_bytes() {
                    bail!("Git returned commit trailers out of order");
                }
                let trailers = fields
                    .next()
                    .ok_or_else(|| eyre!("Git omitted trailer data for commit {}", commit.id))?;
                parsed.push(trailers.to_vec());
                Ok(())
            })?;

            if fields.next() != Some(&[][..]) || fields.next().is_some() {
                bail!("Git returned malformed commit trailer data");
            }
            Ok(parsed)
        },
    )
}

fn ensure_unique_gherrit_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<()> {
    ids.into_iter().try_fold(HashSet::new(), |mut seen, id| {
        if !seen.insert(id) {
            bail!("Stack contains multiple commits with gherrit-pr-id '{id}'");
        }
        Ok(seen)
    })?;
    Ok(())
}

pub(super) struct Commit {
    pub(super) id: ObjectId,
    pub(super) gherrit_id: String,
    pub(super) message_title: String,
    pub(super) message_body: String,
}

impl Commit {
    fn from_git(c: gix::Commit<'_>, trailers: &[u8]) -> Result<Self> {
        let message = c.message()?;
        let message_title = core::str::from_utf8(message.title)?.to_string();
        let message_body =
            message.body.map(|body| core::str::from_utf8(body).unwrap()).unwrap_or("").to_string();
        let mut gherrit_ids = trailers
            .split(|byte| *byte == b'\n')
            .filter_map(|line| line.strip_prefix(b"gherrit-pr-id: "));
        let gherrit_id = gherrit_ids
            .next()
            .ok_or_else(|| eyre!("Commit {} missing gherrit-pr-id trailer", c.id))?;
        if gherrit_ids.next().is_some() {
            bail!("Commit {} has multiple gherrit-pr-id trailers", c.id);
        }
        if gherrit_id.is_empty() {
            bail!("Commit {} missing gherrit-pr-id trailer", c.id);
        }
        if !gherrit_id.iter().all(u8::is_ascii_alphanumeric) {
            bail!("Commit {} has invalid gherrit-pr-id trailer", c.id);
        }
        let gherrit_id = str::from_utf8(gherrit_id)?.to_string();
        let message_body = strip_gherrit_id(&message_body, &gherrit_id);

        Ok(Commit { id: c.id, gherrit_id, message_title, message_body })
    }
}

fn strip_gherrit_id(body: &str, id: &str) -> String {
    let trailer_start = body
        .rfind("\n\n")
        .map(|position| position + 2)
        .into_iter()
        .chain(body.rfind("\r\n\r\n").map(|position| position + 4))
        .max()
        .unwrap_or(0);
    let matching_trailer = gherrit_pr_id_re()
        .captures_iter(&body[trailer_start..])
        .filter(|captures| captures.get(1).is_some_and(|value| value.as_str() == id))
        .filter_map(|captures| captures.get(0))
        .last();
    let Some(trailer) = matching_trailer else {
        return body.to_string();
    };

    let mut body = body.to_string();
    let range = trailer.range();
    body.replace_range(trailer_start + range.start..trailer_start + range.end, "");
    body
}
